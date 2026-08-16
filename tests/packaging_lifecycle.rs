//! Packaging and lifecycle acceptance at the real process boundary (ticket 29)
//! and the Windows login task / console launcher (ticket 30), plus versioned
//! upgrades and recoverable rollback (ticket 31).
//!
//! Covers the PKG-002..PKG-008 / PKG-009..PKG-012 / PKG-013..PKG-014 /
//! SEC-001 / SEC-005 packaging contract: the versioned archive and idempotent
//! installer, the stable user-level entry and XDG layout with owner-only
//! permissions, the fixed start/stop/restart/status lifecycle commands,
//! default and explicit listening ports, loopback-only binding and
//! non-loopback connection refusal, the ready boundary, non-zero exit on
//! startup failures, bounded graceful stop with SIGTERM, the
//! bootstrap-credential secrecy boundary, the per-user Windows login
//! scheduled task with bounded restart, the desktop console launcher, and the
//! side-by-side upgrade / rollback orchestration. Everything runs through the
//! real installed binary and the real packaging scripts over loopback — no
//! internals are reached into.

use reqwest::{Client, StatusCode, header};
use serde_json::json;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Test hook that shrinks the graceful-stop drain bound (PKG-012, default
/// 30 seconds) so the deadline is observable at the process boundary. The same
/// variable name the server reads.
const TEST_DRAIN_GRACE_VARIABLE: &str = "LOCAL_API_RELAY_TEST_SHUTDOWN_GRACE_MS";

struct PackagingEnvironment {
    root: PathBuf,
    /// A throwaway Windows scheduled-task name created through this
    /// environment (ticket 30), removed on drop so failed runs never leak a
    /// task on the real Windows host.
    windows_task: Option<String>,
}

impl PackagingEnvironment {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "local-api-relay-pkg-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            root,
            windows_task: None,
        }
    }

    fn register_windows_task(&mut self, task_name: &str) {
        assert!(
            self.windows_task.is_none(),
            "a Windows task is already registered for this environment"
        );
        self.windows_task = Some(task_name.to_owned());
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn data_home(&self) -> PathBuf {
        self.root.join("data")
    }

    fn config_home(&self) -> PathBuf {
        self.root.join("config")
    }

    fn state_home(&self) -> PathBuf {
        self.root.join("state")
    }

    fn apply_env(&self, command: &mut Command) {
        command
            .env("HOME", self.home())
            .env("XDG_DATA_HOME", self.data_home())
            .env("XDG_CONFIG_HOME", self.config_home())
            .env("XDG_STATE_HOME", self.state_home());
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_local-api-relay"));
        self.apply_env(&mut command);
        command
    }

    fn initialize(&self) -> String {
        let output = self.command().arg("init-admin").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout
            .strip_prefix("Administrator bootstrap credential: ")
            .expect("initialization prints the one-time credential")
            .trim()
            .to_owned()
    }

    fn start(&self, port: u16, extra_env: &[(&str, &str)]) -> Child {
        let mut command = self.command();
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command
            .args(["serve", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}

impl Drop for PackagingEnvironment {
    fn drop(&mut self) {
        // Best-effort: remove a registered throwaway Windows scheduled task
        // even when a test panicked, so failed runs never leak a task on the
        // real Windows host.
        if let Some(task_name) = &self.windows_task {
            delete_windows_task(task_name);
        }
        // Best-effort: stop any service process this environment spawned, even
        // when a test panicked before calling stop, so failed runs never leak
        // detached relays.
        if let Ok(output) = Command::new("pgrep")
            .arg("-f")
            .arg(self.root.display().to_string())
            .output()
            && output.status.success()
        {
            for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
                let _ = Command::new("kill").arg("-KILL").arg(pid).status();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

async fn wait_ready(client: &Client, port: u16) {
    for _ in 0..100 {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/ready"))
            .send()
            .await
            && response.status() == StatusCode::OK
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("service did not become ready");
}

fn session_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .next()
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn activate_administrator(client: &Client, base: &str, credential: &str) -> String {
    let login = client
        .post(format!("{base}/admin/login"))
        .json(&json!({ "password": credential }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let bootstrap_cookie = session_cookie(&login);
    let changed = client
        .post(format!("{base}/admin/change-password"))
        .header(header::COOKIE, &bootstrap_cookie)
        .json(&json!({ "new_password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::OK);
    let login = client
        .post(format!("{base}/admin/login"))
        .json(&json!({ "password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    session_cookie(&login)
}

struct CapturedProbe {
    request_line: String,
}

fn read_http_request(stream: &mut TcpStream) -> CapturedProbe {
    let reader_stream = stream.try_clone().unwrap();
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    CapturedProbe {
        request_line: request_line.trim_end().to_owned(),
    }
}

fn complete_chat_response() -> String {
    r#"{"id":"chatcmpl-scripted","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#.to_owned()
}

/// Serves the route-creation probe as success, then accepts one real call and
/// holds it until the test releases before writing success. The final write
/// tolerates a closed connection (the relay cancels the call on a drain
/// deadline and closes the upstream socket).
fn holding_call_upstream() -> (
    String,
    Receiver<CapturedProbe>,
    Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let response = complete_chat_response();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        stream.flush().unwrap();
        sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let call = read_http_request(&mut stream);
        sender.send(call).unwrap();
        let _ = release_receiver.recv();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        let _ = stream.flush();
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        receiver,
        release_sender,
        worker,
    )
}

async fn configure_relay_route(
    client: &Client,
    base: &str,
    active_cookie: &str,
    base_url: String,
    protocol: &str,
    upstream_model_name: &str,
) -> String {
    let provider_id = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({
            "display_name": format!("{protocol} route"),
            "base_url": base_url,
            "api_key": "packaging-route-key"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": upstream_model_name,
            "protocol": protocol,
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn create_relay_secret(
    client: &Client,
    base: &str,
    active_cookie: &str,
    route_id: &str,
) -> String {
    client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({ "label": "lifecycle scope", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn terminate(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success(), "could not send SIGTERM to the service");
}

/// Polls an upstream probe without blocking the tokio runtime thread, so that
/// spawned relay calls can make progress while the test waits.
async fn await_probe(receiver: &Receiver<CapturedProbe>) -> CapturedProbe {
    for _ in 0..120 {
        if let Ok(probe) = receiver.try_recv() {
            return probe;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("upstream probe did not arrive");
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// The managed rotating state log written by the service (OPS-019).
fn managed_log_path(environment: &PackagingEnvironment) -> PathBuf {
    environment
        .state_home()
        .join("local-api-relay/logs/relay.log")
}

fn log_contains_event(environment: &PackagingEnvironment, code: &str) -> bool {
    let Ok(contents) = fs::read_to_string(managed_log_path(environment)) else {
        return false;
    };
    contents.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| value["event"].as_str().map(str::to_owned))
            .is_some_and(|event| event == code)
    })
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode()
}

/// PKG-012: after SIGTERM the service stops accepting new calls, lets an
/// in-flight call finish inside the 30-second bound, emits the process stop
/// event, and exits cleanly with status 0.
#[tokio::test]
async fn sigterm_drains_in_flight_calls_and_exits_cleanly() {
    let environment = PackagingEnvironment::new("drain-finish");
    let bootstrap_credential = environment.initialize();
    let client = Client::new();
    let port = available_port();
    let mut server = environment.start(port, &[]);
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream, probes, release, upstream_worker) = holding_call_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream,
        "chat_completions",
        "drain-model",
    )
    .await;
    let probe = await_probe(&probes).await;
    assert!(probe.request_line.contains("/v1/chat/completions"));
    let relay_secret = create_relay_secret(&client, &base, &active_cookie, &route_id).await;

    // A real client call that is in flight (held at the upstream) when the
    // SIGTERM arrives.
    let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
    let call_base = base.clone();
    let call_secret = relay_secret.clone();
    let call_worker = tokio::spawn(async move {
        let outcome = Client::new()
            .post(format!("{call_base}/v1/chat/completions"))
            .bearer_auth(&call_secret)
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "hello" }],
                "stream": false
            }))
            .send()
            .await
            .map(|response| response.status().as_u16());
        let _ = result_sender.send(outcome);
    });
    let call_probe = await_probe(&probes).await;
    assert!(call_probe.request_line.contains("/v1/chat/completions"));

    // Graceful stop while the call is in flight.
    terminate(&server);
    release.send(()).unwrap();
    let call_status = result_receiver.await.unwrap();
    assert!(
        matches!(call_status, Ok(200)),
        "the in-flight call must complete inside the drain bound"
    );
    call_worker.await.unwrap();
    upstream_worker.join().unwrap();

    let exit = wait_for_exit(&mut server, Duration::from_secs(10));
    assert!(
        exit.success(),
        "a drained service must exit with status 0, got {exit}"
    );
    assert!(
        log_contains_event(&environment, "process.stopped"),
        "a clean drain must record the process.stopped event"
    );
}

/// PKG-012: when in-flight calls exceed the drain bound, the service cancels
/// the remaining calls, closes its resources, emits the drain event, and still
/// exits cleanly with status 0.
#[tokio::test]
async fn sigterm_drain_deadline_cancels_remaining_calls() {
    let environment = PackagingEnvironment::new("drain-deadline");
    let bootstrap_credential = environment.initialize();
    let client = Client::new();
    let port = available_port();
    let mut server = environment.start(
        port,
        &[(TEST_DRAIN_GRACE_VARIABLE, "400")],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream, probes, release, upstream_worker) = holding_call_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream,
        "chat_completions",
        "drain-model",
    )
    .await;
    await_probe(&probes).await;
    let relay_secret = create_relay_secret(&client, &base, &active_cookie, &route_id).await;

    let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
    let call_base = base.clone();
    let call_secret = relay_secret.clone();
    let call_worker = tokio::spawn(async move {
        let outcome = Client::new()
            .post(format!("{call_base}/v1/chat/completions"))
            .bearer_auth(&call_secret)
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "hello" }],
                "stream": false
            }))
            .send()
            .await
            .map(|response| response.status().as_u16());
        let _ = result_sender.send(outcome);
    });
    let call_probe = await_probe(&probes).await;
    assert!(call_probe.request_line.contains("/v1/chat/completions"));

    // The call stays in flight; SIGTERM must cancel it at the 400 ms drain
    // deadline rather than waiting for the upstream forever.
    let started = Instant::now();
    terminate(&server);
    let exit = wait_for_exit(&mut server, Duration::from_secs(8));
    let elapsed = started.elapsed();
    assert!(
        exit.success(),
        "a drain-deadline exit must still be status 0, got {exit}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "the drain deadline must bound the stop, took {elapsed:?}"
    );
    assert!(
        log_contains_event(&environment, "process.drain_expired"),
        "an expired drain must record the process.drain_expired event"
    );

    // The cancelled call never completed: the client saw an aborted connection,
    // not a successful response.
    let call_status = tokio::time::timeout(Duration::from_secs(5), result_receiver)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !matches!(call_status, Ok(200)),
        "a call cancelled at the drain deadline must not succeed"
    );
    call_worker.await.unwrap();
    release.send(()).unwrap();
    upstream_worker.join().unwrap();
}

// ---------------------------------------------------------------------------
// Packaging helpers (slices 2-4): the versioned archive, idempotent installer,
// and the fixed lifecycle commands, exercised through the real scripts.
// ---------------------------------------------------------------------------

/// The packaging scripts shipped with the repo and inside the archive.
fn packaging_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("packaging")
}

fn installed_root(environment: &PackagingEnvironment) -> PathBuf {
    environment.home().join(".local")
}

fn stable_entry(environment: &PackagingEnvironment) -> PathBuf {
    installed_root(environment).join("bin/local-api-relay")
}

fn service_script(environment: &PackagingEnvironment) -> PathBuf {
    installed_root(environment).join("bin/local-api-relay-service")
}

fn launcher_script(environment: &PackagingEnvironment) -> PathBuf {
    installed_root(environment).join("bin/local-api-relay-launcher")
}

fn versioned_program_dir(environment: &PackagingEnvironment) -> PathBuf {
    installed_root(environment).join(format!(
        "opt/local-api-relay/{}/bin",
        env!("CARGO_PKG_VERSION")
    ))
}

/// One stripped copy of the test binary, shared by every staging archive in
/// this test process. The debug image is ~100 MB, so each environment staging
/// its own copy would overflow the /tmp tmpfs when the packaging tests run in
/// parallel; the stripped result is ~16 MB and the archives hardlink it (the
/// same idiom install.sh uses with `cp -l`). The cache is stripped once,
/// before any archive hardlinks to it, so the inode is never rewritten while
/// a service is executing from it.
fn shared_stripped_binary() -> PathBuf {
    use std::sync::OnceLock;
    static SHARED: OnceLock<PathBuf> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let cache = std::env::temp_dir().join(format!(
                "local-api-relay-stripped-{}-{}",
                env!("CARGO_PKG_VERSION"),
                std::process::id()
            ));
            if !cache.exists() {
                fs::copy(env!("CARGO_BIN_EXE_local-api-relay"), &cache).unwrap();
                let _ = Command::new("strip").arg(&cache).status();
            }
            cache
        })
        .clone()
}

/// Stages an archive-shaped directory exactly like `build-archive.sh` does:
/// the binary plus the idempotent installer and lifecycle script at the top
/// level. The test binary stands in for the release binary and is stripped so
/// the parallel packaging tests do not copy 100 MB debug images around.
fn staging_archive(environment: &PackagingEnvironment) -> PathBuf {
    let archive = environment.root.join("archive");
    // Idempotent: a later call (for example reading the scripts back during a
    // secrecy scan) must not re-copy over a binary that a running service is
    // executing from, which would fail with ETXTBSY.
    if archive.join("install.sh").exists() {
        return archive;
    }
    fs::create_dir_all(&archive).unwrap();
    let staged_binary = archive.join("local-api-relay");
    if fs::hard_link(shared_stripped_binary(), &staged_binary).is_err() {
        fs::copy(shared_stripped_binary(), &staged_binary).unwrap();
    }
    fs::copy(packaging_dir().join("install.sh"), archive.join("install.sh")).unwrap();
    fs::copy(
        packaging_dir().join("local-api-relay-service"),
        archive.join("local-api-relay-service"),
    )
    .unwrap();
    archive
}

fn run_install(environment: &PackagingEnvironment) -> std::process::Output {
    let mut command = Command::new("bash");
    environment.apply_env(&mut command);
    // The Windows login task (PKG-005) is a real Windows-side mutation; the
    // Linux-side packaging tests keep installs hermetic and opt in through
    // run_install_with_windows_task.
    command.env("LOCAL_API_RELAY_WINDOWS_TASK_SKIP", "1");
    command
        .arg(staging_archive(environment).join("install.sh"))
        .output()
        .unwrap()
}

/// Installs with the Windows login task enabled under a throwaway per-test
/// task name (ticket 30, PKG-005/006). Returns the task name, registered on
/// the environment so the task is deleted on drop. Repeat installs reuse the
/// already-registered name so idempotency can be asserted.
fn run_install_with_windows_task(environment: &mut PackagingEnvironment) -> String {
    let task_name = match &environment.windows_task {
        Some(name) => name.clone(),
        None => {
            let name = format!(
                "larr-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            environment.register_windows_task(&name);
            name
        }
    };
    let mut command = Command::new("bash");
    environment.apply_env(&mut command);
    let output = command
        .env("LOCAL_API_RELAY_WINDOWS_TASK_SKIP", "0")
        .env("LOCAL_API_RELAY_WINDOWS_TASK_NAME", &task_name)
        .arg(staging_archive(environment).join("install.sh"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    task_name
}

/// Whether Windows interop is available so scheduled-task and loopback tests
/// can run; on a Linux-only host these tests are skipped rather than failing.
fn windows_interop_available() -> bool {
    Command::new("schtasks.exe")
        .arg("/?")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn delete_windows_task(task_name: &str) {
    let _ = Command::new("schtasks.exe")
        .args(["/Delete", "/TN", task_name, "/F"])
        .output();
}

/// The raw exported definition of a scheduled task (`schtasks /Query /XML`).
fn schtasks_bytes(task_name: &str) -> Vec<u8> {
    let output = Command::new("schtasks.exe")
        .args(["/Query", "/TN", task_name, "/XML"])
        .output()
        .expect("schtasks query");
    assert!(
        output.status.success(),
        "schtasks /Query failed for {task_name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// The exported definition of a scheduled task, decoded to text.
fn schtasks_xml(task_name: &str) -> String {
    decode_windows_output(&schtasks_bytes(task_name))
}

/// schtasks.exe and friends emit UTF-16LE with a BOM when their stdout is a
/// pipe and the console codepage otherwise. The assertions in these tests
/// target ASCII substrings, which survive both encodings unchanged.
fn decode_windows_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let utf16: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&utf16)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn run_installed(
    environment: &PackagingEnvironment,
    program: &Path,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(program);
    environment.apply_env(&mut command);
    command.args(args).output().unwrap()
}

fn service_output(environment: &PackagingEnvironment, args: &[&str]) -> std::process::Output {
    run_installed(environment, &service_script(environment), args)
}

fn service_output_with_env(
    environment: &PackagingEnvironment,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new(service_script(environment));
    environment.apply_env(&mut command);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.args(args).output().unwrap()
}

fn write_service_config(environment: &PackagingEnvironment, port: u16) {
    let config_dir = environment
        .config_home()
        .join("local-api-relay");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("service.json"),
        serde_json::to_vec_pretty(&json!({ "port": port })).unwrap(),
    )
    .unwrap();
}

fn read_pidfile(environment: &PackagingEnvironment) -> Option<u32> {
    let path = environment
        .state_home()
        .join("local-api-relay/service.pid");
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok())
}

fn captured_log_path(environment: &PackagingEnvironment) -> PathBuf {
    environment
        .state_home()
        .join("local-api-relay/logs/serve.log")
}

/// The pids of the serve processes this environment's installed stable entry
/// spawned, so lifecycle tests can assert the service is a single process
/// without seeing unrelated relays from other tests or the host.
fn serve_process_pids(environment: &PackagingEnvironment) -> Vec<u32> {
    let pattern = format!("{} serve", stable_entry(environment).display());
    let output = Command::new("pgrep").arg("-f").arg(&pattern).output().unwrap();
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
        .collect()
}

fn wait_for_no_serve_processes(environment: &PackagingEnvironment, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !serve_process_pids(environment).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the serve process did not exit in time"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Slice 2: the versioned archive, installer, layout, and owner-only boundary.
// ---------------------------------------------------------------------------

#[test]
fn install_lays_out_versioned_files_behind_a_stable_entry() {
    let environment = PackagingEnvironment::new("install-layout");
    let output = run_install(&environment);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Versioned program files are installed side by side under the XDG-style
    // per-user opt tree, and the stable user-level entry selects the version.
    let versioned_binary = versioned_program_dir(&environment).join("local-api-relay");
    assert!(
        versioned_binary.is_file(),
        "versioned binary missing at {}",
        versioned_binary.display()
    );
    let entry_target = fs::read_link(stable_entry(&environment)).unwrap();
    assert_eq!(
        entry_target,
        versioned_binary,
        "the stable entry must select the installed version"
    );

    // The stable entry runs the real binary.
    let version_output = run_installed(&environment, &stable_entry(&environment), &["--version"]);
    assert!(version_output.status.success());
    assert!(
        String::from_utf8_lossy(&version_output.stdout).contains(env!("CARGO_PKG_VERSION")),
        "the stable entry must report the installed version"
    );

    // The lifecycle script is installed at a stable user-level path.
    assert!(service_script(&environment).is_file());

    // XDG application directories exist with the owner-only boundary.
    for directory in [
        environment
            .data_home()
            .join("local-api-relay"),
        environment
            .config_home()
            .join("local-api-relay"),
        environment
            .state_home()
            .join("local-api-relay"),
    ] {
        assert!(directory.is_dir(), "XDG app dir missing: {}", directory.display());
        #[cfg(unix)]
        assert_eq!(
            mode(&directory) & 0o077,
            0,
            "XDG app dir must be owner-only: {}",
            directory.display()
        );
    }
}

#[test]
fn install_is_idempotent_and_repeated_install_keeps_the_layout() {
    let environment = PackagingEnvironment::new("install-repeat");
    let first = run_install(&environment);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let second = run_install(&environment);
    assert!(
        second.status.success(),
        "re-installation must be idempotent: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read_link(stable_entry(&environment)).unwrap(),
        versioned_program_dir(&environment).join("local-api-relay"),
        "re-installation must keep the stable entry on the installed version"
    );
    assert!(service_script(&environment).is_file());
}

#[test]
fn installed_tree_is_self_contained_and_owner_only() {
    let environment = PackagingEnvironment::new("install-tree");
    let output = run_install(&environment);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    // The installed program tree has no separate frontend directory (PKG-004):
    // the console assets are embedded in the binary.
    let tree = installed_root(&environment);
    let mut stack = vec![tree.clone()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                assert_ne!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("assets"),
                    "the installed tree must not carry a separate frontend directory"
                );
                stack.push(path);
            }
        }
    }

    // Every installed program directory is owner-only (PKG-004).
    for directory in [
        installed_root(&environment).join("bin"),
        installed_root(&environment).join("opt"),
        installed_root(&environment).join("opt/local-api-relay"),
        versioned_program_dir(&environment),
    ] {
        #[cfg(unix)]
        assert_eq!(
            mode(&directory) & 0o077,
            0,
            "installed program dir must be owner-only: {}",
            directory.display()
        );
    }
    #[cfg(unix)]
    assert_eq!(
        mode(&versioned_program_dir(&environment).join("local-api-relay")) & 0o077,
        0,
        "the installed binary must be owner-only"
    );

    // Self-contained: the only dynamic dependencies are the base runtime
    // libraries — never Node, an external SQLite, OpenSSL, or a package
    // repository (PKG-002).
    let ldd = Command::new("ldd")
        .arg(versioned_program_dir(&environment).join("local-api-relay"))
        .output()
        .unwrap();
    assert!(ldd.status.success());
    let dependencies = String::from_utf8_lossy(&ldd.stdout);
    assert!(
        !dependencies.contains("libnode") && !dependencies.contains("libsqlite3"),
        "the binary must not link Node or an external SQLite: {dependencies}"
    );
    assert!(
        !dependencies.contains("libssl") && !dependencies.contains("libcrypto"),
        "the binary must not link OpenSSL: {dependencies}"
    );
    assert!(
        dependencies.contains("libc.so.6") && dependencies.contains("ld-linux"),
        "the binary must link only the base runtime: {dependencies}"
    );
}

// ---------------------------------------------------------------------------
// Slice 3: the fixed lifecycle commands against a real installed process.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_commands_start_status_restart_and_stop_a_single_process() {
    let environment = PackagingEnvironment::new("lifecycle");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let port = available_port();
    write_service_config(&environment, port);
    let client = Client::new();

    // start: becomes ready on the configured port and records one pid.
    let start = service_output(&environment, &["start"]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stdout)
    );
    wait_ready(&client, port).await;
    let first_pid = read_pidfile(&environment).expect("start must write a pidfile");
    assert_eq!(
        serve_process_pids(&environment),
        vec![first_pid],
        "the service must be exactly one process"
    );

    // status: running with exit code 0.
    let status = service_output(&environment, &["status"]);
    assert!(status.status.success(), "status must exit 0 while running");
    assert!(String::from_utf8_lossy(&status.stdout).contains("running"));

    // start again while running: idempotent, still one process.
    let start_again = service_output(&environment, &["start"]);
    assert!(
        start_again.status.success(),
        "an idempotent start must succeed: {}",
        String::from_utf8_lossy(&start_again.stdout)
    );
    assert_eq!(
        serve_process_pids(&environment),
        vec![first_pid],
        "a second start must not spawn another process"
    );

    // restart: a fresh process takes over the same port.
    let restart = service_output(&environment, &["restart"]);
    assert!(
        restart.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restart.stdout)
    );
    wait_ready(&client, port).await;
    let second_pid = read_pidfile(&environment).expect("restart must keep a pidfile");
    assert_ne!(second_pid, first_pid, "restart must start a fresh process");
    assert_eq!(serve_process_pids(&environment), vec![second_pid]);

    // stop: the process exits, the pidfile is removed, and status reports
    // stopped with the not-running exit code.
    let stop = service_output(&environment, &["stop"]);
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stdout)
    );
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
    assert!(read_pidfile(&environment).is_none());
    let stopped = service_output(&environment, &["status"]);
    assert_eq!(stopped.status.code(), Some(3), "a stopped service must report status 3");
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("stopped"));

    // stop while stopped stays a successful no-op.
    let stop_again = service_output(&environment, &["stop"]);
    assert!(
        stop_again.status.success(),
        "stopping a stopped service must be a no-op success"
    );
}

