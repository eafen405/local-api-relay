//! Windows-native acceptance: the relay uses `%LOCALAPPDATA%` for data/state
//! and `%APPDATA%` for configuration, and the full `init-admin` -> `serve` ->
//! management configuration -> `/v1/chat/completions` smoke path works on a
//! native Windows build.
//!
//! This suite is Windows-only; on other platforms the file compiles to no
//! tests, so the Linux XDG suite is untouched.

#![cfg(windows)]

use reqwest::{Client, StatusCode, header};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DEFAULT_PORT: u16 = 8787;

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
            "local-api-relay-windows-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_local-api-relay"));
        command
            .env("LOCALAPPDATA", self.root.join("local"))
            .env("APPDATA", self.root.join("roaming"));
        command
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("local/local-api-relay")
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("roaming/local-api-relay")
    }

    fn service_json(&self) -> PathBuf {
        self.config_dir().join("service.json")
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

    fn check(&self) -> std::process::Output {
        let output = self.command().arg("check").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn backup(&self) -> String {
        let output = self
            .command()
            .args(["backup", "--reason", "manual"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .strip_prefix("backup=")
            .expect("backup prints the artifact name")
            .trim()
            .to_owned()
    }

    fn restore(&self, name: &str) {
        let output = self.command().args(["restore", name]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn start(&self, port: u16) -> Child {
        self.command()
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

fn available_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        tokio::time::sleep(Duration::from_millis(50)).await;
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
    session_cookie(&changed)
}

/// A tiny loopback upstream that answers the relay's model-catalog probe and
/// one chat completion, which is enough for the Windows smoke path.
fn mock_upstream() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let catalog = r#"{"object":"list","data":[{"id":"smoke-upstream","object":"model"}]}"#;
        // The route-creation health probe is also a chat-completions POST, so
        // answer the first POST as a probe and the second as the smoke call.
        let chat_responses = [
            r#"{"id":"chatcmpl-windows-probe","object":"chat.completion","created":1,"model":"smoke-upstream","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
            r#"{"id":"chatcmpl-windows-smoke","object":"chat.completion","created":2,"model":"smoke-upstream","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
        ];
        for chat_response in chat_responses {
            loop {
                let (mut stream, _) = listener.accept().expect("mock upstream accept");
                let mut buffer = [0u8; 8192];
                let read = stream.read(&mut buffer).expect("mock upstream read");
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let response = if request.starts_with("GET /v1/models") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        catalog.len(),
                        catalog
                    )
                } else if request.starts_with("POST /v1/chat/completions") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        chat_response.len(),
                        chat_response
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                };
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
                if request.starts_with("POST /v1/chat/completions") {
                    break;
                }
            }
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), handle)
}

#[tokio::test]
async fn windows_native_relay_creates_appdata_layout_and_passes_smoke() {
    let environment = TestEnvironment::new("native-smoke");
    let client = Client::new();
    let port = available_port();

    // First `serve` creates the data/state and config roots under AppData and
    // writes the default 8787 service.json before binding the explicit port.
    let mut first = environment.start(port);
    wait_ready(&client, port).await;
    assert!(environment.data_dir().is_dir());
    assert!(environment.config_dir().is_dir());
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(environment.service_json()).unwrap()).unwrap();
    assert_eq!(settings["port"].as_u64(), Some(DEFAULT_PORT as u64));
    first.kill().unwrap();
    first.wait().unwrap();

    let bootstrap_credential = environment.initialize();
    let check = environment.check();
    assert!(String::from_utf8_lossy(&check.stdout).contains("port=8787"));
    let (upstream_base, upstream_worker) = mock_upstream();
    let mut server = environment.start(port);
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Windows smoke upstream",
            "base_url": upstream_base,
            "api_key": "smoke-upstream-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    let provider_id = provider.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "smoke-upstream",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(route.status(), StatusCode::CREATED);
    let route_id = route.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Windows smoke client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap();
    assert_eq!(key.status(), StatusCode::CREATED);
    let relay_secret = key.json::<serde_json::Value>().await.unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    let chat = chat.json::<serde_json::Value>().await.unwrap();
    assert_eq!(chat["choices"][0]["message"]["content"], "ok");

    server.kill().unwrap();
    server.wait().unwrap();
    upstream_worker.join().unwrap();

    // The remaining CLI surface uses the same AppData-backed paths and must
    // behave like Linux: manual backup, restore, and check all succeed.
    let backup_name = environment.backup();
    environment.restore(&backup_name);
    let check_after_restore = environment.check();
    assert!(String::from_utf8_lossy(&check_after_restore.stdout).contains("port=8787"));
}
