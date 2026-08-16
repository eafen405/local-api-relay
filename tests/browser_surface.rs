//! Browser automation acceptance for the embedded Web management surface
//! (ticket 49).
//!
//! These tests drive a real headless Chromium (via the Playwright Node driver
//! in `tests/browser/driver.js`) against the live loopback relay process and
//! assert **user-visible behavior** — the testing seam the spec mandates for
//! Web management flows. They never assert the frontend component tree or
//! embedded script strings.
//!
//! The harness runs as an isolated installation (Node + Playwright + Chromium)
//! outside the repo, by default under `/tmp/local-api-relay-playwright*`.
//! When it is not installed, each test prints a skip notice and returns, so
//! the rest of the suite stays runnable in a bare environment. The Rust
//! harness resolves the paths (override with `LOCAL_API_RELAY_PLAYWRIGHT_PREFIX`,
//! `LOCAL_API_RELAY_PLAYWRIGHT_BROWSERS_PATH` and
//! `LOCAL_API_RELAY_CHROMIUM_PATH`).

use reqwest::{Client, StatusCode, header};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Credential every test rotates the bootstrap credential to; the browser
/// driver signs in with it for pre-activated scenarios.
const FINAL_CREDENTIAL: &str = "correct-horse-battery-staple";

/// Recovery base interval (one hour) used by timing-sensitive browser tests so
/// the automatic recovery scheduler never fires during the test.
const RECOVERY_TEST_BASE_INTERVAL_MS: i64 = 3_600_000;

/// Skip notice emitted when the isolated browser harness is not installed.
const BROWSER_SKIP_NOTICE: &str =
    "skipped: browser harness unavailable (node + Playwright + a Chromium under /tmp/local-api-relay-playwright*)";

// ---------------------------------------------------------------------------
// Test environment (same process-boundary contract as the other suites)
// ---------------------------------------------------------------------------

struct TestEnvironment {
    root: PathBuf,
}