#[test]
fn lifecycle_launcher_captures_structured_stderr_into_the_state_log() {
    let environment = PackagingEnvironment::new("launcher-log");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let port = available_port();
    write_service_config(&environment, port);
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));

    // The launcher captured the serve process stderr into the state logs dir,
    // and the events are the structured one-line JSON envelope (OPS-017).
    let captured = fs::read_to_string(captured_log_path(&environment)).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(captured.lines().next().unwrap()).unwrap();
    assert_eq!(parsed["event"], "process.start");
    assert!(parsed["version"].as_str().unwrap() == env!("CARGO_PKG_VERSION"));
    #[cfg(unix)]
    assert_eq!(mode(&captured_log_path(&environment)) & 0o077, 0);

    // The binary's own managed rotating log mirrors the events.
    let managed = fs::read_to_string(managed_log_path(&environment)).unwrap();
    assert!(managed.lines().any(|line| line.contains("\"event\":\"process.start\"")));

    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

#[test]
fn lifecycle_launcher_rotates_the_captured_stderr_at_the_size_bound() {
    let environment = PackagingEnvironment::new("launcher-rotate");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let port = available_port();
    write_service_config(&environment, port);

    let size_limit = "200";
    let cap = "1200";
    let rotation_env = [
        ("LOCAL_API_RELAY_SERVICE_LOG_SIZE_LIMIT", size_limit),
        ("LOCAL_API_RELAY_SERVICE_LOG_SIZE_CAP", cap),
    ];
    for _ in 0..8 {
        let start = service_output_with_env(&environment, &["start"], &rotation_env);
        assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
        let stop = service_output_with_env(&environment, &["stop"], &rotation_env);
        assert!(stop.status.success());
        wait_for_no_serve_processes(&environment, Duration::from_secs(5));
    }

    // After several restarts over the tiny size bound, rotated captured logs
    // exist, each bounded, and the whole captured set stays under the cap.
    let log_dir = environment
        .state_home()
        .join("local-api-relay/logs");
    let captured: Vec<PathBuf> = fs::read_dir(&log_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("serve.log"))
        })
        .collect();
    assert!(
        captured.len() >= 2,
        "size rotation must leave rotated captured logs, got {}",
        captured.len()
    );
    let total: u64 = captured
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert!(
        total <= 1200,
        "the captured log set must stay under the launcher cap, got {total}"
    );
    for path in &captured {
        #[cfg(unix)]
        assert_eq!(
            mode(path) & 0o077,
            0,
            "captured logs must stay owner-only: {}",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Slice 4: ports, ready boundary, invalid configuration, and secrecy.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_port_and_explicit_ports_follow_the_stable_contract() {
    let environment = PackagingEnvironment::new("ports");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let client = Client::new();

    // PKG-009: with no configuration the service binds the stable default.
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    wait_ready(&client, 8787).await;
    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));

    // PKG-009: an explicit stable port in the process configuration is honored.
    let configured_port = available_port();
    write_service_config(&environment, configured_port);
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    wait_ready(&client, configured_port).await;
    let status = service_output(&environment, &["status"]);
    assert!(status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stdout).contains(&configured_port.to_string()),
        "status must report the configured port"
    );
    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

