use reqwest::{Client, StatusCode, header};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// A body one KiB over the documented 16 MiB relay request body limit, so the
/// 413 assertions always exercise the over-limit branch (API-016).
const OVER_LIMIT_BODY_CHARS: usize = 16 * 1024 * 1024 + 1024;

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

    fn database_path(&self) -> PathBuf {
        self.root.join("data/local-api-relay/relay.sqlite3")
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

    /// Starts the service with standard error redirected to a file, so tests
    /// can scan the captured launcher-visible diagnostics (OPS-017).
    fn start_with_stderr_file(
        &self,
        port: u16,
        extra_env: &[(&str, &str)],
        stderr_path: &PathBuf,
    ) -> Child {
        let mut command = self.command();
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let stderr = fs::File::create(stderr_path).unwrap();
        command
            .args(["serve", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap()
    }

    fn start_with_commit_failure(&self, port: u16) -> Child {
        self.command()
            .env("LOCAL_API_RELAY_TEST_FAIL_CONFIG_COMMIT", "1")
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
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

async fn get_with_cookie(client: &Client, url: String, cookie: &str) -> reqwest::Response {
    client
        .get(url)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
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

async fn backup_count(client: &Client, base: &str, cookie: &str) -> usize {
    get_with_cookie(client, format!("{base}/admin/backups"), cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap()["status"]["count"]
        .as_u64()
        .unwrap() as usize
}

async fn wait_for_backup_count(
    client: &Client,
    base: &str,
    cookie: &str,
    expected: usize,
) -> String {
    for _ in 0..80 {
        let body = get_with_cookie(client, format!("{base}/admin/backups"), cookie)
            .await
            .json::<serde_json::Value>()
            .await
            .unwrap();
        if body["status"]["count"].as_u64().unwrap() as usize >= expected {
            return body["status"]["last_trigger"].as_str().unwrap().to_owned();
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("backup count did not reach {expected}");
}

#[cfg(unix)]
fn assert_backup_permissions(environment: &TestEnvironment) {
    let backup_dir = environment.root.join("data/local-api-relay/backups");
    assert_eq!(
        fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o077,
        0
    );
    let files = backup_artifact_paths(&backup_dir);
    assert_eq!(files.len(), 1);
    assert_eq!(
        fs::metadata(&files[0]).unwrap().permissions().mode() & 0o077,
        0
    );
}

fn backup_artifact_paths(directory: &PathBuf) -> Vec<PathBuf> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("could not list backups: {error}"),
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("backup-") && name.ends_with(".sqlite3"))
        })
        .collect()
}

/// Serves chat completions forever (one per connection) with a fixed delay
/// before each response, so a restore's synchronous route reset to Checking is
/// observable before the re-probe completes.
fn delayed_persistent_chat_upstream(
    delay: Duration,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        for response in std::iter::repeat(complete_chat_response()) {
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
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// Waits for a child process to exit on its own and returns its status,
/// panicking if it stays alive. Used for startup paths that must block ready
/// and exit non-zero (PKG-011/DATA-008/DATA-013).
fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    for _ in 0..200 {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("process did not exit");
}

/// The v8-schema DDL snapshot used to build old-schema fixtures for the
/// migration and restore drills. Transcribed once from the migration chain
/// (cases 1-7) as it stood when v8 was current: a fixed historical snapshot,
/// decoupled from the current schema, so the drills never mutate the live
/// database with `ALTER TABLE`/`DROP TABLE` (test-evidence hygiene, spec
/// "stable test fixtures/checkers").
const SCHEMA_V8_FIXTURE_SQL: &str = r#"
CREATE TABLE schema_metadata (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);
CREATE TABLE administrator_credentials (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    password_hash TEXT NOT NULL,
    must_change INTEGER NOT NULL CHECK (must_change IN (0, 1)),
    created_at INTEGER NOT NULL
);
CREATE TABLE administrator_sessions (
    id INTEGER PRIMARY KEY,
    token_hash BLOB NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('bootstrap', 'active')),
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX administrator_sessions_expiry ON administrator_sessions (expires_at);
CREATE TABLE upstream_providers (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL CHECK(length(trim(display_name)) > 0),
    base_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE published_models (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    input_price_micrormb_per_million INTEGER NOT NULL CHECK(input_price_micrormb_per_million >= 0),
    output_price_micrormb_per_million INTEGER NOT NULL CHECK(output_price_micrormb_per_million >= 0),
    cached_input_price_micrormb_per_million INTEGER NOT NULL CHECK(cached_input_price_micrormb_per_million >= 0)
);
CREATE TABLE model_routes (
    id TEXT PRIMARY KEY,
    published_model_id TEXT NOT NULL REFERENCES published_models(id),
    upstream_provider_id TEXT NOT NULL REFERENCES upstream_providers(id),
    upstream_model_name TEXT NOT NULL CHECK(length(trim(upstream_model_name)) > 0),
    protocol TEXT NOT NULL CHECK(protocol IN ('chat_completions', 'responses')),
    cost_multiplier_micros INTEGER NOT NULL CHECK(cost_multiplier_micros > 0),
    created_at INTEGER NOT NULL,
    UNIQUE(published_model_id, upstream_provider_id, upstream_model_name, protocol)
);
CREATE TABLE model_route_health (
    model_route_id TEXT PRIMARY KEY REFERENCES model_routes(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK(state IN ('checking', 'available', 'unavailable')),
    checked_at INTEGER,
    failure_category TEXT,
    failed_probe_count INTEGER NOT NULL DEFAULT 0,
    next_probe_at_ms INTEGER
);
CREATE TABLE relay_access_keys (
    id TEXT PRIMARY KEY,
    secret_prefix TEXT NOT NULL UNIQUE,
    secret_hash BLOB NOT NULL UNIQUE,
    label TEXT NOT NULL CHECK(length(trim(label)) > 0),
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE relay_key_route_eligibility (
    relay_access_key_id TEXT NOT NULL REFERENCES relay_access_keys(id) ON DELETE CASCADE,
    model_route_id TEXT NOT NULL REFERENCES model_routes(id) ON DELETE CASCADE,
    PRIMARY KEY (relay_access_key_id, model_route_id)
);
CREATE TABLE backup_metadata (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    application TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    trigger TEXT NOT NULL CHECK (trigger IN ('seed', 'auto', 'manual', 'migration', 'restore')),
    source_schema_version INTEGER NOT NULL
);
CREATE TABLE data_change_signal (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    writes INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE recovery_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    base_interval_ms INTEGER NOT NULL CHECK(base_interval_ms > 0),
    doubling_limit INTEGER NOT NULL CHECK(doubling_limit >= 0)
);
CREATE TABLE call_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_ms INTEGER NOT NULL,
    published_model_name TEXT NOT NULL,
    protocol TEXT NOT NULL CHECK(protocol IN ('chat_completions', 'responses')),
    streamed INTEGER NOT NULL CHECK(streamed IN (0, 1)),
    succeeded INTEGER NOT NULL CHECK(succeeded IN (0, 1)),
    success_provider_id TEXT,
    success_provider_name TEXT,
    input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
    cached_input_tokens INTEGER CHECK(cached_input_tokens IS NULL OR cached_input_tokens >= 0),
    output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
    estimated_cost_rmb REAL,
    completion_ms INTEGER CHECK(completion_ms IS NULL OR completion_ms >= 0),
    first_token_ms INTEGER CHECK(first_token_ms IS NULL OR first_token_ms >= 0)
);
CREATE TABLE call_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    call_record_id INTEGER NOT NULL REFERENCES call_records(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK(sequence >= 0),
    route_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    provider_name TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0),
    http_status INTEGER CHECK(http_status IS NULL OR (http_status >= 100 AND http_status < 600)),
    failure_category TEXT,
    commit_phase TEXT NOT NULL CHECK(commit_phase IN ('pre_commit', 'committed')),
    outcome TEXT NOT NULL CHECK(outcome IN ('fallback', 'failed', 'success', 'stream_terminated', 'client_cancelled'))
);
CREATE INDEX call_records_created_at ON call_records (created_at_ms DESC, id DESC);
CREATE INDEX call_attempts_record ON call_attempts (call_record_id);
CREATE TABLE daily_usage (
    day TEXT NOT NULL,
    published_model_name TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    provider_name TEXT NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost_rmb REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (day, published_model_name, provider_id)
);
CREATE TABLE usage_gaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    category TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    lost_records INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX usage_gaps_started ON usage_gaps (started_at_ms);
INSERT INTO schema_metadata (id, version) VALUES (1, 8);
INSERT INTO published_models (id, name, input_price_micrormb_per_million, output_price_micrormb_per_million, cached_input_price_micrormb_per_million) VALUES
    ('gpt-5.6-sol', 'gpt-5.6-sol', 5000000, 30000000, 500000),
    ('gpt-5.6-terra', 'gpt-5.6-terra', 2000000, 12000000, 200000),
    ('deepseek-v4-flash', 'deepseek-v4-flash', 1000000, 2000000, 20000);
INSERT INTO recovery_settings (id, base_interval_ms, doubling_limit) VALUES (1, 30000, 5);
INSERT INTO backup_metadata (id, application, created_at, trigger, source_schema_version) VALUES (1, 'local-api-relay', 0, 'seed', 0);
INSERT INTO data_change_signal (id, writes) VALUES (1, 0);
INSERT INTO upstream_providers (id, display_name, base_url, api_key, created_at) VALUES ('fixture-provider', 'chat_completions route 1x', 'http://127.0.0.1:1/v1', 'fixture-upstream-key', 1);
INSERT INTO model_routes (id, published_model_id, upstream_provider_id, upstream_model_name, protocol, cost_multiplier_micros, created_at) VALUES ('fixture-route', 'gpt-5.6-sol', 'fixture-provider', 'fixture-upstream-model', 'chat_completions', 1000000, 1);
INSERT INTO model_route_health (model_route_id, state, checked_at, failure_category, failed_probe_count, next_probe_at_ms) VALUES ('fixture-route', 'checking', NULL, NULL, 0, NULL);
"#;

/// The v9 addition to the v8 schema: `data_operations` was created by migration
/// case 8, so a v9 fixture carries it while a v8 fixture does not.
const SCHEMA_V9_FIXTURE_SQL: &str = r#"
CREATE TABLE data_operations (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    running_schema INTEGER NOT NULL,
    supported_schema INTEGER NOT NULL,
    migration_state TEXT NOT NULL CHECK (migration_state IN ('fresh', 'current', 'migrated')),
    migrated_from_schema INTEGER,
    pre_backup_ok INTEGER CHECK (pre_backup_ok IN (0, 1)),
    pre_backup_name TEXT,
    last_phase TEXT NOT NULL CHECK (last_phase IN ('none', 'migration', 'restore')),
    last_result TEXT NOT NULL CHECK (last_result IN ('none', 'ok', 'failed')),
    last_completed_at INTEGER,
    last_failed_stage TEXT,
    last_failed_reason TEXT,
    restore_source TEXT
);
UPDATE schema_metadata SET version = 9 WHERE id = 1;
"#;

/// Replaces the environment's database with a stable old-schema fixture (v8 or
/// v9) for the migration and restore drills, so the drills never mutate the
/// current schema with `ALTER TABLE`/`DROP TABLE`. The fixture keeps the
/// administrator row from the initialized database (argon2 hashes cannot be
/// fabricated by the test) and carries one configured route so the drills
/// observe real data surviving the migration.
fn seed_schema_fixture(environment: &TestEnvironment, version: i64) {
    assert!(version == 8 || version == 9, "fixture supports v8 or v9 only");
    let live = rusqlite::Connection::open(environment.database_path()).unwrap();
    let admin: (String, bool, i64) = live
        .query_row(
            "SELECT password_hash, must_change, created_at FROM administrator_credentials WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    drop(live);

    let database_path = environment.database_path();
    let parent = database_path.parent().unwrap();
    let file_name = database_path.file_name().unwrap().to_string_lossy();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(parent.join(format!("{file_name}{suffix}")));
    }
    let database = rusqlite::Connection::open(database_path).unwrap();
    database.execute_batch(SCHEMA_V8_FIXTURE_SQL).unwrap();
    if version == 9 {
        database.execute_batch(SCHEMA_V9_FIXTURE_SQL).unwrap();
    }
    // The administrator row is inserted before the data-change triggers exist,
    // so the signal counter stays at the seed value 0, exactly as in a genuine
    // v8-era database (a fresh init predates the triggers too).
    database
        .execute(
            "INSERT INTO administrator_credentials (id, password_hash, must_change, created_at)
             VALUES (1, ?1, ?2, ?3)",
            rusqlite::params![admin.0, admin.1, admin.2],
        )
        .unwrap();
    // The data-change triggers a real v8 database carried (migration cases
    // 3-5): writes to these tables mark the data as changed for the automatic
    // snapshot boundary (DATA-011).
    for table in [
        "administrator_credentials",
        "upstream_providers",
        "published_models",
        "model_routes",
        "model_route_health",
        "relay_access_keys",
        "relay_key_route_eligibility",
        "recovery_settings",
    ] {
        for (kind, action) in [
            ("insert", "INSERT"),
            ("update", "UPDATE"),
            ("delete", "DELETE"),
        ] {
            database
                .execute_batch(&format!(
                    "CREATE TRIGGER data_change_{table}_{kind} AFTER {action} ON {table}
                     BEGIN UPDATE data_change_signal SET writes = writes + 1 WHERE id = 1; END;"
                ))
                .unwrap();
        }
    }
    for table in ["call_records", "call_attempts"] {
        database
            .execute_batch(&format!(
                "CREATE TRIGGER data_change_{table}_insert AFTER INSERT ON {table}
                 BEGIN UPDATE data_change_signal SET writes = writes + 1 WHERE id = 1; END;"
            ))
            .unwrap();
    }
    drop(database);
}

/// Stamps the recorded schema version without changing any table, used to build
/// a "newer than supported" database that must be rejected without writes. This
/// is the only honest simulation of a future schema: its shape is unknowable,
/// so no fixture can represent it (DATA-008).
fn stamp_schema_version(environment: &TestEnvironment, version: i64) {
    let database = rusqlite::Connection::open(environment.database_path()).unwrap();
    database
        .execute(
            "UPDATE schema_metadata SET version = ?1 WHERE id = 1",
            [version],
        )
        .unwrap();
    drop(database);
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
            "published_model_id": published_model_id,
            "provider_id": provider_id,
            "upstream_model_name": upstream_model_name,
            "protocol": protocol,
            "cost_multiplier": cost_multiplier
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
        .json::<serde_json::Value>()
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

/// The `last_http_status` of a route row in the Operations snapshot (OPS-013);
/// `None` when the row has never recorded a safe HTTP status.
async fn route_last_http_status(
    client: &Client,
    base: &str,
    cookie: &str,
    route_id: &str,
) -> Option<i64> {
    get_with_cookie(client, format!("{base}/admin/operations"), cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == route_id)
        .unwrap_or_else(|| panic!("route {route_id} is missing from the operations snapshot"))
        ["last_http_status"]
        .as_i64()
}

/// Polls an upstream probe without blocking the tokio runtime thread, so that
/// spawned relay calls can make progress while the test waits.
/// Receives the next non-catalog capture, skipping the relay's zero-token
/// light-validation GETs (REL-002).
fn next_native_probe(receiver: &Receiver<CapturedProbe>) -> CapturedProbe {
    loop {
        let captured = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        if !is_catalog_get(&captured) {
            return captured;
        }
    }
}

async fn await_upstream_probe(receiver: &Receiver<CapturedProbe>) -> CapturedProbe {
    for _ in 0..80 {
        if let Ok(probe) = receiver.try_recv() {
            return probe;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("upstream probe did not arrive");
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

/// Sets the quarantine threshold (REL-004); legacy tests pin 1 to keep the
/// single-failure quarantine contract they assert.
async fn set_quarantine_threshold(client: &Client, base: &str, cookie: &str, threshold: i64) {
    let current = client
        .get(format!("{base}/admin/recovery-settings"))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let response = client
        .patch(format!("{base}/admin/recovery-settings"))
        .header(header::COOKIE, cookie)
        .json(&json!({
            "base_interval_ms": current["base_interval_ms"],
            "doubling_limit": current["doubling_limit"],
            "quarantine_threshold": threshold
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Configures the upstream deadlines (REL-001) so timeout behavior is
/// observable at test speed.
async fn set_relay_timeouts(
    client: &Client,
    base: &str,
    active_cookie: &str,
    first_event_timeout_ms: i64,
    stream_idle_timeout_ms: i64,
    nonstream_timeout_ms: i64,
) {
    let response = client
        .patch(format!("{base}/admin/recovery-settings"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({
            "base_interval_ms": 30_000,
            "doubling_limit": 5,
            "first_event_timeout_ms": first_event_timeout_ms,
            "stream_idle_timeout_ms": stream_idle_timeout_ms,
            "nonstream_timeout_ms": nonstream_timeout_ms
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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

/// The full Operations route row for `route_id`, used by the recovery-schedule
/// tests to assert the injected-clock anchors (`next_probe_at_ms`) and the
/// failed-probe index exactly.
async fn route_row(
    client: &Client,
    base: &str,
    cookie: &str,
    route_id: &str,
) -> serde_json::Value {
    get_with_cookie(client, format!("{base}/admin/operations"), cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap()["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == route_id)
        .unwrap_or_else(|| panic!("route {route_id} is missing from the operations snapshot"))
        .clone()
}

/// Polls a single field of the Operations route row until it equals `expected`.
async fn await_route_field(
    client: &Client,
    base: &str,
    cookie: &str,
    route_id: &str,
    field: &str,
    expected: serde_json::Value,
) {
    for _ in 0..100 {
        let row = route_row(client, base, cookie, route_id).await;
        if row[field] == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "route {route_id} field {field} did not reach {expected}: {}",
        route_row(client, base, cookie, route_id).await[field]
    );
}

/// Writes the recovery-scheduler test clock file: when
/// `LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE` is set, the server reads "now"
/// from this file, so tests drive recovery probes deterministically in the
/// injected clock instead of asserting wall-clock tolerances.
fn write_recovery_clock(path: &Path, epoch_ms: i64) {
    std::fs::write(path, epoch_ms.to_string()).unwrap();
}

async fn create_relay_secret(
    client: &Client,
    base: &str,
    active_cookie: &str,
    route_id: &str,
    label: &str,
) -> String {
    client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, active_cookie)
        .json(&json!({ "label": label, "model_route_ids": [route_id] }))
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

/// The ordered top-level keys of a JSON object as they appear on the wire.
/// Parsing preserves insertion order, so this asserts the literal field order
/// of a relayed request or response body.
fn json_object_key_order(value: &serde_json::Value) -> Vec<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

struct CapturedProbe {
    request_line: String,
    authorization: String,
    body: Vec<u8>,
}

type CancellableSseUpstream = (
    String,
    Receiver<CapturedProbe>,
    Receiver<(bool, bool)>,
    thread::JoinHandle<()>,
);

fn chat_probe_upstream() -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        let response = r#"{"id":"chatcmpl-probe","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
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

fn scripted_chat_upstream(
    responses: Vec<String>,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut last_model: Option<String> = None;
        for response in responses {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let captured = read_http_request(&mut stream);
                if is_catalog_get(&captured) {
                    let model = last_model
                        .clone()
                        .unwrap_or_else(|| "scripted-upstream-model".to_owned());
                    serve_catalog(&mut stream, &model);
                    sender.send(captured).unwrap();
                    continue;
                }
                last_model = model_name_from_body(&captured).or(last_model);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
                stream.flush().unwrap();
                sender.send(captured).unwrap();
                break;
            }
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// Like `scripted_chat_upstream`, but the catalog GETs always list
/// `catalog_model` instead of the remembered upstream model — used to make
/// light validation fail so the relay falls back to a native probe.
fn scripted_chat_upstream_with_catalog_override(
    responses: Vec<String>,
    catalog_model: &str,
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let catalog_model = catalog_model.to_owned();
    let worker = thread::spawn(move || {
        let mut last_model: Option<String> = None;
        for response in responses {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let captured = read_http_request(&mut stream);
                if is_catalog_get(&captured) {
                    serve_catalog(&mut stream, &catalog_model);
                    sender.send(captured).unwrap();
                    continue;
                }
                last_model = model_name_from_body(&captured).or(last_model);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
                stream.flush().unwrap();
                sender.send(captured).unwrap();
                break;
            }
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
        let mut last_model: Option<String> = None;
        for response in responses {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let captured = read_http_request(&mut stream);
                if is_catalog_get(&captured) {
                    let model = last_model
                        .clone()
                        .unwrap_or_else(|| "scripted-upstream-model".to_owned());
                    serve_catalog(&mut stream, &model);
                    sender.send(captured).unwrap();
                    continue;
                }
                last_model = model_name_from_body(&captured).or(last_model);
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
                sender.send(captured).unwrap();
                break;
            }
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// Serves sequential HTTP responses so tests can drive the recovery schedule
/// through an injected clock: each probe request is captured on a channel, and
/// the test advances the clock file to release the next probe.
fn timing_http_upstream(
    responses: Vec<String>,
) -> (
    String,
    Receiver<CapturedProbe>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut last_model: Option<String> = None;
        for response in responses {
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let captured = read_http_request(&mut stream);
                if is_catalog_get(&captured) {
                    let model = last_model
                        .clone()
                        .unwrap_or_else(|| "scripted-upstream-model".to_owned());
                    serve_catalog(&mut stream, &model);
                    sender.send(captured).unwrap();
                    continue;
                }
                last_model = model_name_from_body(&captured).or(last_model);
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
                sender.send(captured).unwrap();
                break;
            }
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

/// Serves the first chat probe immediately, then accepts a second probe and
/// holds it open until the test releases before answering success. A restarted
/// relay therefore reuses the same upstream endpoint without rebinding ports.
fn holding_second_chat_upstream() -> (
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
    let worker = thread::spawn(move || {
        let (mut first_stream, _) = listener.accept().unwrap();
        let first_captured = read_http_request(&mut first_stream);
        write!(
            first_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        first_stream.flush().unwrap();
        sender.send(first_captured).unwrap();

        // Startup light validation GETs are answered with a catalog that
        // does NOT list the configured model, so the relay falls back to the
        // held native probe below (REL-002).
        loop {
            let (mut catalog_stream, _) = listener.accept().unwrap();
            let catalog_captured = read_http_request(&mut catalog_stream);
            if is_catalog_get(&catalog_captured) {
                serve_catalog(&mut catalog_stream, "unrelated-model");
                sender.send(catalog_captured).unwrap();
                continue;
            }
            sender.send(catalog_captured).unwrap();
            let _ = release_receiver.recv();
            write!(
                catalog_stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            catalog_stream.flush().unwrap();
            break;
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        receiver,
        release_sender,
        worker,
    )
}

/// Serves the creation probe as a 500 and the next connection (the admin's
/// manual check) as success on the same endpoint. Proves the manual recovery
/// check restores an unavailable route with a fixed native probe.
fn failing_then_success_chat_upstream() -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>)
{
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

/// Serves a successful creation probe, then reports whether any extra
/// connection arrives within a quiet window. Proves healthy routes receive no
/// periodic probes.
fn quiet_after_probe_upstream() -> (
    String,
    Receiver<CapturedProbe>,
    Receiver<bool>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        let response = complete_chat_response();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        stream.flush().unwrap();
        sender.send(captured).unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut extra_connection = false;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    extra_connection = true;
                    break;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("could not observe upstream traffic: {error}"),
            }
        }
        result_sender.send(extra_connection).unwrap();
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        receiver,
        result_receiver,
        worker,
    )
}

/// Fails the creation probe, accepts the first recovery probe and holds it
/// open while watching for a duplicate, then fails it. Proves at most one
/// recovery probe is in flight per unavailable route.
fn single_recovery_probe_upstream() -> (
    String,
    Receiver<CapturedProbe>,
    Receiver<bool>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let failure =
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_http_request(&mut stream);
        let last_model =
            model_name_from_body(&captured).unwrap_or_else(|| "scripted-upstream-model".to_owned());
        stream.write_all(failure).unwrap();
        stream.flush().unwrap();
        sender.send(captured).unwrap();

        // The recovery attempt's light validation GET passes (the catalog
        // lists the model), so the held connection below is the native probe
        // (REL-002).
        let recovery_captured = loop {
            let (mut recovery_stream, _) = listener.accept().unwrap();
            let probe = read_http_request(&mut recovery_stream);
            if is_catalog_get(&probe) {
                serve_catalog(&mut recovery_stream, &last_model);
                sender.send(probe).unwrap();
                continue;
            }
            sender.send(probe).unwrap();
            break recovery_stream;
        };
        let mut recovery_stream = recovery_captured;

        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(1200);
        let mut duplicate_probe = false;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    duplicate_probe = true;
                    break;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("could not observe upstream traffic: {error}"),
            }
        }
        recovery_stream.write_all(failure).unwrap();
        recovery_stream.flush().unwrap();
        result_sender.send(duplicate_probe).unwrap();
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        receiver,
        result_receiver,
        worker,
    )
}

fn complete_chat_response() -> String {
    r#"{"id":"chatcmpl-scripted","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#.to_owned()
}

/// A valid upstream model catalog listing exactly one model id: scripted
/// upstreams answer the relay's zero-token light validation GETs with this
/// (REL-002/REL-006).
fn catalog_json(model_name: &str) -> String {
    format!(r#"{{"object":"list","data":[{{"id":"{model_name}","object":"model"}}]}}"#)
}

/// Whether a captured request is a model-catalog GET (light validation).
fn is_catalog_get(captured: &CapturedProbe) -> bool {
    captured.request_line.starts_with("GET /v1/models")
}

/// The upstream model name carried by a probe or call request body, if any.
fn model_name_from_body(captured: &CapturedProbe) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(&captured.body)
        .ok()
        .and_then(|body| {
            body.get("model").and_then(|m| m.as_str()).map(str::to_owned)
        })
}

/// Writes a catalog response for `model_name` to the stream.
fn serve_catalog(stream: &mut TcpStream, model_name: &str) {
    let catalog = http_json_response(&catalog_json(model_name));
    stream.write_all(catalog.as_bytes()).unwrap();
    stream.flush().unwrap();
}

fn complete_responses_response() -> String {
    r#"{"id":"resp-scripted","object":"response","created_at":1,"status":"completed","model":"scripted-responses-model","output":[{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}],"error":null,"custom_response":{"kept":true}}"#.to_owned()
}

fn failed_responses_response() -> String {
    r#"{"id":"resp-failed","object":"response","created_at":1,"status":"failed","model":"scripted-responses-model","output":[],"error":null}"#.to_owned()
}

fn responses_error_response() -> String {
    r#"{"id":"resp-error","object":"response","created_at":1,"status":"completed","model":"scripted-responses-model","output":[],"error":{"code":"upstream_error","message":"upstream-secret-must-not-leak"}}"#.to_owned()
}

fn http_json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sse_http_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn truncated_sse_http_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len() + 64
    )
}

fn http_status_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves the creation probe immediately, holds the manual-check probe
/// until release, then serves two 500s for real calls (REL-005 drill).
fn held_manual_check_then_failures_upstream(
) -> (String, Receiver<CapturedProbe>, mpsc::Sender<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let body = complete_chat_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        probe_stream.flush().unwrap();
        sender.send(probe).unwrap();

        // The manual-check probe is held open while the worker keeps serving
        // the real call so the quarantine can happen while the probe is still
        // in flight (REL-005). A detached thread answers the held probe on
        // release.
        let (mut manual_stream, _) = listener.accept().unwrap();
        let manual = read_http_request(&mut manual_stream);
        sender.send(manual).unwrap();
        let manual_body = body.clone();
        thread::spawn(move || {
            let _ = release_receiver.recv();
            write!(
                manual_stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                manual_body.len(),
                manual_body
            )
            .unwrap();
            manual_stream.flush().unwrap();
        });

        let failure =
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        for _ in 0..1 {
            let (mut stream, _) = listener.accept().unwrap();
            let captured = read_http_request(&mut stream);
            stream.write_all(failure).unwrap();
            stream.flush().unwrap();
            sender.send(captured).unwrap();
        }
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        receiver,
        release_sender,
        worker,
    )
}

/// Serves a successful route-creation probe, then a slow non-streaming
/// success (partial body, stall, remainder), then a 500 failure. Used to prove
/// that an old in-flight success cannot restore a route quarantined by a
/// concurrent request.
fn slow_success_then_failure_upstream() -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>)
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let probe_body = complete_chat_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            probe_body.len(),
            probe_body
        )
        .unwrap();
        probe_stream.flush().unwrap();
        sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let body = complete_chat_response();
        let split = body.len() / 2;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            &body[..split]
        )
        .unwrap();
        stream.flush().unwrap();
        sender.send(request).unwrap();
        thread::sleep(Duration::from_millis(1500));
        stream.write_all(&body.as_bytes()[split..]).unwrap();
        stream.flush().unwrap();

        let (mut failure_stream, _) = listener.accept().unwrap();
        let failed = read_http_request(&mut failure_stream);
        write!(
            failure_stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        failure_stream.flush().unwrap();
        sender.send(failed).unwrap();
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

fn stalling_sse_upstream() -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let response = complete_chat_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        probe_stream.flush().unwrap();
        sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream.flush().unwrap();
        sender.send(request).unwrap();
        thread::sleep(Duration::from_secs(6));
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

fn stall_after_first_event_sse_upstream(
) -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let response = complete_chat_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        probe_stream.flush().unwrap();
        sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let first_event = "data: {\"id\":\"chatcmpl-stalled\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
        stream.write_all(first_event.as_bytes()).unwrap();
        stream.flush().unwrap();
        // Stall after the committed first event: the relay's idle deadline
        // fires while this upstream simply stays silent, then closes.
        thread::sleep(Duration::from_secs(3));
        sender.send(request).unwrap();
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

fn paced_sse_upstream() -> (String, Receiver<CapturedProbe>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let response = complete_chat_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        probe_stream.flush().unwrap();
        sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        for index in 0..7 {
            let event = if index == 6 {
                "data: [DONE]\n\n".to_owned()
            } else {
                format!(
                    "data: {{\"id\":\"chatcmpl-paced\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{index}\"}},\"finish_reason\":null}}]}}\n\n"
                )
            };
            stream.write_all(event.as_bytes()).unwrap();
            stream.flush().unwrap();
            if index < 6 {
                thread::sleep(Duration::from_secs(3));
            }
        }
        sender.send(request).unwrap();
    });
    (format!("http://127.0.0.1:{port}/v1"), receiver, worker)
}

fn cancellable_sse_upstream() -> CancellableSseUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (request_sender, request_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let response = complete_chat_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        probe_stream.flush().unwrap();
        request_sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let event = b"data: {\"id\":\"chatcmpl-cancel\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[]}\n\n";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
        )
        .unwrap();
        stream.write_all(event).unwrap();
        stream.flush().unwrap();
        request_sender.send(request).unwrap();

        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = [0_u8; 1];
        let upstream_cancelled = match stream.read(&mut buffer) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut started_another_attempt = false;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    started_another_attempt = true;
                    break;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("could not observe additional upstream attempts: {error}"),
            }
        }
        result_sender
            .send((upstream_cancelled, started_another_attempt))
            .unwrap();
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        request_receiver,
        result_receiver,
        worker,
    )
}

/// The Responses-protocol twin of `cancellable_sse_upstream`: serves a
/// non-streaming probe, then a Responses SSE stream whose first native event
/// (`response.created`) is committed and relayed, and reports whether the relay
/// cancelled the streamed request after the downstream client disconnected and
/// whether any further upstream attempt was made.
fn cancellable_responses_sse_upstream() -> CancellableSseUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (request_sender, request_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut probe_stream, _) = listener.accept().unwrap();
        let probe = read_http_request(&mut probe_stream);
        let response = complete_responses_response();
        write!(
            probe_stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        probe_stream.flush().unwrap();
        request_sender.send(probe).unwrap();

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let event = b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp-cancel\",\"object\":\"response\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\"}}\n\n";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
        )
        .unwrap();
        stream.write_all(event).unwrap();
        stream.flush().unwrap();
        request_sender.send(request).unwrap();

        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = [0_u8; 1];
        let upstream_cancelled = match stream.read(&mut buffer) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut started_another_attempt = false;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => {
                    started_another_attempt = true;
                    break;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("could not observe additional upstream attempts: {error}"),
            }
        }
        result_sender
            .send((upstream_cancelled, started_another_attempt))
            .unwrap();
    });
    (
        format!("http://127.0.0.1:{port}/v1"),
        request_receiver,
        result_receiver,
        worker,
    )
}

fn read_http_request(stream: &mut TcpStream) -> CapturedProbe {
    let reader_stream = stream.try_clone().unwrap();
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut authorization = String::new();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.trim_end().split_once(':').unwrap();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = value.trim().to_owned();
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).unwrap();
    CapturedProbe {
        request_line: request_line.trim_end().to_owned(),
        authorization,
        body,
    }
}

#[tokio::test]
async fn secure_local_management_surface_works_at_the_real_process_boundary() {
    let environment = TestEnvironment::new("management-surface");
    let bootstrap_credential = environment.initialize();
    assert!(bootstrap_credential.len() >= 32);

    let repeated_initialization = environment.command().arg("init-admin").output().unwrap();
    assert!(!repeated_initialization.status.success());
    assert!(
        !String::from_utf8_lossy(&repeated_initialization.stderr).contains(&bootstrap_credential)
    );

    assert_eq!(
        fs::read_to_string(environment.root.join("config/local-api-relay/service.json")).unwrap(),
        "{\n  \"port\": 8787\n}"
    );
    assert_private_paths(&environment);

    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");

    let ready = client.get(format!("{base}/ready")).send().await.unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    assert_eq!(
        ready.json::<serde_json::Value>().await.unwrap(),
        json!({ "status": "ready" })
    );

    let root = client
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(root.contains("<title>本地 API 中转</title>"));
    assert!(!root.contains(&bootstrap_credential));
    // The embedded frontend never carries the one-time bootstrap credential
    // (SEC-004/SEC-005). The browser-facing behavior this test used to pin via
    // a script marker (Operations as the default view after sign-in) is now
    // covered by the real-browser test `browser_login_lands_on_operations_default_view`.
    let script = client
        .get(format!("{base}/assets/app.js"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!script.contains(&bootstrap_credential));

    let bootstrap_login = client
        .post(format!("{base}/admin/login"))
        .json(&json!({ "password": bootstrap_credential }))
        .send()
        .await
        .unwrap();
    assert_eq!(bootstrap_login.status(), StatusCode::OK);
    assert_eq!(
        bootstrap_login
            .headers()
            .get(header::CACHE_CONTROL)
            .unwrap(),
        "no-store"
    );
    let bootstrap_cookie = session_cookie(&bootstrap_login);
    assert!(
        bootstrap_login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("HttpOnly")
    );
    assert_eq!(
        bootstrap_login.json::<serde_json::Value>().await.unwrap()["must_change_password"],
        true
    );

    let bootstrap_operations = get_with_cookie(
        &client,
        format!("{base}/admin/operations"),
        &bootstrap_cookie,
    )
    .await;
    assert_eq!(bootstrap_operations.status(), StatusCode::FORBIDDEN);

    let changed = client
        .post(format!("{base}/admin/change-password"))
        .header(header::COOKIE, &bootstrap_cookie)
        .json(&json!({ "new_password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::OK);
    let active_cookie = session_cookie(&changed);

    let operations =
        get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie).await;
    assert_eq!(operations.status(), StatusCode::OK);
    assert_eq!(
        operations.json::<serde_json::Value>().await.unwrap()["model_routes"]["available"],
        0
    );
    let usage = get_with_cookie(&client, format!("{base}/admin/calls-usage"), &active_cookie).await;
    assert_eq!(usage.status(), StatusCode::OK);
    assert_eq!(
        usage.json::<serde_json::Value>().await.unwrap()["calls"],
        json!([])
    );

    let old_bootstrap = client
        .post(format!("{base}/admin/login"))
        .json(&json!({ "password": bootstrap_credential }))
        .send()
        .await
        .unwrap();
    assert_eq!(old_bootstrap.status(), StatusCode::UNAUTHORIZED);

    let relay = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, "Bearer unrelated-relay-key")
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(relay.status(), StatusCode::UNAUTHORIZED);

    let database = fs::read(environment.database_path()).unwrap();
    assert!(
        !database
            .windows(bootstrap_credential.len())
            .any(|slice| slice == bootstrap_credential.as_bytes())
    );
    assert!(
        !database
            .windows(active_cookie.len())
            .any(|slice| slice == active_cookie.as_bytes())
    );

    let logged_out = client
        .post(format!("{base}/admin/logout"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logged_out.status(), StatusCode::NO_CONTENT);
    let revoked_session =
        get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie).await;
    assert_eq!(revoked_session.status(), StatusCode::UNAUTHORIZED);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn administrator_can_configure_and_check_the_first_model_route() {
    let environment = TestEnvironment::new("first-model-route");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let initial_operations =
        get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
            .await
            .json::<serde_json::Value>()
            .await
            .unwrap();
    assert_eq!(
        initial_operations["catalog"],
        json!([
            { "id": "deepseek-v4-flash", "name": "deepseek-v4-flash", "input_price_rmb": "1", "output_price_rmb": "2", "cached_input_price_rmb": "0.02", "deprecated": false },
            { "id": "gpt-5.6-sol", "name": "gpt-5.6-sol", "input_price_rmb": "5", "output_price_rmb": "30", "cached_input_price_rmb": "0.5", "deprecated": false },
            { "id": "gpt-5.6-terra", "name": "gpt-5.6-terra", "input_price_rmb": "2", "output_price_rmb": "12", "cached_input_price_rmb": "0.2", "deprecated": false }
        ])
    );

    let (upstream_base_url, probe_requests, upstream_worker) = chat_probe_upstream();
    let upstream_key = "upstream-api-key-that-must-not-leak";
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Local scripted upstream",
            "base_url": upstream_base_url,
            "api_key": upstream_key
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    let provider_body = provider.text().await.unwrap();
    assert!(!provider_body.contains(upstream_key));
    let provider_id = serde_json::from_str::<serde_json::Value>(&provider_body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let duplicate_base_url = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Same endpoint, separate credentials",
            "base_url": upstream_base_url,
            "api_key": "another-secret"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate_base_url.status(), StatusCode::CREATED);
    let duplicate_provider_id = duplicate_base_url
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let created_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "scripted-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1.25"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created_route.status(), StatusCode::CREATED);
    assert_eq!(
        created_route.json::<serde_json::Value>().await.unwrap()["health"],
        "available"
    );
    let captured = probe_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();
    assert_eq!(captured.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(captured.authorization, format!("Bearer {upstream_key}"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap(),
        json!({
            "model": "scripted-upstream-model",
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false
        })
    );

    let unavailable_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-terra",
            "provider_id": duplicate_provider_id,
            "upstream_model_name": "offline-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable_route.status(), StatusCode::CREATED);
    assert_eq!(
        unavailable_route.json::<serde_json::Value>().await.unwrap()["health"],
        "unavailable"
    );

    let invalid_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "not-created",
            "protocol": "chat_completions",
            "cost_multiplier": "0"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_route.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        invalid_route.json::<serde_json::Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("greater than zero")
    );

    let negative_multiplier = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "not-created",
            "protocol": "chat_completions",
            "cost_multiplier": "-0.5"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        negative_multiplier.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let operations_response =
        get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie).await;
    let operations_text = operations_response.text().await.unwrap();
    assert!(!operations_text.contains(upstream_key));
    assert!(!operations_text.contains(&upstream_base_url));
    let operations: serde_json::Value = serde_json::from_str(&operations_text).unwrap();
    assert_eq!(operations["providers"][0]["api_key_masked"], "********");
    assert_eq!(operations["routes"].as_array().unwrap().len(), 2);
    assert_eq!(operations["routes"][0]["health"], "available");
    assert_eq!(operations["routes"][0]["cost_multiplier"], "1.25");
    assert!(operations["routes"][0]["last_checked_at"].is_number());
    assert_eq!(operations["routes"][1]["health"], "unavailable");

    let price_update = client
        .patch(format!("{base}/admin/published-models/gpt-5.6-sol/prices"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "input_price_rmb": "6.5",
            "output_price_rmb": "31",
            "cached_input_price_rmb": "0.6"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(price_update.status(), StatusCode::OK);
    server.kill().unwrap();
    server.wait().unwrap();

    let restart_port = available_port();
    let mut restarted_server = environment.start(restart_port);
    wait_ready(&client, restart_port).await;
    let restarted_base = format!("http://127.0.0.1:{restart_port}");
    let login = client
        .post(format!("{restarted_base}/admin/login"))
        .json(&json!({ "password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let restarted_cookie = session_cookie(&login);
    let after_restart = get_with_cookie(
        &client,
        format!("{restarted_base}/admin/operations"),
        &restarted_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(after_restart["catalog"][1]["input_price_rmb"], "6.5");
    assert_eq!(after_restart["routes"].as_array().unwrap().len(), 2);
    restarted_server.kill().unwrap();
    restarted_server.wait().unwrap();
}

#[tokio::test]
async fn administrator_cannot_create_a_relay_access_key_without_an_eligible_model_route() {
    let environment = TestEnvironment::new("relay-key-requires-eligibility");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let response = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Codex client", "model_route_ids": [] }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("eligible model route")
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_access_key_discovers_models_and_transparently_completes_a_chat_call() {
    let environment = TestEnvironment::new("relay-key-first-chat");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let probe_response = r#"{"id":"chatcmpl-probe","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#.to_owned();
    let chat_response = r#"{"id":"chatcmpl-answer","object":"chat.completion","created":2,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"complete","tool_calls":[{"id":"call_1","type":"function"}]},"finish_reason":"stop"}],"upstream_extension":{"kept":true}}"#.to_owned();
    let (upstream_base_url, upstream_requests, upstream_worker) =
        scripted_chat_upstream(vec![probe_response, chat_response]);
    let upstream_key = "upstream-api-key-that-must-not-leak";
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "First chat upstream",
            "base_url": upstream_base_url,
            "api_key": upstream_key
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
            "upstream_model_name": "scripted-upstream-model",
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
    let probe = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(probe.authorization, format!("Bearer {upstream_key}"));

    let created_key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Codex client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap();
    assert_eq!(created_key.status(), StatusCode::CREATED);
    let created_key = created_key.json::<serde_json::Value>().await.unwrap();
    let relay_secret = created_key["secret"].as_str().unwrap().to_owned();
    assert!(relay_secret.starts_with("lar_"));
    assert_eq!(created_key["label"], "Codex client");
    assert!(created_key["prefix"].as_str().unwrap().len() < relay_secret.len());

    let listed_keys = get_with_cookie(
        &client,
        format!("{base}/admin/relay-access-keys?search=Codex"),
        &active_cookie,
    )
    .await;
    assert_eq!(listed_keys.status(), StatusCode::OK);
    let listed_keys = listed_keys.json::<serde_json::Value>().await.unwrap();
    assert_eq!(listed_keys["data"].as_array().unwrap().len(), 1);
    // REL-010: a personal relay keeps the full secret re-displayable, so the
    // management list carries it (owner-only local files protect it at rest).
    assert_eq!(listed_keys["data"][0]["secret"], relay_secret);

    let models = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::OK);
    assert_eq!(
        models.json::<serde_json::Value>().await.unwrap(),
        json!({
            "object": "list",
            "data": [{
                "id": "gpt-5.6-sol",
                "object": "model",
                "created": 0,
                "owned_by": "local-api-relay"
            }]
        })
    );

    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }],
            "client_extension": { "preserve": [1, 2, 3] }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    let chat = chat.json::<serde_json::Value>().await.unwrap();
    assert_eq!(chat["model"], "gpt-5.6-sol");
    assert_eq!(chat["upstream_extension"], json!({ "kept": true }));
    assert_eq!(
        chat["choices"][0]["message"]["tool_calls"][0]["id"],
        "call_1"
    );

    let upstream_chat = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    upstream_worker.join().unwrap();
    assert_eq!(
        upstream_chat.request_line,
        "POST /v1/chat/completions HTTP/1.1"
    );
    assert_eq!(
        upstream_chat.authorization,
        format!("Bearer {upstream_key}")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&upstream_chat.body).unwrap(),
        json!({
            "model": "scripted-upstream-model",
            "messages": [{ "role": "user", "content": "hello" }],
            "stream": false,
            "client_extension": { "preserve": [1, 2, 3] }
        })
    );

    let database = fs::read(environment.database_path()).unwrap();
    // REL-010 stores the relay key in plaintext on purpose; the remaining
    // secrecy invariant is that the upstream API key never reaches any
    // management surface (SEC-008).
    assert!(
        !database
            .windows(upstream_key.len())
            .any(|slice| slice == upstream_key.as_bytes())
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_preserves_client_and_upstream_field_order_at_the_process_boundary() {
    let environment = TestEnvironment::new("relay-field-order");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The scripted upstream response deliberately lists its top-level fields
    // in non-alphabetical order with `model` mid-object and a non-published
    // model identity, so the relay must rewrite `model` in place while
    // preserving the upstream field order (API-008).
    let ordered_response = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"zz_extension":1,"object":"chat.completion","model":"scripted-order-model","aa_extension":2,"id":"chatcmpl-order","created":1}"#;
    let (upstream_base_url, upstream_requests, upstream_worker) =
        scripted_chat_upstream(vec![complete_chat_response(), ordered_response.to_owned()]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-order-model",
    )
    .await;
    // The creation probe occupies the first captured request on the channel.
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Field order client",
    )
    .await;

    // The client request deliberately lists its fields (known and unknown) in
    // non-alphabetical order so the forwarded body can be order-asserted.
    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "z_request_extension": { "b_inner": 1, "a_inner": 2 },
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }],
            "a_request_extension": [1, 2, 3],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    let chat_body: serde_json::Value = serde_json::from_str(&chat.text().await.unwrap()).unwrap();

    // API-008: the successful response preserves the upstream field order
    // (including the `model` key's original mid-object position) and presents
    // the published model name at the client boundary.
    assert_eq!(
        json_object_key_order(&chat_body),
        vec![
            "choices", "zz_extension", "object", "model", "aa_extension", "id", "created"
        ]
    );
    assert_eq!(chat_body["model"], "gpt-5.6-sol");

    // API-006: the forwarded request preserves the client's field order,
    // including unknown fields and their nested order.
    let forwarded = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let forwarded_body: serde_json::Value = serde_json::from_slice(&forwarded.body).unwrap();
    assert_eq!(
        json_object_key_order(&forwarded_body),
        vec![
            "z_request_extension", "model", "messages", "a_request_extension", "stream"
        ]
    );
    assert_eq!(forwarded_body["model"], "scripted-order-model");
    assert_eq!(
        json_object_key_order(&forwarded_body["z_request_extension"]),
        vec!["b_inner", "a_inner"]
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_routes_only_within_key_eligibility_and_sorts_by_multiplier_then_stable_id() {
    let environment = TestEnvironment::new("deterministic-eligible-routing");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) =
        scripted_chat_upstream(vec![complete_chat_response(); 7]);
    let first_provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Shared endpoint first credential",
            "base_url": upstream_base_url,
            "api_key": "first-upstream-key"
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
    let second_provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Shared endpoint second credential",
            "base_url": upstream_base_url,
            "api_key": "second-upstream-key"
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

    let expensive_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": first_provider,
            "upstream_model_name": "expensive-sol",
            "protocol": "chat_completions",
            "cost_multiplier": "3"
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
    let first_low_cost_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": second_provider,
            "upstream_model_name": "low-cost-sol-a",
            "protocol": "chat_completions",
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
        .to_owned();
    let second_low_cost_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": first_provider,
            "upstream_model_name": "low-cost-sol-b",
            "protocol": "chat_completions",
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
        .to_owned();
    let terra_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-terra",
            "provider_id": first_provider,
            "upstream_model_name": "terra-only",
            "protocol": "chat_completions",
            "cost_multiplier": "0.1"
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

    for _ in 0..4 {
        upstream_requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
    }

    let full_scope_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "All eligible routes",
            "model_route_ids": [
                expensive_route,
                first_low_cost_route,
                second_low_cost_route,
                terra_route
            ]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let restricted_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "One model only",
            "model_route_ids": [expensive_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let full_scope_models = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {full_scope_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        full_scope_models["data"],
        json!([
            { "id": "gpt-5.6-sol", "object": "model", "created": 0, "owned_by": "local-api-relay" },
            { "id": "gpt-5.6-terra", "object": "model", "created": 0, "owned_by": "local-api-relay" }
        ])
    );
    let restricted_models = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {restricted_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        restricted_models["data"],
        json!([{
            "id": "gpt-5.6-sol",
            "object": "model",
            "created": 0,
            "owned_by": "local-api-relay"
        }])
    );

    let expected_initial = if first_low_cost_route < second_low_cost_route {
        ("low-cost-sol-a", "second-upstream-key")
    } else {
        ("low-cost-sol-b", "first-upstream-key")
    };
    let first_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {full_scope_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "route deterministically" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(first_chat.status(), StatusCode::OK);
    let first_forwarded = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        first_forwarded.authorization,
        format!("Bearer {}", expected_initial.1)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first_forwarded.body).unwrap()["model"],
        expected_initial.0
    );

    let reprioritized = client
        .patch(format!("{base}/admin/model-routes/{expensive_route}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": first_provider,
            "upstream_model_name": "expensive-sol",
            "protocol": "chat_completions",
            "cost_multiplier": "0.5"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reprioritized.status(), StatusCode::OK);

    let reprioritized_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {full_scope_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "route after reprioritization" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(reprioritized_chat.status(), StatusCode::OK);
    let reprioritized_forwarded = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reprioritized_forwarded.body).unwrap()["model"],
        "expensive-sol"
    );
    assert_eq!(
        reprioritized_forwarded.authorization,
        "Bearer first-upstream-key"
    );

    let terra_chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {full_scope_secret}"))
        .json(&json!({
            "model": "gpt-5.6-terra",
            "messages": [{ "role": "user", "content": "do not cross models" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(terra_chat.status(), StatusCode::OK);
    let terra_forwarded = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&terra_forwarded.body).unwrap()["model"],
        "terra-only"
    );
    upstream_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn administrator_edits_route_eligibility_and_rechecks_changed_provider_connections() {
    let environment = TestEnvironment::new("route-eligibility-editing");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) =
        scripted_chat_upstream(vec![complete_chat_response(); 5]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Editable provider",
            "base_url": upstream_base_url,
            "api_key": "first-upstream-key"
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
    let sol_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "editable-sol",
            "protocol": "chat_completions",
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
        .to_owned();
    let terra_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-terra",
            "provider_id": provider,
            "upstream_model_name": "editable-terra",
            "protocol": "chat_completions",
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
        .to_owned();
    for _ in 0..2 {
        assert_eq!(
            upstream_requests
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .authorization,
            "Bearer first-upstream-key"
        );
    }

    let created_key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Editable scope", "model_route_ids": [sol_route] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let key_id = created_key["id"].as_str().unwrap().to_owned();
    let relay_secret = created_key["secret"].as_str().unwrap().to_owned();

    let updated_scope = client
        .patch(format!("{base}/admin/relay-access-keys/{key_id}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Terra only",
            "model_route_ids": [terra_route]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(updated_scope.status(), StatusCode::OK);
    let terra_models = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(terra_models["data"][0]["id"], "gpt-5.6-terra");
    assert_eq!(terra_models["data"].as_array().unwrap().len(), 1);

    for invalid_scope in [
        json!({ "label": "Broken scope", "model_route_ids": [terra_route, terra_route] }),
        json!({ "label": "Broken scope", "model_route_ids": [] }),
        json!({ "label": "Broken scope", "model_route_ids": ["missing-route"] }),
    ] {
        let rejected = client
            .patch(format!("{base}/admin/relay-access-keys/{key_id}"))
            .header(header::COOKIE, &active_cookie)
            .json(&invalid_scope)
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let unchanged_key = get_with_cookie(
        &client,
        format!("{base}/admin/relay-access-keys?search=Terra%20only"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(unchanged_key["data"][0]["label"], "Terra only");
    assert_eq!(
        unchanged_key["data"][0]["model_route_ids"],
        json!([terra_route])
    );

    let provider_details = get_with_cookie(
        &client,
        format!("{base}/admin/providers/{provider}"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(provider_details["base_url"], upstream_base_url);
    assert!(provider_details.get("api_key").is_none());

    let updated_provider = client
        .patch(format!("{base}/admin/providers/{provider}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Edited provider",
            "base_url": upstream_base_url,
            "api_key": "second-upstream-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(updated_provider.status(), StatusCode::OK);
    for _ in 0..2 {
        assert_eq!(
            upstream_requests
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .authorization,
            "Bearer second-upstream-key"
        );
    }

    let updated_route = client
        .patch(format!("{base}/admin/model-routes/{terra_route}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-terra",
            "provider_id": provider,
            "upstream_model_name": "editable-terra",
            "protocol": "chat_completions",
            "cost_multiplier": "0.25"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(updated_route.status(), StatusCode::OK);

    let checked = client
        .post(format!("{base}/admin/model-routes/{sol_route}/check"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(checked.status(), StatusCode::OK);
    assert_eq!(
        checked.json::<serde_json::Value>().await.unwrap()["health"],
        "available"
    );
    let manual_probe = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(manual_probe.authorization, "Bearer second-upstream-key");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&manual_probe.body).unwrap()["model"],
        "editable-sol"
    );
    upstream_worker.join().unwrap();

    let operations_response =
        get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie).await;
    let operations_text = operations_response.text().await.unwrap();
    assert!(!operations_text.contains("second-upstream-key"));
    assert!(!operations_text.contains(&upstream_base_url));
    let operations: serde_json::Value = serde_json::from_str(&operations_text).unwrap();
    assert_eq!(
        operations["providers"][0]["display_name"],
        "Edited provider"
    );
    assert_eq!(
        operations["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["id"] == terra_route)
            .unwrap()["cost_multiplier"],
        "0.25"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn provider_connection_edits_recheck_an_unavailable_route_but_health_neutral_edits_do_not() {
    // ROUTE-010/UI-007 adjudication: a connection-relevant config edit ends
    // the stale quarantine cycle (its evidence pertained to the old
    // connection) and the system re-checks with the same native probe used at
    // startup — the edit itself never sets health, the probe decides. A
    // connection edit to a working upstream therefore recovers an unavailable
    // route immediately (no recovery-schedule wait), a still-broken connection
    // keeps it unavailable, and non-connection edits (display name, cost
    // multiplier) leave health and the quarantine cycle untouched.
    let environment = TestEnvironment::new("config-edit-health-semantics");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The dead upstream serves the creation probe and the re-checks after a
    // still-broken provider edit and a route-level connection-class edit. The
    // working upstream serves the recovery re-check, then watches for any
    // extra connection so the test can prove the health-neutral edits fire no
    // probes.
    let five_hundred = http_status_response(500, "Internal Server Error", "");
    let (dead_url, dead_probes, dead_worker) = scripted_http_upstream(vec![
        five_hundred.clone(),
        five_hundred.clone(),
        five_hundred,
    ]);
    let (working_url, working_probes, extra_connection, working_worker) =
        quiet_after_probe_upstream();

    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Connection-edit provider",
            "base_url": dead_url,
            "api_key": "dead-key-1"
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
    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "config-edit-model",
            "protocol": "chat_completions",
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
        .to_owned();
    // The creation probe hit the dead upstream: the route is quarantined.
    assert_eq!(
        dead_probes
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .authorization,
        "Bearer dead-key-1"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route).await,
        "unavailable"
    );

    // API-key-only edit against the same broken connection: the route
    // re-enters Checking and the system re-check fails, so it stays
    // unavailable — the correction itself did not set health.
    let still_broken = client
        .patch(format!("{base}/admin/providers/{provider}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Connection-edit provider",
            "base_url": dead_url,
            "api_key": "dead-key-2"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(still_broken.status(), StatusCode::OK);
    assert_eq!(
        dead_probes
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .authorization,
        "Bearer dead-key-2"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route).await,
        "unavailable"
    );

    // Route-level connection-class edit (upstream model name) against the
    // same broken connection: the route re-enters Checking and the system
    // re-check fails, so it stays unavailable — the correction itself did not
    // set health, and the re-check used the updated configuration.
    let route_rechecked = client
        .patch(format!("{base}/admin/model-routes/{route}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "config-edit-model-v2",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(route_rechecked.status(), StatusCode::OK);
    let route_recheck_probe = dead_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(route_recheck_probe.authorization, "Bearer dead-key-2");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&route_recheck_probe.body).unwrap()["model"],
        "config-edit-model-v2"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route).await,
        "unavailable"
    );

    // Repoint the connection at a working upstream: the system-owned re-check
    // recovers the route immediately instead of waiting for the recovery
    // schedule (default base interval B = 30s).
    let recovered = client
        .patch(format!("{base}/admin/providers/{provider}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Connection-edit provider",
            "base_url": working_url,
            "api_key": "recovered-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
    let recovery_probe = working_probes
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(recovery_probe.authorization, "Bearer recovered-key");
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route).await,
        "available"
    );

    // Display-name-only edit: no connection change, health and the quarantine
    // cycle are untouched, no probe fires.
    let renamed = client
        .patch(format!("{base}/admin/providers/{provider}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Renamed provider",
            "base_url": working_url,
            "api_key": "recovered-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(renamed.status(), StatusCode::OK);
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route).await,
        "available"
    );

    // Cost-multiplier-only edit on the route: no connection change, health
    // untouched, no probe fires.
    let repriced = client
        .patch(format!("{base}/admin/model-routes/{route}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "config-edit-model-v2",
            "protocol": "chat_completions",
            "cost_multiplier": "0.5"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(repriced.status(), StatusCode::OK);
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route).await,
        "available"
    );

    // Neither health-neutral edit triggered a re-probe on the working upstream.
    assert!(!extra_connection
        .recv_timeout(Duration::from_secs(2))
        .unwrap());

    dead_worker.join().unwrap();
    working_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn failed_eligibility_edit_keeps_the_existing_relay_configuration_active() {
    let environment = TestEnvironment::new("eligibility-edit-rollback");
    let bootstrap_credential = environment.initialize();
    let first_port = available_port();
    let mut first_server = environment.start(first_port);
    let client = Client::new();
    wait_ready(&client, first_port).await;
    let first_base = format!("http://127.0.0.1:{first_port}");
    let active_cookie = activate_administrator(&client, &first_base, &bootstrap_credential).await;

    let (upstream_base_url, probe_requests, upstream_worker) = chat_probe_upstream();
    let provider = client
        .post(format!("{first_base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Committed provider",
            "base_url": upstream_base_url,
            "api_key": "committed-upstream-key"
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
    let route = client
        .post(format!("{first_base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "committed-sol",
            "protocol": "chat_completions",
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
        .to_owned();
    probe_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();
    let key = client
        .post(format!("{first_base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Committed scope", "model_route_ids": [route] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let key_id = key["id"].as_str().unwrap().to_owned();
    let relay_secret = key["secret"].as_str().unwrap().to_owned();

    // Repoint the committed provider at a fresh live upstream before restart:
    // the restarted process discards persisted health, so its startup probe
    // must succeed for the committed route to stay callable (ROUTE-004/005).
    let (restart_upstream_base_url, restart_probe_requests, restart_upstream_worker) =
        scripted_chat_upstream_with_catalog_override(
            vec![complete_chat_response(); 2],
            "unrelated-model",
        );
    let repointed = client
        .patch(format!("{first_base}/admin/providers/{provider}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Committed provider",
            "base_url": restart_upstream_base_url,
            "api_key": "committed-upstream-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(repointed.status(), StatusCode::OK);
    let repoint_probe = restart_probe_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(repoint_probe.authorization, "Bearer committed-upstream-key");
    first_server.kill().unwrap();
    first_server.wait().unwrap();

    let second_port = available_port();
    let mut second_server = environment.start_with_commit_failure(second_port);
    wait_ready(&client, second_port).await;
    let second_base = format!("http://127.0.0.1:{second_port}");
    let restarted_login = client
        .post(format!("{second_base}/admin/login"))
        .json(&json!({ "password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    assert_eq!(restarted_login.status(), StatusCode::OK);
    let restarted_cookie = session_cookie(&restarted_login);

    // The startup probe is asynchronous and ready does not wait for it; the
    // route becomes Available only after the probe succeeds.
    let startup_probe = restart_probe_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(startup_probe.authorization, "Bearer committed-upstream-key");
    await_route_health(
        &client,
        &second_base,
        &restarted_cookie,
        &route,
        "available",
    )
    .await;
    restart_upstream_worker.join().unwrap();

    let rejected = client
        .patch(format!("{second_base}/admin/relay-access-keys/{key_id}"))
        .header(header::COOKIE, &restarted_cookie)
        .json(&json!({ "label": "Replacement scope", "model_route_ids": [route] }))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let keys = get_with_cookie(
        &client,
        format!("{second_base}/admin/relay-access-keys?search=Committed%20scope"),
        &restarted_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(keys["data"][0]["label"], "Committed scope");
    let models = client
        .get(format!("{second_base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "gpt-5.6-sol");
    assert_eq!(models["data"].as_array().unwrap().len(), 1);

    second_server.kill().unwrap();
    second_server.wait().unwrap();
}

#[tokio::test]
async fn published_models_can_be_created_deprecated_and_route_references_are_guarded() {
    let environment = TestEnvironment::new("published-model-crud");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // REL-007: create a published model; it joins the catalog immediately.
    let created = client
        .post(format!("{base}/admin/published-models"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "name": "custom-aggregated-model",
            "input_price_rmb": "1.5",
            "output_price_rmb": "9",
            "cached_input_price_rmb": "0.15"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let operations = operations_json(&client, &base, &active_cookie).await;
    let model = operations["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["name"] == "custom-aggregated-model")
        .expect("created model in catalog");
    assert_eq!(model["input_price_rmb"], "1.5");
    assert_eq!(model["deprecated"], false);

    // Deprecate it: routes may no longer reference it.
    let deprecated = client
        .post(format!(
            "{base}/admin/published-models/custom-aggregated-model/deprecate"
        ))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(deprecated.status(), StatusCode::OK);
    let operations = operations_json(&client, &base, &active_cookie).await;
    let model = operations["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["name"] == "custom-aggregated-model")
        .unwrap();
    assert_eq!(model["deprecated"], true);

    // The rejected route never probes, so the provider needs no scripted
    // upstream.
    let provider_id = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Deprecated-model provider",
            "base_url": "https://deprecated.invalid/v1",
            "api_key": "deprecated-model-key"
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
    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "custom-aggregated-model",
            "provider_id": provider_id,
            "upstream_model_name": "deprecated-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        route.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a deprecated published model must refuse new route references"
    );
    let body: serde_json::Value = route.json().await.unwrap();
    assert!(
        body["error"]["fields"]["published_model_id"]
            .as_str()
            .unwrap()
            .contains("deprecated")
    );
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_access_key_gets_a_gateway_error_when_its_eligible_route_is_unavailable() {
    let environment = TestEnvironment::new("relay-key-unavailable-route");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let upstream_failure =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned();
    let (upstream_base_url, upstream_requests, upstream_worker) =
        scripted_http_upstream(vec![upstream_failure]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Unavailable upstream",
            "base_url": upstream_base_url,
            "api_key": "upstream-secret"
        }))
        .send()
        .await
        .unwrap();
    let provider_id = provider.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-terra",
            "provider_id": provider_id,
            "upstream_model_name": "offline-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(route.status(), StatusCode::CREATED);
    let route = route.json::<serde_json::Value>().await.unwrap();
    assert_eq!(route["health"], "unavailable");
    let route_id = route["id"].as_str().unwrap().to_owned();
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    upstream_worker.join().unwrap();

    let relay_key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Offline client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_key}"))
        .json(&json!({
            "model": "gpt-5.6-terra",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        json!({
            "error": {
                "message": "no eligible model route is currently available",
                "type": "api_error",
                "param": null,
                "code": "no_available_route"
            }
        })
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_access_key_transparently_completes_a_native_responses_call() {
    let environment = TestEnvironment::new("native-responses");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            complete_responses_response().len(),
            complete_responses_response()
        ),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            complete_responses_response().len(),
            complete_responses_response()
        ),
    ]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Responses upstream",
            "base_url": upstream_base_url,
            "api_key": "responses-upstream-key"
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
    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "scripted-responses-model",
            "protocol": "responses",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(route["health"], "available");
    let route_id = route["id"].as_str().unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Responses client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let models = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "gpt-5.6-sol");
    assert_eq!(models["data"].as_array().unwrap().len(), 1);

    let response = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hello" }] }],
            "instructions": "be concise",
            "tools": [{ "type": "function", "name": "lookup" }],
            "reasoning": { "effort": "low" },
            "metadata": { "trace": "responses-test" },
            "client_extension": { "preserve": [1, 2, 3] }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["model"], "gpt-5.6-sol");
    assert_eq!(body["custom_response"], json!({ "kept": true }));
    // API-008: the successful Responses response keeps the upstream field
    // order with the published model name restored in place.
    assert_eq!(
        json_object_key_order(&body),
        vec![
            "id",
            "object",
            "created_at",
            "status",
            "model",
            "output",
            "error",
            "custom_response"
        ]
    );

    let captured = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    upstream_worker.join().unwrap();
    assert_eq!(captured.request_line, "POST /v1/responses HTTP/1.1");
    assert_eq!(captured.authorization, "Bearer responses-upstream-key");
    let forwarded = serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap();
    assert_eq!(forwarded["model"], "scripted-responses-model");
    assert_eq!(forwarded["stream"], false);
    assert_eq!(forwarded["instructions"], "be concise");
    assert_eq!(forwarded["tools"][0]["name"], "lookup");
    assert_eq!(forwarded["reasoning"]["effort"], "low");
    assert_eq!(forwarded["metadata"]["trace"], "responses-test");
    assert_eq!(forwarded["client_extension"]["preserve"], json!([1, 2, 3]));
    // API-006: the forwarded request keeps the client's field order; the
    // omitted `stream` default is appended after the client's last field.
    assert_eq!(
        json_object_key_order(&forwarded),
        vec![
            "model",
            "input",
            "instructions",
            "tools",
            "reasoning",
            "metadata",
            "client_extension",
            "stream"
        ]
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_normalizes_null_message_content_before_responses_forward() {
    let environment = TestEnvironment::new("responses-null-content");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        http_json_response(&complete_responses_response()),
    ]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Responses null-content upstream",
            "base_url": upstream_base_url,
            "api_key": "responses-null-content-key"
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
    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "scripted-responses-model",
            "protocol": "responses",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(route["health"], "available");
    let route_id = route["id"].as_str().unwrap();
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, route_id, "Responses null content").await;
    // The route creation probe consumed the first scripted response.
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let response = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "input": [
                { "role": "user", "content": "1+1=" },
                { "role": "assistant", "content": null, "reasoning_content": "让我想想" },
                { "role": "user", "content": "继续" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<serde_json::Value>().await.unwrap();

    let captured = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    upstream_worker.join().unwrap();
    let forwarded = serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap();
    assert_eq!(forwarded["input"][0]["content"], "1+1=");
    // The null content is normalized to an empty string and the reasoning
    // content survives, so interrupted thinking can be resumed upstream.
    assert_eq!(forwarded["input"][1]["content"], "");
    assert_eq!(forwarded["input"][1]["reasoning_content"], "让我想想");
    assert_eq!(forwarded["input"][2]["content"], "继续");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_normalizes_null_message_content_before_chat_completions_forward() {
    let environment = TestEnvironment::new("chat-null-content");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_json_response(&complete_chat_response()),
    ]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Chat null-content upstream",
            "base_url": upstream_base_url,
            "api_key": "chat-null-content-key"
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
    let route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "scripted-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(route["health"], "available");
    let route_id = route["id"].as_str().unwrap();
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, route_id, "Chat null content").await;
    // The route creation probe consumed the first scripted response.
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [
                { "role": "user", "content": "1+1=" },
                { "role": "assistant", "content": null, "reasoning_content": "让我想想" },
                { "role": "user", "content": "继续" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<serde_json::Value>().await.unwrap();

    let captured = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    upstream_worker.join().unwrap();
    let forwarded = serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap();
    assert_eq!(forwarded["messages"][0]["content"], "1+1=");
    // The null content is normalized to an empty string and the reasoning
    // content survives, so interrupted thinking can be resumed upstream.
    assert_eq!(forwarded["messages"][1]["content"], "");
    assert_eq!(forwarded["messages"][1]["reasoning_content"], "让我想想");
    assert_eq!(forwarded["messages"][2]["content"], "继续");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn responses_reject_invalid_request_bodies_before_selecting_an_upstream_route() {
    let environment = TestEnvironment::new("responses-validation");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let unavailable_upstream = format!("http://127.0.0.1:{}/v1", available_port());
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Unavailable Responses upstream",
            "base_url": unavailable_upstream,
            "api_key": "responses-validation-key"
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
    let route_id = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "unavailable-responses-model",
            "protocol": "responses",
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
        .to_owned();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Responses validation", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    for invalid_request in [
        json!({ "input": "hello" }),
        json!({ "model": "gpt-5.6-sol" }),
        json!({ "model": "gpt-5.6-sol", "input": null }),
        json!({ "model": "gpt-5.6-sol", "input": { "not": "accepted" } }),
        json!({ "model": "gpt-5.6-sol", "input": "hello", "stream": "not-a-boolean" }),
    ] {
        let response = client
            .post(format!("{base}/v1/responses"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&invalid_request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["error"]["type"],
            "invalid_request_error"
        );
    }

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn responses_semantic_failures_do_not_make_chat_routes_unavailable() {
    let environment = TestEnvironment::new("responses-semantic-failure");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let chat_response = complete_chat_response();
    let responses_response = complete_responses_response();
    let failed_response = failed_responses_response();
    let error_response = responses_error_response();
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&chat_response),
        http_json_response(&responses_response),
        http_json_response(&failed_response),
        http_json_response(&error_response),
        http_json_response(&chat_response),
    ]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Dual-protocol upstream",
            "base_url": upstream_base_url,
            "api_key": "dual-protocol-upstream-key"
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
    let chat_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "shared-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(chat_route["health"], "available");
    let chat_route_id = chat_route["id"].as_str().unwrap();
    let responses_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "shared-upstream-model",
            "protocol": "responses",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(responses_route["health"], "available");
    let responses_route_id = responses_route["id"].as_str().unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Dual-protocol client",
            "model_route_ids": [chat_route_id, responses_route_id]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let semantic_failure = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({ "model": "gpt-5.6-sol", "input": "trigger semantic failure" }))
        .send()
        .await
        .unwrap();
    assert_eq!(semantic_failure.status(), StatusCode::BAD_GATEWAY);
    let semantic_failure = semantic_failure.text().await.unwrap();
    assert!(semantic_failure.contains("upstream_semantic_failure"));
    assert!(!semantic_failure.contains("upstream-secret-must-not-leak"));

    let checked = client
        .post(format!(
            "{base}/admin/model-routes/{responses_route_id}/check"
        ))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(checked.status(), StatusCode::OK);
    assert_eq!(
        checked.json::<serde_json::Value>().await.unwrap()["health"],
        "unavailable"
    );

    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "chat remains available" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);

    let responses_unavailable = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({ "model": "gpt-5.6-sol", "input": "no responses route" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        responses_unavailable.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    upstream_worker.join().unwrap();

    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        operations["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["id"] == chat_route_id)
            .unwrap()["health"],
        "available"
    );
    assert_eq!(
        operations["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["id"] == responses_route_id)
            .unwrap()["health"],
        "unavailable"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_access_keys_reject_invalid_calls_and_stop_working_after_revocation() {
    let environment = TestEnvironment::new("relay-key-validation-and-revocation");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, probe_requests, upstream_worker) = chat_probe_upstream();
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Validation upstream",
            "base_url": upstream_base_url,
            "api_key": "upstream-secret"
        }))
        .send()
        .await
        .unwrap();
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
            "upstream_model_name": "scripted-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    let route_id = route.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    probe_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    let created_key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Validation client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let key_id = created_key["id"].as_str().unwrap().to_owned();
    let relay_secret = created_key["secret"].as_str().unwrap().to_owned();

    let missing_key = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_key.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        missing_key.headers().get(header::WWW_AUTHENTICATE).unwrap(),
        "Bearer"
    );
    assert_eq!(
        missing_key.json::<serde_json::Value>().await.unwrap(),
        json!({
            "error": {
                "message": "a valid relay access key is required",
                "type": "authentication_error",
                "param": null,
                "code": "invalid_api_key"
            }
        })
    );

    let malformed = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        malformed.json::<serde_json::Value>().await.unwrap()["error"],
        json!({
            "message": "request body must be valid JSON",
            "type": "invalid_request_error",
            "param": null,
            "code": null
        })
    );

    let oversized = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [],
            "client_extension": "x".repeat(OVER_LIMIT_BODY_CHARS)
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        oversized.json::<serde_json::Value>().await.unwrap()["error"],
        json!({
            "message": "request body is too large",
            "type": "invalid_request_error",
            "param": null,
            "code": null
        })
    );

    let invalid_stream = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [],
            "stream": "not-a-boolean"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_stream.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid_stream.json::<serde_json::Value>().await.unwrap()["error"]["param"],
        "stream"
    );

    let unknown_model = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "deepseek-v4-flash",
            "messages": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_model.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        unknown_model.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "model_not_found"
    );

    let relay_key_as_admin = client
        .get(format!("{base}/admin/operations"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap();
    assert_eq!(relay_key_as_admin.status(), StatusCode::UNAUTHORIZED);

    let revoked = client
        .post(format!("{base}/admin/relay-access-keys/{key_id}/revoke"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
    let revoked_key = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap();
    assert_eq!(revoked_key.status(), StatusCode::UNAUTHORIZED);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn invalid_upstream_chat_response_is_safely_normalized() {
    let environment = TestEnvironment::new("relay-key-invalid-upstream-response");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let probe_body = r#"{"id":"chatcmpl-probe","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
    let probe_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{probe_body}",
        probe_body.len()
    );
    let unsafe_body = "upstream-private-error-body";
    let unsafe_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{unsafe_body}",
        unsafe_body.len()
    );
    let (upstream_base_url, upstream_requests, upstream_worker) =
        scripted_http_upstream(vec![probe_response, unsafe_response]);
    let upstream_key = "upstream-secret-that-must-not-leak";
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Unsafe response upstream",
            "base_url": upstream_base_url,
            "api_key": upstream_key
        }))
        .send()
        .await
        .unwrap();
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
            "upstream_model_name": "scripted-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let route_id = route["id"].as_str().unwrap().to_owned();
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Unsafe upstream client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let response_text = response.text().await.unwrap();
    assert!(!response_text.contains(unsafe_body));
    assert!(!response_text.contains(upstream_key));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response_text).unwrap(),
        json!({
            "error": {
                "message": "upstream returned an invalid Chat Completions response",
                "type": "api_error",
                "param": null,
                "code": null
            }
        })
    );
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    upstream_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn failed_configuration_commit_keeps_the_previous_operations_state() {
    let environment = TestEnvironment::new("configuration-rollback");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start_with_commit_failure(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let rejected = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Rejected provider",
            "base_url": "https://api.example.invalid/v1",
            "api_key": "secret-that-must-not-be-persisted"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(operations["providers"], json!([]));
    assert_eq!(operations["routes"], json!([]));
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_preserves_chat_sse_events_in_order_through_the_process_boundary() {
    let environment = TestEnvironment::new("chat-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let stream = concat!(
        "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\r\n\r\n",
        "data: [DONE]\n\n"
    );
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response(stream),
    ]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Chat SSE upstream",
            "base_url": upstream_base_url,
            "api_key": "chat-sse-upstream-key"
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
    let route_id = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider,
            "upstream_model_name": "gpt-5.6-sol",
            "protocol": "chat_completions",
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
        .to_owned();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Chat SSE client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let relay_response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(relay_response.status(), StatusCode::OK);
    assert_eq!(
        relay_response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    assert_eq!(relay_response.text().await.unwrap(), stream);

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_presents_the_published_model_in_chat_sse_chunks() {
    let environment = TestEnvironment::new("chat-sse-model-mapping");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    // The chunk deliberately lists its fields in non-alphabetical order with
    // `model` mid-object, so the rewritten data payload can be order-asserted.
    let stream = concat!(
        "data: {\"choices\":[],\"model\":\"scripted-stream-model\",\"created\":1,\"object\":\"chat.completion.chunk\",\"id\":\"chatcmpl-mapped\"}\n\n",
        "data: [DONE]\n\n"
    );
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response(stream),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-stream-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Mapped Chat SSE client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(!body.contains("scripted-stream-model"));
    assert!(body.contains("\"model\":\"gpt-5.6-sol\""));
    assert!(body.ends_with("data: [DONE]\n\n"));
    // The rewritten data payload keeps the upstream field order with the model
    // name swapped in place (API-008).
    let data_line = body
        .lines()
        .find(|line| line.starts_with("data: {"))
        .expect("the relayed stream carries a JSON data payload");
    let payload: serde_json::Value =
        serde_json::from_str(data_line.strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(
        json_object_key_order(&payload),
        vec!["choices", "model", "created", "object", "id"]
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_rewrites_only_the_model_field_and_pins_semantic_losslessness() {
    // ROUTE-013 adjudication: "lossless" is semantic. When the upstream model
    // name differs from the published name, byte fidelity is impossible by
    // definition (API-008 requires presenting the published name at the client
    // boundary), so the rewrite path must preserve the object structure, every
    // known and unknown field, and the field order while changing only the
    // `model` value. The chat event below is deliberately spread over multiple
    // `data:` lines (folded per the SSE spec, where data lines are defined to
    // join with \n) and carries unknown fields in non-alphabetical order, a
    // unicode escape, u64::MAX and 2^53+1 integers, and a float with excess
    // precision, so each documented round-trip boundary is pinned explicitly.
    let environment = TestEnvironment::new("sse-rewrite-semantics");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let stream = concat!(
        "data: {\"id\":\"chatcmpl-boundary\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"scripted-boundary-model\",\"choices\":[],\"zz_extension\":18446744073709551615,\"aa_extension\":\n",
        "data: 9007199254740993,\"escaped\":\"\\u0041\\u00e9\",\"precise\":1.23456789012345678901234567890,\"text\":\"你好\"}\n\n",
        "data: [DONE]\n\n"
    );
    let responses_stream = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp-boundary\",\"object\":\"response\",\"model\":\"scripted-boundary-r\",\"status\":\"in_progress\",\"custom_field\":{\"kept\":true}}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp-boundary\",\"object\":\"response\",\"model\":\"scripted-boundary-r\",\"status\":\"completed\",\"custom_field\":{\"kept\":true}}}\n\n"
    );
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response(stream),
        http_json_response(&complete_responses_response()),
        sse_http_response(responses_stream),
    ]);
    let chat_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url.clone(),
        "chat_completions",
        "scripted-boundary-model",
    )
    .await;
    let chat_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &chat_route,
        "Semantic losslessness chat client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {chat_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(!body.contains("scripted-boundary-model"));
    // The multi-line data event folds into a single `data:` line (SSE data
    // lines are defined to join with \n, so the folded form decodes to the
    // same event) while the terminal stays byte-identical.
    let data_lines: Vec<&str> = body
        .lines()
        .filter(|line| line.starts_with("data:") && *line != "data: [DONE]")
        .collect();
    assert_eq!(
        data_lines.len(),
        1,
        "the multi-line data event must fold into one data line, got: {body:?}"
    );
    assert!(body.ends_with("data: [DONE]\n\n"));
    let payload: serde_json::Value =
        serde_json::from_str(data_lines[0].strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(payload["model"], "gpt-5.6-sol");
    // Unknown fields survive with exact values: u64::MAX and 2^53+1 integers
    // round-trip exactly, the unicode escape normalizes to its decoded value
    // (JSON-equivalent), and the excess-precision float is pinned at the
    // shortest f64 round trip — the documented accepted boundary, since LLM
    // API numeric fields (token counts, indices, timestamps, scores) are all
    // within serde_json's exact round-trip range.
    assert_eq!(payload["zz_extension"].as_u64(), Some(18446744073709551615));
    assert_eq!(payload["aa_extension"].as_i64(), Some(9007199254740993));
    assert_eq!(payload["escaped"], "Aé");
    assert_eq!(payload["text"], "你好");
    assert_eq!(payload["precise"].as_f64(), Some(1.2345678901234567));
    // The rewritten payload keeps the upstream field order (API-008).
    assert_eq!(
        json_object_key_order(&payload),
        vec![
            "id", "object", "created", "model", "choices", "zz_extension", "aa_extension",
            "escaped", "precise", "text"
        ]
    );

    // The Responses rewrite targets the nested `response.model` field; it must
    // rewrite that value while keeping the event names, structure, unknown
    // fields, and the typed terminal (API-012).
    let responses_route = configure_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "responses",
        "scripted-boundary-r",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let responses_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &responses_route,
        "Semantic losslessness responses client",
    )
    .await;
    let responses_body = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {responses_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "input": "stream",
            "stream": true
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!responses_body.contains("scripted-boundary-r"));
    assert!(responses_body.contains("event: response.created\n"));
    assert!(responses_body.contains("event: response.completed\n"));
    assert!(!responses_body.contains("[DONE]"));
    for data_line in responses_body
        .lines()
        .filter(|line| line.starts_with("data: {"))
    {
        let event: serde_json::Value =
            serde_json::from_str(data_line.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(event["response"]["model"], "gpt-5.6-sol");
        assert_eq!(event["response"]["custom_field"]["kept"], true);
        // The nested response object keeps its upstream field order with the
        // model swapped in place (API-008).
        assert_eq!(
            json_object_key_order(&event["response"]),
            vec!["id", "object", "model", "status", "custom_field"]
        );
    }

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_preserves_a_chat_sse_event_larger_than_64_kib() {
    let environment = TestEnvironment::new("large-chat-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let content = "x".repeat(70 * 1024);
    let stream = format!(
        "data: {{\"id\":\"chatcmpl-large\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n"
    );
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response(&stream),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Large Chat SSE client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), stream);

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_preserves_named_responses_sse_events_and_typed_termination() {
    let environment = TestEnvironment::new("responses-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let stream = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"resp-stream\",\"object\":\"response\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"hello\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp-stream\",\"object\":\"response\",\"model\":\"gpt-5.6-sol\",\"status\":\"completed\"}}\n\n"
    );
    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        sse_http_response(stream),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "responses",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Responses SSE client",
    )
    .await;

    let relay_response = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "input": "stream",
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(relay_response.status(), StatusCode::OK);
    assert_eq!(
        relay_response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let body = relay_response.text().await.unwrap();
    assert_eq!(body, stream);
    assert!(!body.contains("[DONE]"));
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let forwarded = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(forwarded.request_line, "POST /v1/responses HTTP/1.1");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&forwarded.body).unwrap()["stream"],
        true
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_uses_failed_and_incomplete_responses_events_as_native_terminators() {
    let environment = TestEnvironment::new("responses-sse-terminators");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let failed = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"sequence_number\":1}\n\n"
    );
    let incomplete = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "event: response.incomplete\n",
        "data: {\"type\":\"response.incomplete\",\"sequence_number\":1}\n\n"
    );
    let invalid_terminal = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.failed\",\"sequence_number\":1}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":2}\n\n"
    );
    // Each scenario gets its own route and key: a semantic-failure or invalid
    // terminal quarantines its route (ROUTE-008/014), so the scenarios must
    // not share a candidate. The probe receivers stay bound so the upstream
    // workers do not drop their listeners.
    let (failed_url, _failed_requests, failed_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        sse_http_response(failed),
    ]);
    let (incomplete_url, _incomplete_requests, incomplete_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        sse_http_response(incomplete),
    ]);
    let (invalid_url, _invalid_requests, invalid_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        sse_http_response(invalid_terminal),
    ]);
    let failed_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failed_url,
        "responses",
        "failed-terminal-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let incomplete_route = configure_route(
        &client,
        &base,
        &active_cookie,
        incomplete_url,
        "responses",
        "incomplete-terminal-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let invalid_route = configure_route(
        &client,
        &base,
        &active_cookie,
        invalid_url,
        "responses",
        "invalid-terminal-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let failed_secret =
        create_relay_secret(&client, &base, &active_cookie, &failed_route, "Failed terminal client")
            .await;
    let incomplete_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &incomplete_route,
        "Incomplete terminal client",
    )
    .await;
    let invalid_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &invalid_route,
        "Invalid terminal client",
    )
    .await;

    for (secret, expected) in [
        (&failed_secret, failed),
        (&incomplete_secret, incomplete),
        (
            &invalid_secret,
            "event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":0}\n\n",
        ),
    ] {
        let response = client
            .post(format!("{base}/v1/responses"))
            .header(header::AUTHORIZATION, format!("Bearer {secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "input": "stream",
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert_eq!(body, expected);
        assert!(!body.contains("[DONE]"));
    }

    failed_worker.join().unwrap();
    incomplete_worker.join().unwrap();
    invalid_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_returns_a_safe_error_when_the_first_sse_event_is_invalid() {
    let environment = TestEnvironment::new("invalid-first-sse-event");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response("data: upstream-secret-is-not-a-chat-chunk\n\n"),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Invalid first SSE client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("invalid_upstream_response"));
    assert!(!body.contains("upstream-secret-is-not-a-chat-chunk"));
    assert!(!body.contains("text/event-stream"));
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_only_ends_a_stream_when_the_upstream_truncates_after_commit() {
    let environment = TestEnvironment::new("truncated-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;
    let first_event = "data: {\"id\":\"chatcmpl-truncated\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[]}\n\n";
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        truncated_sse_http_response(first_event),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Truncated SSE client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.text().await.unwrap(), first_event);
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_quarantines_a_stream_that_ends_before_its_terminal_event() {
    let environment = TestEnvironment::new("clean-eof-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;
    let first_event = "data: {\"id\":\"chatcmpl-eof\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[]}\n\n";
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response(first_event),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Clean EOF SSE client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), first_event);
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn post_commit_stream_idle_timeout_is_health_neutral() {
    let environment = TestEnvironment::new("idle-after-commit");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_relay_timeouts(&client, &base, &active_cookie, 60_000, 1_000, 60_000).await;
    let (upstream_base_url, upstream_requests, upstream_worker) =
        stall_after_first_event_sse_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Idle after commit",
    )
    .await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("chatcmpl-stalled"));
    // REL-001: the in-flight idle cutoff is health-neutral — the route already
    // produced a valid stream, so it must stay available.
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "available"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_times_out_when_an_sse_upstream_is_idle_before_its_first_event() {
    let environment = TestEnvironment::new("idle-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;
    set_relay_timeouts(&client, &base, &active_cookie, 60_000, 1_000, 60_000).await;
    let (upstream_base_url, upstream_requests, upstream_worker) = stalling_sse_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, &route_id, "Idle SSE client").await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let started = Instant::now();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(response.text().await.unwrap().contains("upstream_timeout"));
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_allows_a_long_stream_when_each_upstream_event_arrives_before_idle_timeout() {
    let environment = TestEnvironment::new("paced-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let (upstream_base_url, upstream_requests, upstream_worker) = paced_sse_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Paced SSE client",
    )
    .await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let started = Instant::now();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(18));
    for index in 0..6 {
        assert!(body.contains(&format!("\"content\":\"{index}\"")));
    }
    assert!(body.ends_with("data: [DONE]\n\n"));
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn downstream_cancellation_closes_the_upstream_without_changing_route_health() {
    let environment = TestEnvironment::new("cancel-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let (upstream_base_url, upstream_requests, cancellation, upstream_worker) =
        cancellable_sse_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Cancellation SSE client",
    )
    .await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let request_body = serde_json::to_string(&json!({
        "model": "gpt-5.6-sol",
        "messages": [{ "role": "user", "content": "stream" }],
        "stream": true
    }))
    .unwrap();
    let mut downstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    downstream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        downstream,
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {relay_secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{request_body}",
        request_body.len()
    )
    .unwrap();
    downstream.flush().unwrap();
    let mut reader = BufReader::new(downstream.try_clone().unwrap());
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert!(status_line.starts_with("HTTP/1.1 200"));
    loop {
        let mut header_line = String::new();
        reader.read_line(&mut header_line).unwrap();
        if header_line == "\r\n" {
            break;
        }
    }
    let mut first_chunk = [0_u8; 512];
    let read = reader.read(&mut first_chunk).unwrap();
    assert!(
        std::str::from_utf8(&first_chunk[..read])
            .unwrap()
            .contains("chatcmpl-cancel")
    );
    drop(reader);
    drop(downstream);

    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let (upstream_cancelled, started_another_attempt) =
        cancellation.recv_timeout(Duration::from_secs(4)).unwrap();
    assert!(upstream_cancelled);
    assert!(!started_another_attempt);
    upstream_worker.join().unwrap();

    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        operations["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["id"] == route_id)
            .unwrap()["health"],
        "available"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn downstream_responses_cancellation_closes_the_upstream_without_changing_route_health() {
    let environment = TestEnvironment::new("cancel-responses-sse");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let (upstream_base_url, upstream_requests, cancellation, upstream_worker) =
        cancellable_responses_sse_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "responses",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Responses cancellation client",
    )
    .await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let request_body = serde_json::to_string(&json!({
        "model": "gpt-5.6-sol",
        "input": "stream",
        "stream": true
    }))
    .unwrap();
    let mut downstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    downstream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        downstream,
        "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {relay_secret}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{request_body}",
        request_body.len()
    )
    .unwrap();
    downstream.flush().unwrap();
    let mut reader = BufReader::new(downstream.try_clone().unwrap());
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert!(status_line.starts_with("HTTP/1.1 200"));
    loop {
        let mut header_line = String::new();
        reader.read_line(&mut header_line).unwrap();
        if header_line == "\r\n" {
            break;
        }
    }
    let mut first_chunk = [0_u8; 512];
    let read = reader.read(&mut first_chunk).unwrap();
    assert!(
        std::str::from_utf8(&first_chunk[..read])
            .unwrap()
            .contains("response.created")
    );
    drop(reader);
    drop(downstream);

    // The relayed streamed request must be Responses-native end to end: the
    // same path and a Responses body (`input`, not Chat `messages`) with the
    // stream flag intact, proving no cross-protocol conversion (API-018).
    let forwarded = upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(forwarded.request_line, "POST /v1/responses HTTP/1.1");
    let forwarded_body: serde_json::Value = serde_json::from_slice(&forwarded.body).unwrap();
    assert_eq!(forwarded_body["stream"], true);
    assert!(forwarded_body.get("input").is_some());
    assert!(forwarded_body.get("messages").is_none());

    let (upstream_cancelled, started_another_attempt) =
        cancellation.recv_timeout(Duration::from_secs(4)).unwrap();
    assert!(upstream_cancelled);
    assert!(!started_another_attempt);
    upstream_worker.join().unwrap();

    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "available"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

enum FailingUpstream {
    HttpStatus(u16),
    InvalidJson,
    TruncatedBody,
    ConnectionRefused,
}

#[tokio::test]
async fn attributable_upstream_failures_quarantine_the_route_and_fallback_to_the_next_candidate() {
    let environment = TestEnvironment::new("attributable-fallback");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let success = complete_chat_response();
    for (index, failure) in [
        FailingUpstream::HttpStatus(500),
        FailingUpstream::HttpStatus(429),
        FailingUpstream::HttpStatus(404),
        FailingUpstream::HttpStatus(403),
        FailingUpstream::HttpStatus(401),
        FailingUpstream::InvalidJson,
        FailingUpstream::TruncatedBody,
        FailingUpstream::ConnectionRefused,
    ]
    .iter()
    .enumerate()
    {
        let failing_upstream = match failure {
            FailingUpstream::HttpStatus(status) => {
                let reason = match status {
                    500 => "Internal Server Error",
                    429 => "Too Many Requests",
                    404 => "Not Found",
                    403 => "Forbidden",
                    401 => "Unauthorized",
                    _ => unreachable!(),
                };
                scripted_http_upstream(vec![
                    http_json_response(&success),
                    http_status_response(*status, reason, ""),
                ])
            }
            FailingUpstream::InvalidJson => scripted_http_upstream(vec![
                http_json_response(&success),
                http_json_response("this is not a valid chat completion"),
            ]),
            FailingUpstream::TruncatedBody => {
                let body = complete_chat_response();
                let truncated = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len() + 64,
                    &body[..body.len() / 2]
                );
                scripted_http_upstream(vec![http_json_response(&success), truncated])
            }
            FailingUpstream::ConnectionRefused => {
                scripted_chat_upstream(vec![complete_chat_response()])
            }
        };
        let (failing_url, failing_requests, failing_worker) = failing_upstream;
        let mut failing_worker = Some(failing_worker);
        let (succeeding_url, succeeding_requests, succeeding_worker) =
            scripted_chat_upstream(vec![complete_chat_response(), complete_chat_response()]);

        let failing_route = configure_route(
            &client,
            &base,
            &active_cookie,
            failing_url,
            "chat_completions",
            &format!("failing-model-{index}"),
            "gpt-5.6-sol",
            "1",
        )
        .await;
        let succeeding_route = configure_route(
            &client,
            &base,
            &active_cookie,
            succeeding_url,
            "chat_completions",
            &format!("succeeding-model-{index}"),
            "gpt-5.6-sol",
            "2",
        )
        .await;
        failing_requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        succeeding_requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        if matches!(failure, FailingUpstream::ConnectionRefused) {
            failing_worker.take().unwrap().join().unwrap();
        }
        let relay_secret = client
            .post(format!("{base}/admin/relay-access-keys"))
            .header(header::COOKIE, &active_cookie)
            .json(&json!({
                "label": format!("fallback client {index}"),
                "model_route_ids": [failing_route, succeeding_route]
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["secret"]
            .as_str()
            .unwrap()
            .to_owned();

        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "fallback" }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["id"], "chatcmpl-scripted");

        if !matches!(failure, FailingUpstream::ConnectionRefused) {
            let attempted = failing_requests
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&attempted.body).unwrap()["model"],
                format!("failing-model-{index}")
            );
        }
        let forwarded = succeeding_requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&forwarded.body).unwrap()["model"],
            format!("succeeding-model-{index}")
        );

        assert_eq!(
            route_health(&client, &base, &active_cookie, &failing_route).await,
            "unavailable"
        );
        assert_eq!(
            route_health(&client, &base, &active_cookie, &succeeding_route).await,
            "available"
        );
        // OPS-013: the Operations route row surfaces the safe HTTP status of
        // the most recent probe or attributable failure. An upstream HTTP
        // failure carries its status; an invalid-body 200 response carries
        // 200; a truncated body fails at the transport read (no safe HTTP
        // status) and a connection refusal likewise shows unknown (null),
        // matching the attempt record's own safe HTTP status.
        let expected_status = match failure {
            FailingUpstream::HttpStatus(status) => Some(*status as i64),
            FailingUpstream::InvalidJson => Some(200),
            FailingUpstream::TruncatedBody | FailingUpstream::ConnectionRefused => None,
        };
        assert_eq!(
            route_last_http_status(&client, &base, &active_cookie, &failing_route).await,
            expected_status
        );
        assert_eq!(
            route_last_http_status(&client, &base, &active_cookie, &succeeding_route).await,
            Some(200),
            "a successful creation probe records its 200 status"
        );
        if let Some(worker) = failing_worker.take() {
            worker.join().unwrap();
        }
        succeeding_worker.join().unwrap();
    }

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn operations_route_rows_surface_the_last_safe_http_status() {
    let environment = TestEnvironment::new("ops-safe-http-status");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A healthy probe records its 200; a 500 probe records 500; a transport
    // failure (dead endpoint) leaves the status unknown (null), never zero.
    let (healthy_url, _healthy_requests, healthy_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
    ]);
    let (failing_url, _failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let (dead_url, _dead_requests, dead_worker) = scripted_chat_upstream(vec![]);
    let healthy_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        healthy_url,
        "chat_completions",
        "healthy-status-model",
    )
    .await;
    let failing_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "failing-status-model",
    )
    .await;
    let dead_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        dead_url,
        "chat_completions",
        "dead-status-model",
    )
    .await;

    assert_eq!(
        route_health(&client, &base, &active_cookie, &healthy_route).await,
        "available"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &failing_route).await,
        "unavailable"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &dead_route).await,
        "unavailable"
    );
    assert_eq!(
        route_last_http_status(&client, &base, &active_cookie, &healthy_route).await,
        Some(200)
    );
    assert_eq!(
        route_last_http_status(&client, &base, &active_cookie, &failing_route).await,
        Some(500)
    );
    assert_eq!(
        route_last_http_status(&client, &base, &active_cookie, &dead_route).await,
        None,
        "a transport failure records no safe HTTP status (unknown, not zero)"
    );

    healthy_worker.join().unwrap();
    failing_worker.join().unwrap();
    dead_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn operations_route_rows_group_by_published_model_in_the_operations_table() {
    let environment = TestEnvironment::new("route-groups");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // Three routes across two published models, created in an order that does
    // not match the grouped order, so the assertion proves the snapshot groups
    // by published model (group order by model name) with a stable route-id
    // tie-breaker inside each group (UI-002).
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

    let operations =
        get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie).await;
    let routes = operations.json::<serde_json::Value>().await.unwrap()["routes"]
        .as_array()
        .unwrap()
        .clone();
    let names: Vec<&str> = routes
        .iter()
        .map(|route| route["published_model_name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["deepseek-v4-flash", "gpt-5.6-sol", "gpt-5.6-sol"],
        "routes group contiguously by published model, groups ordered by model name"
    );
    // UI-002 grouping data: the snapshot is the stable (published model,
    // route id) sort — group order by model name, deterministic route-id
    // tie-breaker inside each group.
    let mut sorted = routes.clone();
    sorted.sort_by(|a, b| {
        a["published_model_name"]
            .as_str()
            .cmp(&b["published_model_name"].as_str())
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    assert_eq!(
        routes, sorted,
        "routes are sorted by (published model, route id)"
    );

    // UI-002: the Operations snapshot orders routes by (published model,
    // route id) — group order by model name, deterministic route-id
    // tie-breaker inside each group. The user-visible grouping itself is
    // asserted by the real-browser test `browser_operations_groups_routes_by_published_model`.

    first_upstream.2.join().unwrap();
    second_upstream.2.join().unwrap();
    third_upstream.2.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn onboarding_checklist_covers_six_steps_and_tracks_callable_completion() {
    let environment = TestEnvironment::new("onboarding-six-step-checklist");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // UI-004: the checklist's user-visible rendering (six steps wired to real
    // controls, Done markers, the hide-on-callable gate) is asserted by the
    // real-browser test `browser_onboarding_checklist_tracks_six_steps_and_hides_when_callable`.

    // The checklist reads three snapshot contracts: route health (available),
    // the cost multiplier string, and per-key eligibility. Walk the API states
    // the checklist consumes and pin those fields at the process boundary so a
    // wire-format rename cannot silently break the completion flow.
    let empty_operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(empty_operations["providers"], json!([]));
    assert_eq!(empty_operations["routes"], json!([]));
    let empty_keys = get_with_cookie(&client, format!("{base}/admin/relay-access-keys"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(empty_keys["data"], json!([]));

    let (upstream_base_url, _probe_requests, upstream_worker) =
        scripted_http_upstream(vec![http_json_response(&complete_chat_response())]);
    let route_id = configure_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "checklist-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "available"
    );

    let key = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Checklist client", "model_route_ids": [route_id] }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(key["model_route_ids"], json!([route_id.as_str()]));

    let operations = operations_json(&client, &base, &active_cookie).await;
    let route = operations["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == route_id)
        .unwrap();
    assert_eq!(route["health"], "available");
    assert_eq!(route["cost_multiplier"], "1");

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn form_validation_errors_are_attributed_to_their_fields() {
    let environment = TestEnvironment::new("field-level-validation");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // UI-006 / CFG-012: validation failures carry `error.fields` keyed by the
    // wire field name so the management forms can show an actionable message
    // next to the offending input; the top-level message stays self-contained.
    async fn assert_field_error(
        client: &Client,
        base: &str,
        cookie: &str,
        request: serde_json::Value,
        field: &str,
        expected_message: &str,
    ) {
        let response = client
            .post(format!("{base}/admin/providers"))
            .header(header::COOKIE, cookie)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(
            body["error"]["fields"][field].as_str().unwrap(),
            expected_message,
            "field {field} must carry its actionable message"
        );
        assert_eq!(body["error"]["message"], expected_message);
    }

    // Provider form: each invalid field is attributed to its own input.
    let valid_provider = json!({ "display_name": "Valid provider", "base_url": "https://api.example.com/v1", "api_key": "valid-provider-key" });
    assert_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "display_name": "", "base_url": "https://api.example.com/v1", "api_key": "valid-provider-key" }),
        "display_name",
        "provider name must be between 1 and 128 characters",
    )
    .await;
    assert_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "display_name": "Bad URL", "base_url": "not-a-url", "api_key": "valid-provider-key" }),
        "base_url",
        "base URL must be a valid HTTP URL",
    )
    .await;
    assert_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "display_name": "Empty key", "base_url": "https://api.example.com/v1", "api_key": "" }),
        "api_key",
        "upstream API key must be between 1 and 8192 characters",
    )
    .await;

    let provider_id = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&valid_provider)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Model route form: mapping, protocol and multiplier failures land on the
    // specific field, and a bad provider reference lands on the provider field.
    async fn assert_route_field_error(
        client: &Client,
        base: &str,
        cookie: &str,
        request: serde_json::Value,
        field: &str,
        expected_message: &str,
    ) {
        let response = client
            .post(format!("{base}/admin/model-routes"))
            .header(header::COOKIE, cookie)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["error"]["fields"][field].as_str().unwrap(), expected_message);
    }

    let valid_route = json!({ "published_model_id": "gpt-5.6-sol", "provider_id": provider_id, "upstream_model_name": "upstream-model", "protocol": "chat_completions", "cost_multiplier": "1" });
    assert_route_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "published_model_id": "gpt-5.6-sol", "provider_id": provider_id, "upstream_model_name": "", "protocol": "chat_completions", "cost_multiplier": "1" }),
        "upstream_model_name",
        "upstream model name must be between 1 and 256 characters",
    )
    .await;
    assert_route_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "published_model_id": "gpt-5.6-sol", "provider_id": provider_id, "upstream_model_name": "upstream-model", "protocol": "not-a-protocol", "cost_multiplier": "1" }),
        "protocol",
        "protocol must be chat_completions or responses",
    )
    .await;
    // The ticket's own example: a non-positive multiplier message next to the
    // multiplier input, never a callable configuration (CFG-011).
    assert_route_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "published_model_id": "gpt-5.6-sol", "provider_id": provider_id, "upstream_model_name": "upstream-model", "protocol": "chat_completions", "cost_multiplier": "0" }),
        "cost_multiplier",
        "cost multiplier must be greater than zero",
    )
    .await;
    assert_route_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "published_model_id": "gpt-5.6-sol", "provider_id": "missing-provider", "upstream_model_name": "upstream-model", "protocol": "chat_completions", "cost_multiplier": "1" }),
        "provider_id",
        "selected upstream provider does not exist",
    )
    .await;

    // Relay access key form: label and eligibility failures are field-scoped;
    // a key without at least one valid eligible route is never created.
    async fn assert_key_field_error(
        client: &Client,
        base: &str,
        cookie: &str,
        request: serde_json::Value,
        field: &str,
        expected_message: &str,
    ) {
        let response = client
            .post(format!("{base}/admin/relay-access-keys"))
            .header(header::COOKIE, cookie)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["error"]["fields"][field].as_str().unwrap(), expected_message);
    }

    assert_key_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "label": "", "model_route_ids": [] }),
        "label",
        "relay access key label must be between 1 and 128 characters",
    )
    .await;
    assert_key_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "label": "Client key", "model_route_ids": [] }),
        "model_route_ids",
        "at least one eligible model route is required",
    )
    .await;
    assert_key_field_error(
        &client,
        &base,
        &active_cookie,
        json!({ "label": "Client key", "model_route_ids": ["missing-route"] }),
        "model_route_ids",
        "selected eligible model route does not exist",
    )
    .await;

    // Non-field failures (for example a missing resource id) stay in the
    // general error area without a fields map.
    let missing_route = client
        .patch(format!("{base}/admin/model-routes/missing-route"))
        .header(header::COOKIE, &active_cookie)
        .json(&valid_route)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_route.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let missing_body = missing_route.json::<serde_json::Value>().await.unwrap();
    assert_eq!(missing_body["error"]["message"], "model route does not exist");
    assert!(
        missing_body["error"].get("fields").is_none(),
        "non-field failures must not fabricate a fields map"
    );

    // The user-visible rendering of these attributed errors (rendered beside
    // the offending input, no duplicate in the general error area) is asserted
    // by the real-browser test `browser_validation_errors_render_next_to_fields`.

    server.kill().unwrap();
    server.wait().unwrap();
}

/// CFG-008: a model route's identity is unique over (published model, upstream
/// provider, upstream model name, protocol). A duplicate create and an edit
/// that would collide are both rejected with an actionable message, and
/// neither path leaves a second route behind; the protocol dimension keeps
/// otherwise-identical routes distinct (CFG-007).
#[tokio::test]
async fn duplicate_model_route_identity_is_rejected_with_an_actionable_message() {
    let environment = TestEnvironment::new("duplicate-route-identity");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, _probe_requests, upstream_worker) = chat_probe_upstream();
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Identity upstream",
            "base_url": upstream_base_url,
            "api_key": "identity-upstream-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    let provider_id = provider.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    async fn create_route(
        client: &Client,
        base: &str,
        cookie: &str,
        provider_id: &str,
        protocol: &str,
    ) -> reqwest::Response {
        client
            .post(format!("{base}/admin/model-routes"))
            .header(header::COOKIE, cookie)
            .json(&json!({
                "published_model_id": "gpt-5.6-sol",
                "provider_id": provider_id,
                "upstream_model_name": "shared-upstream-model",
                "protocol": protocol,
                "cost_multiplier": "1"
            }))
            .send()
            .await
            .unwrap()
    }

    let chat_route = create_route(&client, &base, &active_cookie, &provider_id, "chat_completions")
        .await;
    assert_eq!(chat_route.status(), StatusCode::CREATED);
    assert_eq!(
        chat_route.json::<serde_json::Value>().await.unwrap()["health"],
        "available"
    );

    // The exact same identity again: rejected before any probe, with the same
    // actionable message the edit path already produced.
    let duplicate = create_route(&client, &base, &active_cookie, &provider_id, "chat_completions")
        .await;
    assert_eq!(duplicate.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let duplicate_body = duplicate.json::<serde_json::Value>().await.unwrap();
    assert!(
        duplicate_body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists"),
        "duplicate create must carry the actionable identity message, got: {}",
        duplicate_body["error"]["message"]
    );

    // The protocol dimension keeps an otherwise-identical route distinct
    // (CFG-007: one route per protocol), so the responses route is created.
    let responses_route = create_route(&client, &base, &active_cookie, &provider_id, "responses")
        .await;
    assert_eq!(responses_route.status(), StatusCode::CREATED);
    let responses_route_id = responses_route.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Editing the responses route onto the chat route's identity is rejected,
    // and the edit leaves the original route untouched.
    let colliding_edit = client
        .patch(format!("{base}/admin/model-routes/{responses_route_id}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "shared-upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(colliding_edit.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        colliding_edit.json::<serde_json::Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );

    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let routes = operations["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 2, "no rejected duplicate may leave a route behind");
    assert_eq!(
        routes
            .iter()
            .find(|route| route["id"] == responses_route_id)
            .unwrap()["protocol"],
        "responses",
        "the rejected edit must leave the original route unchanged"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

/// CFG-009: route eligibility is a unique set — a key cannot grant the same
/// model route twice, cannot reference a route that does not exist, and cannot
/// be created without at least one eligible route; the key that passes these
/// checks is the only one that can call (CFG-011).
#[tokio::test]
async fn duplicate_route_eligibility_is_rejected_and_keys_require_valid_eligibility() {
    let environment = TestEnvironment::new("duplicate-eligibility-matrix");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, _probe_requests, upstream_worker) = chat_probe_upstream();
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Eligibility upstream",
            "base_url": upstream_base_url,
            "api_key": "eligibility-upstream-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    let provider_id = provider.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    async fn create_route(
        client: &Client,
        base: &str,
        cookie: &str,
        provider_id: &str,
        protocol: &str,
    ) -> String {
        client
            .post(format!("{base}/admin/model-routes"))
            .header(header::COOKIE, cookie)
            .json(&json!({
                "published_model_id": "gpt-5.6-sol",
                "provider_id": provider_id,
                "upstream_model_name": "eligibility-upstream-model",
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

    let chat_route_id = create_route(&client, &base, &active_cookie, &provider_id, "chat_completions")
        .await;
    let responses_route_id = create_route(&client, &base, &active_cookie, &provider_id, "responses")
        .await;

    async fn create_key(
        client: &Client,
        base: &str,
        cookie: &str,
        route_ids: Vec<&str>,
    ) -> reqwest::Response {
        client
            .post(format!("{base}/admin/relay-access-keys"))
            .header(header::COOKIE, cookie)
            .json(&json!({ "label": "Eligibility client", "model_route_ids": route_ids }))
            .send()
            .await
            .unwrap()
    }

    // The same route granted twice is a duplicate set, not a bigger grant.
    let duplicated = create_key(&client, &base, &active_cookie, vec![&chat_route_id, &chat_route_id])
        .await;
    assert_eq!(duplicated.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        duplicated.json::<serde_json::Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must not contain duplicates")
    );

    // A reference to a route that does not exist is rejected, not silently
    // dropped.
    let missing = create_key(&client, &base, &active_cookie, vec!["missing-route"]).await;
    assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        missing.json::<serde_json::Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not exist")
    );

    // A key with no eligibility at all cannot be created (CFG-012), so an
    // invalid eligibility set never becomes callable.
    let none = create_key(&client, &base, &active_cookie, vec![]).await;
    assert_eq!(none.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        none.json::<serde_json::Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at least one eligible model route")
    );

    // The distinct two-route grant is the only key that exists and can call.
    let created = create_key(
        &client,
        &base,
        &active_cookie,
        vec![&chat_route_id, &responses_route_id],
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = created.json::<serde_json::Value>().await.unwrap();
    let relay_secret = created_body["secret"].as_str().unwrap().to_owned();
    let keys = get_with_cookie(&client, format!("{base}/admin/relay-access-keys"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let key_rows = keys["data"].as_array().unwrap();
    assert_eq!(key_rows.len(), 1, "rejected keys must never be created");
    assert_eq!(key_rows[0]["model_route_ids"].as_array().unwrap().len(), 2);

    let models = client
        .get(format!("{base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), StatusCode::OK);
    assert_eq!(
        models.json::<serde_json::Value>().await.unwrap()["data"][0]["id"],
        "gpt-5.6-sol"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

/// CFG-006/CFG-007/CFG-011 acceptance matrix, table-driven over the model
/// route create payload: empty and blank upstream model names, a protocol
/// outside the two native protocols, and non-positive cost multipliers are
/// each rejected with a field-attributed, actionable message (UI-006); none of
/// the rejected rows creates a route, and a key without any eligible route
/// cannot be created — so the invalid configurations never become callable.
/// The same provider still serves a valid, callable route afterwards, proving
/// the rejections did not poison the configuration.
#[tokio::test]
async fn config_validation_matrix_rejects_invalid_routes_without_a_callable_route() {
    let environment = TestEnvironment::new("config-validation-matrix");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        complete_chat_response(),
    ]);
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Validation matrix upstream",
            "base_url": upstream_base_url,
            "api_key": "matrix-upstream-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    let provider_id = provider.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    async fn assert_route_rejected(
        client: &Client,
        base: &str,
        cookie: &str,
        request: serde_json::Value,
        field: &str,
        expected_message: &str,
    ) {
        let response = client
            .post(format!("{base}/admin/model-routes"))
            .header(header::COOKIE, cookie)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(
            body["error"]["fields"][field].as_str().unwrap(),
            expected_message,
            "field {field} must carry its actionable message"
        );
        assert_eq!(body["error"]["message"], expected_message);
    }

    // CFG-006 rows: a non-empty upstream model name (whitespace-only is not a
    // name) and a positive cost multiplier.
    assert_route_rejected(
        &client,
        &base,
        &active_cookie,
        json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }),
        "upstream_model_name",
        "upstream model name must be between 1 and 256 characters",
    )
    .await;
    assert_route_rejected(
        &client,
        &base,
        &active_cookie,
        json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "   ",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }),
        "upstream_model_name",
        "upstream model name must be between 1 and 256 characters",
    )
    .await;
    assert_route_rejected(
        &client,
        &base,
        &active_cookie,
        json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "0"
        }),
        "cost_multiplier",
        "cost multiplier must be greater than zero",
    )
    .await;
    assert_route_rejected(
        &client,
        &base,
        &active_cookie,
        json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "-0.5"
        }),
        "cost_multiplier",
        "cost multiplier must be greater than zero",
    )
    .await;
    // CFG-007 row: only the two native protocols are accepted.
    assert_route_rejected(
        &client,
        &base,
        &active_cookie,
        json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "upstream-model",
            "protocol": "not-a-protocol",
            "cost_multiplier": "1"
        }),
        "protocol",
        "protocol must be chat_completions or responses",
    )
    .await;

    // None of the rejected rows created a route (CFG-011).
    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        operations["routes"].as_array().unwrap().len(),
        0,
        "a rejected model route must never appear in the route table"
    );

    // A key without any eligible route cannot be created, so the invalid
    // configurations can never be called (CFG-009/CFG-012).
    let no_eligibility = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "label": "Matrix client", "model_route_ids": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(no_eligibility.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        no_eligibility.json::<serde_json::Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("eligible model route")
    );

    // The same provider still serves a valid, callable route: the matrix rows
    // above never left a half-created or callable configuration behind.
    let valid_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "upstream-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(valid_route.status(), StatusCode::CREATED);
    let valid_route = valid_route.json::<serde_json::Value>().await.unwrap();
    assert_eq!(valid_route["health"], "available");
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Matrix client",
            "model_route_ids": [valid_route["id"]]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let call = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "callable" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), StatusCode::OK);
    upstream_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

/// CFG-010/UI-007: model route health is system-owned. The wire schemas expose
/// no health field, and a client that smuggles one into a create or an edit
/// payload is ignored: the creation probe decides a new route's health, a
/// health-neutral edit changes nothing, and a connection edit hands the route
/// to the system-owned re-check instead of honoring the submitted value.
#[tokio::test]
async fn admin_cannot_directly_edit_system_owned_route_health() {
    let environment = TestEnvironment::new("system-owned-route-health");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // Probe 1 fails (route becomes unavailable) and probe 2 succeeds, so each
    // step below proves the probe — never the submitted `health` field —
    // decided the outcome.
    let (upstream_base_url, _probe_requests, upstream_worker) =
        failing_then_success_chat_upstream();
    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "System-owned health upstream",
            "base_url": upstream_base_url,
            "api_key": "system-owned-health-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    let provider_id = provider.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Create with "health": "available" smuggled in: the failing probe decides
    // the route is unavailable.
    let created = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "system-owned-model",
            "protocol": "chat_completions",
            "cost_multiplier": "1",
            "health": "available"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = created.json::<serde_json::Value>().await.unwrap();
    assert_eq!(created_body["health"], "unavailable");
    let route_id = created_body["id"].as_str().unwrap();

    // A health-neutral edit (multiplier only) carrying "health": "available"
    // changes nothing: no re-check fires and the route stays unavailable.
    let neutral_edit = client
        .patch(format!("{base}/admin/model-routes/{route_id}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "system-owned-model",
            "protocol": "chat_completions",
            "cost_multiplier": "2",
            "health": "available"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(neutral_edit.status(), StatusCode::OK);
    assert_eq!(
        route_health(&client, &base, &active_cookie, route_id).await,
        "unavailable",
        "a health-neutral edit must not change system-owned health"
    );

    // A connection edit carrying "health": "unavailable" triggers the
    // system-owned re-check; the succeeding probe decides the route is
    // available, ignoring the smuggled field (ROUTE-010).
    let connection_edit = client
        .patch(format!("{base}/admin/model-routes/{route_id}"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": provider_id,
            "upstream_model_name": "system-owned-model-renamed",
            "protocol": "chat_completions",
            "cost_multiplier": "2",
            "health": "unavailable"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(connection_edit.status(), StatusCode::OK);
    assert_eq!(
        route_health(&client, &base, &active_cookie, route_id).await,
        "available",
        "the re-check probe, not the submitted health field, decides the route"
    );

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn route_quarantines_only_after_the_configured_number_of_consecutive_failures() {
    let environment = TestEnvironment::new("quarantine-threshold");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // REL-004: the default threshold is 2, so one attributable failure keeps
    // the route available; the second consecutive failure quarantines it.
    let success = complete_chat_response();
    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_status_response(500, "Internal Server Error", ""),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let (healthy_url, healthy_requests, healthy_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_json_response(&success),
        http_json_response(&success),
        http_json_response(&success),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "threshold-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let healthy_route = configure_route(
        &client,
        &base,
        &active_cookie,
        healthy_url,
        "chat_completions",
        "healthy-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Threshold client",
            "model_route_ids": [failing_route.clone(), healthy_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A probe
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B probe

    // Call 1: the first failure only advances the counter; the route stays
    // available and the client succeeds through the fallback.
    chat_call(&client, &base, &relay_secret).await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A 500 #1
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B success #1
    assert_eq!(
        route_health(&client, &base, &active_cookie, &failing_route).await,
        "available"
    );

    // Call 2: the second consecutive failure reaches the default threshold
    // and quarantines the route; the fallback still succeeds.
    chat_call(&client, &base, &relay_secret).await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A 500 #2
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B success #2
    await_route_health(&client, &base, &active_cookie, &failing_route, "unavailable").await;

    // Call 3: the quarantined route is excluded and the healthy route serves
    // directly.
    chat_call(&client, &base, &relay_secret).await;
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B #3
    assert!(failing_requests.try_recv().is_err());

    failing_worker.join().unwrap();
    healthy_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_probe_result_cannot_restore_a_freshly_quarantined_route() {
    let environment = TestEnvironment::new("stale-probe-epoch");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let (failing_url, failing_requests, release_probe, failing_worker) =
        held_manual_check_then_failures_upstream();
    let (healthy_url, healthy_requests, healthy_worker) =
        scripted_chat_upstream(vec![complete_chat_response(), complete_chat_response()]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "stale-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let healthy_route = configure_route(
        &client,
        &base,
        &active_cookie,
        healthy_url,
        "chat_completions",
        "healthy-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Epoch client",
            "model_route_ids": [failing_route.clone(), healthy_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A probe
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B probe

    // A manual check starts a native probe with the route's current epoch (0)
    // and the upstream holds it open.
    let check_task = {
        let client = client.clone();
        let base = base.clone();
        let active_cookie = active_cookie.clone();
        let failing_route = failing_route.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/admin/model-routes/{failing_route}/check"))
                .header(header::COOKIE, &active_cookie)
                .send()
                .await
                .unwrap()
        })
    };
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // manual probe

    // While that probe is in flight a real call fails and quarantines the
    // route (threshold 1), bumping its quarantine epoch.
    chat_call(&client, &base, &relay_secret).await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A 500
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B success
    await_route_health(&client, &base, &active_cookie, &failing_route, "unavailable").await;

    // The stale probe then completes with success, but its epoch no longer
    // matches: the route must stay quarantined (REL-005).
    release_probe.send(()).unwrap();
    let checked = check_task.await.unwrap();
    assert_eq!(checked.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        route_health(&client, &base, &active_cookie, &failing_route).await,
        "unavailable",
        "the stale success must not restore a freshly quarantined route"
    );

    failing_worker.join().unwrap();
    healthy_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn health_neutral_failures_do_not_quarantine_the_route_or_start_a_fallback() {
    let environment = TestEnvironment::new("health-neutral-failures");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(400, "Bad Request", "{\"error\":{\"message\":\"bad\"}}"),
        http_json_response(&complete_chat_response()),
    ]);
    let route_id = configure_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "health-neutral-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Health neutral client",
    )
    .await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let client_error = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "allowlist-excluded 4xx" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(client_error.status(), StatusCode::BAD_REQUEST);
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let follow_up = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "still selected" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(follow_up.status(), StatusCode::OK);
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "available"
    );
    upstream_worker.join().unwrap();

    let (json_upstream_url, json_upstream_requests, json_upstream_worker) =
        scripted_chat_upstream(vec![complete_chat_response()]);
    let json_route = configure_route(
        &client,
        &base,
        &active_cookie,
        json_upstream_url,
        "chat_completions",
        "json-validation-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let json_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &json_route,
        "JSON validation client",
    )
    .await;
    json_upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let invalid = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {json_secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body("not-json")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert!(json_upstream_requests.try_recv().is_err());
    assert_eq!(
        route_health(&client, &base, &active_cookie, &json_route).await,
        "available"
    );
    json_upstream_worker.join().unwrap();

    let (limit_upstream_url, limit_upstream_requests, limit_upstream_worker) =
        scripted_chat_upstream(vec![complete_chat_response()]);
    let limit_route = configure_route(
        &client,
        &base,
        &active_cookie,
        limit_upstream_url,
        "chat_completions",
        "limit-validation-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let limit_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &limit_route,
        "Body limit client",
    )
    .await;
    limit_upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let oversized = format!(
        r#"{{"model":"gpt-5.6-sol","messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(OVER_LIMIT_BODY_CHARS)
    );
    let over_limit = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {limit_secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(oversized)
        .send()
        .await
        .unwrap();
    assert_eq!(over_limit.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(limit_upstream_requests.try_recv().is_err());
    assert_eq!(
        route_health(&client, &base, &active_cookie, &limit_route).await,
        "available"
    );
    limit_upstream_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn relay_accepts_request_bodies_larger_than_16_kib_and_rejects_oversized_ones() {
    let environment = TestEnvironment::new("large-request-bodies");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A >16 KiB multi-turn/tool-call style payload must pass end-to-end for
    // both native protocols, with the upstream receiving the full preserved
    // body (API-006); the documented 16 MiB request body limit still rejects
    // oversized bodies immediately without an upstream attempt or health
    // change (API-016).
    let (chat_url, chat_requests, chat_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_json_response(&complete_chat_response()),
    ]);
    let chat_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        chat_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let chat_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &chat_route,
        "Large chat client",
    )
    .await;
    chat_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let (responses_url, responses_requests, responses_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        http_json_response(&complete_responses_response()),
    ]);
    let responses_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        responses_url,
        "responses",
        "gpt-5.6-sol",
    )
    .await;
    let responses_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &responses_route,
        "Large responses client",
    )
    .await;
    responses_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    // Chat Completions with a 128 KiB user message succeeds and forwards intact.
    let chat_content = "x".repeat(128 * 1024);
    let chat = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {chat_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": chat_content }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    assert_eq!(
        chat.json::<serde_json::Value>().await.unwrap()["object"],
        "chat.completion"
    );
    let chat_captured = chat_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let chat_forwarded = serde_json::from_slice::<serde_json::Value>(&chat_captured.body).unwrap();
    assert_eq!(
        chat_forwarded["messages"][0]["content"]
            .as_str()
            .unwrap()
            .len(),
        128 * 1024
    );

    // Responses with a 128 KiB input text succeeds and forwards intact.
    let responses_text = "y".repeat(128 * 1024);
    let responses = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {responses_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": responses_text }] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(responses.status(), StatusCode::OK);
    assert_eq!(
        responses.json::<serde_json::Value>().await.unwrap()["object"],
        "response"
    );
    let responses_captured = responses_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let responses_forwarded =
        serde_json::from_slice::<serde_json::Value>(&responses_captured.body).unwrap();
    assert_eq!(
        responses_forwarded["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .len(),
        128 * 1024
    );

    // A body beyond the documented limit still fails immediately: 413, no
    // upstream attempt, route health untouched.
    let oversized = format!(
        r#"{{"model":"gpt-5.6-sol","messages":[{{"role":"user","content":"{}"}}]}}"#,
        "z".repeat(OVER_LIMIT_BODY_CHARS)
    );
    let over_limit = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {chat_secret}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(oversized)
        .send()
        .await
        .unwrap();
    assert_eq!(over_limit.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        over_limit.json::<serde_json::Value>().await.unwrap()["error"],
        json!({
            "message": "request body is too large",
            "type": "invalid_request_error",
            "param": null,
            "code": null
        })
    );
    assert!(chat_requests.try_recv().is_err());
    assert_eq!(
        route_health(&client, &base, &active_cookie, &chat_route).await,
        "available"
    );

    chat_worker.join().unwrap();
    responses_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn health_neutral_4xx_falls_back_to_the_next_candidate_and_stays_unblamed() {
    let environment = TestEnvironment::new("health-neutral-4xx-fallback");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // Route A (cheapest) mislabels an upstream failure as a health-neutral
    // 400; route B is healthy. The call must fall through to B and succeed
    // instead of ending on the first 4xx (ROUTE-009 amended), and A must
    // stay available (health-neutral, no quarantine). The scripted upstream
    // serves the create-time native probe first, then the real call.
    let success = complete_chat_response();
    let (first_url, first_requests, first_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_status_response(400, "Bad Request", "{\"error\":{\"message\":\"busy\"}}"),
    ]);
    let (second_url, second_requests, second_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_json_response(&success),
    ]);
    let first_route = configure_route(
        &client,
        &base,
        &active_cookie,
        first_url,
        "chat_completions",
        "hn-4xx-a",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let second_route = configure_route(
        &client,
        &base,
        &active_cookie,
        second_url,
        "chat_completions",
        "hn-4xx-b",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    for requests in [&first_requests, &second_requests] {
        requests.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Health neutral 4xx fallback client",
            "model_route_ids": [first_route, second_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "fall through 4xx" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        route_health(&client, &base, &active_cookie, &first_route).await,
        "available",
        "a health-neutral 4xx must not quarantine the route"
    );
    // The second candidate actually served the call.
    assert!(second_requests.try_recv().is_ok());
    first_worker.join().unwrap();
    second_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}
#[tokio::test]
async fn non_streaming_candidate_exhaustion_quarantines_all_routes_and_returns_the_final_error() {
    let environment = TestEnvironment::new("non-streaming-exhaustion");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let success = complete_chat_response();
    let (first_url, first_requests, first_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let (second_url, second_requests, second_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_status_response(429, "Too Many Requests", ""),
    ]);
    let (third_url, third_requests, third_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_json_response("not-a-completion"),
    ]);
    let first_route = configure_route(
        &client,
        &base,
        &active_cookie,
        first_url,
        "chat_completions",
        "exhaust-model-a",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let second_route = configure_route(
        &client,
        &base,
        &active_cookie,
        second_url,
        "chat_completions",
        "exhaust-model-b",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    let third_route = configure_route(
        &client,
        &base,
        &active_cookie,
        third_url,
        "chat_completions",
        "exhaust-model-c",
        "gpt-5.6-sol",
        "3",
    )
    .await;
    for requests in [&first_requests, &second_requests, &third_requests] {
        requests.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Exhaustion client",
            "model_route_ids": [first_route, second_route, third_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "exhaust" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"]["message"],
        "upstream returned an invalid Chat Completions response"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &first_route).await,
        "unavailable"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &second_route).await,
        "unavailable"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &third_route).await,
        "unavailable"
    );
    first_worker.join().unwrap();
    second_worker.join().unwrap();
    third_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn responses_semantic_failure_falls_back_to_the_next_responses_candidate() {
    let environment = TestEnvironment::new("responses-semantic-fallback");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        http_json_response(&failed_responses_response()),
    ]);
    let (succeeding_url, succeeding_requests, succeeding_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        http_json_response(&complete_responses_response()),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "responses",
        "failing-responses-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let succeeding_route = configure_route(
        &client,
        &base,
        &active_cookie,
        succeeding_url,
        "responses",
        "succeeding-responses-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    succeeding_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Responses fallback client",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = client
        .post(format!("{base}/v1/responses"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "input": "semantic failure then fallback"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["model"], "gpt-5.6-sol");
    let forwarded = succeeding_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&forwarded.body).unwrap()["model"],
        "succeeding-responses-model"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &failing_route).await,
        "unavailable"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &succeeding_route).await,
        "available"
    );
    failing_worker.join().unwrap();
    succeeding_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

enum StreamPrecommitFailure {
    InvalidFirstEvent,
    NonSseContentType,
    FirstEventIdleTimeout,
}

#[tokio::test]
async fn streaming_precommit_failures_fallback_to_the_next_candidate_without_leaking_bytes() {
    let environment = TestEnvironment::new("streaming-precommit-fallback");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let success_stream = "data: {\"id\":\"chatcmpl-stream\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[]}\n\ndata: [DONE]\n\n";
    for (index, failure) in [
        StreamPrecommitFailure::InvalidFirstEvent,
        StreamPrecommitFailure::NonSseContentType,
        StreamPrecommitFailure::FirstEventIdleTimeout,
    ]
    .iter()
    .enumerate()
    {
        let (failing_url, failing_requests, failing_worker) = match failure {
            StreamPrecommitFailure::InvalidFirstEvent => scripted_http_upstream(vec![
                http_json_response(&complete_chat_response()),
                sse_http_response("data: upstream-private-invalid-first-event\n\n"),
            ]),
            StreamPrecommitFailure::NonSseContentType => scripted_http_upstream(vec![
                http_json_response(&complete_chat_response()),
                http_json_response(&complete_chat_response()),
            ]),
            StreamPrecommitFailure::FirstEventIdleTimeout => stalling_sse_upstream(),
        };
        let (succeeding_url, succeeding_requests, succeeding_worker) =
            scripted_http_upstream(vec![
                http_json_response(&complete_chat_response()),
                sse_http_response(success_stream),
            ]);
        let failing_route = configure_route(
            &client,
            &base,
            &active_cookie,
            failing_url,
            "chat_completions",
            &format!("failing-stream-{index}"),
            "gpt-5.6-sol",
            "1",
        )
        .await;
        let succeeding_route = configure_route(
            &client,
            &base,
            &active_cookie,
            succeeding_url,
            "chat_completions",
            &format!("succeeding-stream-{index}"),
            "gpt-5.6-sol",
            "2",
        )
        .await;
        failing_requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        succeeding_requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let relay_secret = client
            .post(format!("{base}/admin/relay-access-keys"))
            .header(header::COOKIE, &active_cookie)
            .json(&json!({
                "label": format!("Streaming fallback client {index}"),
                "model_route_ids": [failing_route, succeeding_route]
            }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["secret"]
            .as_str()
            .unwrap()
            .to_owned();

        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "stream" }],
                "stream": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(response.text().await.unwrap(), success_stream);
        assert_eq!(
            route_health(&client, &base, &active_cookie, &failing_route).await,
            "unavailable"
        );
        assert_eq!(
            route_health(&client, &base, &active_cookie, &succeeding_route).await,
            "available"
        );
        failing_worker.join().unwrap();
        succeeding_worker.join().unwrap();
    }

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn concurrent_old_request_success_does_not_restore_a_quarantined_route() {
    let environment = TestEnvironment::new("concurrent-quarantine");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;
    let (upstream_base_url, upstream_requests, upstream_worker) =
        slow_success_then_failure_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Concurrent quarantine client",
    )
    .await;
    upstream_requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();

    let slow_base = base.clone();
    let slow_secret = relay_secret.clone();
    let slow_client = client.clone();
    let slow_call = tokio::spawn(async move {
        slow_client
            .post(format!("{slow_base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {slow_secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "slow success" }]
            }))
            .send()
            .await
            .unwrap()
    });
    let slow_probe = await_upstream_probe(&upstream_requests).await;
    assert!(!slow_probe.body.is_empty());

    let failing = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "triggers quarantine" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(failing.status(), StatusCode::INTERNAL_SERVER_ERROR);
    await_upstream_probe(&upstream_requests).await;

    let slow_response = slow_call.await.unwrap();
    assert_eq!(slow_response.status(), StatusCode::OK);
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );
    upstream_worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restart_rechecks_all_routes_concurrently_and_stays_ready() {
    let environment = TestEnvironment::new("startup-recheck");
    let bootstrap_credential = environment.initialize();
    let client = Client::new();

    // Phase 1: two routes become Available against live upstreams.
    let port = available_port();
    let mut server = environment.start(port);
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let (first_upstream, first_probes, release_first, first_worker) =
        holding_second_chat_upstream();
    let (second_upstream, second_probes, release_second, second_worker) =
        holding_second_chat_upstream();
    let first_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        first_upstream,
        "chat_completions",
        "first-model",
    )
    .await;
    let second_route = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        second_upstream,
        "chat_completions",
        "second-model",
    )
    .await;
    first_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    second_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Startup scope",
            "model_route_ids": [first_route.clone(), second_route.clone()]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    server.kill().unwrap();
    server.wait().unwrap();

    // Phase 2: restart. The startup probes reuse the same upstream endpoints,
    // which hold both connections open, proving the probes run concurrently
    // and that ready does not wait for them.
    let restart_port = available_port();
    let mut restarted_server = environment.start_with(
        restart_port,
        &[("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "50")],
    );
    wait_ready(&client, restart_port).await;
    let restarted_base = format!("http://127.0.0.1:{restart_port}");
    assert_eq!(
        client
            .get(format!("{restarted_base}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // Startup checks are light-validation-first (REL-002): each provider
    // receives one catalog GET, and because the catalog does not list the
    // configured models, the relay falls back to the held native probe.
    let first_catalog = first_probes.recv_timeout(Duration::from_secs(3)).unwrap();
    let second_catalog = second_probes.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(first_catalog.request_line, "GET /v1/models HTTP/1.1");
    assert_eq!(second_catalog.request_line, "GET /v1/models HTTP/1.1");
    let first_startup_probe = first_probes.recv_timeout(Duration::from_secs(3)).unwrap();
    let second_startup_probe = second_probes.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(
        first_startup_probe.request_line,
        "POST /v1/chat/completions HTTP/1.1"
    );
    assert_eq!(
        second_startup_probe.request_line,
        "POST /v1/chat/completions HTTP/1.1"
    );
    let first_body: serde_json::Value = serde_json::from_slice(&first_startup_probe.body).unwrap();
    assert_eq!(
        first_body,
        json!({
            "model": "first-model",
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false
        })
    );

    // While the startup probes are in flight every route is Checking, so the
    // model list is empty even though the service is ready.
    let models_while_checking = client
        .get(format!("{restarted_base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(models_while_checking["data"].as_array().unwrap().len(), 0);
    let restarted_login = client
        .post(format!("{restarted_base}/admin/login"))
        .json(&json!({ "password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    let restarted_cookie = session_cookie(&restarted_login);
    let operations_while_checking = get_with_cookie(
        &client,
        format!("{restarted_base}/admin/operations"),
        &restarted_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(operations_while_checking["model_routes"]["checking"], 2);
    assert_eq!(operations_while_checking["model_routes"]["available"], 0);

    // Release both probes; the routes become Available and rejoin candidates.
    release_first.send(()).unwrap();
    release_second.send(()).unwrap();
    first_worker.join().unwrap();
    second_worker.join().unwrap();
    await_route_health(
        &client,
        &restarted_base,
        &restarted_cookie,
        &first_route,
        "available",
    )
    .await;
    await_route_health(
        &client,
        &restarted_base,
        &restarted_cookie,
        &second_route,
        "available",
    )
    .await;
    let models = client
        .get(format!("{restarted_base}/v1/models"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "gpt-5.6-sol");

    restarted_server.kill().unwrap();
    restarted_server.wait().unwrap();
}

#[tokio::test]
async fn recovery_probes_follow_the_capped_doubling_schedule() {
    let environment = TestEnvironment::new("recovery-doubling-clock");
    let bootstrap_credential = environment.initialize();
    let clock_file = environment.root.join("recovery-clock");
    let clock_file_str = clock_file.to_str().unwrap();
    const CLOCK_START_MS: i64 = 1_000_000_000_000;
    write_recovery_clock(&clock_file, CLOCK_START_MS);
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "20"),
            ("LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE", clock_file_str),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_recovery_settings(&client, &base, &active_cookie, 250, 2).await;

    // Creation probe fails, then four recovery probes fail. The injected clock
    // pins every anchor: the intervals must be B, 2B, 4B, then the 4B cap
    // repeated (ROUTE-020 with N=2), and no probe may fire before the clock
    // crosses its due time.
    let failing = http_status_response(500, "Internal Server Error", "");
    let (upstream_base_url, probes, worker) = timing_http_upstream(vec![
        failing.clone(),
        failing.clone(),
        failing.clone(),
        failing.clone(),
        failing,
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "doubling-model",
    )
    .await;

    let native_probe = || {
        json!({
            "model": "doubling-model",
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false
        })
    };
    let creation = probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&creation.body).unwrap(),
        native_probe()
    );
    // The clock has not reached B, so no recovery probe may fire even though
    // real time exceeds the 250 ms base interval.
    assert!(
        probes.recv_timeout(Duration::from_millis(350)).is_err(),
        "a recovery probe fired before the injected clock reached the base interval"
    );
    let row = route_row(&client, &base, &active_cookie, &route_id).await;
    assert_eq!(row["failed_probe_count"], 0);
    assert_eq!(row["next_probe_at_ms"], json!(CLOCK_START_MS + 250));
    assert_eq!(row["current_interval_ms"], 250);

    // Advance the injected clock past each due time: the probe fires, its
    // failure is anchored to the current clock value, and the next due time is
    // exactly B * 2^min(k+1, N) later.
    let mut due = CLOCK_START_MS + 250;
    for (step, next_interval) in [500, 1000, 1000, 1000].into_iter().enumerate() {
        write_recovery_clock(&clock_file, due);
        let probe = next_native_probe(&probes);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&probe.body).unwrap(),
            native_probe()
        );
        let next_due = due + next_interval;
        await_route_field(
            &client,
            &base,
            &active_cookie,
            &route_id,
            "next_probe_at_ms",
            json!(next_due),
        )
        .await;
        let row = route_row(&client, &base, &active_cookie, &route_id).await;
        assert_eq!(row["failed_probe_count"], step as i64 + 1);
        due = next_due;
    }
    worker.join().unwrap();

    // Operations exposes the three-state counts and the recovery schedule.
    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(operations["model_routes"]["unavailable"], 1);
    let route = operations["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|route| route["id"] == route_id)
        .unwrap();
    assert_eq!(route["health"], "unavailable");
    assert_eq!(route["failure_category"], "native_check_failed");
    assert_eq!(route["failed_probe_count"], 4);
    assert!(route["state_age_seconds"].as_i64().unwrap() >= 0);
    assert_eq!(route["next_probe_at_ms"].as_i64().unwrap(), due);
    assert_eq!(route["current_interval_ms"].as_i64().unwrap(), 1000);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn recovery_with_zero_doubling_limit_keeps_a_constant_interval() {
    let environment = TestEnvironment::new("recovery-zero-doubling-clock");
    let bootstrap_credential = environment.initialize();
    let clock_file = environment.root.join("recovery-clock");
    let clock_file_str = clock_file.to_str().unwrap();
    const CLOCK_START_MS: i64 = 1_000_000_000_000;
    write_recovery_clock(&clock_file, CLOCK_START_MS);
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "20"),
            ("LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE", clock_file_str),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_recovery_settings(&client, &base, &active_cookie, 250, 0).await;

    // N=0 pins every interval to B regardless of the failed-probe index
    // (ROUTE-020): the due times are B, 2B, 3B in the injected clock.
    let failing = http_status_response(500, "Internal Server Error", "");
    let (upstream_base_url, probes, worker) =
        timing_http_upstream(vec![failing.clone(), failing.clone(), failing]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "zero-doubling-model",
    )
    .await;

    let creation = probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&creation.body).unwrap()["model"],
        "zero-doubling-model"
    );
    assert!(
        probes.recv_timeout(Duration::from_millis(350)).is_err(),
        "a recovery probe fired before the injected clock reached the base interval"
    );
    let row = route_row(&client, &base, &active_cookie, &route_id).await;
    assert_eq!(row["failed_probe_count"], 0);
    assert_eq!(row["next_probe_at_ms"], json!(CLOCK_START_MS + 250));

    let mut due = CLOCK_START_MS + 250;
    for step in 0..2 {
        write_recovery_clock(&clock_file, due);
        probes.recv_timeout(Duration::from_secs(2)).unwrap();
        let next_due = due + 250;
        await_route_field(
            &client,
            &base,
            &active_cookie,
            &route_id,
            "next_probe_at_ms",
            json!(next_due),
        )
        .await;
        let row = route_row(&client, &base, &active_cookie, &route_id).await;
        assert_eq!(row["failed_probe_count"], step as i64 + 1);
        assert_eq!(row["current_interval_ms"], 250);
        due = next_due;
    }
    worker.join().unwrap();

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn successful_recovery_probe_restores_a_route_and_a_new_failure_restarts_the_schedule() {
    let environment = TestEnvironment::new("recovery-restores-route-clock");
    let bootstrap_credential = environment.initialize();
    let clock_file = environment.root.join("recovery-clock");
    let clock_file_str = clock_file.to_str().unwrap();
    const CLOCK_START_MS: i64 = 1_000_000_000_000;
    write_recovery_clock(&clock_file, CLOCK_START_MS);
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "20"),
            ("LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE", clock_file_str),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_recovery_settings(&client, &base, &active_cookie, 250, 3).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    // Recovering route upstream: creation probe fails, recovery probe
    // succeeds, the relay call fails (quarantine), the next recovery probe
    // fails. The stable route serves the creation probe and the fallback call.
    let failing = http_status_response(500, "Internal Server Error", "");
    let (recovering_url, recovering_probes, recovering_worker) = timing_http_upstream(vec![
        failing.clone(),
        http_json_response(&complete_chat_response()),
        failing.clone(),
        failing,
    ]);
    let (stable_url, stable_probes, stable_worker) =
        scripted_chat_upstream(vec![complete_chat_response(), complete_chat_response()]);

    let recovering_route = configure_route(
        &client,
        &base,
        &active_cookie,
        recovering_url,
        "chat_completions",
        "recovering-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    recovering_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    let stable_route = configure_route(
        &client,
        &base,
        &active_cookie,
        stable_url,
        "chat_completions",
        "stable-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    stable_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    let row = route_row(&client, &base, &active_cookie, &recovering_route).await;
    assert_eq!(row["failed_probe_count"], 0);
    assert_eq!(row["next_probe_at_ms"], json!(CLOCK_START_MS + 250));

    // The recovery probe succeeds at the base-interval due time: the route
    // returns to Available and the schedule clears (ROUTE-021: success resets
    // the failed index and drops the pending probe).
    write_recovery_clock(&clock_file, CLOCK_START_MS + 250);
    recovering_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    await_route_health(
        &client,
        &base,
        &active_cookie,
        &recovering_route,
        "available",
    )
    .await;
    let row = route_row(&client, &base, &active_cookie, &recovering_route).await;
    assert_eq!(row["failed_probe_count"], 0);
    assert!(row["next_probe_at_ms"].is_null());

    // The relay prefers the cheaper recovering route; its failure quarantines
    // it and falls back to the stable route. The quarantine restarts the
    // schedule from B at the current injected clock value (T0+B), not from any
    // previous schedule (ROUTE-021).
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Fallback client",
            "model_route_ids": [recovering_route.clone(), stable_route.clone()]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let call = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "choose cheapest healthy route" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(call.status(), StatusCode::OK);
    assert_eq!(
        call.json::<serde_json::Value>().await.unwrap()["model"],
        "gpt-5.6-sol"
    );
    let relay_probe = recovering_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&relay_probe.body).unwrap()["model"],
        "recovering-model"
    );
    await_route_field(
        &client,
        &base,
        &active_cookie,
        &recovering_route,
        "next_probe_at_ms",
        json!(CLOCK_START_MS + 500),
    )
    .await;

    // Advancing to the new due time fires the next recovery probe, which fails
    // and schedules the doubled interval from the quarantine anchor.
    write_recovery_clock(&clock_file, CLOCK_START_MS + 500);
    recovering_probes.recv_timeout(Duration::from_secs(2)).unwrap();
    await_route_field(
        &client,
        &base,
        &active_cookie,
        &recovering_route,
        "next_probe_at_ms",
        json!(CLOCK_START_MS + 1000),
    )
    .await;
    let row = route_row(&client, &base, &active_cookie, &recovering_route).await;
    assert_eq!(row["failed_probe_count"], 1);
    recovering_worker.join().unwrap();
    stable_worker.join().unwrap();
    await_route_health(
        &client,
        &base,
        &active_cookie,
        &recovering_route,
        "unavailable",
    )
    .await;

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn default_recovery_settings_follow_the_capped_doubling_schedule() {
    let environment = TestEnvironment::new("recovery-default-clock");
    let bootstrap_credential = environment.initialize();
    let clock_file = environment.root.join("recovery-clock");
    let clock_file_str = clock_file.to_str().unwrap();
    const CLOCK_START_MS: i64 = 1_000_000_000_000;
    write_recovery_clock(&clock_file, CLOCK_START_MS);
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "20"),
            ("LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE", clock_file_str),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The defaults (B = 30 s, N = 5, ROUTE-019) are verified entirely in the
    // injected clock: six failed recovery probes produce intervals B, 2B, 4B,
    // 8B, 16B, 32B, then the 32B cap repeats (ROUTE-020).
    let failing = http_status_response(500, "Internal Server Error", "");
    let (upstream_base_url, probes, worker) = timing_http_upstream(vec![
        failing.clone(),
        failing.clone(),
        failing.clone(),
        failing.clone(),
        failing.clone(),
        failing.clone(),
        failing,
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "default-schedule-model",
    )
    .await;

    let creation = probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&creation.body).unwrap()["model"],
        "default-schedule-model"
    );
    // With B = 30 s in the injected clock, no probe may fire during 2 s of real
    // time while the clock stays at its start.
    assert!(
        probes.recv_timeout(Duration::from_secs(2)).is_err(),
        "a recovery probe fired before the injected clock reached the default base interval"
    );
    let row = route_row(&client, &base, &active_cookie, &route_id).await;
    assert_eq!(row["failed_probe_count"], 0);
    assert_eq!(row["next_probe_at_ms"], json!(CLOCK_START_MS + 30_000));
    assert_eq!(row["current_interval_ms"], 30_000);

    // The default doubling sequence: B * 2^min(k+1, 5) for k = 0..5, then the
    // 32B cap repeats.
    let mut due = CLOCK_START_MS + 30_000;
    let mut next_interval = 60_000;
    for step in 0..6 {
        write_recovery_clock(&clock_file, due);
        probes.recv_timeout(Duration::from_secs(2)).unwrap();
        let next_due = due + next_interval;
        await_route_field(
            &client,
            &base,
            &active_cookie,
            &route_id,
            "next_probe_at_ms",
            json!(next_due),
        )
        .await;
        let row = route_row(&client, &base, &active_cookie, &route_id).await;
        assert_eq!(row["failed_probe_count"], step as i64 + 1);
        assert_eq!(row["current_interval_ms"], next_interval);
        due = next_due;
        next_interval = (next_interval * 2).min(960_000);
    }
    worker.join().unwrap();
    let row = route_row(&client, &base, &active_cookie, &route_id).await;
    assert_eq!(row["next_probe_at_ms"], json!(due));
    assert_eq!(row["current_interval_ms"], 960_000);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn available_routes_receive_no_periodic_probe_traffic() {
    let environment = TestEnvironment::new("no-periodic-probes");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server =
        environment.start_with(port, &[("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "50")]);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_recovery_settings(&client, &base, &active_cookie, 250, 2).await;

    let (upstream_base_url, probes, quiet_result, worker) = quiet_after_probe_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "quiet-model",
    )
    .await;
    probes.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert!(
        !quiet_result.recv_timeout(Duration::from_secs(3)).unwrap(),
        "an Available route received periodic probe traffic"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "available"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn at_most_one_recovery_probe_is_in_flight_per_unavailable_route() {
    let environment = TestEnvironment::new("single-recovery-inflight");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server =
        environment.start_with(port, &[("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS", "50")]);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_recovery_settings(&client, &base, &active_cookie, 200, 3).await;

    let (upstream_base_url, probes, duplicate_result, worker) = single_recovery_probe_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "inflight-model",
    )
    .await;
    probes.recv_timeout(Duration::from_secs(2)).unwrap();
    probes.recv_timeout(Duration::from_secs(3)).unwrap();
    // The scheduler's recovery probe is held open; the admin's manual check is
    // rejected while a recovery probe is in flight (ROUTE-018).
    let conflicted = client
        .post(format!("{base}/admin/model-routes/{route_id}/check"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(conflicted.status(), StatusCode::CONFLICT);
    worker.join().unwrap();
    assert!(
        !duplicate_result
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        "a second recovery probe started while one was still in flight"
    );
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn manual_check_recovers_an_unavailable_route_with_a_fixed_native_probe() {
    let environment = TestEnvironment::new("manual-recovery-check");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The default 30s base interval keeps the automatic recovery scheduler out
    // of this test; the admin check is the only recovery probe.
    let (upstream_base_url, probes, worker) = failing_then_success_chat_upstream();
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "manual-model",
    )
    .await;
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );
    probes.recv_timeout(Duration::from_secs(2)).unwrap();

    let checked = client
        .post(format!("{base}/admin/model-routes/{route_id}/check"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(checked.status(), StatusCode::OK);
    assert_eq!(
        checked.json::<serde_json::Value>().await.unwrap()["health"],
        "available"
    );
    let manual_probe = probes.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        manual_probe.request_line,
        "POST /v1/chat/completions HTTP/1.1"
    );
    let body: serde_json::Value = serde_json::from_slice(&manual_probe.body).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "manual-model",
            "messages": [{ "role": "user", "content": "ping" }],
            "max_tokens": 1,
            "stream": false
        })
    );
    worker.join().unwrap();
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "available"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[test]
fn startup_failures_never_claim_ready() {
    let environment = TestEnvironment::new("startup-failures");
    environment.initialize();

    let occupied_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied_listener.local_addr().unwrap().port();
    let collision = environment
        .command()
        .args(["serve", "--port", &occupied_port.to_string()])
        .output()
        .unwrap();
    assert!(!collision.status.success());

    let invalid_port = environment
        .command()
        .args(["serve", "--port", "0"])
        .output()
        .unwrap();
    assert!(!invalid_port.status.success());

    let corrupt = TestEnvironment::new("corrupt-database");
    fs::create_dir_all(corrupt.database_path().parent().unwrap()).unwrap();
    fs::write(corrupt.database_path(), b"not a SQLite database").unwrap();
    let corrupt_start = corrupt
        .command()
        .args(["serve", "--port", "18789"])
        .output()
        .unwrap();
    assert!(!corrupt_start.status.success());
}

#[tokio::test]
async fn manual_backup_creates_verified_protected_complete_snapshot() {
    let environment = TestEnvironment::new("manual-backup");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Backup canary provider",
            "base_url": "https://api.example.invalid/v1",
            "api_key": "canary-backup-secret-abc123"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);

    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(created_body["trigger"], "manual");
    assert_eq!(created_body["schema_version"], 17);
    assert!(created_body["size"].as_i64().unwrap() > 0);

    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    assert_eq!(list_body["status"]["state"], "ok");
    assert_eq!(list_body["status"]["count"], 1);
    assert_eq!(list_body["status"]["retention"], 10);
    assert_eq!(list_body["status"]["last_trigger"], "manual");
    assert_eq!(list_body["status"]["schema_version"], 17);
    assert_eq!(list_body["data"].as_array().unwrap().len(), 1);

    let operations = get_with_cookie(&client, format!("{base}/admin/operations"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(operations["backups"]["state"], "ok");
    assert_eq!(operations["backups"]["count"], 1);

    // The artifact is a consistent complete SQLite snapshot containing
    // configuration, application identity, and upstream secrets.
    let backup_dir = environment.root.join("data/local-api-relay/backups");
    let files = backup_artifact_paths(&backup_dir);
    assert_eq!(files.len(), 1);
    let backup_connection = rusqlite::Connection::open(&files[0]).unwrap();
    let integrity: String = backup_connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let application: String = backup_connection
        .query_row(
            "SELECT application FROM backup_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(application, "local-api-relay");
    let canary_key: String = backup_connection
        .query_row(
            "SELECT api_key FROM upstream_providers WHERE display_name = 'Backup canary provider'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(canary_key, "canary-backup-secret-abc123");
    let model_name: String = backup_connection
        .query_row(
            "SELECT name FROM published_models WHERE id = 'gpt-5.6-sol'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(model_name, "gpt-5.6-sol");

    #[cfg(unix)]
    assert_backup_permissions(&environment);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn automatic_backup_respects_change_and_twenty_four_hour_boundary() {
    const CLOCK: i64 = 1_800_000_000;
    let environment = TestEnvironment::new("auto-backup-boundary");
    let bootstrap_credential = environment.initialize();

    // Phase A: fixed test clock and fast tick. One automatic backup appears,
    // and a later durable change is gated by the 24-hour window.
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_BACKUP_TICK_MS", "100"),
            ("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &CLOCK.to_string()),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let first_trigger = wait_for_backup_count(&client, &base, &active_cookie, 1).await;
    assert_eq!(first_trigger, "auto");

    let provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Changed provider",
            "base_url": "https://api.example.invalid/v1",
            "api_key": "changed-provider-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(provider.status(), StatusCode::CREATED);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(backup_count(&client, &base, &active_cookie).await, 1);

    // Phase B: advance the clock beyond the 24-hour window; the pending change
    // now becomes eligible and produces a second automatic backup.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_BACKUP_TICK_MS", "100"),
            (
                "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
                &(CLOCK + 25 * 60 * 60).to_string(),
            ),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let second_trigger = wait_for_backup_count(&client, &base, &active_cookie, 2).await;
    assert_eq!(second_trigger, "auto");
    thread::sleep(Duration::from_millis(400));
    assert_eq!(backup_count(&client, &base, &active_cookie).await, 2);

    // Phase C: a later restart with unchanged data creates no backup even
    // though the 24-hour window has passed again.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_BACKUP_TICK_MS", "100"),
            (
                "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
                &(CLOCK + 30 * 60 * 60).to_string(),
            ),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    thread::sleep(Duration::from_millis(400));
    assert_eq!(backup_count(&client, &base, &active_cookie).await, 2);

    // Phase D: with unchanged data and the 24-hour window fully open, no
    // automatic backup may be created either.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_BACKUP_TICK_MS", "100"),
            (
                "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
                &(CLOCK + 50 * 60 * 60).to_string(),
            ),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    thread::sleep(Duration::from_millis(400));
    assert_eq!(backup_count(&client, &base, &active_cookie).await, 2);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn failed_backup_verification_keeps_existing_backups_and_marks_degraded() {
    let environment = TestEnvironment::new("backup-verify-failure");
    let bootstrap_credential = environment.initialize();

    // Phase 1: one valid backup without injection.
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    server.kill().unwrap();
    server.wait().unwrap();

    // Phase 2: with verification failure injected, the new backup is discarded,
    // the existing backup is preserved, and the status reports the failure.
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "verify")],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let rejected = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(body["status"]["state"], "degraded");
    assert_eq!(body["status"]["last_failed_stage"], "verify");
    assert_eq!(
        body["status"]["last_failed_reason"],
        "injected verify failure"
    );
    assert_eq!(body["status"]["count"], 1);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    server.kill().unwrap();
    server.wait().unwrap();

    // Phase 3: a later successful backup clears the failure and rotates.
    let port = available_port();
    let mut server = environment.start(port);
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(body["status"]["state"], "ok");
    assert_eq!(body["status"]["count"], 2);
    assert!(body["status"]["last_failed_stage"].is_null());

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn rotation_keeps_the_ten_most_recent_backups_and_protects_on_rotation_failure() {
    const CLOCK: i64 = 1_800_000_000;
    let environment = TestEnvironment::new("backup-rotation");
    let bootstrap_credential = environment.initialize();

    // Create twelve backups under a fixed clock; only the ten most recent are
    // retained, deterministically ordered by the backup sequence.
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &CLOCK.to_string())],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    for _ in 0..12 {
        let response = client
            .post(format!("{base}/admin/backups"))
            .header(header::COOKIE, &active_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    let body = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(body["status"]["count"], 10);
    let backup_dir = environment.root.join("data/local-api-relay/backups");
    let files: Vec<String> = backup_artifact_paths(&backup_dir)
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 10);
    assert!(!files.contains(&"backup-000000-".to_owned()));
    assert!(!files.contains(&"backup-000001-".to_owned()));

    // A rotation failure must not delete any existing backup; the new verified
    // snapshot stays and the status reports the rotation stage.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &(CLOCK + 1).to_string()),
            ("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "rotate"),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let rejected = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(body["status"]["state"], "degraded");
    assert_eq!(body["status"]["last_failed_stage"], "rotate");
    assert_eq!(body["status"]["count"], 11);
    assert_eq!(backup_artifact_paths(&backup_dir).len(), 11);

    // After the injection is removed, the next backup rotates back to ten.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &(CLOCK + 2).to_string())],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(body["status"]["state"], "ok");
    assert_eq!(body["status"]["count"], 10);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[cfg(unix)]
fn assert_private_paths(environment: &TestEnvironment) {
    for path in [
        environment.root.join("data/local-api-relay"),
        environment.root.join("config/local-api-relay"),
        environment.root.join("state/local-api-relay"),
    ] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    for path in [
        environment.database_path(),
        environment.root.join("config/local-api-relay/service.json"),
    ] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
}

#[cfg(not(unix))]
fn assert_private_paths(_environment: &TestEnvironment) {}

// ---------------------------------------------------------------------------
// Ticket 23 — call records and the model route attempt chain
// (OPS-001..OPS-005, OPS-020/OPS-021, UI-010/UI-011)
// ---------------------------------------------------------------------------

/// A successful chat completion carrying the tokens the route reported, so a
/// call record can attribute usage to the final successful attempt (OPS-002).
fn chat_response_with_usage(input_tokens: i64, cached_tokens: i64, output_tokens: i64) -> String {
    format!(
        r#"{{"id":"chatcmpl-usage","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{input_tokens},"completion_tokens":{output_tokens},"prompt_tokens_details":{{"cached_tokens":{cached_tokens}}}}}}}"#
    )
}

async fn calls_usage_json(client: &Client, base: &str, cookie: &str) -> serde_json::Value {
    calls_usage_window(client, base, cookie, "24h").await
}

async fn calls_usage_window(client: &Client, base: &str, cookie: &str, window: &str) -> serde_json::Value {
    get_with_cookie(
        client,
        format!("{base}/admin/calls-usage?window={window}"),
        cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap()
}

#[tokio::test]
async fn successful_chat_call_records_usage_attribution_and_completion_time() {
    let environment = TestEnvironment::new("call-record-success");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 30, 40),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Call record client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    let body = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(body["total"], 1);
    let call = &body["calls"][0];
    assert_eq!(call["published_model_name"], "gpt-5.6-sol");
    assert_eq!(call["protocol"], "chat_completions");
    assert_eq!(call["streamed"], false);
    assert_eq!(call["succeeded"], true);
    assert_eq!(call["success_provider_name"], "chat_completions route 1x");
    assert!(call["success_provider_id"].as_str().is_some());
    assert_eq!(call["input_tokens"], 100);
    assert_eq!(call["cached_input_tokens"], 30);
    assert_eq!(call["output_tokens"], 40);
    assert!(
        call["completion_ms"].as_i64().unwrap() >= 0,
        "successful call must record its completion time"
    );
    assert!(
        call["first_token_ms"].is_null(),
        "non-streaming call has no first-token latency"
    );
    assert_close(
        call["estimated_cost_rmb"].as_f64().unwrap(),
        (70.0 * 5.0 + 30.0 * 0.5 + 40.0 * 30.0) / 1_000_000.0,
        1e-9,
        "estimated cost follows the OPS-006 formula",
    );
    let attempts = call["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["sequence"], 0);
    assert_eq!(attempts[0]["http_status"], 200);
    assert!(attempts[0]["failure_category"].is_null());
    assert_eq!(attempts[0]["commit_phase"], "committed");
    assert_eq!(attempts[0]["outcome"], "success");
    assert_eq!(attempts[0]["provider_name"], "chat_completions route 1x");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn fallback_attempts_form_an_ordered_chain_with_normalized_failures() {
    let environment = TestEnvironment::new("call-record-fallback-chain");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let (succeeding_url, succeeding_requests, succeeding_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(200, 60, 80),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
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
        succeeding_url,
        "chat_completions",
        "succeeding-chain-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "fallback chain client",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "fallback" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    failing_worker.join().unwrap();
    succeeding_worker.join().unwrap();

    let body = calls_usage_json(&client, &base, &active_cookie).await;
    // One client call stays one call record even when Fallback happened
    // (OPS-001).
    assert_eq!(body["total"], 1);
    let call = &body["calls"][0];
    assert_eq!(call["succeeded"], true);
    assert_eq!(call["success_provider_name"], "chat_completions route 2x");
    assert_eq!(call["input_tokens"], 200);
    assert_eq!(call["cached_input_tokens"], 60);
    assert_eq!(call["output_tokens"], 80);
    let attempts = call["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["sequence"], 0);
    assert_eq!(attempts[0]["provider_name"], "chat_completions route 1x");
    assert_eq!(attempts[0]["http_status"], 500);
    assert_eq!(attempts[0]["failure_category"], "upstream_http_5xx");
    assert_eq!(attempts[0]["commit_phase"], "pre_commit");
    assert_eq!(attempts[0]["outcome"], "fallback");
    assert!(attempts[0]["duration_ms"].as_i64().unwrap() >= 0);
    assert_eq!(attempts[1]["sequence"], 1);
    assert_eq!(attempts[1]["provider_name"], "chat_completions route 2x");
    assert_eq!(attempts[1]["http_status"], 200);
    assert!(attempts[1]["failure_category"].is_null());
    assert_eq!(attempts[1]["commit_phase"], "committed");
    assert_eq!(attempts[1]["outcome"], "success");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn exhausted_candidates_record_one_failed_call_with_unknown_values() {
    let environment = TestEnvironment::new("call-record-all-failed");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "exhausted-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Exhausted client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "boom" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    let body = calls_usage_json(&client, &base, &active_cookie).await;
    // An exhausted candidate set still keeps one failed call record (OPS-005).
    assert_eq!(body["total"], 1);
    let call = &body["calls"][0];
    assert_eq!(call["succeeded"], false);
    assert!(call["success_provider_id"].is_null());
    assert!(call["success_provider_name"].is_null());
    assert!(call["input_tokens"].is_null());
    assert!(call["cached_input_tokens"].is_null());
    assert!(call["output_tokens"].is_null());
    assert!(call["estimated_cost_rmb"].is_null());
    assert!(
        call["completion_ms"].is_null(),
        "failed call must show unknown rather than zero"
    );
    assert!(call["first_token_ms"].is_null());
    let attempts = call["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["outcome"], "failed");
    assert_eq!(attempts[0]["commit_phase"], "pre_commit");
    assert_eq!(attempts[0]["http_status"], 500);
    assert_eq!(attempts[0]["failure_category"], "upstream_http_5xx");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn stream_terminated_after_commit_records_one_attempt_without_usage() {
    let environment = TestEnvironment::new("call-record-stream-terminated");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let first_event = "data: {\"id\":\"chatcmpl-eof\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.6-sol\",\"choices\":[]}\n\n";
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        sse_http_response(first_event),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Terminated stream client",
    )
    .await;

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "stream" }],
            "stream": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), first_event);
    upstream_worker.join().unwrap();

    let body = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(body["total"], 1);
    let call = &body["calls"][0];
    assert_eq!(call["succeeded"], false);
    assert!(call["input_tokens"].is_null());
    assert!(call["cached_input_tokens"].is_null());
    assert!(call["output_tokens"].is_null());
    let attempts = call["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["commit_phase"], "committed");
    assert_eq!(attempts[0]["outcome"], "stream_terminated");
    assert_eq!(attempts[0]["failure_category"], "invalid_upstream_response");
    assert_eq!(attempts[0]["http_status"], 200);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn call_records_page_paginates_and_clamps_page_size() {
    let environment = TestEnvironment::new("call-record-pagination");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(1, 0, 1),
        chat_response_with_usage(2, 0, 2),
        chat_response_with_usage(3, 0, 3),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Pagination client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    for index in 0..3 {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": format!("call {index}") }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    upstream_worker.join().unwrap();

    let first_page = get_with_cookie(
        &client,
        format!("{base}/admin/calls-usage?page=0&page_size=2"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(first_page["total"], 3);
    assert_eq!(first_page["page"], 0);
    assert_eq!(first_page["page_size"], 2);
    assert_eq!(first_page["calls"].as_array().unwrap().len(), 2);
    let second_page = get_with_cookie(
        &client,
        format!("{base}/admin/calls-usage?page=1&page_size=2"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(second_page["calls"].as_array().unwrap().len(), 1);
    assert_eq!(second_page["calls"][0]["input_tokens"], 1);

    let clamped_low = get_with_cookie(
        &client,
        format!("{base}/admin/calls-usage?page=0&page_size=0"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(clamped_low["page_size"], 1);
    let clamped_high = get_with_cookie(
        &client,
        format!("{base}/admin/calls-usage?page=0&page_size=1000"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(clamped_high["page_size"], 100);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn canary_fields_never_enter_call_records_or_attempts() {
    let environment = TestEnvironment::new("call-record-canary");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let prompt_canary = "canary-prompt-content";
    let tool_canary = "canary-tool-params";
    let header_canary = "canary-header-value";
    let error_canary = "canary-upstream-error-body";
    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(
            500,
            "Internal Server Error",
            &format!(r#"{{"error":{{"message":"{error_canary}"}}}}"#),
        ),
    ]);
    let (succeeding_url, succeeding_requests, succeeding_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        complete_chat_response(),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "canary-failing-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let succeeding_route = configure_route(
        &client,
        &base,
        &active_cookie,
        succeeding_url,
        "chat_completions",
        "canary-succeeding-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "canary client",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .header("X-Canary-Header", header_canary)
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": prompt_canary }],
            "tool_params_canary": tool_canary
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    failing_worker.join().unwrap();
    succeeding_worker.join().unwrap();

    let body = calls_usage_json(&client, &base, &active_cookie).await;
    let body_text = serde_json::to_string(&body).unwrap();
    for canary in [prompt_canary, tool_canary, header_canary, error_canary] {
        assert!(
            !body_text.contains(canary),
            "calls page must not expose {canary}"
        );
    }
    let attempts = body["calls"][0]["attempts"].as_array().unwrap();
    assert_eq!(attempts[0]["failure_category"], "upstream_http_5xx");
    assert_eq!(attempts[0]["http_status"], 500);

    let database = fs::read(environment.database_path()).unwrap();
    for canary in [prompt_canary, tool_canary, header_canary, error_canary] {
        assert!(
            !database.windows(canary.len()).any(|slice| slice == canary.as_bytes()),
            "call records and attempts must not persist {canary}"
        );
    }

    server.kill().unwrap();
    server.wait().unwrap();
}

// ---------------------------------------------------------------------------
// Ticket 24 — cost estimation and usage aggregation
// (CFG-003..CFG-005, OPS-006..OPS-009, UI-010)
// ---------------------------------------------------------------------------

/// A successful chat completion with usage but no cached-token detail, so the
/// cache component must be treated as zero (OPS-006).
fn chat_response_without_cache(input_tokens: i64, output_tokens: i64) -> String {
    format!(
        r#"{{"id":"chatcmpl-nocache","object":"chat.completion","created":1,"model":"scripted-upstream-model","choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":{input_tokens},"completion_tokens":{output_tokens}}}}}"#
    )
}

/// Logs in with the already-changed admin password (for servers restarted past
/// the 8-hour session lifetime).
async fn login_administrator(client: &Client, base: &str) -> String {
    let login = client
        .post(format!("{base}/admin/login"))
        .json(&json!({ "password": "correct-horse-battery-staple" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    session_cookie(&login)
}

fn assert_close(actual: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: expected {expected}, got {actual}"
    );
}

#[tokio::test]
async fn usage_totals_and_cost_follow_the_spec_formula() {
    let environment = TestEnvironment::new("usage-cost-formula");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(1_000_000, 100_000, 200_000),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "usage-formula-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Cost formula client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "cost" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    // OPS-006: (uncached*input + cached*cached + output*output) / 1e6 * ratio
    // with gpt-5.6-sol prices 5 / 30 / 0.5 RMB per million tokens at 1x.
    let expected_cost = (900_000.0 * 5.0 + 100_000.0 * 0.5 + 200_000.0 * 30.0) / 1_000_000.0;

    let body = calls_usage_window(&client, &base, &active_cookie, "24h").await;
    let totals = &body["totals"];
    assert_eq!(totals["input_tokens"], 1_000_000);
    assert_eq!(totals["cached_input_tokens"], 100_000);
    assert_eq!(totals["output_tokens"], 200_000);
    assert_close(
        totals["estimated_cost_rmb"].as_f64().unwrap(),
        expected_cost,
        1e-6,
        "window cost",
    );
    assert_close(
        totals["cache_hit_rate"].as_f64().unwrap(),
        0.1,
        1e-9,
        "cache hit rate",
    );
    assert_eq!(body["calls"][0]["input_tokens"], 1_000_000);
    assert_close(
        body["calls"][0]["estimated_cost_rmb"].as_f64().unwrap(),
        expected_cost,
        1e-6,
        "per-call cost",
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn missing_cache_usage_counts_as_zero_and_zero_input_is_safe() {
    let environment = TestEnvironment::new("usage-missing-cache");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_without_cache(1000, 500),
        chat_response_with_usage(0, 0, 0),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "no-cache-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "No cache client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    for content in ["first", "zero"] {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": content }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    upstream_worker.join().unwrap();

    let body = calls_usage_window(&client, &base, &active_cookie, "all").await;
    let totals = &body["totals"];
    assert_eq!(totals["input_tokens"], 1000);
    assert_eq!(totals["cached_input_tokens"], 0);
    assert_eq!(totals["output_tokens"], 500);
    // (1000 * 5 + 0 * 0.5 + 500 * 30) / 1e6 = 0.02 RMB; the zero-input call
    // contributes nothing and must not make the cache hit rate NaN.
    assert_close(
        totals["estimated_cost_rmb"].as_f64().unwrap(),
        0.02,
        1e-9,
        "cost without cache detail",
    );
    assert_close(totals["cache_hit_rate"].as_f64().unwrap(), 0.0, 1e-9, "zero-input hit rate");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn usage_distribution_breaks_down_by_model_and_provider() {
    let environment = TestEnvironment::new("usage-distribution");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (sol_url, sol_requests, sol_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(200_000, 0, 300_000),
    ]);
    let (flash_url, flash_requests, flash_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(1_000_000, 0, 500_000),
    ]);
    let sol_route = configure_route(
        &client,
        &base,
        &active_cookie,
        sol_url,
        "chat_completions",
        "sol-upstream",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let flash_route = configure_route(
        &client,
        &base,
        &active_cookie,
        flash_url,
        "chat_completions",
        "flash-upstream",
        "deepseek-v4-flash",
        "1",
    )
    .await;
    sol_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    flash_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "distribution client",
            "model_route_ids": [sol_route, flash_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    for model in ["gpt-5.6-sol", "deepseek-v4-flash"] {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({
                "model": model,
                "messages": [{ "role": "user", "content": "dist" }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    sol_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    flash_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    sol_worker.join().unwrap();
    flash_worker.join().unwrap();

    let body = calls_usage_window(&client, &base, &active_cookie, "all").await;
    let totals = &body["totals"];
    assert_eq!(totals["input_tokens"], 1_200_000);
    assert_eq!(totals["output_tokens"], 800_000);
    assert_close(
        totals["estimated_cost_rmb"].as_f64().unwrap(),
        12.0,
        1e-6,
        "combined cost",
    );
    let models = totals["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    let sol = models
        .iter()
        .find(|model| model["published_model_name"] == "gpt-5.6-sol")
        .expect("gpt-5.6-sol in the distribution");
    assert_eq!(sol["input_tokens"], 200_000);
    assert_close(sol["estimated_cost_rmb"].as_f64().unwrap(), 10.0, 1e-6, "sol cost");
    let providers = sol["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["provider_name"], "chat_completions route 1x");
    assert_eq!(providers[0]["input_tokens"], 200_000);
    let flash = models
        .iter()
        .find(|model| model["published_model_name"] == "deepseek-v4-flash")
        .expect("deepseek-v4-flash in the distribution");
    assert_close(flash["estimated_cost_rmb"].as_f64().unwrap(), 2.0, 1e-6, "flash cost");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn six_windows_aggregate_usage_by_recency() {
    const CLOCK: i64 = 1_800_000_000;
    const TWO_DAYS_MS: i64 = 2 * 86_400_000;
    let environment = TestEnvironment::new("usage-windows");
    let bootstrap_credential = environment.initialize();

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 0, 100),
        chat_response_with_usage(300, 0, 300),
    ]);
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &CLOCK.to_string())],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "window-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Window client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "day one" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    // Advance two days: the first call falls out of the 1h/24h windows but
    // stays inside 7d/14d and all-time.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[(
            "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
            &(CLOCK + TWO_DAYS_MS / 1000).to_string(),
        )],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = login_administrator(&client, &base).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "day three" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    let hourly = calls_usage_window(&client, &base, &active_cookie, "1h").await;
    assert_eq!(hourly["totals"]["input_tokens"], 300, "1h keeps only the recent call");
    let five_hour = calls_usage_window(&client, &base, &active_cookie, "5h").await;
    assert_eq!(five_hour["totals"]["input_tokens"], 300, "5h keeps only the recent call");
    let daily = calls_usage_window(&client, &base, &active_cookie, "24h").await;
    assert_eq!(daily["totals"]["input_tokens"], 300, "24h keeps only the recent call");
    let weekly = calls_usage_window(&client, &base, &active_cookie, "7d").await;
    assert_eq!(weekly["totals"]["input_tokens"], 400, "7d keeps both calls");
    let fortnight = calls_usage_window(&client, &base, &active_cookie, "14d").await;
    assert_eq!(fortnight["totals"]["input_tokens"], 400, "14d keeps both calls");
    let all_time = calls_usage_window(&client, &base, &active_cookie, "all").await;
    assert_eq!(all_time["totals"]["input_tokens"], 400, "all keeps both calls");
    let unknown = calls_usage_window(&client, &base, &active_cookie, "bogus").await;
    assert_eq!(unknown["window"], "24h", "unknown windows fall back to 24h");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn price_edits_change_future_costs_without_changing_routing() {
    let environment = TestEnvironment::new("usage-price-edits");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (cheap_url, cheap_requests, cheap_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(1000, 0, 1000),
        chat_response_with_usage(1000, 0, 1000),
    ]);
    let (expensive_url, expensive_requests, expensive_worker) =
        scripted_chat_upstream(vec![complete_chat_response()]);
    let cheap_route = configure_route(
        &client,
        &base,
        &active_cookie,
        cheap_url,
        "chat_completions",
        "cheap-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let expensive_route = configure_route(
        &client,
        &base,
        &active_cookie,
        expensive_url,
        "chat_completions",
        "expensive-model",
        "gpt-5.6-sol",
        "3",
    )
    .await;
    cheap_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    expensive_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "price edit client",
            "model_route_ids": [cheap_route, expensive_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    for _ in 0..2 {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": "priced" }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let captured = cheap_requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&captured.body).unwrap()["model"],
            "cheap-model"
        );

        // Edit the published model prices between the two calls (CFG-013 only
        // changes the cost; routing order follows the multiplier).
        let edited = client
            .patch(format!("{base}/admin/published-models/gpt-5.6-sol/prices"))
            .header(header::COOKIE, &active_cookie)
            .json(&json!({
                "input_price_rmb": "10",
                "output_price_rmb": "60",
                "cached_input_price_rmb": "0.5"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(edited.status(), StatusCode::OK);
    }
    cheap_worker.join().unwrap();
    expensive_worker.join().unwrap();

    let body = calls_usage_window(&client, &base, &active_cookie, "all").await;
    let calls = body["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    // Newest first: the second call used the edited prices.
    assert_close(
        calls[0]["estimated_cost_rmb"].as_f64().unwrap(),
        (1000.0 * 10.0 + 1000.0 * 60.0) / 1_000_000.0,
        1e-9,
        "cost after price edit",
    );
    assert_close(
        calls[1]["estimated_cost_rmb"].as_f64().unwrap(),
        (1000.0 * 5.0 + 1000.0 * 30.0) / 1_000_000.0,
        1e-9,
        "cost before price edit",
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn failed_calls_are_excluded_from_usage_aggregation() {
    let environment = TestEnvironment::new("usage-failures-excluded");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A fallback call that succeeds on the second route.
    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let (succeeding_url, succeeding_requests, succeeding_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 0, 50),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "agg-failing-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let succeeding_route = configure_route(
        &client,
        &base,
        &active_cookie,
        succeeding_url,
        "chat_completions",
        "agg-succeeding-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let fallback_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "aggregation fallback client",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {fallback_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "fallback" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    // An exhausted single-route call that fails outright.
    let (exhausted_url, exhausted_requests, exhausted_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(500, "Internal Server Error", ""),
    ]);
    let exhausted_route = configure_route(
        &client,
        &base,
        &active_cookie,
        exhausted_url,
        "chat_completions",
        "agg-exhausted-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    exhausted_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let exhausted_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &exhausted_route,
        "Aggregation exhausted client",
    )
    .await;
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {exhausted_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "exhausted" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    exhausted_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    failing_worker.join().unwrap();
    succeeding_worker.join().unwrap();
    exhausted_worker.join().unwrap();

    let body = calls_usage_window(&client, &base, &active_cookie, "all").await;
    assert_eq!(body["total"], 2, "both calls are recorded");
    // Only the successful route's usage enters the aggregation (OPS-004/005).
    let totals = &body["totals"];
    assert_eq!(totals["input_tokens"], 100);
    assert_eq!(totals["output_tokens"], 50);
    assert_close(
        totals["estimated_cost_rmb"].as_f64().unwrap(),
        (100.0 * 5.0 + 50.0 * 30.0) / 1_000_000.0 * 2.0,
        1e-9,
        "aggregated cost from success only",
    );
    assert_eq!(totals["models"].as_array().unwrap().len(), 1);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn daily_aggregation_outlives_per_call_records() {
    const CLOCK: i64 = 1_800_000_000;
    const FIFTEEN_DAYS_S: i64 = 15 * 86_400;
    let environment = TestEnvironment::new("usage-daily-retention");
    let bootstrap_credential = environment.initialize();

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(1000, 0, 1000),
    ]);
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &CLOCK.to_string()),
            ("LOCAL_API_RELAY_TEST_RETENTION_TICK_MS", "50"),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "daily-model",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Daily aggregation client",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "day zero" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();
    let before = calls_usage_window(&client, &base, &active_cookie, "all").await;
    assert_eq!(before["totals"]["input_tokens"], 1000);

    // Jump past the 14-day per-call retention; the daily aggregate persists.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            (
                "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
                &(CLOCK + FIFTEEN_DAYS_S).to_string(),
            ),
            ("LOCAL_API_RELAY_TEST_RETENTION_TICK_MS", "50"),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = login_administrator(&client, &base).await;

    let mut pruned = false;
    for _ in 0..100 {
        let body = calls_usage_window(&client, &base, &active_cookie, "all").await;
        if body["total"].as_i64() == Some(0) {
            pruned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(pruned, "per-call records must be pruned after the retention window");

    let all_time = calls_usage_window(&client, &base, &active_cookie, "all").await;
    assert_eq!(all_time["total"], 0);
    assert_eq!(
        all_time["totals"]["input_tokens"], 1000,
        "daily aggregation survives per-call record expiry"
    );
    assert_close(
        all_time["totals"]["estimated_cost_rmb"].as_f64().unwrap(),
        (1000.0 * 5.0 + 1000.0 * 30.0) / 1_000_000.0,
        1e-9,
        "daily cost survives expiry",
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn streaming_responses_semantic_failure_is_recorded_failed_and_quarantines() {
    let environment = TestEnvironment::new("usage-responses-semantic-stream");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let completed = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":1,\"usage\":{\"input_tokens\":100,\"output_tokens\":50,\"input_tokens_details\":{\"cached_tokens\":10}},\"response\":{\"status\":\"completed\"}}\n\n"
    );
    let failed = concat!(
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"sequence_number\":1,\"usage\":{\"input_tokens\":999,\"output_tokens\":999,\"input_tokens_details\":{\"cached_tokens\":0}},\"response\":{\"status\":\"failed\"}}\n\n"
    );
    let (upstream_base_url, _upstream_requests, upstream_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_responses_response()),
        sse_http_response(completed),
        sse_http_response(failed),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "responses",
        "gpt-5.6-sol",
    )
    .await;
    let relay_secret = create_relay_secret(
        &client,
        &base,
        &active_cookie,
        &route_id,
        "Responses semantic stream client",
    )
    .await;

    let call = |content: &'static str| {
        client
            .post(format!("{base}/v1/responses"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .json(&json!({ "model": "gpt-5.6-sol", "input": content, "stream": true }))
            .send()
    };
    // The completed terminal is a reliable success (API-012).
    let response = call("completed").await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("response.completed"));
    // The failed terminal relays natively but records a failed call and
    // quarantines the committed route (API-011/ROUTE-008/OPS-004).
    let response = call("failed").await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("response.failed"));
    assert_eq!(
        route_health(&client, &base, &active_cookie, &route_id).await,
        "unavailable"
    );
    upstream_worker.join().unwrap();

    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(usage["total"], 2);
    let failed_call = &usage["calls"][0];
    assert_eq!(failed_call["published_model_name"], "gpt-5.6-sol");
    assert_eq!(failed_call["protocol"], "responses");
    assert_eq!(failed_call["succeeded"], false);
    assert!(
        failed_call["input_tokens"].is_null() && failed_call["output_tokens"].is_null(),
        "usage from a semantically failed response must not enter the call record"
    );
    assert!(failed_call["estimated_cost_rmb"].is_null());
    let attempts = failed_call["attempts"].as_array().unwrap();
    assert_eq!(attempts[0]["commit_phase"], "committed");
    assert_eq!(attempts[0]["outcome"], "stream_terminated");
    assert_eq!(attempts[0]["failure_category"], "upstream_semantic_failure");

    let completed_call = &usage["calls"][1];
    assert_eq!(completed_call["succeeded"], true);
    assert_eq!(completed_call["input_tokens"], 100);
    assert_eq!(completed_call["cached_input_tokens"], 10);
    assert_eq!(completed_call["output_tokens"], 50);
    assert_close(
        completed_call["estimated_cost_rmb"].as_f64().unwrap(),
        (90.0 * 5.0 + 10.0 * 0.5 + 50.0 * 30.0) / 1_000_000.0,
        1e-9,
        "completed stream cost",
    );
    assert_eq!(completed_call["attempts"][0]["outcome"], "success");

    server.kill().unwrap();
    server.wait().unwrap();
}

// ---------------------------------------------------------------------------
// Ticket 25 — surface storage degradation and usage gaps
// (DATA-004..DATA-005, OPS-010..OPS-012, OPS-016)
// ---------------------------------------------------------------------------

async fn operations_json(client: &Client, base: &str, cookie: &str) -> serde_json::Value {
    get_with_cookie(client, format!("{base}/admin/operations"), cookie)
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap()
}

fn find_gap<'a>(gaps: &'a [serde_json::Value], kind: &str) -> &'a serde_json::Value {
    gaps.iter()
        .find(|gap| gap["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} gap in {gaps:?}"))
}

/// Sends one chat completion through the relay and asserts the client call
/// itself still succeeds even when operational persistence fails (DATA-004).
async fn chat_call(client: &Client, base: &str, secret: &str) {
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn operational_write_failures_degrade_storage_without_failing_the_client() {
    let environment = TestEnvironment::new("storage-degraded-call-records");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE", "call_records")],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 30, 40),
        chat_response_with_usage(200, 0, 50),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, &route_id, "Degraded client").await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // creation probe

    for _ in 0..2 {
        chat_call(&client, &base, &relay_secret).await;
        upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    upstream_worker.join().unwrap();

    // OPS-011: the Operations storage area shows Degraded with the affected
    // record category, the normalized error, the known lost count, and the
    // accounting gap start/end.
    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["storage"]["state"], "degraded");
    assert!(operations["storage"]["since"].as_i64().unwrap() > 0);
    let categories = operations["storage"]["categories"].as_array().unwrap();
    assert_eq!(categories.len(), 1);
    assert_eq!(categories[0]["category"], "call_records");
    assert_eq!(categories[0]["lost_records"], 2);
    assert!(
        categories[0]["error"]
            .as_str()
            .unwrap()
            .contains("injected"),
        "the normalized error must come from the persistence failure"
    );
    let accounting_gaps = operations["storage"]["accounting_gaps"].as_array().unwrap();
    assert_eq!(accounting_gaps.len(), 1);
    assert_eq!(accounting_gaps[0]["category"], "call_records");
    assert!(accounting_gaps[0]["started_at_ms"].as_i64().unwrap() > 0);
    assert!(accounting_gaps[0]["ended_at_ms"].is_null());
    assert_eq!(accounting_gaps[0]["lost_records"], 2);
    assert_eq!(operations["usage"]["state"], "incomplete");

    // OPS-016: the failed writes left a known persistence gap and no fabricated
    // usage — the successful calls produced no call record and no totals.
    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(usage["total"], 0);
    assert_eq!(usage["totals"]["input_tokens"], 0);
    assert_eq!(usage["usage_integrity"]["complete"], false);
    let gap = find_gap(usage["usage_integrity"]["gaps"].as_array().unwrap(), "persistence");
    assert_eq!(gap["category"], "call_records");
    assert_eq!(gap["lost_records"], 2);
    assert!(gap["ended_at_ms"].is_null());

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn storage_degradation_clears_only_after_same_category_rewrite_and_quick_check() {
    let environment = TestEnvironment::new("storage-degraded-recovers");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    // The one-time injection fails only the first call-record write, so a later
    // successful write can prove the OPS-012 recovery condition in-process.
    let mut server = environment.start_with(
        port,
        &[(
            "LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE_ONCE",
            "call_records",
        )],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 30, 40),
        chat_response_with_usage(200, 0, 50),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, &route_id, "Recovery client").await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    chat_call(&client, &base, &relay_secret).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["storage"]["state"], "degraded");
    assert_eq!(operations["storage"]["categories"][0]["category"], "call_records");
    assert_eq!(operations["storage"]["categories"][0]["lost_records"], 1);
    assert!(operations["storage"]["accounting_gaps"][0]["ended_at_ms"].is_null());

    // The same category re-writes successfully: the Degraded state clears only
    // after the lightweight integrity check, while the historical gap remains
    // as a permanent incompleteness marker (OPS-012/OPS-016).
    chat_call(&client, &base, &relay_secret).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["storage"]["state"], "healthy");
    assert_eq!(operations["storage"]["categories"].as_array().unwrap().len(), 0);
    assert_eq!(operations["storage"]["accounting_gaps"].as_array().unwrap().len(), 0);
    assert_eq!(operations["usage"]["state"], "incomplete");
    let gap = find_gap(operations["usage"]["gaps"].as_array().unwrap(), "persistence");
    let started = gap["started_at_ms"].as_i64().unwrap();
    let ended = gap["ended_at_ms"].as_i64().unwrap();
    assert!(ended >= started, "the gap must close after recovery");

    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(usage["total"], 1, "only the second call record was persisted");
    assert_eq!(usage["usage_integrity"]["complete"], false);
    let window_gap = find_gap(usage["usage_integrity"]["gaps"].as_array().unwrap(), "persistence");
    assert_eq!(window_gap["lost_records"], 1);
    assert!(window_gap["ended_at_ms"].as_i64().unwrap() >= started);

    // The incompleteness marker survives a restart: gaps are durable and are
    // never backfilled or hidden by storage recovery (OPS-016).
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start(port);
    let base = format!("http://127.0.0.1:{port}");
    wait_ready(&client, port).await;
    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(usage["usage_integrity"]["complete"], false);
    let after_restart_gap =
        find_gap(usage["usage_integrity"]["gaps"].as_array().unwrap(), "persistence");
    assert_eq!(after_restart_gap["lost_records"], 1);
    assert!(after_restart_gap["ended_at_ms"].as_i64().unwrap() >= started);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn failed_health_persistence_still_quarantines_the_route_in_memory() {
    let environment = TestEnvironment::new("storage-degraded-route-health");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE", "route_health")],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    let success = complete_chat_response();
    // Route A answers its creation probe, then fails the first client call with
    // 500; its quarantine write itself fails (injected). Route B always
    // succeeds. A is multiplier 1 so it leads the candidate order.
    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_status_response(500, "Internal Server Error", ""),
        http_json_response(&success),
    ]);
    let (healthy_url, healthy_requests, healthy_worker) = scripted_http_upstream(vec![
        http_json_response(&success),
        http_json_response(&success),
        http_json_response(&success),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "scripted-upstream-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let healthy_route = configure_route(
        &client,
        &base,
        &active_cookie,
        healthy_url,
        "chat_completions",
        "scripted-upstream-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Health client",
            "model_route_ids": [failing_route, healthy_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A probe
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B probe

    // The failing route's health history write is blocked; the client still
    // gets its successful response through the fallback (DATA-004/DATA-005).
    chat_call(&client, &base, &relay_secret).await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // A 500
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // B success

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["storage"]["state"], "degraded");
    let categories = operations["storage"]["categories"].as_array().unwrap();
    assert_eq!(categories[0]["category"], "route_health");
    assert!(categories[0]["lost_records"].as_i64().unwrap() >= 1);
    assert_eq!(
        operations["storage"]["accounting_gaps"].as_array().unwrap().len(),
        0,
        "route health is not usage accounting, so there is no accounting gap"
    );
    let routes = operations["routes"].as_array().unwrap();
    let failing = routes.iter().find(|route| route["id"] == failing_route).unwrap();
    assert_eq!(failing["health"], "unavailable");
    let healthy = routes.iter().find(|route| route["id"] == healthy_route).unwrap();
    assert_eq!(healthy["health"], "available");

    // The route is already excluded in memory: the next call skips route A and
    // goes straight to the healthy route.
    chat_call(&client, &base, &relay_secret).await;
    healthy_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        failing_requests.try_recv().is_err(),
        "route A must be excluded in memory after the failed quarantine"
    );

    // A restart without injection returns to a Healthy storage status and the
    // startup probes re-check both routes normally.
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start(port);
    let base = format!("http://127.0.0.1:{port}");
    wait_ready(&client, port).await;
    await_route_health(&client, &base, &active_cookie, &failing_route, "available").await;
    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["storage"]["state"], "healthy");
    chat_call(&client, &base, &relay_secret).await;

    failing_worker.join().unwrap();
    healthy_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn successful_calls_without_reported_usage_are_marked_as_known_gaps() {
    let environment = TestEnvironment::new("usage-gap-missing-upstream");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A successful call whose upstream reports no usage must not be estimated;
    // it is listed as a known usage gap instead (OPS-016).
    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        complete_chat_response(),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, &route_id, "No usage client").await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    chat_call(&client, &base, &relay_secret).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(usage["total"], 1);
    assert!(usage["calls"][0]["input_tokens"].is_null());
    assert_eq!(
        usage["totals"]["input_tokens"], 0,
        "missing usage must not be estimated or backfilled"
    );
    assert_eq!(usage["usage_integrity"]["complete"], false);
    let gap = find_gap(
        usage["usage_integrity"]["gaps"].as_array().unwrap(),
        "missing_upstream_usage",
    );
    assert_eq!(gap["category"], "upstream_usage");
    assert_eq!(gap["started_at_ms"], gap["ended_at_ms"]);
    assert_eq!(gap["lost_records"], 1);

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["storage"]["state"], "healthy");
    assert_eq!(operations["usage"]["state"], "incomplete");

    server.kill().unwrap();
    server.wait().unwrap();
}

// ---------------------------------------------------------------------------
// Ticket 27 — backup-gated migrations and explicit restore (DATA-006 to
// DATA-008, DATA-013 to DATA-017, OPS-015, UI-012, PKG-010/011). All drills run
// at the real loopback process boundary with the same XDG-isolated SQLite
// database and scripted upstreams as the rest of the suite.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn old_schema_startup_creates_gated_backup_migrates_and_reports_status() {
    let environment = TestEnvironment::new("old-schema-migration");
    let bootstrap_credential = environment.initialize();
    seed_schema_fixture(&environment, 9);
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The fixture carries the administrator row and one configured route, so
    // the migrated database and its pre-migration snapshot hold real data. The
    // binary must first create and verify a migration backup, then migrate in
    // one transaction.

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["migration"]["running_schema"], 17);
    assert_eq!(operations["migration"]["supported_schema"], 17);
    assert_eq!(operations["migration"]["migration_state"], "migrated");
    assert_eq!(operations["migration"]["migrated_from_schema"], 9);
    assert_eq!(operations["migration"]["pre_backup"]["ok"], true);
    assert_eq!(operations["migration"]["pre_backup"]["schema_version"], 9);
    assert_eq!(operations["migration"]["last_phase"], "migration");
    assert_eq!(operations["migration"]["last_result"], "ok");
    assert!(operations["migration"]["last_completed_at"].as_i64().is_some());

    // The pre-migration artifact is a verified schema-9 snapshot of the data.
    let backup_dir = environment.root.join("data/local-api-relay/backups");
    let files = backup_artifact_paths(&backup_dir);
    assert_eq!(files.len(), 1);

    // The migration is recorded in the 14-day operational event history
    // (OPS-010/OPS-017).
    let migration_events =
        events_json(&client, &base, &active_cookie, Some("migration"), 0, 50).await;
    let migration = migration_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_code"] == "migration.completed")
        .expect("migration.completed event");
    assert_eq!(migration["payload"]["migrated_from"], 9);
    assert_eq!(migration["payload"]["running_schema"], 17);
    assert!(migration["payload"]["pre_backup_ok"] == true);

    let backup_connection = rusqlite::Connection::open(&files[0]).unwrap();
    let integrity: String = backup_connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let schema: i64 = backup_connection
        .query_row("SELECT version FROM schema_metadata WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(schema, 9);
    let trigger: String = backup_connection
        .query_row("SELECT trigger FROM backup_metadata WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(trigger, "migration");
    let routes: i64 = backup_connection
        .query_row("SELECT COUNT(*) FROM model_routes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(routes, 1);

    // The live database is now on the current schema.
    let live = rusqlite::Connection::open(environment.database_path()).unwrap();
    let version: i64 = live
        .query_row("SELECT version FROM schema_metadata WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 17);
    let ops_exists: i64 = live
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'data_operations')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ops_exists, 1);
    let events_exists: i64 = live
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'operational_events')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events_exists, 1);
    let http_status_column: i64 = live
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('model_route_health')
             WHERE name = 'last_http_status'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(http_status_column, 1, "the migration must add last_http_status");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn migration_pre_backup_failure_keeps_the_old_database_and_blocks_ready() {
    let environment = TestEnvironment::new("migration-pre-backup-failure");
    environment.initialize();
    seed_schema_fixture(&environment, 8);
    let port = available_port();

    // The gate backup fails, so the migration is blocked and ready never
    // happens; the old database is preserved.
    let mut server =
        environment.start_with(port, &[("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "create")]);
    let status = wait_exit(&mut server);
    assert!(!status.success());

    let database = rusqlite::Connection::open(environment.database_path()).unwrap();
    let version: i64 = database
        .query_row("SELECT version FROM schema_metadata WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 8, "the old database must be left untouched");
    let ops_exists: i64 = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'data_operations')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ops_exists, 0);
    let artifacts = backup_artifact_paths(&environment.root.join("data/local-api-relay/backups"));
    assert!(
        artifacts.is_empty(),
        "a failed pre-backup must not leave a partial artifact"
    );
}

#[tokio::test]
async fn failed_migration_rolls_back_and_preserves_the_old_database() {
    let environment = TestEnvironment::new("failed-migration-rollback");
    environment.initialize();
    seed_schema_fixture(&environment, 8);
    let port = available_port();

    // The gate backup succeeds, then the migration chain fails: the single
    // transaction must roll back, leaving the old database untouched and the
    // process not ready (DATA-008).
    let mut server =
        environment.start_with(port, &[("LOCAL_API_RELAY_TEST_FAIL_MIGRATION", "1")]);
    let status = wait_exit(&mut server);
    assert!(!status.success());

    let database = rusqlite::Connection::open(environment.database_path()).unwrap();
    let version: i64 = database
        .query_row("SELECT version FROM schema_metadata WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 8, "the migration must roll back atomically");
    let ops_exists: i64 = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'data_operations')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ops_exists, 0, "the rolled-back DDL must not survive");

    // The pre-migration snapshot itself was created and verified before the
    // migration attempt.
    let artifacts = backup_artifact_paths(&environment.root.join("data/local-api-relay/backups"));
    assert_eq!(artifacts.len(), 1);
}

#[tokio::test]
async fn newer_schema_is_rejected_without_writing_or_downgrade() {
    let environment = TestEnvironment::new("newer-schema-rejected");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    activate_administrator(&client, &base, &bootstrap_credential).await;
    server.kill().unwrap();
    server.wait().unwrap();
    stamp_schema_version(&environment, 18);

    let mut server = environment.start(port);
    let status = wait_exit(&mut server);
    assert!(!status.success());

    let database = rusqlite::Connection::open(environment.database_path()).unwrap();
    let version: i64 = database
        .query_row("SELECT version FROM schema_metadata WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        version, 18,
        "a newer-than-supported schema must be rejected without downgrade"
    );
    let artifacts = backup_artifact_paths(&environment.root.join("data/local-api-relay/backups"));
    assert!(
        artifacts.is_empty(),
        "rejecting a newer schema must not create a pre-backup"
    );
}

#[tokio::test]
async fn corrupted_database_is_preserved_as_evidence_and_blocks_ready() {
    let environment = TestEnvironment::new("corrupted-database");
    let database_path = environment.database_path();
    fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    fs::write(&database_path, vec![0xABu8; 8192]).unwrap();
    let before = fs::read(&database_path).unwrap();

    let mut server = environment.start(available_port());
    let status = wait_exit(&mut server);
    assert!(!status.success());

    let after = fs::read(&database_path).unwrap();
    assert_eq!(
        before, after,
        "the corrupted database file must be preserved byte-identical as evidence"
    );
}

#[tokio::test]
async fn empty_existing_database_file_is_not_silently_initialized() {
    let environment = TestEnvironment::new("empty-database-file");
    let database_path = environment.database_path();
    fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    fs::write(&database_path, b"").unwrap();

    let mut server = environment.start(available_port());
    let status = wait_exit(&mut server);
    assert!(!status.success());

    assert_eq!(
        fs::metadata(&database_path).unwrap().len(),
        0,
        "an existing empty file must be preserved, never silently initialized"
    );
}

#[tokio::test]
async fn explicit_restore_preserves_current_database_and_rechecks_restored_routes() {
    let environment = TestEnvironment::new("explicit-restore");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // A delayed persistent upstream makes the post-restore Checking window
    // observable before the re-probe completes (OPS-015).
    let (upstream_base_url, upstream_requests, _upstream_worker) =
        delayed_persistent_chat_upstream(Duration::from_millis(600));
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    await_route_health(&client, &base, &active_cookie, &route_id, "available").await;

    // A successful call records usage that the backup must carry.
    let relay_secret = create_relay_secret(&client, &base, &active_cookie, &route_id, "Restore client")
        .await;
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
        .json(&json!({
            "model": "gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    // Snapshot the current state, then diverge from it.
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let backup_name = list_body["data"][0]["name"].as_str().unwrap().to_owned();

    let second = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Post-backup provider",
            "base_url": "https://post-backup.invalid/v1",
            "api_key": "post-backup-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    // Restore: the switch is explicit, synchronous, and returns the outcome.
    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": backup_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    let restored_body: serde_json::Value = restored.json().await.unwrap();
    assert_eq!(restored_body["restored_from"], backup_name);
    assert_eq!(restored_body["schema_version"], 17);
    assert_eq!(restored_body["routes_reset_to_checking"], 1);
    assert!(restored_body["pre_restore_backup"].as_str().is_some());

    // The restored configuration is back and the restored health is not shown
    // as current: the route is Checking until its re-probe completes.
    let operations = operations_json(&client, &base, &active_cookie).await;
    let providers: Vec<&str> = operations["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(providers, vec!["chat_completions route 1x"]);
    assert_eq!(operations["routes"].as_array().unwrap()[0]["health"], "checking");
    // The restored health history is preserved but never shown as current
    // health: the row stays Checking until its re-probe completes, while the
    // safe HTTP status recorded before the backup remains visible (OPS-015).
    assert_eq!(
        operations["routes"].as_array().unwrap()[0]["last_http_status"],
        200,
        "the restore reset keeps the historical safe HTTP status"
    );
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "ok");
    assert_eq!(operations["migration"]["restore_source"], backup_name);

    // The restore is recorded in the 14-day operational event history
    // (OPS-010/OPS-017), carrying only safe metadata.
    let migration_events =
        events_json(&client, &base, &active_cookie, Some("migration"), 0, 50).await;
    let restore = migration_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_code"] == "restore.completed")
        .expect("restore.completed event");
    assert_eq!(restore["payload"]["source"], backup_name);
    assert_eq!(restore["payload"]["running_schema"], 17);
    assert!(restore["payload"]["pre_restore_backup"].as_str().is_some());

    // The re-probe rebuilds health and the route re-enters service.
    await_route_health(&client, &base, &active_cookie, &route_id, "available").await;

    // The pre-restore snapshot of the current database exists (trigger restore).
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let triggers: Vec<&str> = list_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["trigger"].as_str().unwrap())
        .collect();
    assert!(triggers.contains(&"restore"), "pre-restore backup missing: {triggers:?}");

    // Usage recorded before the snapshot survived the restore.
    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert!(usage["total"].as_u64().unwrap() >= 1);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restore_of_an_older_schema_candidate_upgrades_it_under_the_same_contract() {
    let environment = TestEnvironment::new("restore-old-schema-candidate");
    let bootstrap_credential = environment.initialize();
    seed_schema_fixture(&environment, 8);
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The fixture's startup migration creates the schema-8 pre-backup that
    // becomes the older-schema restore candidate.

    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let old_candidate = list_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["schema_version"] == 8)
        .expect("migration pre-backup with schema 8")
        .clone();
    let old_name = old_candidate["name"].as_str().unwrap().to_owned();

    // Diverge from the old configuration.
    let second = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Post-migration provider",
            "base_url": "https://post-migration.invalid/v1",
            "api_key": "post-migration-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    // Restoring the schema-8 candidate upgrades it under the same forward-only
    // contract and then switches it in.
    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": old_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    let restored_body: serde_json::Value = restored.json().await.unwrap();
    assert_eq!(restored_body["candidate_schema"], 8);
    assert_eq!(restored_body["schema_version"], 17);

    // The restored candidate predates any login, so the session that
    // authenticated the restore is gone; its administrator row is the
    // untouched bootstrap state, so the bootstrap credential logs in again.
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    let operations = operations_json(&client, &base, &active_cookie).await;
    let providers: Vec<&str> = operations["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(providers, vec!["chat_completions route 1x"]);
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "ok");
    assert_eq!(operations["migration"]["migration_state"], "migrated");
    assert_eq!(operations["migration"]["migrated_from_schema"], 8);

    // The artifact itself is never modified by a restore: restoring the same
    // older-schema candidate again re-verifies and re-migrates it from v8.
    let again = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": old_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    let again_body: serde_json::Value = again.json().await.unwrap();
    assert_eq!(
        again_body["candidate_schema"], 8,
        "the restore candidate must stay pristine after the first restore"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restore_rejects_a_newer_schema_candidate_and_keeps_the_current_database() {
    let environment = TestEnvironment::new("restore-newer-schema-candidate");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let backup_name = list_body["data"][0]["name"].as_str().unwrap().to_owned();

    // Stamp the artifact as a newer-than-supported database.
    let backup_dir = environment.root.join("data/local-api-relay/backups");
    let files = backup_artifact_paths(&backup_dir);
    assert_eq!(files.len(), 1);
    let artifact = rusqlite::Connection::open(&files[0]).unwrap();
    artifact
        .execute("UPDATE schema_metadata SET version = 18 WHERE id = 1", [])
        .unwrap();
    drop(artifact);

    let second = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Keep-me provider",
            "base_url": "https://keep-me.invalid/v1",
            "api_key": "keep-me-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": backup_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let operations = operations_json(&client, &base, &active_cookie).await;
    let providers: Vec<&str> = operations["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(
        providers,
        vec!["Keep-me provider"],
        "a rejected restore must keep the current database selected"
    );
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "failed");
    assert_eq!(operations["migration"]["last_failed_stage"], "verify_candidate");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restore_rejects_a_corrupted_candidate_and_keeps_the_current_database() {
    let environment = TestEnvironment::new("restore-corrupted-candidate");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let backup_name = list_body["data"][0]["name"].as_str().unwrap().to_owned();

    // Corrupt the artifact so it still lists but fails isolated verification:
    // its application identity no longer matches (DATA-014).
    let backup_dir = environment.root.join("data/local-api-relay/backups");
    let files = backup_artifact_paths(&backup_dir);
    let artifact = rusqlite::Connection::open(&files[0]).unwrap();
    artifact
        .execute(
            "UPDATE backup_metadata SET application = 'other-application' WHERE id = 1",
            [],
        )
        .unwrap();
    drop(artifact);

    let second = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Keep-me provider",
            "base_url": "https://keep-me.invalid/v1",
            "api_key": "keep-me-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": backup_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(
        operations["providers"].as_array().unwrap().len(),
        1,
        "a rejected restore must keep the current database selected"
    );
    assert_eq!(operations["migration"]["last_result"], "failed");
    assert_eq!(operations["migration"]["last_failed_stage"], "verify_candidate");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restore_pre_backup_failure_keeps_the_current_database() {
    let environment = TestEnvironment::new("restore-pre-backup-failure");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
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
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let backup_name = list_body["data"][0]["name"].as_str().unwrap().to_owned();

    // Restart with the create-stage failure so the pre-restore snapshot of the
    // current database cannot be made (DATA-015).
    server.kill().unwrap();
    server.wait().unwrap();
    let mut server =
        environment.start_with(port, &[("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "create")]);
    wait_ready(&client, port).await;

    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": backup_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "failed");
    assert_eq!(operations["migration"]["last_failed_stage"], "backup_current");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restore_candidate_migration_failure_keeps_the_current_database() {
    let environment = TestEnvironment::new("restore-migrate-candidate-failure");
    let bootstrap_credential = environment.initialize();
    seed_schema_fixture(&environment, 8);
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // The fixture's startup migration builds the older-schema candidate the
    // same way the migration drill does.
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let old_name = list_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["schema_version"] == 8)
        .expect("migration pre-backup with schema 8")["name"]
        .as_str()
        .unwrap()
        .to_owned();

    let second = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Keep-me provider",
            "base_url": "https://keep-me.invalid/v1",
            "api_key": "keep-me-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    // Restart with the migration-failure injection. The live database is
    // already current, so only the candidate's forward migration fails.
    server.kill().unwrap();
    server.wait().unwrap();
    let mut server =
        environment.start_with(port, &[("LOCAL_API_RELAY_TEST_FAIL_MIGRATION", "1")]);
    wait_ready(&client, port).await;

    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": old_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(
        operations["providers"].as_array().unwrap().len(),
        2,
        "a candidate migration failure must keep the current database selected"
    );
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "failed");
    assert_eq!(operations["migration"]["last_failed_stage"], "migrate_candidate");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn no_portable_import_export_or_cross_machine_migration_surface() {
    // DATA-017: a capability-list check that the admin API, the CLI, and the
    // shipped web surface expose no portable import/export or cross-machine
    // migration entry. Local managed backup and restore remain the only
    // transfer affordances (DATA-009..016).
    let environment = TestEnvironment::new("no-portable-import-export");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // CLI capability list: exactly the documented five commands, and no
    // portable import/export/transfer subcommand. (Forward schema migration
    // prose in the help — "never migrates" — is the allowed DATA-006..008
    // behavior, not a portable transfer capability.)
    let help = environment.command().arg("--help").output().unwrap();
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout);
    for command in ["init-admin", "serve", "check", "backup", "restore"] {
        assert!(
            help_text.contains(command),
            "the CLI help must list the {command} command: {help_text}"
        );
    }
    for forbidden in ["import", "export", "transfer"] {
        assert!(
            !help_text.contains(forbidden),
            "the CLI must not offer a {forbidden} subcommand: {help_text}"
        );
    }

    // Admin API: portable-transfer paths are not routed (404).
    for path in [
        "/admin/import",
        "/admin/export",
        "/admin/transfer",
        "/admin/config/import",
        "/admin/config/export",
        "/admin/database/import",
        "/admin/database/export",
        "/admin/backups/import",
        "/admin/backups/export",
        "/admin/migrate",
    ] {
        let response = client
            .post(format!("{base}{path}"))
            .header(header::COOKIE, &active_cookie)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "portable-transfer admin path {path} must not be routed"
        );
    }

    // Web surface: the shipped assets reference no portable-transfer admin
    // path and carry no Import/Export/Transfer affordance labels.
    let index = client
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let script = client
        .get(format!("{base}/assets/app.js"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let styles = client
        .get(format!("{base}/assets/app.css"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for asset in [&index, &script, &styles] {
        for forbidden in [
            "/admin/import",
            "/admin/export",
            "/admin/transfer",
            "/admin/config/import",
            "/admin/config/export",
            "/admin/migrate",
        ] {
            assert!(
                !asset.contains(forbidden),
                "the web surface must not reference {forbidden}"
            );
        }
        for label in ["Import", "Export", "Transfer"] {
            assert!(
                !asset.contains(label),
                "the web surface must not render a {label} affordance"
            );
        }
    }

    server.kill().unwrap();
    server.wait().unwrap();
}

// Ticket 46 — restore stage progress (UI-012/OPS-015): the data security panel
// shows the in-flight restore stages instead of a static label, and a failed
// restore points at the exact stage with an actionable reason while the
// surface stays operational.

#[tokio::test]
async fn restore_reports_in_flight_stage_progress_at_the_process_boundary() {
    let environment = TestEnvironment::new("restore-stage-progress");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    // Pause the in-flight restore after each reported stage so the verify →
    // switch → recheck transitions stay observable before the synchronous
    // switch completes (UI-012/OPS-015).
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_RESTORE_STAGE_PAUSE_MS", "400")],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // One route so the post-restore re-probe has a configuration to reset to
    // Checking; the second upstream response answers that re-probe.
    let (upstream_base_url, _probe_requests, upstream_worker) =
        scripted_chat_upstream_with_catalog_override(
            vec![complete_chat_response(), complete_chat_response()],
            "unrelated-model",
        );
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "progress-model",
    )
    .await;

    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let backup_name = list_body["data"][0]["name"].as_str().unwrap().to_owned();

    // Idle while no restore has run in this process.
    let progress = get_with_cookie(
        &client,
        format!("{base}/admin/restore/progress"),
        &active_cookie,
    )
    .await;
    let progress_body: serde_json::Value = progress.json().await.unwrap();
    assert_eq!(progress_body["state"], "idle");

    // Run the restore while watching the in-flight progress concurrently: the
    // panel must observe the `restoring` wire state (current stage + completed
    // stages + candidate) while the synchronous restore is still running
    // (UI-012).
    let restore_task = {
        let client = client.clone();
        let base = base.clone();
        let active_cookie = active_cookie.clone();
        let backup_name = backup_name.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/admin/restore"))
                .header(header::COOKIE, &active_cookie)
                .json(&json!({ "name": backup_name }))
                .send()
                .await
                .unwrap()
        })
    };
    let mut restoring_seen = false;
    let mut checked_unauth = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !restoring_seen {
        let progress = get_with_cookie(
            &client,
            format!("{base}/admin/restore/progress"),
            &active_cookie,
        )
        .await;
        let progress_body: serde_json::Value = progress.json().await.unwrap();
        if progress_body["state"] == "restoring" {
            let stage = progress_body["stage"].as_str().expect("restoring stage");
            let stages = progress_body["stages"].as_array().expect("restoring stages");
            assert_eq!(
                stages.last().and_then(|value| value.as_str()),
                Some(stage),
                "the current stage must be the last completed stage"
            );
            assert_eq!(progress_body["candidate"], backup_name);
            restoring_seen = true;
            // The progress is scoped to the administrator session that started
            // the restore: a request without that session is rejected even
            // while the restore is in flight.
            if !checked_unauth {
                let unauth = client
                    .get(format!("{base}/admin/restore/progress"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
                checked_unauth = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        restoring_seen,
        "the panel must observe the in-flight restoring state during the restore"
    );

    let restored = restore_task.await.unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    let restored_body: serde_json::Value = restored.json().await.unwrap();
    assert_eq!(restored_body["restored_from"], backup_name);

    // OPS-015 "current or recent stage": right after the restore, the progress
    // surface still carries the full stage sequence it ran through — verify →
    // switch → recheck — plus the candidate, scoped to the initiating session
    // (a request without that session is rejected).
    let progress = get_with_cookie(
        &client,
        format!("{base}/admin/restore/progress"),
        &active_cookie,
    )
    .await;
    let progress_body: serde_json::Value = progress.json().await.unwrap();
    assert_eq!(
        progress_body["state"], "recent",
        "the completed restore must stay readable as a recent stage sequence"
    );
    let stages: Vec<&str> = progress_body["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| stage.as_str().unwrap())
        .collect();
    assert_eq!(
        stages, vec!["verify", "switch", "recheck"],
        "the restore must report its coarse stages in order"
    );
    assert_eq!(
        progress_body["candidate"],
        backup_name,
        "the progress must identify the candidate backup"
    );
    let unauth = client
        .get(format!("{base}/admin/restore/progress"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauth.status(),
        StatusCode::UNAUTHORIZED,
        "the progress is scoped to the administrator session that started the restore"
    );

    // The re-probe rebuilds health, consuming the second upstream response.
    await_route_health(&client, &base, &active_cookie, &route_id, "available").await;

    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "ok");
    assert_eq!(operations["migration"]["restore_source"], backup_name);

    upstream_worker.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn restore_failure_keeps_the_surface_operational_with_stage_and_reason() {
    let environment = TestEnvironment::new("restore-failure-keeps-operational");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
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
    let list = get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await;
    let list_body: serde_json::Value = list.json().await.unwrap();
    let backup_name = list_body["data"][0]["name"].as_str().unwrap().to_owned();

    // Restart with the pre-restore snapshot failure so the restore fails at a
    // known stage while the current database stays selected (DATA-015).
    server.kill().unwrap();
    server.wait().unwrap();
    let mut server = environment.start_with(
        port,
        &[("LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE", "create")],
    );
    wait_ready(&client, port).await;

    let restored = client
        .post(format!("{base}/admin/restore"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({ "name": backup_name }))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // No restore is in flight after the failure: the progress surface shows the
    // recent attempt with the stages it completed before failing (the failure
    // stage itself lives in the durable OPS-015 status below).
    let progress = get_with_cookie(
        &client,
        format!("{base}/admin/restore/progress"),
        &active_cookie,
    )
    .await;
    let progress_body: serde_json::Value = progress.json().await.unwrap();
    assert_eq!(progress_body["state"], "recent");
    let stages: Vec<&str> = progress_body["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| stage.as_str().unwrap())
        .collect();
    assert_eq!(
        stages, vec!["verify"],
        "the failed restore must have completed the verify stage before failing"
    );

    // OPS-015: the durable migration/restore status names the exact failed
    // stage and an actionable reason the panel renders.
    let operations = operations_json(&client, &base, &active_cookie).await;
    assert_eq!(operations["migration"]["last_phase"], "restore");
    assert_eq!(operations["migration"]["last_result"], "failed");
    assert_eq!(operations["migration"]["last_failed_stage"], "backup_current");
    assert!(
        operations["migration"]["last_failed_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the failed restore must carry an actionable reason"
    );

    // The surface stays operational: the current database remains selected and
    // the operator can keep configuring after the failed restore.
    let second = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": "Post-failure provider",
            "base_url": "https://post-failure.invalid/v1",
            "api_key": "post-failure-key"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    // UI-012: the panel's user-visible rendering of the in-flight stage
    // progress and the failure guidance (exact stage, actionable reason,
    // continue-operating affordance) is asserted by the real-browser test
    // `browser_failed_restore_reports_stage_and_returns_to_operations`.

    server.kill().unwrap();
    server.wait().unwrap();
}

// ---------------------------------------------------------------------------
// Ticket 28 — operational diagnostics, retention, and the privacy boundary
// (SEC-008..SEC-009, OPS-009..OPS-021, UI-001/UI-003/UI-010..UI-013)
// ---------------------------------------------------------------------------

async fn events_json(
    client: &Client,
    base: &str,
    cookie: &str,
    section: Option<&str>,
    page: i64,
    page_size: i64,
) -> serde_json::Value {
    let section = section
        .map(|section| format!("&section={section}"))
        .unwrap_or_default();
    get_with_cookie(
        client,
        format!("{base}/admin/operations/events?page={page}&page_size={page_size}{section}"),
        cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap()
}

/// The managed rotating log files under the XDG state directory.
fn managed_log_files(environment: &TestEnvironment) -> Vec<PathBuf> {
    let directory = environment.root.join("state/local-api-relay/logs");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("could not list managed logs: {error}"),
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("relay.log"))
        })
        .collect();
    files.sort();
    files
}

/// The calendar dates embedded in the rotated log file names.
fn log_file_dates(environment: &TestEnvironment) -> Vec<String> {
    managed_log_files(environment)
        .iter()
        .filter_map(|path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            name.strip_prefix("relay.log.").map(|date| date[..10].to_owned())
        })
        .collect()
}

/// UTC calendar day `YYYY-MM-DD` for an epoch-second instant.
fn civil_day(epoch_seconds: i64) -> String {
    let z = epoch_seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Asserts every non-empty log line is a JSON object whose top-level keys are
/// exactly the metadata allowlist (OPS-018/OPS-020).
fn assert_structured_log_lines(contents: &str) {
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("log line is not valid JSON: {line}: {error}"));
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("log line is not an object: {line}"));
        for key in object.keys() {
            assert!(
                ["ts", "severity", "event", "version", "section", "correlation", "payload"]
                    .contains(&key.as_str()),
                "log line carries a non-allowlisted key: {key} in {line}"
            );
        }
        assert!(
            ["info", "warning", "error"].contains(&object["severity"].as_str().unwrap()),
            "log line has an invalid severity: {line}"
        );
        assert!(!object["event"].as_str().unwrap().is_empty());
        assert!(!object["version"].as_str().unwrap().is_empty());
    }
}

/// Dumps every text value in the live database except the given (table,
/// column) pairs, so a canary scan can exclude the fields the persistence
/// contract deliberately stores (upstream API keys and Base URLs, SEC-007/
/// CFG-002) while proving everything else stays within the metadata allowlist
/// (OPS-020).
fn database_dump(environment: &TestEnvironment, exclude: &[(&str, &str)]) -> String {
    let connection = rusqlite::Connection::open(environment.database_path()).unwrap();
    let mut dump = String::new();
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .unwrap();
    let tables: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for table in tables {
        let mut columns: Vec<String> = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        columns.retain(|column| !exclude.contains(&(table.as_str(), column.as_str())));
        if columns.is_empty() {
            continue;
        }
        let sql = format!(
            "SELECT {} FROM {table}",
            columns
                .iter()
                .map(|column| format!("CAST({column} AS TEXT)"))
                .collect::<Vec<_>>()
                .join(" || '\n' || ")
        );
        let mut statement = connection.prepare(&sql).unwrap();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let text = match row.get_ref(0).unwrap() {
                rusqlite::types::ValueRef::Null => String::new(),
                rusqlite::types::ValueRef::Integer(value) => value.to_string(),
                rusqlite::types::ValueRef::Real(value) => value.to_string(),
                rusqlite::types::ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
                rusqlite::types::ValueRef::Blob(value) => String::from_utf8_lossy(value).into_owned(),
            };
            dump.push_str(&text);
            dump.push('\n');
        }
    }
    dump
}

#[tokio::test]
async fn operational_events_drill_down_records_abnormal_states() {
    let environment = TestEnvironment::new("events-drill-down");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    // The one-shot call-record write failure lets the Storage and usage
    // sections record degradation and recovery in the same process (OPS-012).
    let mut server = environment.start_with(
        port,
        &[(
            "LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE_ONCE",
            "call_records",
        )],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 30, 40),
        chat_response_with_usage(200, 0, 50),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, &route_id, "Events client").await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap(); // creation probe

    // A manual backup records a backup event with safe metadata only.
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // Call 1: the call-record write fails once -> Storage Degraded and the
    // usage gap opens. Call 2: the same category re-persists -> Storage
    // recovers and the gap closes (OPS-012/OPS-016).
    chat_call(&client, &base, &relay_secret).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    chat_call(&client, &base, &relay_secret).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();

    // Every abnormal state can enter its 14-day operational event history
    // (OPS-010). The events carry only the metadata allowlist (OPS-018).
    let storage_events = events_json(&client, &base, &active_cookie, Some("storage"), 0, 50).await;
    let storage_codes: Vec<&str> = storage_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event_code"].as_str().unwrap())
        .collect();
    assert!(
        storage_codes.contains(&"storage.degraded"),
        "storage history must record degradation: {storage_codes:?}"
    );
    assert!(
        storage_codes.contains(&"storage.recovered"),
        "storage history must record recovery: {storage_codes:?}"
    );
    for event in storage_events["events"].as_array().unwrap() {
        assert_eq!(event["section"], "storage");
        assert!(event["occurred_at_ms"].as_i64().unwrap() > 0);
        assert!(
            ["info", "warning", "error"].contains(&event["severity"].as_str().unwrap())
        );
        assert!(!event["version"].as_str().unwrap().is_empty());
        assert!(event["payload"].is_object());
    }

    let usage_events = events_json(&client, &base, &active_cookie, Some("usage"), 0, 50).await;
    let usage_codes: Vec<&str> = usage_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event_code"].as_str().unwrap())
        .collect();
    assert!(
        usage_codes.contains(&"usage.gap_opened"),
        "usage history must record the opened gap: {usage_codes:?}"
    );
    assert!(
        usage_codes.contains(&"usage.gap_closed"),
        "usage history must record the closed gap: {usage_codes:?}"
    );

    let routes_events = events_json(&client, &base, &active_cookie, Some("routes"), 0, 50).await;
    assert!(
        routes_events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event_code"] == "routes.check"
                && event["payload"]["result"] == "available"),
        "route checks must appear in the routes history"
    );

    let backups_events =
        events_json(&client, &base, &active_cookie, Some("backups"), 0, 50).await;
    let backup = backups_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_code"] == "backup.created")
        .expect("backup.created event");
    assert_eq!(backup["severity"], "info");
    assert!(backup["payload"]["name"].as_str().unwrap().ends_with(".sqlite3"));
    assert!(backup["payload"]["size"].as_i64().unwrap() > 0);

    let process_events =
        events_json(&client, &base, &active_cookie, Some("process"), 0, 50).await;
    let process_codes: Vec<&str> = process_events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event_code"].as_str().unwrap())
        .collect();
    assert!(process_codes.contains(&"process.start"));
    assert!(process_codes.contains(&"process.ready"));

    // The unfiltered history is the union of all sections, and an unknown
    // section is rejected rather than probing an arbitrary filter.
    let all = events_json(&client, &base, &active_cookie, None, 0, 50).await;
    assert!(all["total"].as_i64().unwrap() >= 6);
    let unknown = get_with_cookie(
        &client,
        format!("{base}/admin/operations/events?section=made_up"),
        &active_cookie,
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Pagination preserves the per-section total.
    let paged = events_json(&client, &base, &active_cookie, Some("process"), 0, 1).await;
    assert_eq!(paged["events"].as_array().unwrap().len(), 1);
    assert!(paged["total"].as_i64().unwrap() >= 2);

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn ordinary_successful_calls_emit_no_call_events_and_abnormal_calls_carry_correlation() {
    let environment = TestEnvironment::new("events-call-silence");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let mut server = environment.start(port);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;
    set_quarantine_threshold(&client, &base, &active_cookie, 1).await;

    // Route A passes its probe then fails the call with an attributable 500;
    // Route B always succeeds.
    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&complete_chat_response()),
        http_status_response(
            500,
            "Internal Server Error",
            r#"{"error":{"message":"boom"}}"#,
        ),
    ]);
    let (succeeding_url, succeeding_requests, succeeding_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(10, 0, 5),
        chat_response_with_usage(20, 0, 6),
        chat_response_with_usage(30, 0, 7),
    ]);
    let failing_route = configure_route(
        &client,
        &base,
        &active_cookie,
        failing_url,
        "chat_completions",
        "canary-failing-model",
        "gpt-5.6-sol",
        "1",
    )
    .await;
    let succeeding_route = configure_route(
        &client,
        &base,
        &active_cookie,
        succeeding_url,
        "chat_completions",
        "canary-succeeding-model",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "Chain client",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    // Call 1 falls back: A quarantines with the 500, B succeeds. The one call
    // emits a single `call.fallback` event with this call's correlation id and
    // allowlisted metadata (OPS-003/OPS-018).
    chat_call(&client, &base, &relay_secret).await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();

    let calls = events_json(&client, &base, &active_cookie, Some("calls"), 0, 50).await;
    assert_eq!(calls["total"], 1, "fallback call history must have one event");
    let event = &calls["events"][0];
    assert_eq!(event["event_code"], "call.fallback");
    assert_eq!(event["severity"], "warning");
    let correlation = event["correlation_id"].as_str().unwrap();
    assert!(!correlation.is_empty());
    assert_eq!(event["payload"]["published_model_name"], "gpt-5.6-sol");
    assert_eq!(event["payload"]["protocol"], "chat_completions");
    assert_eq!(event["payload"]["attempts"], 2);
    assert_eq!(event["payload"]["fallback"], true);
    assert_eq!(event["payload"]["failure_category"], "upstream_http_5xx");

    let routes = events_json(&client, &base, &active_cookie, Some("routes"), 0, 50).await;
    let quarantine = routes["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_code"] == "routes.quarantined")
        .expect("routes.quarantined event");
    assert_eq!(quarantine["payload"]["route_id"], failing_route);
    assert_eq!(quarantine["payload"]["failure_category"], "upstream_http_5xx");

    // Calls 2 and 3 are ordinary successful single-attempt calls: no extra
    // call events (OPS-018).
    chat_call(&client, &base, &relay_secret).await;
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    chat_call(&client, &base, &relay_secret).await;
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    failing_worker.join().unwrap();
    succeeding_worker.join().unwrap();

    let calls = events_json(&client, &base, &active_cookie, Some("calls"), 0, 50).await;
    assert_eq!(
        calls["total"], 1,
        "ordinary successful calls must not emit call events"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn operational_event_retention_prunes_history_but_keeps_daily_aggregates() {
    let environment = TestEnvironment::new("events-retention");
    let bootstrap_credential = environment.initialize();
    let clock = 1_800_000_000_i64;
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &clock.to_string()),
            ("LOCAL_API_RELAY_TEST_RETENTION_TICK_MS", "200"),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    let (upstream_base_url, upstream_requests, upstream_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        chat_response_with_usage(100, 10, 50),
    ]);
    let route_id = configure_relay_route(
        &client,
        &base,
        &active_cookie,
        upstream_base_url,
        "chat_completions",
        "scripted-upstream-model",
    )
    .await;
    let relay_secret =
        create_relay_secret(&client, &base, &active_cookie, &route_id, "Retention client").await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    chat_call(&client, &base, &relay_secret).await;
    upstream_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    upstream_worker.join().unwrap();
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // Under the fixed clock the history and the per-call record exist.
    let backups = events_json(&client, &base, &active_cookie, Some("backups"), 0, 50).await;
    assert_eq!(backups["total"], 1);
    let usage = calls_usage_json(&client, &base, &active_cookie).await;
    assert_eq!(usage["total"], 1);
    assert_eq!(usage["totals"]["input_tokens"], 100);

    server.kill().unwrap();
    server.wait().unwrap();

    // Restart 15 days later: the 14-day retention prunes per-call records and
    // operational events while the permanent daily aggregate survives (OPS-009).
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            (
                "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
                &(clock + 15 * 86_400).to_string(),
            ),
            ("LOCAL_API_RELAY_TEST_RETENTION_TICK_MS", "200"),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    tokio::time::sleep(Duration::from_secs(1)).await; // let the retention tick run

    let backups = events_json(&client, &base, &active_cookie, Some("backups"), 0, 50).await;
    assert_eq!(backups["total"], 0, "old backup events must be pruned after 14 days");
    let calls = calls_usage_window(&client, &base, &active_cookie, "all").await;
    assert_eq!(calls["total"], 0, "per-call records must be pruned after 14 days");
    assert_eq!(
        calls["totals"]["input_tokens"], 100,
        "the permanent daily aggregate survives the diagnostic prune"
    );
    let process_events =
        events_json(&client, &base, &active_cookie, Some("process"), 0, 50).await;
    assert!(
        process_events["total"].as_i64().unwrap() >= 2,
        "the current process events remain after the prune"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn structured_logs_rotate_on_day_and_size_and_enforce_retention_caps() {
    let environment = TestEnvironment::new("log-rotation");
    let bootstrap_credential = environment.initialize();
    let clock = 1_800_000_000_i64;
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &clock.to_string()),
            ("LOCAL_API_RELAY_TEST_LOG_SIZE_LIMIT", "1024"),
            ("LOCAL_API_RELAY_TEST_LOG_SIZE_CAP", "4096"),
        ],
    );
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // Generate enough events to force several size rotations: every manual
    // backup records one `backup.created` event.
    for _ in 0..25 {
        let created = client
            .post(format!("{base}/admin/backups"))
            .header(header::COOKIE, &active_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
    }

    // Size rotation at the 1 KiB test limit, total cap at 4 KiB, oldest first.
    let files = managed_log_files(&environment);
    assert!(
        files.len() >= 3,
        "size rotation must leave several managed files, got {}",
        files.len()
    );
    let mut total = 0_u64;
    for path in &files {
        let size = fs::metadata(path).unwrap().len();
        total += size;
        assert!(
            size <= 1024,
            "a managed log file must not exceed the rotation limit: {} has {size}",
            path.display()
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o077,
            0,
            "managed logs must be private to the current user"
        );
        let contents = fs::read_to_string(path).unwrap();
        assert_structured_log_lines(&contents);
    }
    assert!(
        total <= 4096,
        "the managed log set must stay under the total cap, got {total}"
    );
    let day_one = civil_day(clock);
    assert!(
        log_file_dates(&environment)
            .iter()
            .all(|date| date == &day_one),
        "the first day's rotations must be dated on day one"
    );

    // Day-boundary rotation: restart on the next calendar day. The first event
    // must land in a fresh active file for the new day, not in yesterday's
    // content (OPS-019).
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            ("LOCAL_API_RELAY_TEST_CLOCK_EPOCH", &(clock + 86_400).to_string()),
            ("LOCAL_API_RELAY_TEST_LOG_SIZE_LIMIT", "1024"),
            ("LOCAL_API_RELAY_TEST_LOG_SIZE_CAP", "4096"),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let active = environment.root.join("state/local-api-relay/logs/relay.log");
    let active_contents = fs::read_to_string(&active).unwrap();
    let day_two = civil_day(clock + 86_400);
    let active_ts_values: Vec<i64> = active_contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["ts"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert!(
        !active_ts_values.is_empty()
            && active_ts_values
                .iter()
                .all(|ts| civil_day(*ts / 1000) == day_two),
        "the active log must rotate at the day boundary onto day {day_two}"
    );

    // 14-day retention: restart sixteen days after day one; every rotated file
    // from day one and day two is older than the retention window and must be
    // deleted (OPS-019).
    server.kill().unwrap();
    server.wait().unwrap();
    let port = available_port();
    let mut server = environment.start_with(
        port,
        &[
            (
                "LOCAL_API_RELAY_TEST_CLOCK_EPOCH",
                &(clock + 16 * 86_400).to_string(),
            ),
            ("LOCAL_API_RELAY_TEST_LOG_SIZE_LIMIT", "1024"),
            ("LOCAL_API_RELAY_TEST_LOG_SIZE_CAP", "4096"),
        ],
    );
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let dates = log_file_dates(&environment);
    assert!(
        !dates.iter().any(|date| date == &day_one),
        "day-one rotated logs must be pruned after 14 days: {dates:?}"
    );
    assert!(
        !dates.iter().any(|date| date == &day_two),
        "day-two rotated logs must be pruned after 14 days: {dates:?}"
    );
    let total: u64 = managed_log_files(&environment)
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum();
    assert!(total <= 4096, "the total cap holds after retention, got {total}");

    server.kill().unwrap();
    server.wait().unwrap();
}

#[tokio::test]
async fn canary_fields_never_leak_into_events_logs_pages_or_database() {
    let environment = TestEnvironment::new("ops-canary");
    let bootstrap_credential = environment.initialize();
    let port = available_port();
    let stderr_path = environment.root.join("stderr.log");
    let mut server = environment.start_with_stderr_file(port, &[], &stderr_path);
    let client = Client::new();
    wait_ready(&client, port).await;
    let base = format!("http://127.0.0.1:{port}");
    let active_cookie = activate_administrator(&client, &base, &bootstrap_credential).await;

    // Unique canary values planted into every forbidden category: request
    // bodies, response content, upstream error bodies, headers, queries,
    // upstream secrets, and the complete Base URL.
    let prompt_canary = "Canary prompt 9f3a — hide me";
    let tool_canary = "canary_tool_argument_9f3a";
    let response_canary = "Canary response 9f3a — hide me";
    let error_canary = "canary upstream error body 9f3a";
    let header_canary = "canary-header-value-9f3a";
    let query_canary = "canary=query-9f3a";
    let upstream_key_canary = "sk-canary-upstream-key-9f3a";
    let provider_name_canary = "Canary Provider 9f3a";
    let upstream_model_canary = "canary-upstream-model-9f3a";

    // Upstream A passes its probe, then returns a canary-tagged success, then
    // an attributable 500 whose body is the canary error. Upstream B always
    // succeeds, so the second call falls back and succeeds.
    let canary_success = format!(
        r#"{{"id":"chatcmpl-canary","object":"chat.completion","created":1,"model":"{upstream_model_canary}","choices":[{{"index":0,"message":{{"role":"assistant","content":"{response_canary}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{{"cached_tokens":2}}}}}}"#
    );
    let (failing_url, failing_requests, failing_worker) = scripted_http_upstream(vec![
        http_json_response(&canary_success),
        http_json_response(&canary_success),
        http_status_response(
            500,
            "Internal Server Error",
            &format!(r#"{{"error":{{"message":"{error_canary}"}}}}"#),
        ),
    ]);
    let (succeeding_url, succeeding_requests, succeeding_worker) = scripted_chat_upstream(vec![
        complete_chat_response(),
        complete_chat_response(),
    ]);
    // The failing provider carries the canary display name, Base URL, and API
    // key so the privacy scan can prove none of them leak past their
    // deliberately stored location (SEC-007/CFG-002/OPS-020).
    let failing_provider = client
        .post(format!("{base}/admin/providers"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "display_name": provider_name_canary,
            "base_url": failing_url,
            "api_key": upstream_key_canary
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
    let failing_route = client
        .post(format!("{base}/admin/model-routes"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "published_model_id": "gpt-5.6-sol",
            "provider_id": failing_provider,
            "upstream_model_name": upstream_model_canary,
            "protocol": "chat_completions",
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
        .to_owned();
    let succeeding_route = configure_route(
        &client,
        &base,
        &active_cookie,
        succeeding_url,
        "chat_completions",
        "succeeding-model-9f3a",
        "gpt-5.6-sol",
        "2",
    )
    .await;
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let base_url_canary = failing_url;

    let relay_secret = client
        .post(format!("{base}/admin/relay-access-keys"))
        .header(header::COOKIE, &active_cookie)
        .json(&json!({
            "label": "canary relay key 9f3a",
            "model_route_ids": [failing_route, succeeding_route]
        }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();

    // Call 1: a successful call whose request and response carry canary
    // content. Call 2: an attributable 500 on A falls back to B, so the call
    // history and events must carry only the normalized category.
    let send_canary_call = || {
        client
            .post(format!("{base}/v1/chat/completions?{query_canary}"))
            .header(header::AUTHORIZATION, format!("Bearer {relay_secret}"))
            .header("X-Canary-Header", header_canary)
            .json(&json!({
                "model": "gpt-5.6-sol",
                "messages": [{ "role": "user", "content": prompt_canary }],
                "tools": [{ "type": "function", "function": { "name": "f", "parameters": { "arg": tool_canary } } }]
            }))
            .send()
    };
    let response = send_canary_call().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    let response = send_canary_call().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    failing_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    succeeding_requests.recv_timeout(Duration::from_secs(2)).unwrap();
    failing_worker.join().unwrap();
    succeeding_worker.join().unwrap();

    // A manual backup records metadata-only backup events.
    let created = client
        .post(format!("{base}/admin/backups"))
        .header(header::COOKIE, &active_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    // The canary values the diagnostic surfaces must never carry. The provider
    // display name and upstream model name are safe local identifiers that
    // legitimately appear in the management surface (OPS-021).
    // REL-010 stores the relay key secret in plaintext on purpose, so it is
    // no longer a leak in the key-management surface; it stays forbidden on
    // every other surface (checked separately below).
    let forbidden = [
        prompt_canary,
        tool_canary,
        response_canary,
        error_canary,
        header_canary,
        query_canary,
        upstream_key_canary,
        &base_url_canary,
    ];
    let assert_clean = |label: &str, contents: &str| {
        for canary in forbidden {
            assert!(
                !contents.contains(canary),
                "{label} must not expose {canary:?}"
            );
        }
    };

    // API surfaces: Operations, Calls & usage, event history, backups, keys.
    for (label, payload) in [
        ("operations", operations_json(&client, &base, &active_cookie).await),
        ("calls-usage", calls_usage_json(&client, &base, &active_cookie).await),
        ("events", events_json(&client, &base, &active_cookie, None, 0, 200).await),
        ("backups", get_with_cookie(&client, format!("{base}/admin/backups"), &active_cookie).await.json::<serde_json::Value>().await.unwrap()),
        ("relay-keys", get_with_cookie(&client, format!("{base}/admin/relay-access-keys"), &active_cookie).await.json::<serde_json::Value>().await.unwrap()),
    ] {
        let serialized = serde_json::to_string(&payload).unwrap();
        assert_clean(label, &serialized);
        if label != "relay-keys" {
            assert!(
                !serialized.contains(&relay_secret),
                "{label} must not expose the relay key secret (REL-010 allows only the key-management surface)"
            );
        }
    }
    assert!(
        serde_json::to_string(
            &get_with_cookie(&client, format!("{base}/admin/relay-access-keys"), &active_cookie)
                .await
                .json::<serde_json::Value>()
                .await
                .unwrap(),
        )
        .unwrap()
        .contains(&relay_secret),
        "the key-management surface must re-display the full relay key (REL-010)"
    );
    assert!(
        serde_json::to_string(&operations_json(&client, &base, &active_cookie).await)
            .unwrap()
            .contains(provider_name_canary),
        "the safe provider display name is the intended local identifier"
    );

    // OPS-020 (revised): the focused edit panel is the single legitimate
    // place that carries the complete Base URL — the management list and
    // read-only surfaces carry no Base URL form at all (stricter than the
    // allowed masked/truncated rendering). Pin the edit-panel endpoint as
    // the one exception and assert the Operations provider list never
    // carries the complete value.
    let edit_panel = get_with_cookie(
        &client,
        format!("{base}/admin/providers/{failing_provider}"),
        &active_cookie,
    )
    .await
    .json::<serde_json::Value>()
    .await
    .unwrap();
    assert_eq!(
        edit_panel["base_url"], base_url_canary,
        "the focused edit panel must load the complete Base URL for editing (CFG-002)"
    );
    let operations = operations_json(&client, &base, &active_cookie).await;
    for provider in operations["providers"].as_array().unwrap() {
        assert!(
            provider.get("base_url").is_none(),
            "the Operations provider list must not carry a complete Base URL field"
        );
        assert!(
            !serde_json::to_string(provider)
                .unwrap()
                .contains(base_url_canary.as_str()),
            "the Operations provider list must not render the complete Base URL"
        );
    }


    // Pages: the embedded assets are static and carry no canary values.
    for (label, path) in [
        ("index", format!("{base}/")),
        ("app.js", format!("{base}/assets/app.js")),
        ("app.css", format!("{base}/assets/app.css")),
    ] {
        let body = client.get(&path).send().await.unwrap().text().await.unwrap();
        assert_clean(label, &body);
    }
    // The status-area event-history drill-down wiring is covered by the
    // real-browser test `browser_status_area_drills_into_route_event_history`.

    // Logs: every managed log file and the captured standard error are
    // allowlisted metadata only.
    for path in managed_log_files(&environment) {
        let contents = fs::read_to_string(&path).unwrap();
        assert_clean("managed log", &contents);
        assert_structured_log_lines(&contents);
    }
    let stderr = fs::read_to_string(&stderr_path).unwrap();
    assert_clean("standard error", &stderr);
    assert_structured_log_lines(&stderr);

    // Database: every stored value except the deliberately plaintext upstream
    // key and Base URL columns stays within the metadata allowlist; the raw
    // file never carries request/response content or secrets.
    let dump = database_dump(
        &environment,
        &[
            ("upstream_providers", "api_key"),
            ("upstream_providers", "base_url"),
            ("relay_access_keys", "secret"),
        ],
    );
    assert_clean("database", &dump);
    let raw = fs::read(environment.database_path()).unwrap();
    for canary in [
        prompt_canary,
        tool_canary,
        response_canary,
        error_canary,
        header_canary,
        query_canary,
    ] {
        assert!(
            !raw.windows(canary.len()).any(|window| window == canary.as_bytes()),
            "the database file must not persist {canary:?}"
        );
    }
    let database = rusqlite::Connection::open(environment.database_path()).unwrap();
    let stored_key: String = database
        .query_row(
            "SELECT api_key FROM upstream_providers WHERE display_name = ?1",
            [provider_name_canary],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_key, upstream_key_canary,
        "the upstream key is stored plaintext by the SEC-007 contract"
    );
    // REL-010: the relay key is stored in plaintext (owner-only files) so the
    // management surface can re-display it.
    let stored_secret: Option<String> = database
        .query_row(
            "SELECT secret FROM relay_access_keys WHERE label = 'canary relay key 9f3a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_secret.as_deref(),
        Some(relay_secret.as_str()),
        "the relay key must be stored in plaintext for re-display (REL-010)"
    );

    server.kill().unwrap();
    server.wait().unwrap();
}