impl TestEnvironment {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "local-api-relay-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_local-api-relay"));
        command
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"));
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

    fn start(&self, port: u16) -> Child {
        self.start_with(port, &[])
    }

    fn start_with(&self, port: u16, extra_env: &[(&str, &str)]) -> Child {
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

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Wraps the relay child process so every exit path — including a browser
/// harness skip — waits for the child instead of leaving a zombie (and keeps
/// the port free). `kill()` consumes the guard; the drop fallback covers early
/// returns.
struct ServerGuard {
    server: Child,
}

impl ServerGuard {
    fn new(server: Child) -> Self {
        Self { server }
    }

    fn kill(mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// The listening port embedded in a `http://127.0.0.1:<port>` base URL.
fn port_of(base: &str) -> u16 {
    base.rsplit(':').next().unwrap().parse().unwrap()
}

async fn wait_ready(client: &Client, port: u16) {
    for _ in 0..80 {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/ready"))
            .send()
            .await
            && response.status() == StatusCode::OK
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("service did not become ready on port {port}");
}

fn session_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
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
        .json(&json!({ "new_password": FINAL_CREDENTIAL }))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::OK);
    session_cookie(&changed)
}

async fn get_with_cookie(client: &Client, url: String, cookie: &str) -> reqwest::Response {
    client
        .get(url)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

async fn set_recovery_settings(
    client: &Client,
    base: &str,
    active_cookie: &str,
    base_interval_ms: i64,
    doubling_limit: i64,
) {
    let response = client
        .patch(format!("{base}/admin/recovery-settings"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({
            "base_interval_ms": base_interval_ms,
            "doubling_limit": doubling_limit
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[allow(clippy::too_many_arguments)]
async fn configure_route(
    client: &Client,
    base: &str,
    active_cookie: &str,
    base_url: String,
    protocol: &str,
    upstream_model_name: &str,
    published_model_id: &str,
    cost_multiplier: &str,
) -> String {
    let provider_id = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({
            "display_name": format!("{protocol} route {cost_multiplier}x"),
            "base_url": base_url,
            "api_key": format!("{protocol}-route-key-{cost_multiplier}")
        }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({
            "published_model_id": published_model_id,
            "provider_id": provider_id,
            "upstream_model_name": upstream_model_name,
            "protocol": protocol,
            "cost_multiplier": cost_multiplier
        }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn configure_relay_route(
    client: &Client,
    base: &str,
    active_cookie: &str,
    base_url: String,
    protocol: &str,
    upstream_model_name: &str,
) -> String {
    configure_route(
        client,
        base,
        active_cookie,
        base_url,
        protocol,
        upstream_model_name,
        "gpt-5.6-sol",
        "1",
    )
    .await
}

async fn route_health(client: &Client, base: &str, cookie: &str, route_id: &str) -> String {
    get_with_cookie(client, format!("{base}/admin/operations"), cookie)
        .await
        .json::<Value>()
        .await
        .unwrap()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == route_id)
        .unwrap_or_else(|| panic!("route {route_id} is missing from the operations snapshot"))["health"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn await_route_health(
    client: &Client,
    base: &str,
    cookie: &str,
    route_id: &str,
    expected: &str,
) {
    for _ in 0..100 {
        if route_health(client, base, cookie, route_id).await == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("route {route_id} did not reach health {expected}");
}

// ---------------------------------------------------------------------------
// Scripted upstreams (identical contract to the other suites)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CapturedProbe {
    request_line: String,
    body: Vec<u8>,
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
        let (name, value) = line.trim_end().split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    CapturedProbe {
        request_line: request_line.trim_end().to_owned(),
        body,
    }
}

fn complete_chat_response() -> String {
    r#"{"id":"chatcmpl-scripted","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#.to_owned()
}

fn http_json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_status_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn scripted_chat_upstream(
    responses: Vec<String>,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let captured = read_http_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            stream.flush().unwrap();
            sender.send(captured).unwrap();
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

fn scripted_http_upstream(
    responses: Vec<String>,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let captured = read_http_request(&mut stream);
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            sender.send(captured).unwrap();
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// Serves the creation probe as a 500, then the next connection (the manual
/// check) as success after `delay` — long enough for the browser to observe
/// the Check button's loading state.
fn failing_then_slow_success_chat_upstream(
    delay: Duration,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let response = complete_chat_response();
    let failure =
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        stream.write_all(failure).unwrap();
        stream.flush().unwrap();
        sender.send(captured).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        thread::sleep(delay);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        stream.flush().unwrap();
        sender.send(captured).unwrap();
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// Serves the creation probe as a 500, then holds the next connection (the
/// manual check's probe) open forever, so a second manual Check overlaps an
/// in-flight check and is rejected with 409. The held write tolerates the
/// relay giving up (probe idle timeout) so the worker never panics.
fn failing_then_holding_chat_upstream() -> (
    String,
    Receiver<CapturedProbe>,
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let response = complete_chat_response();
    let failure =
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        stream.write_all(failure).unwrap();
        stream.flush().unwrap();
        sender.send(captured).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        sender.send(captured).unwrap();
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

/// Serves exactly `connections` connections as success after `delay`. The
/// route-check-disabled flow needs two: the creation probe (blocking the
/// create response) and the post-restart startup probe, so the worker can be
/// joined once both are served.
fn slow_success_chat_upstream(
    delay: Duration,
    connections: usize,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let response = complete_chat_response();
    let worker = thread::spawn(move || {
        for _ in 0..connections {
            let (mut stream, _) = listener.accept().unwrap();
            let captured = read_http_request(&mut stream);
            thread::sleep(delay);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            let _ = stream.flush();
            sender.send(captured).unwrap();
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// The fixed native probe contract the manual check must send (ROUTE-016/022):
/// the route's own protocol, upstream model and a minimal non-streaming input.
fn assert_fixed_native_probe(probe: &CapturedProbe, expected_model: &str) {
    assert_eq!(probe.request_line, "POST /v1/chat/completions HTTP/1.1");
    let body: Value = serde_json::from_slice(&probe.body).unwrap();
    assert_eq!(
        body,
        json!({
            "model": expected_model,
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false
        }),
        "the manual check must send the fixed native probe, never an arbitrary prompt or model"
    );
}

// ---------------------------------------------------------------------------
// Browser harness: locates the isolated Node + Playwright + Chromium
// installation and runs one driver scenario against the live server.
// ---------------------------------------------------------------------------

mod harness {
    use super::*;

    pub struct Harness {
        pub driver: PathBuf,
        pub node_modules: PathBuf,
        pub browsers: PathBuf,
        pub chromium: PathBuf,
    }

    /// Locates the isolated browser harness. Returns `None` (the tests skip)
    /// when any piece is missing: node on PATH, the driver script, the
    /// Playwright npm prefix and a Chromium executable.
    pub fn locate() -> Option<Harness> {
        let node_ok = Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !node_ok {
            return None;
        }
        let driver = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/browser/driver.js");
        if !driver.exists() {
            return None;
        }
        let node_modules = std::env::var("LOCAL_API_RELAY_PLAYWRIGHT_PREFIX")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/local-api-relay-playwright"))
            .join("node_modules");
        if !node_modules.join("playwright/index.js").exists() {
            return None;
        }
        let browsers = std::env::var("LOCAL_API_RELAY_PLAYWRIGHT_BROWSERS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/local-api-relay-playwright-browsers"));
        let chromium = find_chromium(&browsers)?;
        Some(Harness {
            driver,
            node_modules,
            browsers,
            chromium,
        })
    }

    fn find_chromium(browsers: &PathBuf) -> Option<PathBuf> {
        let entries = fs::read_dir(browsers).ok()?;
        let mut headless_shell: Option<PathBuf> = None;
        let mut full: Option<PathBuf> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("chromium_headless_shell-") {
                let candidate = entry.path().join("chrome-linux/headless_shell");
                if candidate.exists() {
                    headless_shell = Some(candidate);
                }
            } else if name.starts_with("chromium-") {
                let candidate = entry.path().join("chrome-linux/chrome");
                if candidate.exists() {
                    full = Some(candidate);
                }
            }
        }
        headless_shell.or(full)
    }

    /// Runs one driver scenario and returns its structured evidence. The
    /// driver exits non-zero or prints `{"ok":false,...}` on failure.
    pub async fn run(
        scenario: &str,
        base: &str,
        credential: &str,
        new_credential: Option<&str>,
        extra: Option<&Value>,
    ) -> Result<Value, String> {
        let harness = locate().ok_or_else(|| "browser harness unavailable".to_owned())?;
        let mut command = tokio::process::Command::new("node");
        command
            .arg(&harness.driver)
            .arg(scenario)
            .arg("--base")
            .arg(base)
            .arg("--credential")
            .arg(credential);
        if let Some(new_credential) = new_credential {
            command.arg("--new-credential").arg(new_credential);
        }
        if let Some(extra) = extra {
            command.arg("--extra").arg(extra.to_string());
        }
        command
            .env("NODE_PATH", &harness.node_modules)
            .env("PLAYWRIGHT_BROWSERS_PATH", &harness.browsers)
            .env("LOCAL_API_RELAY_CHROMIUM_PATH", &harness.chromium)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(trace) = std::env::var("LOCAL_API_RELAY_DRIVER_TRACE") {
            command.env("LOCAL_API_RELAY_DRIVER_TRACE", trace);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start the browser driver: {error}"))?;
        use tokio::io::AsyncReadExt;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "driver stdout is unavailable".to_owned())?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| "driver stderr is unavailable".to_owned())?;
        let wait_started = std::time::Instant::now();
        let status = match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
            Ok(status) => status.map_err(|error| format!("driver wait error: {error}"))?,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err("browser scenario timed out after 120s".to_owned());
            }
        };
        eprintln!(
            "[browser harness] {scenario}: driver exited in {:?}",
            wait_started.elapsed()
        );
        let read_started = std::time::Instant::now();
        let mut stdout_text = String::new();
        let mut stderr_text = String::new();
        stdout
            .read_to_string(&mut stdout_text)
            .await
            .map_err(|error| format!("failed to read driver stdout: {error}"))?;
        stderr
            .read_to_string(&mut stderr_text)
            .await
            .map_err(|error| format!("failed to read driver stderr: {error}"))?;
        eprintln!(
            "[browser harness] {scenario}: driver output read in {:?}",
            read_started.elapsed()
        );
        if !status.success() {
            let detail = if stderr_text.trim().is_empty() {
                stdout_text.trim().to_owned()
            } else {
                stderr_text.trim().to_owned()
            };
            return Err(format!("driver exited with {:?}: {detail}", status.code()));
        }
        let payload: Value = serde_json::from_str(stdout_text.trim())
            .map_err(|error| format!("driver stdout is not JSON ({error}): {}", stdout_text.trim()))?;
        if payload["ok"].as_bool() != Some(true) {
            return Err(format!(
                "scenario failed: {}",
                payload["error"].as_str().unwrap_or("unknown error")
            ));
        }
        Ok(payload["evidence"].clone())
    }
}

// ---------------------------------------------------------------------------
// Scenario runner: skips cleanly when the harness is missing and serializes
// browser tests (one Chromium at a time keeps memory and ports predictable).
// ---------------------------------------------------------------------------

static BROWSER_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serializes browser tests (one Chromium at a time keeps memory and ports
/// predictable). Timing-sensitive tests acquire this lock *before* their
/// setup so the observed route state is fresh when the driver runs.
async fn browser_lock() -> tokio::sync::MutexGuard<'static, ()> {
    BROWSER_SERIAL.lock().await
}

/// Runs one driver scenario against the live server, panicking on failure.
/// Callers must hold the browser lock (see `browser_scenario`).
async fn run_driver(
    scenario: &str,
    base: &str,
    credential: &str,
    new_credential: Option<&str>,
    extra: Option<&Value>,
) -> Value {
    harness::run(scenario, base, credential, new_credential, extra)
        .await
        .unwrap_or_else(|message| panic!("browser scenario {scenario} failed: {message}"))
}

/// Skips cleanly when the harness is missing, then serializes and runs the
/// driver scenario.
async fn browser_scenario(
    scenario: &str,
    base: &str,
    credential: &str,
    new_credential: Option<&str>,
    extra: Option<&Value>,
) -> Option<Value> {
    if harness::locate().is_none() {
        eprintln!("{BROWSER_SKIP_NOTICE}");
        return None;
    }
    let _guard = browser_lock().await;
    Some(run_driver(scenario, base, credential, new_credential, extra).await)
}

/// Fresh-env scaffolding: initialize, start, wait ready. Does NOT activate the
/// administrator — fresh-login tests hand the bootstrap credential to the
/// browser (which rotates it, SEC-004); pre-activated tests call
/// `activate_administrator` themselves before driving the browser.
async fn start_service(
    label: &str,
) -> (TestEnvironment, String, ServerGuard, Client, String) {
    let environment = TestEnvironment::new(label);
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let server = ServerGuard::new(environment.start(port));
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    (environment, base, server, client, bootstrap_credential)
}

/// Signs in with an already-activated credential (no password change) and
/// returns the session cookie for API setup.
async fn login_only(client: &Client, base: &str, credential: &str) -> String {
    let login = client
        .post(format!("{base}/admin/login"))
        .json(&json!({ "password": credential }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    session_cookie(&login)
}

#[tokio::test]
async fn browser_login_lands_on_operations_default_view() {
    // UI-001/T3: the default view is Operations, the primary navigation
    // carries the three persistent views, and no Sub2API domain object
    // (Accounts, Groups, Channels) appears anywhere (UI-003/UI-013).
    let (environment, base, server, _client, bootstrap_credential) =
        start_service("browser-login-default-view").await;
    let Some(evidence) = browser_scenario(
        "login-default-view",
        &base,
        &bootstrap_credential,
        Some(FINAL_CREDENTIAL),
        None,
    )
    .await
    else {
        return;
    };

    assert_eq!(
        evidence["mustChangePassword"], true,
        "the first browser login must force the credential rotation (SEC-004)"
    );
    assert_eq!(evidence["h1"], "操作台", "Operations is the default view");
    assert_eq!(
        evidence["navLabels"],
        json!(["操作台", "调用与用量", "设置"]),
        "the primary navigation carries the three persistent views"
    );
    assert_eq!(evidence["currentView"], "操作台");
    assert_eq!(evidence["hasStatusGrid"], true, "the status grid is present");
    let text = evidence["text"].as_str().unwrap();
    for forbidden in ["Accounts", "Groups", "Channels"] {
        assert!(
            !text.contains(forbidden),
            "the Operations surface must not render the Sub2API domain object {forbidden:?}"
        );
    }

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_calls_usage_is_the_secondary_view_and_navigation_round_trips() {
    // UI-001: Calls & usage is the second persistent main view, with the fixed
    // time-window selector and usage totals on the same surface; navigation
    // returns to Operations.
    let (environment, base, server, _client, bootstrap_credential) =
        start_service("browser-usage-secondary-view").await;
    let Some(evidence) = browser_scenario(
        "usage-secondary-view",
        &base,
        &bootstrap_credential,
        Some(FINAL_CREDENTIAL),
        None,
    )
    .await
    else {
        return;
    };

    assert_eq!(evidence["h1"], "调用与用量");
    let windows: Vec<&str> = evidence["windowButtons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|window| window.as_str().unwrap())
        .collect();
    assert_eq!(
        windows,
        vec!["1h", "5h", "24h", "7d", "14d", "all"],
        "the six fixed time windows are offered (OPS-008)"
    );
    assert_eq!(evidence["hasUsageTotals"], true);
    let usage_text = evidence["usageText"].as_str().unwrap();
    assert!(usage_text.contains("Token 分布"));
    assert!(usage_text.contains("尚无调用记录"));
    assert_eq!(evidence["backH1"], "操作台", "navigation round-trips");

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_sidebar_workbench_shows_sidebar_and_multi_column_layout() {
    // T3: the Operations default view renders a persistent sidebar, a
    // 3-column status grid and a two-column workbench at desktop width; model
    // routes, relay access keys, upstream providers, upstream models and the
    // published-model catalog all remain on the page. Calls & usage keeps the
    // same sidebar and a two-column workbench.
    let (environment, base, server, _client, bootstrap_credential) =
        start_service("browser-sidebar-workbench").await;
    let Some(evidence) = browser_scenario(
        "sidebar-workbench",
        &base,
        &bootstrap_credential,
        Some(FINAL_CREDENTIAL),
        None,
    )
    .await
    else {
        return;
    };

    assert_eq!(evidence["hasSidebar"], true, "the shell renders a sidebar");
    assert_eq!(
        evidence["navLabels"],
        json!(["操作台", "调用与用量", "设置"]),
        "the sidebar carries the three persistent views"
    );
    assert!(
        evidence["operationsColumns"].as_u64().unwrap() >= 2,
        "the Operations workbench has at least two columns at desktop width"
    );
    assert_eq!(
        evidence["statusColumns"].as_u64().unwrap(),
        3,
        "the status cards form a 3-column grid at desktop width"
    );
    let regions = &evidence["regionCounts"];
    assert_eq!(regions["routes"].as_u64().unwrap(), 1, "model routes region is present");
    assert_eq!(regions["relayKeys"].as_u64().unwrap(), 1, "relay access keys region is present");
    assert_eq!(regions["providers"].as_u64().unwrap(), 1, "upstream providers region is present");
    assert_eq!(regions["upstreamModels"].as_u64().unwrap(), 1, "upstream models region is present");
    assert_eq!(regions["catalog"].as_u64().unwrap(), 1, "published-model catalog region is present");
    let main_regions = &evidence["mainRegions"];
    assert_eq!(main_regions["routes"].as_u64().unwrap(), 1, "model routes live in the main column");
    assert_eq!(main_regions["relayKeys"].as_u64().unwrap(), 1, "relay access keys live in the main column");
    let aside_regions = &evidence["asideRegions"];
    assert_eq!(aside_regions["providers"].as_u64().unwrap(), 1, "upstream providers live in the auxiliary column");
    assert_eq!(aside_regions["upstreamModels"].as_u64().unwrap(), 1, "upstream models live in the auxiliary column");
    assert_eq!(aside_regions["catalog"].as_u64().unwrap(), 1, "published-model catalog lives in the auxiliary column");
    assert_eq!(evidence["usageHasSidebar"], true, "Calls & usage keeps the sidebar");
    assert!(
        evidence["usageColumns"].as_u64().unwrap() >= 2,
        "the Calls & usage workbench has at least two columns at desktop width"
    );
    assert_eq!(
        evidence["hasSettingsView"], true,
        "the Settings view is reachable from the sidebar"
    );

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_narrow_viewport_falls_back_to_single_column() {
    // T3: at <=760px the shell collapses to one column; Operations and Calls &
    // usage remain usable with single-column content.
    let (environment, base, server, _client, bootstrap_credential) =
        start_service("browser-narrow-single-column").await;
    let Some(evidence) = browser_scenario(
        "narrow-single-column",
        &base,
        &bootstrap_credential,
        Some(FINAL_CREDENTIAL),
        None,
    )
    .await
    else {
        return;
    };

    assert_eq!(
        evidence["operationsColumns"].as_u64().unwrap(),
        1,
        "Operations workbench is a single column at <=760px"
    );
    assert_eq!(
        evidence["statusColumns"].as_u64().unwrap(),
        1,
        "status cards collapse to a single column at <=760px"
    );
    assert_eq!(
        evidence["usageColumns"].as_u64().unwrap(),
        1,
        "Calls & usage workbench is a single column at <=760px"
    );
    assert_eq!(
        evidence["usageMetricColumns"].as_u64().unwrap(),
        1,
        "usage metric cards collapse to a single column at <=760px"
    );
    assert_eq!(
        evidence["settingsColumns"].as_u64().unwrap(),
        1,
        "settings cards collapse to a single column at <=760px"
    );

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_operations_groups_routes_by_published_model() {
    // UI-002: three routes across two published models render as two
    // model-grouped sections whose rows carry provider, upstream model,
    // protocol, multiplier and system-owned health.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-route-groups").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let first_upstream = scripted_http_upstream(vec![http_json_response(&complete_chat_response())]);
    configure_route(
        &client,
        &base,
        &active_cookie,
        first_upstream.0,
        "chat_completions",
        "gpt-group-a",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let second_upstream = scripted_http_upstream(vec![http_json_response(&complete_chat_response())]);
    configure_route(
        &client,
        &base,
        &active_cookie,
        second_upstream.0,
        "chat_completions",
        "deepseek-group",
        "deepseek-v4-flash",
        "1",
    )
    .await;
    let third_upstream = scripted_http_upstream(vec![http_json_response(&complete_chat_response())]);
    configure_route(
        &client,
        &base,
        &active_cookie,
        third_upstream.0,
        "chat_completions",
        "gpt-group-b",
        "gpt-5.6-sol",
        "1",
    )
    .await;

    let Some(evidence) =
        browser_scenario("route-groups", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    let groups = evidence["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "one route group section per published model");
    assert_eq!(groups[0]["title"], "deepseek-v4-flash");
    assert_eq!(groups[1]["title"], "gpt-5.6-sol");
    let deepseek_rows = groups[0]["rows"].as_array().unwrap();
    assert_eq!(deepseek_rows.len(), 1);
    assert_eq!(deepseek_rows[0]["provider"], "chat_completions route 1x");
    assert_eq!(deepseek_rows[0]["upstreamModel"], "deepseek-group");
    assert_eq!(deepseek_rows[0]["protocol"], "chat_completions");
    assert_eq!(deepseek_rows[0]["multiplier"], "1x");
    assert_eq!(deepseek_rows[0]["health"], "available");
    let gpt_rows = groups[1]["rows"].as_array().unwrap();
    assert_eq!(gpt_rows.len(), 2);
    let mut upstream_models: Vec<&str> = gpt_rows
        .iter()
        .map(|row| row["upstreamModel"].as_str().unwrap())
        .collect();
    upstream_models.sort_unstable();
    assert_eq!(upstream_models, vec!["gpt-group-a", "gpt-group-b"]);
    for row in groups.iter().flat_map(|group| group["rows"].as_array().unwrap()) {
        assert_eq!(row["health"], "available", "healthy routes show Available");
        // UI-002: each row also surfaces state age, last check and next probe.
        assert!(
            !row["stateAge"].as_str().unwrap().is_empty(),
            "the row shows a state age"
        );
        assert!(
            !row["lastCheck"].as_str().unwrap().is_empty(),
            "the row shows the last check time"
        );
    }

    first_upstream.2.join().unwrap();
    second_upstream.2.join().unwrap();
    third_upstream.2.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_focus_panels_add_edit_and_cancel_return_to_operations() {
    // UI-005: adding and editing a provider and a model route happen in a
    // focused panel and return to the original Operations context; the edit
    // panels load the saved values; cancel closes the panel without leaving.
    let (environment, base, server, _client, bootstrap_credential) =
        start_service("browser-focus-panels").await;
    let Some(evidence) = browser_scenario("focus-panels", &base, &bootstrap_credential, Some(FINAL_CREDENTIAL), None)
        .await
    else {
        return;
    };

    assert_eq!(evidence["addProviderTitle"], "添加上游供应商");
    assert_eq!(evidence["providerSaved"], true);
    assert_eq!(evidence["backOnOperationsAfterSave"], true);
    assert_eq!(evidence["editProviderTitle"], "编辑上游供应商");
    assert_eq!(evidence["editLoadedName"], "Browser provider");
    assert_eq!(evidence["editLoadedBaseUrl"], "https://browser-provider.example/v1");
    assert_eq!(evidence["providerRenamed"], true);
    assert_eq!(evidence["addRouteTitle"], "添加模型路由");
    assert_eq!(evidence["routeSaved"], true);
    assert_eq!(evidence["editRouteTitle"], "编辑模型路由");
    assert_eq!(evidence["editLoadedUpstreamModel"], "browser-upstream-model");
    assert_eq!(evidence["editLoadedMultiplier"], "1");
    assert_eq!(
        evidence["editPanelHasHealthField"], false,
        "health is system-owned: the route edit panel offers no health field (UI-007)"
    );
    assert_eq!(evidence["cancelReturnsToOperations"], true);
    assert_eq!(evidence["panelClosedAfterCancel"], true);

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_relay_key_create_search_and_revoke() {
    // UI-009: creating a key shows the full secret exactly once with a copy
    // affordance and the one-time notice; afterwards the list shows only the
    // label/prefix/status and never the secret; search filters the list; and
    // revoking requires an explicit confirmation before the row turns Revoked.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-relay-key").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let upstream = scripted_http_upstream(vec![http_json_response(&complete_chat_response())]);
    configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream.0,
        "chat_completions",
        "key-route-model",
    )
    .await;

    let Some(evidence) = browser_scenario("relay-key", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    assert_eq!(evidence["createKeyTitle"], "创建中转访问密钥");
    assert!(
        evidence["secretLength"].as_u64().unwrap() >= 32,
        "the one-time secret is a long random token"
    );
    assert_eq!(evidence["copyButtonAtCreation"], true);
    assert_eq!(
        evidence["oneTimeNotice"],
        true,
        "the panel says the key stays re-displayable (REL-010)"
    );
    assert_eq!(evidence["secretInputShown"], true, "the one-time secret input is shown");
    assert_eq!(evidence["listShowsLabel"], true);
    assert_eq!(evidence["listShowsPrefix"], true, "the list shows the secret prefix");
    assert_eq!(evidence["listShowsActive"], true);
    assert_eq!(
        evidence["listShowsScope"], true,
        "the row shows the key's eligible route scope (UI-009)"
    );
    assert_eq!(
        evidence["fullSecretAbsentAfterClose"], true,
        "the full secret is never rendered again after the one-time display"
    );
    assert_eq!(
        evidence["copyButtonAfterReload"], true,
        "no copy-full-secret affordance survives the one-time display"
    );
    assert_eq!(evidence["searchNoMatchShown"], true);
    assert_eq!(evidence["searchMatchesLabel"], true);
    assert_eq!(evidence["confirmPrompt"], true);
    assert_eq!(evidence["revokedStatusShown"], true);
    assert_eq!(
        evidence["revokeActionCountAfterRevoke"],
        0,
        "a revoked key offers no further revoke/edit actions"
    );
    assert_eq!(evidence["fullSecretAbsentAfterRevoke"], true);

    upstream.2.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_route_check_recovers_an_unavailable_route_with_a_fixed_native_probe() {
    // UI-008/ROUTE-022: the Check interaction shows the loading state (button
    // disabled, labelled "Checking…") while it runs, then restores the route
    // to Available; the row offers no arbitrary prompt or target-model input,
    // and the captured probe is the fixed native probe.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-route-check-success").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    // A one-hour recovery base interval keeps the automatic recovery scheduler
    // out of this test: the manual check is the only recovery path, so the
    // route stays unavailable however long the serialized browser window waits.
    set_recovery_settings(&client, &base, &active_cookie, RECOVERY_TEST_BASE_INTERVAL_MS, 5).await;
    let (upstream_base_url, probes, worker) =
        failing_then_slow_success_chat_upstream(Duration::from_millis(1500));
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "browser-check-model",
    )
    .await;
    probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    let Some(evidence) =
        browser_scenario("route-check-success", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    assert_eq!(evidence["disabledDuringCheck"], true);
    assert_eq!(evidence["labelDuringCheck"], "检查中…");
    assert_eq!(evidence["rowHasNoPromptInput"], true);
    assert_eq!(evidence["finalHealth"], "available");

    let manual_probe = probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_fixed_native_probe(&manual_probe, "browser-check-model");
    worker.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_route_check_disabled_while_checking_then_available() {
    // UI-008: while a route is Checking (startup probe in flight) its Check
    // button is disabled with an explanatory title, and the probe outcome
    // decides Available without any admin-set health.
    if harness::locate().is_none() {
        eprintln!("{BROWSER_SKIP_NOTICE}");
        return;
    }
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-route-check-disabled").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    // A one-hour recovery base interval keeps the automatic recovery scheduler
    // out of this test.
    set_recovery_settings(&client, &base, &active_cookie, RECOVERY_TEST_BASE_INTERVAL_MS, 5).await;
    // Create the route against a slow upstream (creation probe takes 12s and
    // blocks the create response, leaving the route Available afterwards).
    let (upstream_base_url, probes, worker) =
        slow_success_chat_upstream(Duration::from_secs(12), 2);
    configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "slow-probe-model",
    )
    .await;

    // Restart: startup resets every route to Checking and probes concurrently
    // without waiting for ready (ROUTE-004/005), so the route is Checking for
    // the probe duration. This window is timing-sensitive, so the restart and
    // the driver run happen *inside* the serialized browser window.
    let _guard = browser_lock().await;
    server.kill();
    let server = ServerGuard::new(environment.start(port_of(&base)));
    wait_ready(&client, port_of(&base)).await;

    let evidence = run_driver(
        "route-check-disabled",
        &base,
        FINAL_CREDENTIAL,
        None,
        Some(&json!({ "probe_delay_ms": 12_000 })),
    )
    .await;
    assert_eq!(evidence["disabledWhileChecking"], true);
    assert_eq!(evidence["titleWhileChecking"], "启动检查进行中");
    assert_eq!(evidence["finalHealth"], "available");
    // Both the creation probe and the post-restart startup probe were the
    // fixed native probe (ROUTE-016).
    let create_probe = probes.recv_timeout(Duration::from_secs(20)).unwrap();
    let startup_probe = probes.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_fixed_native_probe(&create_probe, "slow-probe-model");
    assert_fixed_native_probe(&startup_probe, "slow-probe-model");

    worker.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_route_check_error_shows_safe_feedback_and_leaves_retry_available() {
    // UI-008: a manual Check that overlaps an in-flight check (the shared
    // one-per-route guard, ROUTE-018) is rejected with a safe, actionable
    // message; the row button re-enables for retry, and the held probe was the
    // fixed native probe (ROUTE-022).
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-route-check-error").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    // A one-hour recovery base interval keeps the automatic recovery scheduler
    // out of this test: the only probe traffic is what the browser triggers.
    set_recovery_settings(&client, &base, &active_cookie, RECOVERY_TEST_BASE_INTERVAL_MS, 5).await;
    let (upstream_base_url, probes, release, worker) = failing_then_holding_chat_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "overlap-check-model",
    )
    .await;
    probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    let Some(evidence) =
        browser_scenario("route-check-error", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    assert!(
        evidence["feedbackText"]
            .as_str()
            .unwrap()
            .contains("already in progress"),
        "the overlap is reported with the safe actionable message: {}",
        evidence["feedbackText"]
    );
    assert_eq!(evidence["retryEnabled"], true, "the button stays available for retry");
    let text = evidence["text"].as_str().unwrap();
    assert!(
        !text.contains("chat_completions-route-key-1"),
        "the error surface must not leak the upstream API key (UI-008)"
    );

    // The held check's probe was the fixed native probe, not an arbitrary
    // prompt or model (ROUTE-022). Releasing lets the worker finish cleanly.
    let held_probe = probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_fixed_native_probe(&held_probe, "overlap-check-model");
    release.send(()).unwrap();
    worker.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_call_detail_expands_metadata_only_attempt_chain() {
    // UI-010/UI-011: a call row shows the published model and the successful
    // provider, and its detail expands the ordered model-route attempt chain
    // with metadata only — never request/response content.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-call-detail-chain").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let failing_upstream = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let succeeding_upstream = scripted_chat_upstream(vec![
        complete_chat_response(),
        complete_chat_response(),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_upstream.0,
        "chat_completions",
        "failing-chain-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let succeeding_route = configure_route(
        &client,
        &base,
        &active_cookie,
        succeeding_upstream.0,
        "chat_completions",
        "succeeding-chain-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_upstream.1.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_upstream.1.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "chain browser client",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let content_canary = "browser-chain-content-canary-7f3a";
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": content_canary }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let Some(evidence) = browser_scenario(
        "call-detail-chain",
        &base,
        FINAL_CREDENTIAL,
        None,
        Some(&json!({ "content_canary": content_canary })),
    )
    .await
    else {
        return;
    };
    let call_row = evidence["callRowText"].as_str().unwrap();
    assert!(
        call_row.contains("gpt-5.6-sol"),
        "the call row labels the published model first (UI-010)"
    );
    assert!(
        call_row.contains("chat_completions route 2x"),
        "the call row labels the successful upstream provider (UI-010)"
    );
    assert_eq!(evidence["attemptRowCount"], 2, "the fallback chain has two attempts");
    let chain = evidence["chainText"].as_str().unwrap();
    for expected in ["500", "200", "upstream http 5xx", "pre commit", "fallback", "committed", "success"] {
        assert!(chain.contains(expected), "the chain must show {expected:?}: {chain}");
    }
    assert_eq!(
        evidence["bodyContainsContentCanary"], false,
        "the calls surface must be metadata-only (OPS-020): no prompt content"
    );

    failing_upstream.2.join().unwrap();
    succeeding_upstream.2.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_onboarding_checklist_tracks_six_steps_and_hides_when_callable() {
    // UI-004: the empty configuration shows the six-step checklist wired to
    // real controls; once the whole chain is callable the checklist disappears.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-checklist").await;

    // Phase 1: a fresh first-login observes the empty checklist; the browser
    // itself rotates the bootstrap credential (SEC-004), so the API setup that
    // follows signs in with the rotated credential.
    let Some(empty) = browser_scenario("checklist", &base, &bootstrap_credential, Some(FINAL_CREDENTIAL), None).await
    else {
        return;
    };
    assert_eq!(empty["visible"], true, "the empty state is incomplete");
    assert_eq!(empty["stepCount"], 6);
    let steps: Vec<&str> = empty["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        vec![
            "添加上游供应商",
            "选择发布模型",
            "映射明确的上游模型与协议",
            "设置正数成本倍率",
            "为模型路由授予访问密钥资格",
            "验证并让配置可调用",
        ]
    );
    for step in empty["steps"].as_array().unwrap() {
        assert_eq!(step["done"], false, "an empty configuration has no done steps");
    }

    // Make the full chain callable through the API: healthy route + active key
    // with eligibility (the checklist completion contracts, pinned at the
    // process boundary). The browser rotated the credential in phase 1, so the
    // API signs in with the rotated credential.
    let active_cookie = login_only(&client, &base, FINAL_CREDENTIAL).await;
    let upstream = scripted_http_upstream(vec![http_json_response(&complete_chat_response())]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream.0,
        "chat_completions",
        "checklist-model",
    )
    .await;
    await_route_health(&client, &base, &active_cookie, &route_id, "available").await;
    let key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Checklist client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(key["model_route_ids"], json!([route_id.as_str()]));

    let Some(callable) = browser_scenario("checklist", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    assert_eq!(
        callable["visible"], false,
        "the checklist hides once the whole chain is callable"
    );
    assert!(
        !callable["text"].as_str().unwrap().contains("完成首个可调用配置"),
        "the completed configuration shows no onboarding checklist"
    );

    upstream.2.join().unwrap();
    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_validation_errors_render_next_to_fields() {
    // UI-006: submitting an incomplete/invalid route form renders field-
    // attributed, actionable errors beside the offending inputs (blank upstream
    // model name, non-positive multiplier) and never becomes callable.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-field-errors").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    // A provider must exist before the route form opens; its endpoint is a
    // dead loopback port, so the route-creation probe fails fast and the
    // create returns quickly (the form's own validation is what this test
    // exercises).
    let dead_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead_listener.local_addr().unwrap().port();
    drop(dead_listener);
    client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Validation provider",
            "base_url": format!("http://127.0.0.1:{dead_port}/v1"),
            "api_key": "validation-provider-key"
        }))
        .send()
        .await
        .unwrap();

    let Some(evidence) = browser_scenario("field-errors", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    // The store validates fail-fast (CFG-011/CFG-012), so each invalid
    // submission attributes its own field error.
    let first = &evidence["first"];
    assert_eq!(
        first["errors"].as_array().unwrap().len(),
        1,
        "the blank upstream-model submission reports exactly its own field"
    );
    let upstream_error = &first["errors"][0];
    assert_eq!(upstream_error["field"], "upstream_model_name");
    assert!(
        upstream_error["message"]
            .as_str()
            .unwrap()
            .contains("upstream model name"),
        "the upstream-model error is actionable: {}",
        upstream_error["message"]
    );
    assert_eq!(
        first["generalError"], "",
        "a rendered field error is not duplicated in the general error area"
    );

    let second = &evidence["second"];
    assert_eq!(
        second["errors"].as_array().unwrap().len(),
        1,
        "the non-positive multiplier submission reports exactly its own field"
    );
    let multiplier_error = &second["errors"][0];
    assert_eq!(multiplier_error["field"], "cost_multiplier");
    assert!(
        multiplier_error["message"]
            .as_str()
            .unwrap()
            .contains("greater than zero"),
        "the multiplier error is actionable: {}",
        multiplier_error["message"]
    );
    assert_eq!(
        second["generalError"], "",
        "a rendered field error is not duplicated in the general error area"
    );
    assert_eq!(
        evidence["panelStillOpen"], true,
        "the invalid submission keeps the panel open for correction"
    );
    // UI-006: a key without any eligible model route is rejected with a
    // field-attributed error beside the eligibility group (CFG-012).
    let key_errors = evidence["keyErrors"].as_array().unwrap();
    assert_eq!(key_errors.len(), 1, "the empty-eligibility key reports its field");
    assert!(
        key_errors[0]["message"]
            .as_str()
            .unwrap()
            .contains("at least one eligible model route"),
        "the eligibility error is actionable: {}",
        key_errors[0]["message"]
    );

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_data_security_panel_shows_backup_metadata_and_create() {
    // UI-012: the Data security panel opens from the Operations status area,
    // shows backup metadata, creates a manual backup, and offers no cloud,
    // download or delete controls.
    let (environment, base, server, _client, bootstrap_credential) =
        start_service("browser-data-security-panel").await;
    let Some(evidence) = browser_scenario(
        "data-security-panel",
        &base,
        &bootstrap_credential,
        Some(FINAL_CREDENTIAL),
        None,
    )
    .await
    else {
        return;
    };

    assert_eq!(evidence["title"], "数据安全");
    let panel_text = evidence["panelText"].as_str().unwrap();
    assert!(
        panel_text.contains("上次验证") && panel_text.contains("保留"),
        "the panel shows backup metadata (last verified, trigger, schema, size)"
    );
    assert!(panel_text.contains("尚未创建备份"));
    for excluded in ["cloud", "Download", "Delete"] {
        assert!(
            !panel_text.to_lowercase().contains(&excluded.to_lowercase()),
            "the Data security panel must not offer {excluded:?} (UI-012)"
        );
    }
    assert_eq!(
        evidence["rowsAfterCreate"], 1,
        "the manual backup appears in the protected list"
    );

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_failed_restore_reports_stage_and_returns_to_operations() {
    // UI-012/OPS-015: an explicit restore that fails reports its exact stage
    // and an actionable reason, states the current database was preserved, and
    // returns the operator to Operations.
    let environment = TestEnvironment::new("browser-restore-failure-panel");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let server = ServerGuard::new(environment.start(port));
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A candidate must exist before the failing process starts.
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // Restart with the pre-restore snapshot failure so the restore fails at a
    // known stage while the current database stays selected (DATA-015).
    server.kill();
    let server = ServerGuard::new(environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "create")],
    ));
    wait_ready(&client, port).await;

    let Some(evidence) =
        browser_scenario("restore-failure-panel", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    assert!(
        evidence["confirmMessage"]
            .as_str()
            .unwrap()
            .contains("从备份"),
        "restore requires explicit confirmation"
    );
    let failure_text = evidence["failureText"].as_str().unwrap();
    assert!(failure_text.contains("恢复失败"));
    assert!(failure_text.contains("backup current"), "the exact failed stage is named");
    assert!(
        failure_text.contains("已保留并继续使用"),
        "the operator is told the current database was preserved"
    );
    assert_eq!(evidence["hasReturnAction"], true);
    assert_eq!(evidence["backH1"], "操作台", "the surface stays operational");

    server.kill();
    drop(environment);
}

#[tokio::test]
async fn browser_status_area_drills_into_route_event_history() {
    // OPS-010: an abnormal Operations status area opens its 14-day metadata-
    // only event history from the same page, showing the route check events.
    let (environment, base, server, client, bootstrap_credential) =
        start_service("browser-status-area-event-history").await;
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    // A dead endpoint keeps the route unavailable, so the Model routes status
    // area is abnormal and offers the event-history drill-down.
    let dead_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead_listener.local_addr().unwrap().port();
    drop(dead_listener);
    configure_relay_route(
        &client,
        &base,
        &active_cookie,
        format!("http://127.0.0.1:{dead_port}/v1"),
        "chat_completions",
        "dead-endpoint-model",
    )
    .await;

    let Some(evidence) =
        browser_scenario("status-area-event-history", &base, FINAL_CREDENTIAL, None, None).await
    else {
        return;
    };
    assert_eq!(evidence["title"], "运维事件历史");
    assert!(
        evidence["eventRows"].as_u64().unwrap() >= 1,
        "the failed probe produced at least one route event"
    );
    assert!(
        evidence["tableText"].as_str().unwrap().contains("routes.check"),
        "the route event history shows the probe check events"
    );

    server.kill();
    drop(environment);
}