#[test]
fn invalid_process_configuration_blocks_ready_and_exits_nonzero() {
    let environment = PackagingEnvironment::new("invalid-config");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let entry = stable_entry(&environment);

    // A zero port in the process configuration is invalid (PKG-011).
    let config_dir = environment.config_home().join("local-api-relay");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("service.json"), br#"{"port": 0}"#).unwrap();
    let invalid = run_installed(&environment, &entry, &["serve"]);
    assert!(
        !invalid.status.success(),
        "an invalid port must block ready and exit nonzero"
    );
    assert!(!String::from_utf8_lossy(&invalid.stdout).contains("ready"));

    // Non-JSON process configuration is also a blocking failure.
    fs::write(config_dir.join("service.json"), b"not json").unwrap();
    let corrupt = run_installed(&environment, &entry, &["serve"]);
    assert!(!corrupt.status.success(), "corrupt configuration must block ready");
}

#[test]
fn port_conflict_blocks_ready_and_exits_nonzero() {
    let environment = PackagingEnvironment::new("port-conflict");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let occupied_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied_listener.local_addr().unwrap().port();
    let serve = run_installed(
        &environment,
        &stable_entry(&environment),
        &["serve", "--port", &occupied_port.to_string()],
    );
    assert!(
        !serve.status.success(),
        "a port conflict must block ready and exit nonzero"
    );
}

// ---------------------------------------------------------------------------
// Slice 4b: Linux-runnable loopback-only binding and remote-interface refusal
// (ticket 47). The Windows loopback test needs Windows interop and skips on a
// Linux-only host; these tests run everywhere in the WSL2/CI environment.
// ---------------------------------------------------------------------------

/// The host's non-loopback interface IPv4 addresses, observed through
/// `getifaddrs` (Linux). A loopback-only listener must refuse TCP connections
/// on these addresses (SEC-001). Empty only on a host with no non-loopback
/// IPv4 address — for example a container with just `lo` — where the refusal
/// half of the test cannot be exercised and the loopback listener assertions
/// still carry the evidence. A failed enumeration is a test-infrastructure
/// failure and panics rather than masquerading as "no address to test".
fn non_loopback_ipv4_addresses() -> Vec<Ipv4Addr> {
    use std::ffi::CStr;
    let mut addresses = Vec::new();
    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        let result = libc::getifaddrs(&mut ifaddrs);
        assert!(
            result == 0,
            "getifaddrs failed with {result}; the refusal evidence cannot be collected"
        );
        let mut current = ifaddrs;
        while !current.is_null() {
            let entry = &*current;
            let on_loopback_interface = CStr::from_ptr(entry.ifa_name).to_bytes() == b"lo";
            if !entry.ifa_addr.is_null()
                && !on_loopback_interface
                && (*entry.ifa_addr).sa_family == libc::AF_INET as libc::sa_family_t
            {
                // `sin_addr.s_addr` holds the address in network byte order
                // (wire bytes in memory), so `to_ne_bytes()` recovers the
                // octets on any endianness — the same convention as the
                // /proc renderings below.
                let address = *(entry.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
                if !ip.is_unspecified() && !ip.is_link_local() && !ip.is_multicast() {
                    addresses.push(ip);
                }
            }
            current = entry.ifa_next;
        }
        libc::freeifaddrs(ifaddrs);
    }
    addresses
}

/// The TCP LISTEN sockets owned by a specific process, cross-referencing the
/// process's open socket file descriptors (`/proc/<pid>/fd`, each a
/// `socket:[inode]` link) with the namespace socket tables (`/proc/net/tcp`
/// and `/proc/net/tcp6`). Parsed as (local address, local port).
/// `/proc/<pid>/net/tcp` is namespace-scoped, not process-scoped, so the
/// inode cross-reference is what limits the observation to the relay itself
/// rather than every listener another process shares the network namespace
/// with (SEC-001/PKG-009). Both address families are covered so a widened
/// IPv6 listener cannot slip past the loopback-only evidence.
fn process_listening_tcp_sockets(pid: u32) -> Vec<(IpAddr, u16)> {
    use std::collections::HashSet;
    let mut socket_inodes = HashSet::new();
    if let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) {
        for entry in entries.flatten() {
            let target = fs::read_link(entry.path()).unwrap_or_default();
            if let Some(inode) = target
                .to_str()
                .and_then(|target| target.strip_prefix("socket:["))
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|inode| inode.parse::<u64>().ok())
            {
                socket_inodes.insert(inode);
            }
        }
    }

    let mut sockets = Vec::new();
    for (table, address_width) in [("/proc/net/tcp", 4), ("/proc/net/tcp6", 6)] {
        let Ok(table) = fs::read_to_string(table) else {
            continue;
        };
        for line in table.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            if !socket_inodes.contains(&fields[9].parse::<u64>().unwrap()) {
                continue;
            }
            let (host, port) = fields[1].rsplit_once(':').expect("local address");
            // The kernel renders addresses little-endian: 127.0.0.1 is the
            // hex string "0100007F", and ::1 is the all-zeros string ending
            // in "01000000".
            let address = match address_width {
                4 => IpAddr::V4(Ipv4Addr::from(
                    u32::from_str_radix(host, 16).unwrap().to_le_bytes(),
                )),
                _ => IpAddr::V6(Ipv6Addr::from(
                    u128::from_str_radix(host, 16).unwrap().to_le_bytes(),
                )),
            };
            sockets.push((address, u16::from_str_radix(port, 16).unwrap()));
        }
    }
    sockets
}

/// Asserts every listener the relay process holds is exactly the loopback
/// address 127.0.0.1 — the SEC-001 literal contract — and that it holds no
/// IPv6 listener at all (an IPv6 address fails the match).
fn assert_loopback_only_listeners(listeners: &[(IpAddr, u16)]) {
    for (address, listener_port) in listeners {
        assert!(
            matches!(address, IpAddr::V4(ip) if *ip == Ipv4Addr::LOCALHOST),
            "every relay listener must be 127.0.0.1 only, got {address}:{listener_port}"
        );
    }
}

/// Ticket 47 / SEC-001 / PKG-009: the installed relay listens on the loopback
/// address only, and a connection from a non-loopback interface is refused.
/// Every IPv4/IPv6 listener of the relay process must be exactly 127.0.0.1,
/// and a raw TCP connect from every non-loopback IPv4 address of the host
/// must be refused, with the ready endpoint unreachable through that address.
#[tokio::test]
async fn service_listens_only_on_loopback_and_refuses_non_loopback_connections() {
    if !Path::new("/proc/net/tcp").exists() {
        eprintln!("skipping: /proc/net/tcp is unavailable (non-Linux host)");
        return;
    }
    let environment = PackagingEnvironment::new("loopback-only");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let client = Client::new();
    let port = available_port();
    write_service_config(&environment, port);
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    wait_ready(&client, port).await;
    let pid = read_pidfile(&environment).expect("the service writes a pidfile");

    // SEC-001/PKG-009: every listener of the relay process is 127.0.0.1.
    let listeners = process_listening_tcp_sockets(pid);
    assert!(
        listeners.iter().any(|(address, listener_port)| {
            *listener_port == port && *address == IpAddr::V4(Ipv4Addr::LOCALHOST)
        }),
        "the relay must listen on 127.0.0.1:{port}, got {listeners:?}"
    );
    assert_loopback_only_listeners(&listeners);

    // SEC-001: a non-loopback interface connection is refused. The refusal
    // half is skipped only on a host with no non-loopback IPv4 address at
    // all; the loopback listener evidence above still runs everywhere.
    let non_loopback = non_loopback_ipv4_addresses();
    if non_loopback.is_empty() {
        eprintln!("skipping non-loopback refusal: no non-loopback IPv4 address on this host");
    } else {
        let probe_client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        for address in non_loopback {
            // No listener is bound on that address, so the kernel answers RST:
            // the connect must fail with ECONNREFUSED, not merely error out
            // (SEC-001 "远端接口连接拒绝").
            let refused = TcpStream::connect_timeout(
                &SocketAddr::new(IpAddr::V4(address), port),
                Duration::from_secs(2),
            );
            assert!(
                matches!(
                    refused,
                    Err(ref error) if error.kind() == std::io::ErrorKind::ConnectionRefused
                ),
                "a TCP connect to {address}:{port} must be refused, got {refused:?}"
            );
            let ready = probe_client
                .get(format!("http://{address}:{port}/ready"))
                .send()
                .await;
            let relay_answered = ready
                .map(|response| response.status() == StatusCode::OK)
                .unwrap_or(false);
            assert!(
                !relay_answered,
                "the ready endpoint must not answer through the non-loopback interface {address}"
            );
        }
    }

    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

/// Ticket 47 / PKG-009: with an explicit port configured, the relay binds
/// that exact port on the loopback address — no widened listener and no
/// silent switch to the default port or to a scan-picked port. The process's
/// own socket table must show the configured port listening on 127.0.0.1 and
/// no listener at all on the default port.
#[tokio::test]
async fn explicit_port_keeps_the_listener_loopback_only_without_switching() {
    if !Path::new("/proc/net/tcp").exists() {
        eprintln!("skipping: /proc/net/tcp is unavailable (non-Linux host)");
        return;
    }
    let environment = PackagingEnvironment::new("explicit-port-loopback");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let client = Client::new();
    let configured_port = available_port();
    assert_ne!(
        configured_port, 8787,
        "the explicit port must differ from the default port"
    );
    write_service_config(&environment, configured_port);
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    wait_ready(&client, configured_port).await;
    let pid = read_pidfile(&environment).expect("the service writes a pidfile");

    let listeners = process_listening_tcp_sockets(pid);
    let listener_ports: Vec<u16> = listeners
        .iter()
        .map(|(_, listener_port)| *listener_port)
        .collect();

    // The exact configured port listens on 127.0.0.1, loopback-only.
    assert!(
        listeners.iter().any(|(address, listener_port)| {
            *listener_port == configured_port && *address == IpAddr::V4(Ipv4Addr::LOCALHOST)
        }),
        "the relay must listen on 127.0.0.1:{configured_port}, got {listeners:?}"
    );
    assert_loopback_only_listeners(&listeners);

    // No silent switch or widened listening: the default port is not also
    // bound by this process.
    assert!(
        !listeners
            .iter()
            .any(|(_, listener_port)| *listener_port == 8787),
        "an explicit port must not widen the listener to the default port too, got {listener_ports:?}"
    );

    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

#[test]
fn bootstrap_credential_never_enters_scripts_env_or_logs() {
    let environment = PackagingEnvironment::new("secrecy");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));

    // Initialization prints the one-time credential exactly once on stdout.
    let init = run_installed(&environment, &stable_entry(&environment), &["init-admin"]);
    assert!(init.status.success());
    let stdout = String::from_utf8_lossy(&init.stdout);
    let credential = stdout
        .strip_prefix("Administrator bootstrap credential: ")
        .expect("initialization prints the one-time credential")
        .trim()
        .to_owned();
    assert!(
        !String::from_utf8_lossy(&init.stderr).contains(&credential),
        "the credential must not appear on stderr"
    );

    // Start the service, then scan every installed artifact and every log for
    // the credential (SEC-005): scripts, process configuration, the captured
    // launcher stderr, and the managed rotating log.
    let port = available_port();
    write_service_config(&environment, port);
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    let pid = read_pidfile(&environment).expect("pidfile");

    let mut targets = vec![
        fs::read(staging_archive(&environment).join("install.sh")).unwrap(),
        fs::read(staging_archive(&environment).join("local-api-relay-service")).unwrap(),
        fs::read(service_script(&environment)).unwrap(),
        fs::read(launcher_script(&environment)).unwrap(),
        fs::read(environment.config_home().join("local-api-relay/service.json")).unwrap(),
        fs::read(captured_log_path(&environment)).unwrap(),
        fs::read(managed_log_path(&environment)).unwrap(),
    ];
    // The service process environment must not carry the credential.
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
    targets.push(environ);
    for (index, target) in targets.iter().enumerate() {
        assert!(
            !target.windows(credential.len()).any(|window| window == credential.as_bytes()),
            "the bootstrap credential leaked into artifact #{index}"
        );
    }

    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

/// SIGINT keeps the pre-existing interactive Ctrl+C graceful stop working when
/// the process can catch it, while the lifecycle commands stop through SIGTERM.
/// A process that inherited SIGINT ignored (a background job or login-task
/// launch) is exercised through SIGTERM instead, matching how the installed
/// service is actually stopped.
#[tokio::test]
async fn interactive_sigint_or_launcher_sigterm_stops_gracefully() {
    let environment = PackagingEnvironment::new("sigint-stop");
    let client = Client::new();
    let port = available_port();
    let mut server = environment.start(port, &[]);
    wait_ready(&client, port).await;
    let pid = server.id();

    // Does the child inherit a catchable SIGINT disposition? The SigIgn bitmask
    // uses bit (signal - 1), so SIGINT (2) is bit 1.
    let sigint_ignored = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("SigIgn:").map(str::trim))
                .and_then(|mask| u64::from_str_radix(mask, 16).ok())
                .map(|bits| bits & (1 << 1) != 0)
        })
        .unwrap_or(false);

    if sigint_ignored {
        terminate(&server);
    } else {
        let status = Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .status()
            .unwrap();
        assert!(status.success(), "could not send SIGINT to the service");
    }
    let exit = wait_for_exit(&mut server, Duration::from_secs(10));
    assert!(
        exit.success(),
        "an interactive stop must drain and exit 0, got {exit}"
    );
    assert!(
        log_contains_event(&environment, "process.stopped"),
        "a graceful stop must record the process.stopped event"
    );
}

// ---------------------------------------------------------------------------
// Slice 5: the Windows login task and the desktop console launcher (ticket 30).
// ---------------------------------------------------------------------------

/// The exported task XML must carry the PKG-005/006 contract: a per-user
/// logon trigger (not any-user), an interactive-only principal (runs only
/// after the user logs on, no stored password — SEC-005), a bounded
/// abnormal-exit restart policy that never restarts infinitely, and the
/// long-running `wsl.exe` serve action through the stable entry.
fn assert_login_task_contract(xml: &str, expected_arguments: &str) {
    assert!(xml.contains("<LogonTrigger>"), "must use a per-user logon trigger");
    assert!(
        xml.contains("<LogonType>InteractiveToken</LogonType>"),
        "must run only in the interactive session (no stored password)"
    );
    assert!(!xml.contains("<Password"), "must not store any password");
    assert!(
        xml.contains("<RestartOnFailure>"),
        "must carry a bounded restart policy"
    );
    assert!(xml.contains("<Count>3</Count>"), "bounded restart count");
    assert!(
        xml.contains("<Interval>PT1M</Interval>"),
        "bounded restart interval"
    );
    assert!(
        xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
        "the long-running serve action must not be time-limited"
    );
    assert!(xml.contains("<Command>wsl.exe</Command>"), "wsl.exe action");
    assert!(
        xml.contains(expected_arguments),
        "task arguments {expected_arguments:?} missing from exported XML:\n{xml}"
    );
}

/// PKG-005/PKG-006: the installer registers a per-user, logon-triggered
/// Windows scheduled task that directly holds a long-running `wsl.exe`
/// invocation of `local-api-relay serve` with the distro and WSL user pinned
/// and an absolute stable-entry path, runs only in the interactive session
/// (never before Windows logon, no stored password), and carries the bounded
/// restart policy. Reinstalling the same archive keeps the task registered
/// with the same definition (idempotent install).
#[test]
fn install_creates_the_windows_login_task_with_bounded_restart() {
    if !windows_interop_available() {
        eprintln!("skipping: Windows interop is not available");
        return;
    }
    let mut environment = PackagingEnvironment::new("win-task");
    let task_name = run_install_with_windows_task(&mut environment);

    let wsl_user = std::env::var("USER").unwrap_or_else(|_| "user".to_owned());
    let wsl_distro = std::env::var("WSL_DISTRO_NAME").unwrap_or_default();
    let expected_arguments = format!(
        "-d {wsl_distro} -u {wsl_user} -- {} serve",
        environment.home().join(".local/bin/local-api-relay").display()
    );
    let xml = schtasks_xml(&task_name);
    assert_login_task_contract(&xml, &expected_arguments);

    // Reinstalling the same archive keeps the task registered with the same
    // contract (repeat install is a safe no-op for the task).
    let repeat = run_install_with_windows_task(&mut environment);
    assert_eq!(repeat, task_name, "the repeat install reuses the task name");
    let xml_again = schtasks_xml(&task_name);
    assert_login_task_contract(&xml_again, &expected_arguments);
}

/// Runs the installed console launcher with the test environment applied.
fn run_launcher(
    environment: &PackagingEnvironment,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new(launcher_script(environment));
    environment.apply_env(&mut command);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// PKG-008: the installer generates the desktop console launcher owner-only.
/// When the relay is ready, the launcher exits 0 and targets the management
/// page; the browser open is suppressed by the test hook so no window pops on
/// the real desktop.
#[tokio::test]
async fn console_launcher_opens_the_management_page_when_ready() {
    let environment = PackagingEnvironment::new("launcher-ready");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    #[cfg(unix)]
    assert_eq!(
        mode(&launcher_script(&environment)) & 0o077,
        0,
        "the launcher must be owner-only"
    );

    environment.initialize();
    let port = available_port();
    write_service_config(&environment, port);
    let client = Client::new();
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    wait_ready(&client, port).await;

    let output = run_launcher(&environment, &[("LOCAL_API_RELAY_LAUNCHER_NO_BROWSER", "1")]);
    assert!(
        output.status.success(),
        "a ready launcher must exit 0: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("http://127.0.0.1:{port}/")),
        "the management URL is missing from: {stdout}"
    );
    assert!(
        stdout.contains("browser suppressed"),
        "the hook must be visible in: {stdout}"
    );

    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

/// PKG-008: when the relay is not running, the console launcher exits
/// nonzero and shows the service status and the actionable diagnostic
/// commands instead of opening the browser.
#[tokio::test]
async fn console_launcher_shows_diagnostics_when_not_ready() {
    let environment = PackagingEnvironment::new("launcher-not-ready");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let port = available_port();
    write_service_config(&environment, port);

    let output = run_launcher(&environment, &[]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a not-ready launcher must exit 1"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("not ready"), "must report not ready: {stdout}");
    assert!(
        stdout.contains("local-api-relay-service"),
        "must show the lifecycle command: {stdout}"
    );
    assert!(stdout.contains("status"), "must show the status diagnostic: {stdout}");
    assert!(
        stdout.contains("schtasks.exe"),
        "must show the login-task diagnostic: {stdout}"
    );
}

/// SEC-005: the exported login-task definition and the generated console
/// launcher must never carry the bootstrap credential or any other secret.
#[test]
fn windows_login_task_and_launcher_carry_no_credential() {
    if !windows_interop_available() {
        eprintln!("skipping: Windows interop is not available");
        return;
    }
    let mut environment = PackagingEnvironment::new("win-task-secrecy");
    let task_name = run_install_with_windows_task(&mut environment);
    let credential = environment.initialize();
    let credential_bytes = credential.as_bytes();

    let targets = [
        schtasks_bytes(&task_name),
        fs::read(launcher_script(&environment)).unwrap(),
        fs::read(service_script(&environment)).unwrap(),
        fs::read(staging_archive(&environment).join("install.sh")).unwrap(),
    ];
    for (index, target) in targets.iter().enumerate() {
        assert!(
            !target
                .windows(credential_bytes.len())
                .any(|window| window == credential_bytes),
            "the bootstrap credential leaked into task/launcher artifact #{index}"
        );
    }
}

/// Ticket 30 / PKG-015: the Windows side reaches the WSL listener through
/// localhost forwarding while the relay keeps listening on 127.0.0.1 only.
/// A Windows System32 curl.exe answers the dedicated ready endpoint and, with
/// the same relay access key a WSL client uses, completes a real chat call
/// through the same loopback instance.
#[tokio::test]
async fn windows_loopback_reaches_the_relay_without_widening_the_listener() {
    let system_curl = "/mnt/c/Windows/System32/curl.exe";
    if !Path::new(system_curl).exists() {
        eprintln!("skipping: Windows System32 curl.exe not found");
        return;
    }
    let environment = PackagingEnvironment::new("win-loopback");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    let bootstrap_credential = environment.initialize();
    let client = Client::new();
    let port = available_port();
    write_service_config(&environment, port);
    let start = service_output(&environment, &["start"]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stdout));
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");

    // PKG-009: the relay's listener stays loopback-only — the Windows side
    // reaches it through localhost forwarding, not a widened listener.
    let port_hex = format!("{:04X}", port);
    let tcp_table = fs::read_to_string("/proc/net/tcp").unwrap();
    let listeners: Vec<&str> = tcp_table
        .lines()
        .filter(|line| line.contains(&format!(":{port_hex} ")))
        .collect();
    assert!(!listeners.is_empty(), "no listener bound to port {port}");
    for line in &listeners {
        let local_address = line.split_whitespace().nth(1).unwrap_or("");
        assert!(
            local_address.starts_with("0100007F"),
            "the listener must stay loopback-only, got: {line}"
        );
    }

    // The Windows localhost path answers the dedicated ready endpoint while
    // the WSL listener stays bound to 127.0.0.1 only (PKG-009/015).
    let ready = Command::new(system_curl)
        .args(["-s", "-o", "NUL", "-w", "%{http_code}", &format!("{base}/ready")])
        .output()
        .unwrap();
    assert!(
        ready.status.success(),
        "Windows curl failed: {}",
        String::from_utf8_lossy(&ready.stdout)
    );
    assert_eq!(
        String::from_utf8_lossy(&ready.stdout).trim(),
        "200",
        "Windows localhost must reach the relay ready endpoint"
    );

    // A relay access key created on the management surface works from
    // Windows against the same loopback instance.
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let (upstream, _probes, release, upstream_worker) = holding_call_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream,
        "chat_completions",
        "win-model",
    )
    .await;
    let relay_secret = create_relay_secret(&client, &base, &active_cookie, &route_id).await;

    // API-003 lists a model only while a route for it is Available, so wait
    // for the route probe to complete before the Windows call.
    let model_ready = async {
        for _ in 0..160 {
            if let Ok(response) = client
                .get(format!("{base}/v1/models"))
                .bearer_auth(&relay_secret)
                .send()
                .await
                && response.status() == StatusCode::OK
                && response
                    .json::<serde_json::Value>()
                    .await
                    .unwrap()["data"]
                    .as_array()
                    .is_some_and(|models| {
                        models.iter().any(|model| model["id"] == "gpt-5.6-sol")
                    })
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("the model route never became available for the Windows call");
    };
    tokio::time::timeout(Duration::from_secs(45), model_ready)
        .await
        .unwrap();

    // A real relay call from Windows with the same access key reaches the
    // same loopback instance the WSL clients use. The upstream worker answers
    // only after release, so release it first — otherwise the relay's own
    // upstream first-event timeout fires before the response is written.
    release.send(()).unwrap();
    let chat = Command::new(system_curl)
        .args([
            "-s",
            "-H",
            &format!("Authorization: Bearer {relay_secret}"),
            "-d",
            r#"{"model":"gpt-5.6-sol","messages":[{"role":"user","content":"hello"}]}"#,
            &format!("{base}/v1/chat/completions"),
        ])
        .output()
        .unwrap();
    assert!(
        chat.status.success(),
        "the Windows chat call failed: {}",
        String::from_utf8_lossy(&chat.stdout)
    );
    let body = String::from_utf8_lossy(&chat.stdout);
    assert!(
        body.contains("chatcmpl-scripted"),
        "the Windows chat call must see the same instance's upstream response: {body}"
    );
    upstream_worker.join().unwrap();

    let stop = service_output(&environment, &["stop"]);
    assert!(stop.status.success());
    wait_for_no_serve_processes(&environment, Duration::from_secs(5));
}

/// Ticket 32 finding: the Windows login task starts serve directly without
/// the service script's pidfile, so `status` must report the running relay
/// by probing the configured loopback port instead of a misleading
/// "stopped". A stopped relay must still report status 3.
#[tokio::test]
async fn status_reports_running_for_a_directly_started_serve_without_pidfile() {
    let environment = PackagingEnvironment::new("status-task-managed");
    let install = run_install(&environment);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
    environment.initialize();
    let port = available_port();
    write_service_config(&environment, port);
    let client = Client::new();

    // Start serve directly, as the Windows login task does: no pidfile.
    let mut server = environment.start(port, &[]);
    wait_ready(&client, port).await;
    assert!(
        read_pidfile(&environment).is_none(),
        "the direct task path must not write a pidfile"
    );

    let status = service_output(&environment, &["status"]);
    assert!(
        status.status.success(),
        "status must exit 0 while the relay serves: {}",
        String::from_utf8_lossy(&status.stdout)
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("running"), "status must report running: {stdout}");
    assert!(
        stdout.contains(&port.to_string()),
        "status must report the configured port: {stdout}"
    );

    // When the directly started serve stops, status reports stopped again.
    terminate(&server);
    let exit = wait_for_exit(&mut server, Duration::from_secs(10));
    assert!(exit.success(), "the direct serve must stop gracefully: {exit}");
    let stopped = service_output(&environment, &["status"]);
    assert_eq!(
        stopped.status.code(),
        Some(3),
        "status must report stopped when nothing serves"
    );
}

// ---------------------------------------------------------------------------
// Versioned upgrades and recoverable rollback (ticket 31, PKG-013/PKG-014).
// ---------------------------------------------------------------------------

/// Runs install.sh with extra environment, keeping installs hermetic (the
/// Windows login task stays skipped).
fn install_with_env(
    environment: &PackagingEnvironment,
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new("bash");
    environment.apply_env(&mut command);
    command.env("LOCAL_API_RELAY_WINDOWS_TASK_SKIP", "1");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command
        .arg(staging_archive(environment).join("install.sh"))
        .output()
        .unwrap()
}

/// The binary the stable entry currently resolves to.
fn entry_target(environment: &PackagingEnvironment) -> PathBuf {
    let output = Command::new("readlink")
        .arg("-f")
        .arg(stable_entry(environment))
        .output()
        .unwrap();
    assert!(output.status.success(), "could not resolve the stable entry");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

/// The versions installed side by side under `~/.local/opt/local-api-relay/`.
fn installed_versions(environment: &PackagingEnvironment) -> Vec<String> {
    let directory = installed_root(environment).join("opt/local-api-relay");
    let mut versions: Vec<String> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort();
    versions
}

fn upgrade_state_path(environment: &PackagingEnvironment) -> PathBuf {
    environment
        .state_home()
        .join("local-api-relay/upgrade.state")
}

/// The key/value upgrade state written by install.sh for the rollback command.
fn upgrade_state(environment: &PackagingEnvironment) -> HashMap<String, String> {
    fs::read_to_string(upgrade_state_path(environment))
        .unwrap_or_else(|_| panic!("upgrade state missing"))
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// The live database's persisted schema version, read directly because the
/// contract permits database inspection for persistence invariants.
fn database_schema(environment: &PackagingEnvironment) -> Option<i64> {
    let path = environment
        .data_home()
        .join("local-api-relay/relay.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .query_row(
            "SELECT version FROM schema_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .ok()
}

/// Managed backup artifacts read directly from the backup directory: name,
/// trigger, and the schema they were taken at.
fn backup_artifacts(environment: &PackagingEnvironment) -> Vec<(String, String, i64)> {
    let directory = environment
        .data_home()
        .join("local-api-relay/backups");
    let mut artifacts = Vec::new();
    let Ok(entries) = fs::read_dir(&directory) else {
        return artifacts;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sqlite3") {
            continue;
        }
        let Ok(connection) = rusqlite::Connection::open(&path) else {
            continue;
        };
        let Ok((trigger, schema)) = connection.query_row(
            "SELECT trigger, source_schema_version FROM backup_metadata WHERE id = 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        ) else {
            continue;
        };
        artifacts.push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            trigger,
            schema,
        ));
    }
    artifacts
}

/// Initializes the administrator with a lowered supported schema, so the
/// database is created at an older version for migration drills.
fn initialize_with_schema(environment: &PackagingEnvironment, schema: i64) -> String {
    let mut command = environment.command();
    command.env("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", schema.to_string());
    let output = command.arg("init-admin").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout
        .strip_prefix("Administrator bootstrap credential: ")
        .expect("initialization prints the one-time credential")
        .trim()
        .to_owned()
}

/// T2: `check --json` emits a complete machine-readable snapshot, keeps the
/// existing human-readable text output parseable by packaging, and uses `null`
/// (never the string "none") for a missing database schema.
#[test]
fn check_json_emits_complete_snapshot_and_text_remains_compatible() {
    let environment = PackagingEnvironment::new("check-json");
    let json_output = environment
        .command()
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(
        json_output.status.success(),
        "{}",
        String::from_utf8_lossy(&json_output.stderr)
    );
    let stdout = String::from_utf8(json_output.stdout).unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_str(&stdout).expect("check --json prints valid JSON");
    let object = snapshot.as_object().expect("check --json prints a JSON object");
    for key in [
        "version",
        "port",
        "supported_schema",
        "database_schema",
        "migration_needed",
        "ready_url",
    ] {
        assert!(object.contains_key(key), "snapshot missing {key}: {snapshot}");
    }
    assert_eq!(snapshot["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(snapshot["port"], 8787);
    assert_eq!(snapshot["supported_schema"], 17);
    assert!(
        snapshot["database_schema"].is_null(),
        "missing schema must be JSON null, not a string: {snapshot}"
    );
    assert_eq!(snapshot["migration_needed"], false);
    assert_eq!(snapshot["ready_url"], "http://127.0.0.1:8787/ready");

    let text_output = environment.command().arg("check").output().unwrap();
    assert!(
        text_output.status.success(),
        "{}",
        String::from_utf8_lossy(&text_output.stderr)
    );
    let text = String::from_utf8(text_output.stdout).unwrap();
    assert!(text.contains(&format!("version={}", env!("CARGO_PKG_VERSION"))));
    assert!(text.contains("supported_schema=17"));
    assert!(text.contains("port=8787"));
    assert!(text.contains("database_schema=none"));
    assert!(text.contains("migration_needed=false"));
}

/// T2: with an existing older database the JSON snapshot carries the numeric
/// schema and a `true` migration flag, so consumers can distinguish "none"
/// from a real migratable schema.
#[test]
fn check_json_reports_an_older_database_schema_as_migration_needed() {
    let environment = PackagingEnvironment::new("check-json-old-schema");
    initialize_with_schema(&environment, 9);
    let json_output = environment
        .command()
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(
        json_output.status.success(),
        "{}",
        String::from_utf8_lossy(&json_output.stderr)
    );
    let snapshot: serde_json::Value =
        serde_json::from_str(&String::from_utf8(json_output.stdout).unwrap())
            .expect("check --json prints valid JSON");
    assert_eq!(snapshot["supported_schema"], 17);
    assert_eq!(snapshot["database_schema"], 9);
    assert_eq!(snapshot["migration_needed"], true);
    assert_eq!(snapshot["ready_url"], "http://127.0.0.1:8787/ready");
}

/// T2: JSON mode preserves the existing exit-code semantics even when the
/// pre-flight check fails.
#[test]
fn check_json_exit_code_matches_text_mode_on_failure() {
    let environment = PackagingEnvironment::new("check-json-fail");
    let text_output = environment
        .command()
        .env("LOCAL_API_RELAY_TEST_FAIL_CHECK", "1")
        .arg("check")
        .output()
        .unwrap();
    let json_output = environment
        .command()
        .env("LOCAL_API_RELAY_TEST_FAIL_CHECK", "1")
        .args(["check", "--json"])
        .output()
        .unwrap();
    assert!(!text_output.status.success());
    assert!(!json_output.status.success());
    assert_eq!(
        text_output.status.code(),
        json_output.status.code(),
        "JSON mode must exit with the same code as text mode"
    );
    let stdout = String::from_utf8(json_output.stdout).unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_str(&stdout).expect("failing check --json still prints valid JSON");
    assert!(
        snapshot.get("version").is_some(),
        "failing JSON snapshot still carries fields: {snapshot}"
    );
}

/// A repeating upstream that answers every connection with a scripted Chat
/// Completions success and counts the connections, so a relay's startup probes
/// and the post-rollback recovery call all succeed during an upgrade drill.
fn repeating_chat_upstream() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = connections.clone();
    let response = complete_chat_response();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break; };
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = read_http_request(&mut stream);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), connections)
}

async fn wait_for_connections(counter: &AtomicUsize, minimum: usize) {
    for _ in 0..200 {
        if counter.load(Ordering::Relaxed) >= minimum {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("upstream did not receive {minimum} connections");
}

/// Makes a real relay chat call with the given access key, retrying until the
/// restored route has been re-probed and can serve it.
async fn call_chat_until_ok(
    client: &Client,
    base: &str,
    secret: &str,
    counter: &AtomicUsize,
) {
    let before = counter.load(Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(secret)
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "hello" }],
                "stream": false
            }))
            .send()
            .await
            .unwrap();
        if response.status() == StatusCode::OK {
            assert!(
                counter.load(Ordering::Relaxed) > before,
                "the recovery call must reach the upstream"
            );
            return;
        }
        if Instant::now() >= deadline {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            panic!(
                "the recovery call did not succeed: {status} body: {body} connections: {}",
                counter.load(Ordering::Relaxed)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// PKG-013: installing a newer version keeps the previous program version side
/// by side, verifies before switching, atomically switches the stable entry,
/// restarts the running service, and keeps the client address and management
/// entry stable.
#[tokio::test]
async fn upgrade_installs_side_by_side_switches_entry_and_restarts_on_the_same_port() {
    let environment = PackagingEnvironment::new("upgrade-switch");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let port = available_port();
    write_service_config(&environment, port);
    let client = Client::new();

    let started = service_output(&environment, &["start"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    wait_ready(&client, port).await;

    // Upgrade to a distinct version through the same archive; the test hook
    // overrides the reported version.
    let upgrade = install_with_env(&environment, &[("LOCAL_API_RELAY_TEST_VERSION", "0.1.1")]);
    assert!(
        upgrade.status.success(),
        "the upgrade must succeed: {}",
        String::from_utf8_lossy(&upgrade.stderr)
    );
    wait_ready(&client, port).await;

    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.1/bin/local-api-relay"),
        "the stable entry must select the new version"
    );
    let versions = installed_versions(&environment);
    assert!(
        versions.contains(&"0.1.0".to_owned()) && versions.contains(&"0.1.1".to_owned()),
        "the previous program version must stay installed side by side: {versions:?}"
    );
    // The client address and the management entry stay stable after the
    // restart on the same port.
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let state = upgrade_state(&environment);
    assert_eq!(
        state.get("previous_version").map(String::as_str),
        Some("0.1.0")
    );
    assert!(
        !state.contains_key("pre_backup"),
        "a no-migration upgrade records no pre-backup"
    );

    let stopped = service_output(&environment, &["stop"]);
    assert!(stopped.status.success());
}

/// PKG-014: after a no-migration upgrade, a failed restart switches the stable
/// entry straight back to the previous version and the untouched database
/// serves the previous binary again.
#[tokio::test]
async fn upgrade_restart_failure_without_migration_auto_rolls_back() {
    let environment = PackagingEnvironment::new("upgrade-auto-rollback");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let port = available_port();
    write_service_config(&environment, port);
    let started = service_output(&environment, &["start"]);
    assert!(started.status.success());
    let client = Client::new();
    wait_ready(&client, port).await;

    // The new serve fails after the entry switched; with no migration the
    // live database is untouched and the installer must switch the entry
    // straight back and leave no upgrade state behind.
    let upgrade = install_with_env(
        &environment,
        &[
            ("LOCAL_API_RELAY_TEST_VERSION", "0.1.1"),
            ("LOCAL_API_RELAY_UPGRADE_SKIP_TRIAL", "1"),
            ("LOCAL_API_RELAY_TEST_FAIL_SERVE", "1"),
        ],
    );
    assert!(
        !upgrade.status.success(),
        "the upgrade must fail: {}",
        String::from_utf8_lossy(&upgrade.stdout)
    );
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.0/bin/local-api-relay"),
        "a no-migration restart failure must switch the entry straight back"
    );
    assert!(
        !upgrade_state_path(&environment).exists(),
        "a rolled-back upgrade must not leave upgrade state"
    );

    // The injected failure also broke the automatic restart; a clean start
    // restores the previous version's service on the untouched database.
    let clean = service_output(&environment, &["start"]);
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    wait_ready(&client, port).await;
    assert_eq!(
        database_schema(&environment),
        Some(17),
        "the live database must be untouched by a no-migration upgrade"
    );
    let stopped = service_output(&environment, &["stop"]);
    assert!(stopped.status.success());
}

/// PKG-013: when the new version's pre-flight verification fails, the upgrade
/// aborts before anything switches and the running previous service is
/// restored.
#[tokio::test]
async fn upgrade_preflight_check_failure_aborts_before_switching() {
    let environment = PackagingEnvironment::new("upgrade-check-fail");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let port = available_port();
    write_service_config(&environment, port);
    let started = service_output(&environment, &["start"]);
    assert!(started.status.success());
    let client = Client::new();
    wait_ready(&client, port).await;

    let upgrade = install_with_env(
        &environment,
        &[
            ("LOCAL_API_RELAY_TEST_VERSION", "0.1.1"),
            ("LOCAL_API_RELAY_TEST_FAIL_CHECK", "1"),
        ],
    );
    assert!(
        !upgrade.status.success(),
        "a failing pre-flight must abort the upgrade"
    );
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.0/bin/local-api-relay"),
        "a pre-switch failure must not move the stable entry"
    );
    assert!(!upgrade_state_path(&environment).exists());
    assert_eq!(database_schema(&environment), Some(17));
    // The failure hook only affects `check`, so the previous service restarts.
    wait_ready(&client, port).await;
    let stopped = service_output(&environment, &["stop"]);
    assert!(stopped.status.success());
}

/// PKG-013: when a migration is required and its pre-migration backup cannot
/// be created and verified, the upgrade aborts before the switch and the live
/// database is never modified.
#[tokio::test]
async fn upgrade_preflight_backup_failure_aborts_before_switching_and_keeps_the_database() {
    let environment = PackagingEnvironment::new("upgrade-backup-fail");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    initialize_with_schema(&environment, 9);
    let port = available_port();
    write_service_config(&environment, port);
    let started = service_output_with_env(
        &environment,
        &["start"],
        &[("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", "9")],
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert_eq!(database_schema(&environment), Some(9));

    let upgrade = install_with_env(
        &environment,
        &[
            ("LOCAL_API_RELAY_TEST_VERSION", "0.1.1"),
            ("LOCAL_API_RELAY_UPGRADE_SKIP_TRIAL", "1"),
            ("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "create"),
        ],
    );
    assert!(
        !upgrade.status.success(),
        "a failing pre-migration backup must abort the upgrade"
    );
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.0/bin/local-api-relay"),
        "a backup failure before the switch must not move the stable entry"
    );
    assert_eq!(
        database_schema(&environment),
        Some(9),
        "the live database must stay at its pre-upgrade schema"
    );
    let artifacts = backup_artifacts(&environment);
    assert!(
        artifacts.iter().all(|(_, trigger, _)| trigger != "migration"),
        "a failed pre-migration backup must not produce a migration artifact: {artifacts:?}"
    );
    assert!(!upgrade_state_path(&environment).exists());
}

/// PKG-013: when the new binary's trial serve fails its startup preconditions
/// (here an injected serve failure) the upgrade aborts before the switch, and
/// the live database is untouched.
#[tokio::test]
async fn upgrade_trial_serve_failure_aborts_before_switching() {
    let environment = PackagingEnvironment::new("upgrade-trial-fail");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let port = available_port();
    write_service_config(&environment, port);
    let started = service_output(&environment, &["start"]);
    assert!(started.status.success());
    let client = Client::new();
    wait_ready(&client, port).await;

    let upgrade = install_with_env(
        &environment,
        &[
            ("LOCAL_API_RELAY_TEST_VERSION", "0.1.1"),
            ("LOCAL_API_RELAY_TEST_FAIL_SERVE", "1"),
        ],
    );
    assert!(
        !upgrade.status.success(),
        "a failing trial serve must abort the upgrade before the switch"
    );
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.0/bin/local-api-relay"),
        "a trial failure must not move the stable entry"
    );
    assert!(!upgrade_state_path(&environment).exists());
    assert_eq!(database_schema(&environment), Some(17));

    // The injected failure also broke the automatic restore; a clean start
    // brings the previous version back.
    let clean = service_output(&environment, &["start"]);
    assert!(clean.status.success());
    wait_ready(&client, port).await;
    let stopped = service_output(&environment, &["stop"]);
    assert!(stopped.status.success());
}

/// PKG-013/PKG-014 end to end: an upgrade that needs a forward migration
/// creates and verifies the pre-migration backup, the migration commits, and
/// the rollback restores that backup with the previous binary — never a live
/// downgrade — after which the restored route is re-probed and serves a real
/// client call again.
#[tokio::test]
async fn upgrade_with_migration_commits_and_rollback_restores_the_pre_backup() {
    let environment = PackagingEnvironment::new("upgrade-migrate-rollback");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let bootstrap_credential = initialize_with_schema(&environment, 9);
    let port = available_port();
    write_service_config(&environment, port);
    let started = service_output_with_env(
        &environment,
        &["start"],
        &[("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", "9")],
    );
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A route and relay key configured before the upgrade, so the migration
    // pre-backup carries them and the post-rollback service can serve calls.
    let (upstream, connections) = repeating_chat_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream,
        "chat_completions",
        "recovery-model",
    )
    .await;
    let relay_secret = create_relay_secret(&client, &base, &active_cookie, &route_id).await;
    wait_for_connections(&connections, 1).await;

    let upgrade = install_with_env(&environment, &[("LOCAL_API_RELAY_TEST_VERSION", "0.1.1")]);
    assert!(
        upgrade.status.success(),
        "the migration upgrade must succeed: {}",
        String::from_utf8_lossy(&upgrade.stderr)
    );
    wait_ready(&client, port).await;
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.1/bin/local-api-relay")
    );
    assert_eq!(
        database_schema(&environment),
        Some(17),
        "the forward migration must commit"
    );
    let state = upgrade_state(&environment);
    let pre_backup = state
        .get("pre_backup")
        .cloned()
        .expect("the upgrade must record the migration pre-backup");

    // Roll back with the previous binary (which supports only schema 9): it
    // must restore the migration pre-backup and switch the entry back. The
    // live database is never downgraded in place. The hermetic task-skip hook
    // keeps the restart on the lifecycle script (the real Windows login task
    // is not touched in tests).
    let rollback = service_output_with_env(
        &environment,
        &["rollback"],
        &[
            ("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", "9"),
            ("LOCAL_API_RELAY_WINDOWS_TASK_SKIP", "1"),
        ],
    );
    assert!(
        rollback.status.success(),
        "stderr:\n{}\nstdout:\n{}\nserve.log:\n{}",
        String::from_utf8_lossy(&rollback.stderr),
        String::from_utf8_lossy(&rollback.stdout),
        fs::read_to_string(captured_log_path(&environment)).unwrap_or_default()
    );
    assert!(
        String::from_utf8_lossy(&rollback.stdout).contains(&pre_backup),
        "the rollback must restore the recorded pre-backup: {}",
        String::from_utf8_lossy(&rollback.stdout)
    );
    wait_ready(&client, port).await;
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.0/bin/local-api-relay")
    );
    assert_eq!(
        database_schema(&environment),
        Some(9),
        "the rollback must restore the pre-migration schema"
    );
    // The new-schema database was preserved as a restore-gate backup, not lost.
    let artifacts = backup_artifacts(&environment);
    assert!(
        artifacts.iter().any(|(_, trigger, schema)| trigger == "restore" && *schema == 17),
        "the rollback must preserve the new-schema database: {artifacts:?}"
    );

    // The recovery call: the restored route re-enters Checking, is re-probed
    // with the same native probe, and serves a real client call again with the
    // same relay access key.
    call_chat_until_ok(&client, &base, &relay_secret, &connections).await;
    let stopped = service_output(&environment, &["stop"]);
    assert!(stopped.status.success());
}

/// PKG-014: when the restart fails after a forward migration already
/// committed, the stable entry stays on the new version (the only binary that
/// can read the new schema) and the explicit rollback restores the migration
/// pre-backup with the previous binary.
#[tokio::test]
async fn upgrade_migration_committed_restart_failure_keeps_entry_and_rollback_recovers() {
    let environment = PackagingEnvironment::new("upgrade-committed-fail");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    initialize_with_schema(&environment, 9);
    let port = available_port();
    write_service_config(&environment, port);
    let started = service_output_with_env(
        &environment,
        &["start"],
        &[("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", "9")],
    );
    assert!(started.status.success());
    let client = Client::new();
    wait_ready(&client, port).await;

    // The new serve opens the live database (committing the forward migration)
    // and then fails before it binds.
    let upgrade = install_with_env(
        &environment,
        &[
            ("LOCAL_API_RELAY_TEST_VERSION", "0.1.1"),
            ("LOCAL_API_RELAY_UPGRADE_SKIP_TRIAL", "1"),
            ("LOCAL_API_RELAY_TEST_FAIL_SERVE", "1"),
        ],
    );
    assert!(!upgrade.status.success(), "the upgrade must fail");
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.1/bin/local-api-relay"),
        "a committed migration must keep the entry on the new version"
    );
    assert_eq!(
        database_schema(&environment),
        Some(17),
        "the forward migration must commit before the serve failure"
    );
    let state = upgrade_state(&environment);
    assert!(
        state.contains_key("pre_backup"),
        "the upgrade state must keep the pre-backup for the rollback"
    );

    // The explicit rollback with the previous binary repairs the database and
    // switches the entry back.
    let rollback = service_output_with_env(
        &environment,
        &["rollback"],
        &[("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", "9")],
    );
    assert!(
        rollback.status.success(),
        "{}",
        String::from_utf8_lossy(&rollback.stderr)
    );
    assert_eq!(
        entry_target(&environment),
        installed_root(&environment).join("opt/local-api-relay/0.1.0/bin/local-api-relay")
    );
    assert_eq!(database_schema(&environment), Some(9));
    assert!(
        !upgrade_state_path(&environment).exists(),
        "a consumed rollback must not leave upgrade state"
    );

    // The previous service starts cleanly on the restored database.
    let started = service_output_with_env(
        &environment,
        &["start"],
        &[("LOCAL_API_RELAY_TEST_SCHEMA_VERSION", "9")],
    );
    assert!(started.status.success());
    wait_ready(&client, port).await;
    let stopped = service_output(&environment, &["stop"]);
    assert!(stopped.status.success());
}

/// A rollback without any upgrade state is a clear no-op, never a guess.
#[tokio::test]
async fn rollback_without_upgrade_state_is_rejected() {
    let environment = PackagingEnvironment::new("rollback-no-state");
    let first = run_install(&environment);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let rollback = service_output(&environment, &["rollback"]);
    assert_eq!(rollback.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rollback.stderr).contains("no upgrade state"),
        "{}",
        String::from_utf8_lossy(&rollback.stderr)
    );
}
