use crate::{
    backup, paths, store, timeutil,
    store::{ProbeConfiguration, ModelRouteCandidate, RouteHealth, SessionKind, Store, Usage},
    web,
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State, rejection::BytesRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, get, patch, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::net::TcpListener;

const SESSION_COOKIE: &str = "local_api_relay_admin";
const SESSION_SECONDS: i64 = 8 * 60 * 60;
/// Hard upper bound for any single upstream exchange, well above every
/// configurable deadline (REL-001); the configured settings deliver the real
/// user-visible deadlines and this cap only prevents unbounded hangs.
const UPSTREAM_HARD_TIMEOUT: Duration = Duration::from_secs(3_600);

/// The relay's inbound request body limit (16 MiB); the value and its API-016
/// semantics are documented in the README "Relay Calls" section.
const MAX_RELAY_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The bounded graceful-stop drain window (PKG-012): after a stop signal the
/// service stops accepting new calls and waits at most this long for in-flight
/// calls to finish before cancelling the remaining calls and exiting.
const DRAIN_GRACE_DURATION: Duration = Duration::from_secs(30);

/// Test hook that shrinks the drain bound so the deadline is observable at the
/// process boundary.
const TEST_DRAIN_GRACE_VARIABLE: &str = "LOCAL_API_RELAY_TEST_SHUTDOWN_GRACE_MS";

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    upstream_client: reqwest::Client,
    streaming_upstream_client: reqwest::Client,
    /// Model route ids with a recovery probe currently in flight; shared by the
    /// recovery scheduler and the admin's manual check so that at most one
    /// recovery probe runs per unavailable route (ROUTE-018).
    recovery_in_flight: Arc<Mutex<HashSet<String>>>,
    /// Shared Storage health: Healthy/Degraded/Not ready with the affected
    /// operational record categories (OPS-010/OPS-011).
    storage_health: Arc<Mutex<StorageHealth>>,
    /// In-memory route health diverging from the persisted `model_route_health`
    /// row: a health transition that failed to persist still takes effect in
    /// memory immediately (DATA-005). Usually empty.
    route_health_override: Arc<Mutex<HashMap<String, RouteHealthOverride>>>,
    /// In-flight explicit restore progress for the data security panel
    /// (UI-012/OPS-015 "current or recent stage"): present only while a restore
    /// is running; the durable outcome lives in `data_operations`.
    restore_progress: Arc<Mutex<Option<RestoreProgress>>>,
}

/// In-flight explicit restore progress (UI-012/OPS-015). The store reports
/// coarse stage transitions (verify → switch → recheck) through the restore
/// call; the server retains the completed stage sequence for a short window
/// after the restore finishes so a delayed poll still shows the stages
/// (OPS-015 "current or recent stage"). The hash of the administrator session
/// token that started the restore scopes progress polling to that session: the
/// store lock is held for the whole synchronous restore, so the normal session
/// check would block every poll until the switch finished.
#[derive(Debug, Clone)]
struct RestoreProgress {
    candidate: String,
    stage: store::RestoreStage,
    started_at: i64,
    session_token_hash: [u8; 32],
    /// Stage sequence observed so far, in order.
    stages: Vec<store::RestoreStage>,
    /// Epoch seconds when the restore completed or failed; `None` while the
    /// restore is still running.
    finished_at: Option<i64>,
}

/// How long the most recent restore's stage sequence stays visible after it
/// completes, so the data security panel (or a delayed poll) can still read
/// the stages it ran through (OPS-015).
const RECENT_RESTORE_RETENTION_SECONDS: i64 = 10;

/// Storage status per the Operations console (OPS-010/OPS-011). Route health
/// transitions and call records are degradable operational records: a
/// persistence failure moves the shared status to Degraded immediately and the
/// current state only clears when every affected category re-persists and a
/// lightweight SQLite integrity check passes (OPS-012).
#[derive(Debug)]
struct StorageHealth {
    state: StorageState,
    /// Epoch seconds when the current state began.
    since: i64,
    categories: BTreeMap<String, StorageCategoryFailure>,
}

impl StorageHealth {
    fn healthy(now: i64) -> Self {
        Self {
            state: StorageState::Healthy,
            since: now,
            categories: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageState {
    Healthy,
    Degraded,
    NotReady,
}

impl StorageState {
    fn as_str(self) -> &'static str {
        match self {
            StorageState::Healthy => "healthy",
            StorageState::Degraded => "degraded",
            StorageState::NotReady => "not_ready",
        }
    }
}

#[derive(Debug)]
struct StorageCategoryFailure {
    since: i64,
    error: String,
    /// Known number of lost records, or None when unknown.
    lost_records: Option<u64>,
}

/// The in-memory health value a route should report, overriding the persisted
/// row until the same write persists successfully.
#[derive(Debug, Clone)]
struct RouteHealthOverride {
    state: RouteHealth,
    failure_category: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum StreamProtocol {
    ChatCompletions,
    Responses,
}

impl StreamProtocol {
    fn as_str(self) -> &'static str {
        match self {
            StreamProtocol::ChatCompletions => "chat_completions",
            StreamProtocol::Responses => "responses",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ModelRouteAttemptOutcome {
    Fallback,
    Failed,
    Success,
    StreamTerminated,
    ClientCancelled,
}

impl ModelRouteAttemptOutcome {
    fn as_str(self) -> &'static str {
        match self {
            ModelRouteAttemptOutcome::Fallback => "fallback",
            ModelRouteAttemptOutcome::Failed => "failed",
            ModelRouteAttemptOutcome::Success => "success",
            ModelRouteAttemptOutcome::StreamTerminated => "stream_terminated",
            ModelRouteAttemptOutcome::ClientCancelled => "client_cancelled",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CommitPhase {
    PreCommit,
    Committed,
}

impl CommitPhase {
    fn as_str(self) -> &'static str {
        match self {
            CommitPhase::PreCommit => "pre_commit",
            CommitPhase::Committed => "committed",
        }
    }
}

/// One model route attempt inside a client call. The committed streaming
/// attempt is created with a pending outcome that is resolved when the
/// downstream stream ends (success, post-commit termination, or client
/// cancellation).
struct ModelRouteAttemptDraft {
    sequence: i64,
    route_id: String,
    provider_id: String,
    provider_name: String,
    started_at: Instant,
    ended_at: Option<Instant>,
    http_status: Option<i64>,
    failure_category: Option<String>,
    phase: CommitPhase,
    outcome: Option<ModelRouteAttemptOutcome>,
}

/// Builds the single metadata-only call record (OPS-001) for one client call
/// while the relay walks its candidate list. Writes are best-effort: a
/// persistence failure never fails the relay response (DATA-004). When a
/// committed stream is dropped early (client disconnect), the Drop
/// implementation finalizes the attempt as `client_cancelled` without touching
/// route health (API-017).
struct CallRecorder {
    state: AppState,
    created_at_ms: i64,
    started_at: Instant,
    published_model_name: String,
    protocol: StreamProtocol,
    streamed: bool,
    attempts: Vec<ModelRouteAttemptDraft>,
    first_chunk_at: Option<Instant>,
    usage: Option<Usage>,
    finalized: bool,
    /// Locally generated correlation identifier shared by every operational
    /// event for this call, so a fallback/failure chain is traceable without
    /// any request or response content (OPS-018).
    correlation_id: String,
}

impl CallRecorder {
    fn new(
        state: AppState,
        published_model_name: &str,
        protocol: StreamProtocol,
        streamed: bool,
    ) -> Self {
        Self {
            state,
            created_at_ms: timeutil::now_epoch_ms(),
            started_at: Instant::now(),
            published_model_name: published_model_name.to_owned(),
            protocol,
            streamed,
            attempts: Vec::new(),
            first_chunk_at: None,
            usage: None,
            finalized: false,
            correlation_id: crate::auth::generate_secret(),
        }
    }

    fn begin_model_route_attempt(&mut self, candidate: &ModelRouteCandidate) {
        self.attempts.push(ModelRouteAttemptDraft {
            sequence: self.attempts.len() as i64,
            route_id: candidate.route_id.clone(),
            provider_id: candidate.provider_id.clone(),
            provider_name: candidate.provider_name.clone(),
            started_at: Instant::now(),
            ended_at: None,
            http_status: None,
            failure_category: None,
            phase: CommitPhase::PreCommit,
            outcome: None,
        });
    }

    /// Ends the current (pre-commit or resolved) attempt with its outcome.
    fn finish_model_route_attempt(
        &mut self,
        http_status: Option<i64>,
        failure_category: Option<String>,
        phase: CommitPhase,
        outcome: ModelRouteAttemptOutcome,
    ) {
        if let Some(attempt) = self.attempts.last_mut() {
            attempt.http_status = http_status;
            attempt.failure_category = failure_category;
            attempt.phase = phase;
            attempt.outcome = Some(outcome);
            attempt.ended_at = Some(Instant::now());
        }
    }

    /// Marks the current attempt as committed; its outcome stays pending until
    /// the downstream stream ends.
    fn mark_committed(&mut self, http_status: i64) {
        if let Some(attempt) = self.attempts.last_mut() {
            attempt.http_status = Some(http_status);
            attempt.phase = CommitPhase::Committed;
        }
    }

    fn committed_route_id(&self) -> Option<&str> {
        self.attempts.last().map(|attempt| attempt.route_id.as_str())
    }

    /// Resolves the committed streaming attempt's outcome at stream end.
    fn resolve_committed_model_route_attempt(
        &mut self,
        outcome: ModelRouteAttemptOutcome,
        failure_category: Option<String>,
    ) {
        if let Some(attempt) = self.attempts.last_mut()
            && attempt.phase == CommitPhase::Committed
            && attempt.outcome.is_none()
        {
            attempt.outcome = Some(outcome);
            attempt.failure_category = failure_category;
            attempt.ended_at = Some(Instant::now());
        }
    }

    fn note_first_chunk(&mut self) {
        if self.first_chunk_at.is_none() {
            self.first_chunk_at = Some(Instant::now());
        }
    }

    fn note_usage(&mut self, usage: Option<Usage>) {
        if let Some(usage) = usage {
            self.usage = Some(usage);
        }
    }

    fn finalize(&mut self, succeeded: bool) -> store::NewCallRecord {
        self.finalized = true;
        let now = Instant::now();
        let elapsed = |attempt: &ModelRouteAttemptDraft| {
            attempt
                .ended_at
                .unwrap_or(now)
                .duration_since(attempt.started_at)
                .as_millis() as i64
        };
        let completion_ms = succeeded.then(|| now.duration_since(self.started_at).as_millis() as i64);
        let first_token_ms = (self.streamed && succeeded)
            .then(|| self.first_chunk_at.map(|at| at.duration_since(self.started_at).as_millis() as i64))
            .flatten();
        let success_attempt = self
            .attempts
            .iter()
            .find(|attempt| attempt.outcome == Some(ModelRouteAttemptOutcome::Success));
        // Only the final successful attempt's reliable usage enters the record;
        // a stream that terminated or was cancelled carries no trusted usage.
        let usage = succeeded.then(|| self.usage.take()).flatten();
        let record = store::NewCallRecord {
            created_at_ms: self.created_at_ms,
            published_model_name: self.published_model_name.clone(),
            protocol: self.protocol.as_str().to_owned(),
            streamed: self.streamed,
            succeeded,
            success_provider_id: success_attempt.map(|attempt| attempt.provider_id.clone()),
            success_provider_name: success_attempt.map(|attempt| attempt.provider_name.clone()),
            usage,
            completion_ms,
            first_token_ms,
            attempts: self
                .attempts
                .iter()
                .map(|attempt| store::ModelRouteAttempt {
                    sequence: attempt.sequence,
                    route_id: attempt.route_id.clone(),
                    provider_id: attempt.provider_id.clone(),
                    provider_name: attempt.provider_name.clone(),
                    started_at_ms: self.created_at_ms
                        + attempt
                            .started_at
                            .duration_since(self.started_at)
                            .as_millis() as i64,
                    duration_ms: elapsed(attempt),
                    http_status: attempt.http_status,
                    failure_category: attempt.failure_category.clone(),
                    commit_phase: attempt.phase.as_str().to_owned(),
                    outcome: attempt
                        .outcome
                        .map(ModelRouteAttemptOutcome::as_str)
                        .unwrap_or("success")
                        .to_owned(),
                })
                .collect(),
        };
        self.emit_call_events();
        record
    }

    /// Emits operational events for abnormal calls only: an ordinary successful
    /// call (every attempt succeeded) is silent because its metadata already
    /// belongs to the call record and usage views (OPS-018). Fallback, failed,
    /// stream-terminated, and client-cancelled calls emit one allowlisted event
    /// sharing this call's correlation identifier so the chain is traceable
    /// without any request or response content.
    fn emit_call_events(&self) {
        if self.attempts.is_empty() {
            return;
        }
        let all_succeeded = self
            .attempts
            .iter()
            .all(|attempt| attempt.outcome == Some(ModelRouteAttemptOutcome::Success));
        if all_succeeded {
            return;
        }
        let fallback = self
            .attempts
            .iter()
            .any(|attempt| attempt.outcome == Some(ModelRouteAttemptOutcome::Fallback));
        let (code, severity) = if fallback {
            ("call.fallback", crate::log::SEVERITY_WARNING)
        } else {
            match self.attempts.last().and_then(|attempt| attempt.outcome) {
                Some(ModelRouteAttemptOutcome::StreamTerminated) => {
                    ("call.stream_terminated", crate::log::SEVERITY_ERROR)
                }
                Some(ModelRouteAttemptOutcome::ClientCancelled) => {
                    ("call.cancelled", crate::log::SEVERITY_INFO)
                }
                _ => ("call.failed", crate::log::SEVERITY_ERROR),
            }
        };
        let last_failure_category = self
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| attempt.failure_category.clone());
        persist_operational_event(
            &self.state,
            crate::log::SECTION_CALLS,
            severity,
            code,
            Some(&self.correlation_id),
            &json!({
                "published_model_name": self.published_model_name,
                "protocol": self.protocol.as_str(),
                "attempts": self.attempts.len(),
                "fallback": fallback,
                "failure_category": last_failure_category
            }),
        );
    }
}

impl Drop for CallRecorder {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        // The committed stream was dropped before its terminal event: the
        // client disconnected. This is health-neutral (API-017), so no route is
        // quarantined here.
        self.resolve_committed_model_route_attempt(ModelRouteAttemptOutcome::ClientCancelled, None);
        let record = self.finalize(false);
        write_call_record(&self.state, &record);
    }
}

struct SseEvent {
    raw: Bytes,
    event_type: Option<String>,
    data: Option<String>,
}

#[derive(Default)]
struct SseReader {
    buffered: Vec<u8>,
}

struct SseRelay {
    upstream: reqwest::Response,
    reader: SseReader,
    queued_events: VecDeque<SseEvent>,
    protocol: StreamProtocol,
    published_model_name: String,
    /// Deadline between two chunks of an in-flight SSE stream (REL-001).
    idle_timeout: Duration,
    /// Usage reported by the last relayed protocol event; captured so the
    /// call record only ever carries usage the successful route itself
    /// reported (OPS-004).
    captured_usage: Option<Usage>,
    /// True when the stream ended on a Responses typed terminal event whose
    /// payload marks the response as a semantic failure (API-011/ROUTE-008).
    /// The committed attempt is then recorded as failed and the route is
    /// quarantined instead of crediting the call with success.
    terminal_semantic_failure: bool,
    finished: bool,
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

#[derive(Deserialize)]
struct ChangePasswordRequest {
    new_password: String,
}

#[derive(Serialize)]
struct SessionResponse {
    authenticated: bool,
    must_change_password: bool,
}

#[derive(Serialize)]
struct LoginResponse {
    must_change_password: bool,
}

#[derive(Deserialize)]
struct CreateProviderRequest {
    display_name: String,
    base_url: String,
    api_key: String,
}

#[derive(Deserialize)]
struct CreateModelRouteRequest {
    published_model_id: String,
    provider_id: String,
    upstream_model_name: String,
    protocol: String,
    cost_multiplier: serde_json::Value,
}

#[derive(Deserialize)]
struct UpdatePricesRequest {
    input_price_rmb: serde_json::Value,
    output_price_rmb: serde_json::Value,
    cached_input_price_rmb: serde_json::Value,
}

#[derive(Deserialize)]
struct RecoverySettingsRequest {
    base_interval_ms: i64,
    doubling_limit: i64,
    first_event_timeout_ms: Option<i64>,
    stream_idle_timeout_ms: Option<i64>,
    nonstream_timeout_ms: Option<i64>,
    freshness_interval_ms: Option<i64>,
    quarantine_threshold: Option<i64>,
    upstream_sync_interval_ms: Option<i64>,
}

#[derive(Deserialize)]
struct CreateRelayAccessKeyRequest {
    label: String,
    model_route_ids: Vec<String>,
}

#[derive(Deserialize)]
struct RestoreBackupRequest {
    name: String,
}

#[derive(Deserialize)]
struct RelayAccessKeyListQuery {
    search: Option<String>,
}

#[derive(Deserialize)]
struct OperationalEventsQuery {
    section: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}

const DEFAULT_EVENTS_PAGE_SIZE: i64 = 50;
const MAX_EVENTS_PAGE_SIZE: i64 = 200;

pub async fn serve(
    database_path: PathBuf,
    backup_dir: PathBuf,
    log_dir: PathBuf,
    port: u16,
) -> Result<()> {
    crate::log::init(log_dir);
    paths::validate_port(port)?;
    let mut store = Store::open(database_path, backup_dir)?;
    store.record_event(
        crate::log::SECTION_PROCESS,
        crate::log::SEVERITY_INFO,
        "process.start",
        None,
        &json!({ "port": port }),
    );
    // Test hook: fail after the store opened (so a forward migration commits on
    // an old-schema database) but before the listener binds. This makes the
    // "restart failed after the migration committed" rollback drill
    // deterministic (PKG-014).
    if std::env::var("LOCAL_API_RELAY_TEST_FAIL_SERVE").ok().as_deref() == Some("1") {
        bail!("injected serve failure");
    }
    // Discard persisted health before the loopback listener binds: every
    // configured route returns to Checking so stale health can never influence
    // candidate selection, while ready still does not wait for the probes
    // themselves (ROUTE-004/005).
    let startup_probes = store.startup_probe_configurations()?;
    let upstream_client = reqwest::Client::builder()
        .timeout(UPSTREAM_HARD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("could not initialize the upstream HTTP client")?;
    let streaming_upstream_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("could not initialize the streaming upstream HTTP client")?;
    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        upstream_client,
        streaming_upstream_client,
        recovery_in_flight: Arc::new(Mutex::new(HashSet::new())),
        storage_health: Arc::new(Mutex::new(StorageHealth::healthy(timeutil::now_epoch()))),
        route_health_override: Arc::new(Mutex::new(HashMap::new())),
        restore_progress: Arc::new(Mutex::new(None)),
    };
    spawn_automatic_backup_task(state.clone());
    spawn_startup_probe_task(state.clone(), startup_probes);
    spawn_recovery_scheduler(state.clone());
    spawn_freshness_scheduler(state.clone());
    spawn_upstream_sync_scheduler(state.clone());
    spawn_call_retention_task(state.clone());
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind loopback listener at {address}"))?;
    let _ = with_store(&state, |store| {
        store.record_event(
            crate::log::SECTION_PROCESS,
            crate::log::SEVERITY_INFO,
            "process.ready",
            None,
            &json!({ "port": port }),
        );
        Ok(())
    });

    let drain_grace = std::env::var(TEST_DRAIN_GRACE_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DRAIN_GRACE_DURATION);
    // PKG-012: the stop is bounded. When the stop signal fires, axum stops
    // accepting new calls and drains the in-flight ones; the drain guard arms
    // the deadline at that same moment, so an upstream that never responds
    // cannot hold the service open past the window. Whichever resolves first
    // wins the race: a completed drain exits cleanly, an expired deadline
    // drops the server future, cancels the remaining calls, and exits cleanly.
    let server = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(wait_for_stop_signal());
    let drain_guard = async {
        wait_for_stop_signal().await;
        persist_operational_event(
            &state,
            crate::log::SECTION_PROCESS,
            crate::log::SEVERITY_INFO,
            "process.stopping",
            None,
            &json!({}),
        );
        tokio::time::sleep(drain_grace).await;
    };
    tokio::select! {
        result = server => match result {
            Ok(()) => {
                persist_operational_event(
                    &state,
                    crate::log::SECTION_PROCESS,
                    crate::log::SEVERITY_INFO,
                    "process.stopped",
                    None,
                    &json!({}),
                );
                Ok(())
            }
            Err(error) => Err(error).context("loopback server failed"),
        },
        _ = drain_guard => {
            persist_operational_event(
                &state,
                crate::log::SECTION_PROCESS,
                crate::log::SEVERITY_WARNING,
                "process.drain_expired",
                None,
                &json!({}),
            );
            Ok(())
        }
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(web::index))
        .route("/assets/app.css", get(web::styles))
        .route("/assets/app.js", get(web::script))
        .route("/ready", get(ready))
        .route("/admin/session", get(session_status))
        .route("/admin/login", post(login))
        .route("/admin/change-password", post(change_password))
        .route("/admin/logout", post(logout))
        .route("/admin/operations", get(operations))
        .route("/admin/operations/events", get(operational_events))
        .route(
            "/admin/backups",
            get(admin_backups).post(create_manual_backup),
        )
        .route("/admin/restore", post(restore_from_backup))
        .route("/admin/restore/progress", get(restore_progress))
        .route("/admin/providers", post(create_provider))
        .route(
            "/admin/providers/:provider_id",
            get(provider_configuration).patch(update_provider),
        )
        .route(
            "/admin/providers/:provider_id/models",
            get(provider_cached_models).post(refresh_provider_models),
        )
        .route("/admin/model-routes", post(create_model_route))
        .route("/admin/model-routes/:route_id", patch(update_model_route))
        .route(
            "/admin/model-routes/:route_id/check",
            post(check_model_route),
        )
        .route(
            "/admin/relay-access-keys",
            get(list_relay_access_keys).post(create_relay_access_key),
        )
        .route(
            "/admin/relay-access-keys/:key_id",
            patch(update_relay_access_key),
        )
        .route(
            "/admin/relay-access-keys/:key_id/revoke",
            post(revoke_relay_access_key),
        )
        .route(
            "/admin/published-models",
            post(create_published_model),
        )
        .route(
            "/admin/published-models/:model_id/prices",
            patch(update_published_model_prices),
        )
        .route(
            "/admin/published-models/:model_id/deprecate",
            post(deprecate_published_model),
        )
        .route(
            "/admin/recovery-settings",
            get(recovery_settings).patch(update_recovery_settings),
        )
        .route("/admin/calls-usage", get(calls_usage))
        .route("/v1/models", get(list_relay_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1", any(relay_unauthorized))
        .route("/v1/*path", any(relay_unauthorized))
        .layer(DefaultBodyLimit::max(MAX_RELAY_REQUEST_BODY_BYTES))
        .with_state(state)
}

async fn ready() -> impl IntoResponse {
    no_store_json(StatusCode::OK, json!({ "status": "ready" }))
}

async fn session_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let Some(token) = read_cookie(&headers) else {
        return Ok(no_store_json(
            StatusCode::OK,
            serde_json::to_value(SessionResponse {
                authenticated: false,
                must_change_password: false,
            })
            .unwrap(),
        ));
    };
    let session = with_store(&state, |store| store.session(&token))?;
    let response = match session {
        Some(session) => SessionResponse {
            authenticated: true,
            must_change_password: session.kind == SessionKind::Bootstrap,
        },
        None => SessionResponse {
            authenticated: false,
            must_change_password: false,
        },
    };
    Ok(no_store_json(
        StatusCode::OK,
        serde_json::to_value(response).unwrap(),
    ))
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, AdminError> {
    if request.password.is_empty() {
        return Err(AdminError::new(
            StatusCode::UNAUTHORIZED,
            "invalid administrator credentials",
        ));
    }
    let expires_at = session_expiry();
    let result = with_store(&state, |store| store.login(&request.password, expires_at))?;
    let Some((token, kind)) = result else {
        return Err(AdminError::new(
            StatusCode::UNAUTHORIZED,
            "invalid administrator credentials",
        ));
    };
    let mut response = no_store_json(
        StatusCode::OK,
        serde_json::to_value(LoginResponse {
            must_change_password: kind == SessionKind::Bootstrap,
        })
        .unwrap(),
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, session_cookie(&token));
    Ok(response)
}

async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChangePasswordRequest>,
) -> Result<Response, AdminError> {
    let token = read_cookie(&headers).ok_or_else(|| {
        AdminError::new(
            StatusCode::UNAUTHORIZED,
            "administrator session is required",
        )
    })?;
    let expires_at = session_expiry();
    let password_change = {
        let mut store = state.store.lock().map_err(|_| {
            AdminError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "local state is unavailable",
            )
        })?;
        store.change_password(&token, &request.new_password, expires_at)
    };
    let new_token = password_change.map_err(|error| {
        let message = error.to_string();
        if message.starts_with("password must be") {
            AdminError::new(StatusCode::UNPROCESSABLE_ENTITY, message)
        } else if message.contains("requires the bootstrap session") {
            AdminError::new(
                StatusCode::FORBIDDEN,
                "administrator password change is not available for this session",
            )
        } else {
            AdminError::internal(error)
        }
    })?;
    let mut response = no_store_json(StatusCode::OK, json!({ "changed": true }));
    response
        .headers_mut()
        .append(header::SET_COOKIE, session_cookie(&new_token));
    Ok(response)
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, AdminError> {
    if let Some(token) = read_cookie(&headers) {
        with_store(&state, |store| store.logout(&token))?;
    }
    let mut response = no_store_json(StatusCode::NO_CONTENT, json!(null));
    response
        .headers_mut()
        .append(header::SET_COOKIE, expired_cookie());
    Ok(response)
}

async fn operations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let snapshot = with_store(&state, |store| store.operations_snapshot())?;
    let backups = with_store(&state, |store| store.backup_status())?;
    let recovery = with_store(&state, |store| store.recovery_settings())?;
    let usage_integrity = with_store(&state, |store| store.usage_integrity_all())?;
    let usage_has_data = with_store(&state, |store| store.usage_data_present())?;
    let data_operations = with_store(&state, |store| store.data_operations_status())?;
    let now = timeutil::now_epoch();
    let storage = state
        .storage_health
        .lock()
        .map(|health| {
            let accounting_gaps: Vec<_> = usage_integrity
                .gaps
                .iter()
                .filter(|gap| {
                    gap.kind == store::USAGE_GAP_KIND_PERSISTENCE && gap.ended_at_ms.is_none()
                })
                .map(|gap| {
                    json!({
                        "category": gap.category,
                        "started_at_ms": gap.started_at_ms,
                        "ended_at_ms": gap.ended_at_ms,
                        "lost_records": gap.lost_records
                    })
                })
                .collect();
            json!({
                "state": health.state.as_str(),
                "since": health.since,
                "categories": health.categories.iter().map(|(category, failure)| json!({
                    "category": category,
                    "since": failure.since,
                    "error": failure.error,
                    "lost_records": failure.lost_records
                })).collect::<Vec<_>>(),
                "accounting_gaps": accounting_gaps
            })
        })
        .unwrap_or_else(|_| {
            json!({
                "state": StorageState::NotReady.as_str(),
                "since": now,
                "categories": [],
                "accounting_gaps": []
            })
        });
    let usage_state = if usage_integrity.gaps.is_empty() && !usage_has_data {
        "no_data"
    } else if usage_integrity.gaps.is_empty() {
        "complete"
    } else {
        "incomplete"
    };
    let overrides = overrides_snapshot(&state);
    let mut available = 0usize;
    let mut checking = 0usize;
    let mut unavailable = 0usize;
    let route_rows: Vec<serde_json::Value> = snapshot
        .routes
        .iter()
        .map(|route| {
            let health = effective_health(&overrides, &route.id, route.health);
            match health {
                RouteHealth::Available => available += 1,
                RouteHealth::Checking => checking += 1,
                RouteHealth::Unavailable => unavailable += 1,
            }
            let failure_category = overrides
                .get(&route.id)
                .and_then(|override_| override_.failure_category.clone())
                .or_else(|| route.failure_category.clone());
            json!({
                "id": route.id,
                "published_model_id": route.published_model_id,
                "published_model_name": route.published_model_name,
                "provider_id": route.provider_id,
                "provider_name": route.provider_name,
                "upstream_model_name": route.upstream_model_name,
                "protocol": route.protocol,
                "cost_multiplier": route.cost_multiplier,
                "health": health.as_str(),
                "last_checked_at": route.last_checked_at,
                "failure_category": failure_category,
                "last_http_status": route.last_http_status,
                "state_age_seconds": route.last_checked_at.map(|checked_at| (now - checked_at).max(0)),
                "failed_probe_count": route.failed_probe_count,
                "next_probe_at_ms": route.next_probe_at_ms,
                "current_interval_ms": (health == RouteHealth::Unavailable)
                    .then(|| recovery.interval_for(route.failed_probe_count))
            })
        })
        .collect();
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "storage": storage,
            "model_routes": { "available": available, "checking": checking, "unavailable": unavailable },
            "recovery": {
                "base_interval_ms": recovery.base_interval_ms,
                "doubling_limit": recovery.doubling_limit,
                "first_event_timeout_ms": recovery.first_event_timeout_ms,
                "stream_idle_timeout_ms": recovery.stream_idle_timeout_ms,
                "nonstream_timeout_ms": recovery.nonstream_timeout_ms,
                "freshness_interval_ms": recovery.freshness_interval_ms,
                "quarantine_threshold": recovery.quarantine_threshold,
                "upstream_sync_interval_ms": recovery.upstream_sync_interval_ms
            },
            "backups": backup_status_json(&backups),
            "migration": data_operations_json(&data_operations),
            "usage": {
                "state": usage_state,
                "gaps": usage_integrity.gaps.iter().map(usage_gap_json).collect::<Vec<_>>()
            },
            "catalog": snapshot.catalog.iter().map(|model| json!({
                "id": model.id,
                "name": model.name,
                "input_price_rmb": model.input_price_rmb,
                "output_price_rmb": model.output_price_rmb,
                "cached_input_price_rmb": model.cached_input_price_rmb,
                "deprecated": model.deprecated
            })).collect::<Vec<_>>(),
            "providers": snapshot.providers.iter().map(|provider| {
                let cached = with_store(&state, |store| {
                    let models = store.cached_upstream_models(&provider.id)?;
                    let fetched_at = store.cached_upstream_models_fetched_at(&provider.id)?;
                    Ok((models, fetched_at))
                })?;
                Ok(json!({
                    "id": provider.id,
                    "display_name": provider.display_name,
                    "api_key_masked": provider.api_key_masked,
                    "cached_models": cached.0,
                    "catalog_fetched_at_ms": cached.1
                }))
            }).collect::<Result<Vec<_>, AdminError>>()?,
            "routes": route_rows
        }),
    ))
}

/// The 14-day operational event history for the Operations drill-down
/// (OPS-010/OPS-018): one page of allowlisted metadata events newest-first,
/// optionally filtered to one section. Section names are validated against the
/// allowlist so arbitrary filters cannot probe the history.
async fn operational_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OperationalEventsQuery>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let section = query.section.as_deref();
    if let Some(section) = section
        && ![
            crate::log::SECTION_PROCESS,
            crate::log::SECTION_ROUTES,
            crate::log::SECTION_CALLS,
            crate::log::SECTION_STORAGE,
            crate::log::SECTION_BACKUPS,
            crate::log::SECTION_MIGRATION,
            crate::log::SECTION_USAGE,
            crate::log::SECTION_LOGS,
        ]
        .contains(&section)
    {
        return Err(AdminError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown operational event section",
        ));
    }
    let page = query.page.unwrap_or(0).max(0);
    let page_size = query
        .page_size
        .unwrap_or(DEFAULT_EVENTS_PAGE_SIZE)
        .clamp(1, MAX_EVENTS_PAGE_SIZE);
    let result =
        with_store(&state, |store| store.operational_event_page(section, page, page_size))?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "events": result.events.iter().map(|event| json!({
                "id": event.id,
                "occurred_at_ms": event.occurred_at_ms,
                "section": event.section,
                "severity": event.severity,
                "event_code": event.event_code,
                "version": event.version,
                "correlation_id": event.correlation_id,
                "payload": event.payload
            })).collect::<Vec<_>>(),
            "page": page,
            "page_size": page_size,
            "total": result.total
        }),
    ))
}

async fn admin_backups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let status = with_store(&state, |store| store.backup_status())?;
    let artifacts = with_store(&state, |store| store.list_backups())?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "status": backup_status_json(&status),
            "data": artifacts.iter().map(|artifact| json!({
                "name": artifact.name,
                "created_at": artifact.created_at,
                "trigger": artifact.trigger,
                "schema_version": artifact.schema_version,
                "size": artifact.size
            })).collect::<Vec<_>>()
        }),
    ))
}

async fn create_manual_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let artifact = with_configuration_store(&state, |store| {
        store.create_backup(backup::TriggerKind::Manual)
    })?;
    Ok(no_store_json(
        StatusCode::CREATED,
        json!({
            "created_at": artifact.created_at,
            "trigger": artifact.trigger,
            "schema_version": artifact.schema_version,
            "size": artifact.size
        }),
    ))
}

/// Explicit restore (DATA-014/015/016): the selected backup is verified in
/// isolation and, only when every check passes, switched into place with the
/// current database preserved first. On success the restored database's model
/// routes all return to Checking and are re-probed with the same native probes
/// as startup; any in-memory state that referenced pre-restore routes is
/// discarded, and the restored database's own health never surfaces as current
/// (OPS-015). While the synchronous restore runs, the shared in-flight
/// progress (UI-012/OPS-015) reports the coarse stage so the data security
/// panel can show verify → switch → recheck instead of a static label; after
/// the restore completes or fails the stage sequence stays readable as the
/// recent restore for a short window (OPS-015 "current or recent stage").
async fn restore_from_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RestoreBackupRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let candidate = request.name.clone();
    if let Ok(mut guard) = state.restore_progress.lock() {
        *guard = Some(RestoreProgress {
            candidate: candidate.clone(),
            stage: store::RestoreStage::Verify,
            started_at: timeutil::now_epoch(),
            session_token_hash: read_cookie(&headers)
                .map(|token| crate::auth::hash_token(&token))
                .unwrap_or_default(),
            stages: Vec::new(),
            finished_at: None,
        });
    }
    // The synchronous restore holds the store lock for its whole run (verify,
    // switch, recheck). Run it on the blocking pool so the async workers stay
    // free to serve the progress polls while it is blocked (UI-012/OPS-015).
    let blocking_state = state.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut store = blocking_state.store.lock().map_err(|_| {
            AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, "local state is unavailable")
        })?;
        let mut progress = |stage: store::RestoreStage| {
            if let Ok(mut guard) = blocking_state.restore_progress.lock()
                && let Some(current) = guard.as_mut()
            {
                current.stage = stage;
                if current.stages.last() != Some(&stage) {
                    current.stages.push(stage);
                }
            }
            maybe_pause_restore_for_test();
        };
        let result = store.restore_from_backup(&candidate, &mut progress);
        drop(store);
        result.map_err(store_error_to_admin)
    })
    .await
    .map_err(|_| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, "local state is unavailable"))?;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            mark_restore_finished(&state);
            return Err(error);
        }
    };
    mark_restore_finished(&state);
    if let Ok(mut guard) = state.route_health_override.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = state.recovery_in_flight.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = state.storage_health.lock() {
        *guard = StorageHealth::healthy(timeutil::now_epoch());
    }
    let store::RestoreOutcome {
        candidate_name,
        candidate_schema,
        restored_schema,
        pre_restore_backup_name,
        completed_at,
        probe_configurations,
    } = outcome;
    let routes_reset_to_checking = probe_configurations.len();
    spawn_startup_probe_task(state, probe_configurations);
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "restored_from": candidate_name,
            "candidate_schema": candidate_schema,
            "schema_version": restored_schema,
            "pre_restore_backup": pre_restore_backup_name,
            "completed_at": completed_at,
            "routes_reset_to_checking": routes_reset_to_checking
        }),
    ))
}

/// Marks the in-flight restore as finished while retaining its stage sequence
/// for the recent-restore window; the durable outcome lives in the
/// `data_operations` migration/restore status.
fn mark_restore_finished(state: &AppState) {
    if let Ok(mut guard) = state.restore_progress.lock()
        && let Some(progress) = guard.as_mut()
    {
        progress.finished_at = Some(timeutil::now_epoch());
    }
}

/// Test hook that pauses the in-flight restore after each reported stage so a
/// process-boundary test can observe the verify → switch → recheck transitions
/// before the synchronous switch completes (UI-012/OPS-015).
const TEST_RESTORE_STAGE_PAUSE_VARIABLE: &str = "LOCAL_API_RELAY_TEST_RESTORE_STAGE_PAUSE_MS";

fn maybe_pause_restore_for_test() {
    let pause_millis = std::env::var(TEST_RESTORE_STAGE_PAUSE_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(millis) = pause_millis {
        std::thread::sleep(Duration::from_millis(millis));
    }
}

/// In-flight restore progress for the data security panel (UI-012/OPS-015).
/// Only safe metadata is carried — the candidate artifact name and the coarse
/// stage — never backup contents. `idle` means no restore has run recently.
/// The endpoint is scoped to the administrator session that started the
/// restore: the store lock is held for the whole synchronous restore, so the
/// normal DB-backed session check would block every poll until the switch
/// finished and the panel could never show the in-flight stages. The restore
/// POST itself was already authenticated, and the payload equals the safe
/// metadata the Operations status area already shows.
async fn restore_progress(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let progress = state
        .restore_progress
        .lock()
        .map_err(|_| AdminError::new(StatusCode::INTERNAL_SERVER_ERROR, "local state is unavailable"))?
        .clone();
    let now = timeutil::now_epoch();
    match progress {
        Some(progress) => {
            let token_hash = read_cookie(&headers).map(|token| crate::auth::hash_token(&token));
            if token_hash.as_ref() != Some(&progress.session_token_hash) {
                return Err(AdminError::new(
                    StatusCode::UNAUTHORIZED,
                    "administrator session is required",
                ));
            }
            match progress.finished_at {
                None => Ok(no_store_json(
                    StatusCode::OK,
                    json!({
                        "state": "restoring",
                        "candidate": progress.candidate,
                        "stage": progress.stage.as_str(),
                        "stages": progress
                            .stages
                            .iter()
                            .map(|stage| stage.as_str())
                            .collect::<Vec<_>>(),
                        "started_at": progress.started_at
                    }),
                )),
                Some(finished_at) if now - finished_at <= RECENT_RESTORE_RETENTION_SECONDS => {
                    Ok(no_store_json(
                        StatusCode::OK,
                        json!({
                            "state": "recent",
                            "candidate": progress.candidate,
                            "stages": progress
                                .stages
                                .iter()
                                .map(|stage| stage.as_str())
                                .collect::<Vec<_>>(),
                            "completed_at": finished_at
                        }),
                    ))
                }
                Some(_) => Ok(no_store_json(StatusCode::OK, json!({ "state": "idle" }))),
            }
        }
        None => Ok(no_store_json(StatusCode::OK, json!({ "state": "idle" }))),
    }
}

fn backup_status_json(status: &backup::BackupStatus) -> serde_json::Value {
    json!({
        "state": status.state,
        "last_backup_at": status.last_backup_at,
        "last_trigger": status.last_trigger,
        "schema_version": status.schema_version,
        "last_size": status.last_size,
        "next_auto_backup_at": status.next_auto_backup_at,
        "count": status.count,
        "retention": status.retention,
        "last_failed_stage": status.last_failed_stage,
        "last_failed_reason": status.last_failed_reason
    })
}

/// Migration/restore status for the Operations surface (OPS-015): running and
/// supported schema versions, the migration pre-backup result, and the most
/// recent migration or restore operation with its verification result and
/// completion time. Restored route health is never shown here as current — the
/// routes are reset to Checking after a restore (DATA-016).
fn data_operations_json(status: &store::DataOperationsStatus) -> serde_json::Value {
    json!({
        "running_schema": status.running_schema,
        "supported_schema": status.supported_schema,
        "migration_state": status.migration_state,
        "migrated_from_schema": status.migrated_from_schema,
        // A migration gate backup always snapshots the source schema, so the
        // recorded `migrated_from_schema` is exactly the artifact's schema.
        "pre_backup": status.pre_backup_name.as_ref().map(|name| json!({
            "ok": status.pre_backup_ok,
            "name": name,
            "schema_version": status.migrated_from_schema
        })),
        "last_phase": status.last_phase,
        "last_result": status.last_result,
        "last_completed_at": status.last_completed_at,
        "last_failed_stage": status.last_failed_stage,
        "last_failed_reason": status.last_failed_reason,
        "restore_source": status.restore_source
    })
}

fn spawn_automatic_backup_task(state: AppState) {
    tokio::spawn(async move {
        let tick_interval = std::env::var("LOCAL_API_RELAY_TEST_BACKUP_TICK_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(60 * 60));
        let mut interval = tokio::time::interval(tick_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            let result = match state.store.lock() {
                Ok(mut store) => store.maybe_create_auto_backup(),
                Err(_) => continue,
            };
            if let Err(error) = result {
                eprintln!("local-api-relay: automatic backup failed: {error:#}");
            }
        }
    });
}

/// Prunes per-call records, their attempt chains, and operational events after
/// the 14-day retention window, leaving the current status, managed backup
/// metadata, and permanent daily usage aggregates intact (OPS-009).
fn spawn_call_retention_task(state: AppState) {
    tokio::spawn(async move {
        let tick = std::env::var("LOCAL_API_RELAY_TEST_RETENTION_TICK_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(60 * 60));
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            let result = match state.store.lock() {
                Ok(mut store) => {
                    let retention_ms = crate::log::RETENTION_DAYS * 86_400_000;
                    let calls = store.delete_expired_call_records(timeutil::now_epoch_ms(), retention_ms);
                    let events = store.delete_expired_operational_events(timeutil::now_epoch_ms(), retention_ms);
                    calls.and(events)
                }
                Err(_) => continue,
            };
            if let Err(error) = result {
                eprintln!("local-api-relay: diagnostic retention failed: {error:#}");
            }
        }
    });
}

/// Fetches a provider's upstream model catalog (REL-002/REL-006): a GET
/// '/v1/models' call authenticated with the provider's upstream API key. The
/// result is the sorted, deduplicated model id list, or a normalized error
/// when the upstream cannot serve a catalog.
async fn fetch_upstream_model_list(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let response = tokio::time::timeout(Duration::from_secs(30), async {
        client.get(&endpoint).bearer_auth(api_key).send().await
    })
    .await
    .map_err(|_| "catalog fetch timed out".to_owned())?
    .map_err(|error| format!("catalog transport failure: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("catalog status {}", response.status().as_u16()));
    }
    let body: serde_json::Value = tokio::time::timeout(Duration::from_secs(30), response.json())
        .await
        .map_err(|_| "catalog body timed out".to_owned())?
        .map_err(|error| format!("catalog body failure: {error}"))?;
    if body.get("object").and_then(serde_json::Value::as_str) != Some("list") {
        return Err("upstream did not return a models list".to_owned());
    }
    let mut names: Vec<String> = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Light validation (REL-002): whether the fetched upstream catalog still lists
/// the route's target upstream model. This is the zero-token first signal;
/// native probes remain the confirming signal.
fn light_validation_passes(list: Option<&Vec<String>>, upstream_model_name: &str) -> bool {
    list.is_some_and(|names| names.iter().any(|name| name == upstream_model_name))
}

/// Probes every configured route concurrently on process start. The routes
/// were already reset to Checking synchronously before the listener bound; the
/// service becomes ready without waiting for these probes (ROUTE-004/005).
/// Each provider's catalog is fetched once (REL-002): routes whose target
/// model the catalog lists become Available directly, anything else falls back
/// to the native probe.
fn spawn_startup_probe_task(state: AppState, probes: Vec<ProbeConfiguration>) {
    tokio::spawn(async move {
        let mut groups: Vec<(String, String, String, Vec<ProbeConfiguration>)> = Vec::new();
        for probe in probes {
            match groups
                .iter_mut()
                .find(|(provider_id, _, _, _)| *provider_id == probe.provider_id)
            {
                Some((_, _, _, entries)) => entries.push(probe),
                None => groups.push((
                    probe.provider_id.clone(),
                    probe.base_url.clone(),
                    probe.api_key.clone(),
                    vec![probe],
                )),
            }
        }
        for (provider_id, base_url, api_key, group) in groups {
            let group_state = state.clone();
            tokio::spawn(async move {
                let list =
                    fetch_upstream_model_list(&group_state.upstream_client, &base_url, &api_key)
                        .await;
                let now_ms = timeutil::now_epoch_ms();
                if let Ok(names) = &list {
                    let _ = with_store(&group_state, |store| {
                        store.cache_upstream_models(&provider_id, names, now_ms)
                    });
                }
                for probe in group {
                    let probe_state = group_state.clone();
                    let list = list.clone();
                    tokio::spawn(async move {
                        if light_validation_passes(list.as_ref().ok(), &probe.upstream_model_name) {
                            record_probe_result(
                                &probe_state,
                                &probe.route_id,
                                probe.quarantine_epoch,
                                true,
                                Some(200),
                            );
                        } else {
                            let (available, http_status) =
                                native_probe(&probe_state.upstream_client, &probe).await;
                            record_probe_result(
                                &probe_state,
                                &probe.route_id,
                                probe.quarantine_epoch,
                                available,
                                http_status,
                            );
                        }
                    });
                }
            });
        }
    });
}

/// Runs recovery probes for unavailable routes on a capped-doubling schedule.
/// Available routes are never probed; at most one recovery probe is in flight
/// per route (ROUTE-018/019/020).
fn spawn_recovery_scheduler(state: AppState) {
    tokio::spawn(async move {
        let tick = std::env::var("LOCAL_API_RELAY_TEST_RECOVERY_TICK_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(200));
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            let now_ms =
                timeutil::recovery_clock_now_ms().unwrap_or_else(timeutil::now_epoch_ms);
            let due = {
                let Ok(mut store) = state.store.lock() else {
                    continue;
                };
                match store.recovery_due_probes(now_ms) {
                    Ok(due) => due,
                    Err(error) => {
                        eprintln!("local-api-relay: could not read recovery probes: {error:#}");
                        continue;
                    }
                }
            };
            // Group the due probes by provider so each provider's catalog is
            // fetched once per tick (REL-002/REL-006).
            let mut groups: Vec<(String, String, String, Vec<ProbeConfiguration>)> = Vec::new();
            for probe in due {
                match groups
                    .iter_mut()
                    .find(|(provider_id, _, _, _)| *provider_id == probe.provider_id)
                {
                    Some((_, _, _, entries)) => entries.push(probe),
                    None => groups.push((
                        probe.provider_id.clone(),
                        probe.base_url.clone(),
                        probe.api_key.clone(),
                        vec![probe],
                    )),
                }
            }
            for (provider_id, base_url, api_key, group) in groups {
                // Claim the one-per-route in-flight guard for the whole
                // attempt (catalog fetch + native probe, ROUTE-018): a route
                // with a recovery attempt already running is skipped, so its
                // light validation is never duplicated either.
                let claimed: Vec<ProbeConfiguration> = {
                    let mut guard = match state.recovery_in_flight.lock() {
                        Ok(guard) => guard,
                        Err(_) => continue,
                    };
                    group
                        .into_iter()
                        .filter(|probe| guard.insert(probe.route_id.clone()))
                        .collect()
                };
                if claimed.is_empty() {
                    continue;
                }
                let group_state = state.clone();
                tokio::spawn(async move {
                    let list =
                        fetch_upstream_model_list(&group_state.upstream_client, &base_url, &api_key)
                            .await;
                    if let Ok(names) = &list {
                        let now_ms = timeutil::now_epoch_ms();
                        let _ = with_store(&group_state, |store| {
                            store.cache_upstream_models(&provider_id, names, now_ms)
                        });
                    }
                    for probe in claimed {
                        let probe_state = group_state.clone();
                        let list = list.clone();
                        tokio::spawn(async move {
                            // Light validation first (zero-token). A pass is
                            // confirmed by the native probe; a definitive miss
                            // (catalog fine but the model is gone) fails without
                            // spending a generation; a fetch failure falls back
                            // to the native probe alone.
                            let (available, http_status) = match &list {
                                Ok(names)
                                    if names
                                        .iter()
                                        .any(|name| name == &probe.upstream_model_name) =>
                                {
                                    native_probe(&probe_state.upstream_client, &probe).await
                                }
                                Ok(_) => (false, Some(200)),
                                Err(_) => native_probe(&probe_state.upstream_client, &probe).await,
                            };
                            record_probe_result(
                                &probe_state,
                                &probe.route_id,
                                probe.quarantine_epoch,
                                available,
                                http_status,
                            );
                            if let Ok(mut guard) = probe_state.recovery_in_flight.lock() {
                                guard.remove(&probe.route_id);
                            }
                        });
                    }
                });
            }
        }
    });
}

/// Periodic light-validation sweep for Available routes (REL-003): every
/// configured freshness interval (0 disables the sweep) each Available route's
/// provider catalog is fetched once and routes whose target model disappeared
/// from it re-enter Checking so a native probe decides their real health.
fn spawn_freshness_scheduler(state: AppState) {
    tokio::spawn(async move {
        let tick_ms = std::env::var("LOCAL_API_RELAY_TEST_FRESHNESS_TICK_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60_000);
        let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // consume the immediate first tick
        let mut last_sweep_ms: i64 = timeutil::now_epoch_ms();
        loop {
            interval.tick().await;
            let settings = {
                let Ok(mut store) = state.store.lock() else {
                    continue;
                };
                match store.recovery_settings() {
                    Ok(settings) => settings,
                    Err(error) => {
                        eprintln!("local-api-relay: could not read freshness settings: {error:#}");
                        continue;
                    }
                }
            };
            if settings.freshness_interval_ms <= 0 {
                continue;
            }
            let now_ms = timeutil::now_epoch_ms();
            if now_ms - last_sweep_ms < settings.freshness_interval_ms {
                continue;
            }
            last_sweep_ms = now_ms;
            let available = {
                let Ok(mut store) = state.store.lock() else {
                    continue;
                };
                match store.available_route_probe_configurations() {
                    Ok(routes) => routes,
                    Err(error) => {
                        eprintln!("local-api-relay: could not read available routes: {error:#}");
                        continue;
                    }
                }
            };
            let mut groups: Vec<(String, String, String, Vec<ProbeConfiguration>)> = Vec::new();
            for probe in available {
                match groups
                    .iter_mut()
                    .find(|(provider_id, _, _, _)| *provider_id == probe.provider_id)
                {
                    Some((_, _, _, entries)) => entries.push(probe),
                    None => groups.push((
                        probe.provider_id.clone(),
                        probe.base_url.clone(),
                        probe.api_key.clone(),
                        vec![probe],
                    )),
                }
            }
            for (provider_id, base_url, api_key, group) in groups {
                let group_state = state.clone();
                tokio::spawn(async move {
                    let list =
                        fetch_upstream_model_list(&group_state.upstream_client, &base_url, &api_key)
                            .await;
                    if let Ok(names) = &list {
                        let now_ms = timeutil::now_epoch_ms();
                        let _ = with_store(&group_state, |store| {
                            store.cache_upstream_models(&provider_id, names, now_ms)
                        });
                    }
                    for probe in group {
                        if light_validation_passes(list.as_ref().ok(), &probe.upstream_model_name) {
                            continue;
                        }
                        let probe_state = group_state.clone();
                        let route_id = probe.route_id.clone();
                        if let Ok(mut store) = probe_state.store.lock() {
                            let _ = store.reset_route_checking(&route_id);
                        }
                        tokio::spawn(async move {
                            let (available, http_status) =
                                native_probe(&probe_state.upstream_client, &probe).await;
                            record_probe_result(
                                &probe_state,
                                &probe.route_id,
                                probe.quarantine_epoch,
                                available,
                                http_status,
                            );
                        });
                    }
                });
            }
        }
    });
}

/// Fetches one provider's upstream catalog and caches it (REL-006). Used by
/// the periodic sync scheduler, provider edits, and the manual refresh.
async fn fetch_and_cache_provider_catalog(
    state: &AppState,
    provider_id: &str,
    base_url: &str,
    api_key: &str,
) {
    let list = fetch_upstream_model_list(&state.upstream_client, base_url, api_key).await;
    if let Ok(names) = list {
        let now_ms = timeutil::now_epoch_ms();
        let _ = with_store(state, |store| store.cache_upstream_models(provider_id, &names, now_ms));
    }
}

/// Periodic upstream model-catalog fetch (REL-006): every configured sync
/// interval (0 disables it) each provider's catalog is fetched once and
/// cached. Provider edits and startup checks also refresh the cache, so this
/// task only keeps long-lived catalogs from going stale.
fn spawn_upstream_sync_scheduler(state: AppState) {
    tokio::spawn(async move {
        let tick_ms = std::env::var("LOCAL_API_RELAY_TEST_SYNC_TICK_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60_000);
        let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut last_sync_ms: i64 = timeutil::now_epoch_ms();
        loop {
            interval.tick().await;
            let settings = {
                let Ok(mut store) = state.store.lock() else {
                    continue;
                };
                match store.recovery_settings() {
                    Ok(settings) => settings,
                    Err(error) => {
                        eprintln!("local-api-relay: could not read sync settings: {error:#}");
                        continue;
                    }
                }
            };
            if settings.upstream_sync_interval_ms <= 0 {
                continue;
            }
            let now_ms = timeutil::now_epoch_ms();
            if now_ms - last_sync_ms < settings.upstream_sync_interval_ms {
                continue;
            }
            last_sync_ms = now_ms;
            let providers = {
                let Ok(mut store) = state.store.lock() else {
                    continue;
                };
                match store.provider_connections() {
                    Ok(providers) => providers,
                    Err(error) => {
                        eprintln!("local-api-relay: could not read providers: {error:#}");
                        continue;
                    }
                }
            };
            for (provider_id, base_url, api_key) in providers {
                let provider_state = state.clone();
                tokio::spawn(async move {
                    fetch_and_cache_provider_catalog(
                        &provider_state,
                        &provider_id,
                        &base_url,
                        &api_key,
                    )
                    .await;
                });
            }
        }
    });
}

async fn create_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateProviderRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let provider = with_configuration_store(&state, |store| {
        store.create_provider(&request.display_name, &request.base_url, &request.api_key)
    })?;
    Ok(no_store_json(
        StatusCode::CREATED,
        json!({
            "id": provider.id,
            "display_name": provider.display_name,
            "api_key_masked": provider.api_key_masked
        }),
    ))
}

async fn provider_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let provider =
        with_configuration_store(&state, |store| store.provider_configuration(&provider_id))?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "id": provider.id,
            "display_name": provider.display_name,
            "base_url": provider.base_url
        }),
    ))
}

/// The provider's cached upstream model catalog (REL-006): the model ids the
/// last successful fetch observed, with the fetch time. The catalog is a
/// diagnostic projection — it never feeds candidate selection directly.
async fn provider_cached_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let (models, fetched_at) = with_store(&state, |store| {
        let models = store.cached_upstream_models(&provider_id)?;
        let fetched_at = store.cached_upstream_models_fetched_at(&provider_id)?;
        Ok((models, fetched_at))
    })?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "provider_id": provider_id,
            "models": models,
            "fetched_at_ms": fetched_at
        }),
    ))
}

/// Fetches the provider's upstream catalog now and returns the fresh list
/// (REL-006). The catalog fetch is advisory: a failing fetch leaves the
/// previous cache in place and reports the normalized failure.
async fn refresh_provider_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let connection = with_configuration_store(&state, |store| {
        store.provider_connection(&provider_id)
    })?;
    let Some((base_url, api_key)) = connection else {
        return Err(AdminError::new(
            StatusCode::NOT_FOUND,
            "upstream provider does not exist",
        ));
    };
    let list = fetch_upstream_model_list(&state.upstream_client, &base_url, &api_key).await;
    match list {
        Ok(models) => {
            let now_ms = timeutil::now_epoch_ms();
            with_store(&state, |store| {
                store.cache_upstream_models(&provider_id, &models, now_ms)
            })?;
            Ok(no_store_json(
                StatusCode::OK,
                json!({
                    "provider_id": provider_id,
                    "models": models,
                    "fetched_at_ms": now_ms
                }),
            ))
        }
        Err(reason) => Ok(no_store_json(
            StatusCode::OK,
            json!({
                "provider_id": provider_id,
                "models": serde_json::Value::Null,
                "fetched_at_ms": serde_json::Value::Null,
                "error": reason
            }),
        )),
    }
}

async fn update_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
    Json(request): Json<CreateProviderRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let probes = with_configuration_store(&state, |store| {
        store.update_provider(
            &provider_id,
            &request.display_name,
            &request.base_url,
            &request.api_key,
        )
    })?;
    for probe in probes {
        let (available, http_status) = native_probe(&state.upstream_client, &probe).await;
        record_probe_result(
            &state,
            &probe.route_id,
            probe.quarantine_epoch,
            available,
            http_status,
        );
    }
    Ok(no_store_json(StatusCode::OK, json!({ "updated": true })))
}

async fn create_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateModelRouteRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let cost_multiplier =
        decimal_request_value(&request.cost_multiplier, "cost multiplier", "cost_multiplier")?;
    let probe = with_configuration_store(&state, |store| {
        store.create_model_route(
            &request.published_model_id,
            &request.provider_id,
            &request.upstream_model_name,
            &request.protocol,
            &cost_multiplier,
        )
    })?;
    let (available, http_status) = native_probe(&state.upstream_client, &probe).await;
    record_probe_result(
        &state,
        &probe.route_id,
        probe.quarantine_epoch,
        available,
        http_status,
    );
    let health = if available {
        RouteHealth::Available
    } else {
        RouteHealth::Unavailable
    };
    Ok(no_store_json(
        StatusCode::CREATED,
        json!({ "id": probe.route_id, "health": health.as_str() }),
    ))
}

async fn update_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<String>,
    Json(request): Json<CreateModelRouteRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let cost_multiplier =
        decimal_request_value(&request.cost_multiplier, "cost multiplier", "cost_multiplier")?;
    let probe = with_configuration_store(&state, |store| {
        store.update_model_route(
            &route_id,
            &request.published_model_id,
            &request.provider_id,
            &request.upstream_model_name,
            &request.protocol,
            &cost_multiplier,
        )
    })?;
    if let Some(probe) = probe {
        let (available, http_status) = native_probe(&state.upstream_client, &probe).await;
        record_probe_result(
            &state,
            &probe.route_id,
            probe.quarantine_epoch,
            available,
            http_status,
        );
    }
    Ok(no_store_json(StatusCode::OK, json!({ "updated": true })))
}

async fn check_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<String>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    {
        let mut in_flight = state.recovery_in_flight.lock().map_err(|_| {
            AdminError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "local state is unavailable",
            )
        })?;
        if !in_flight.insert(route_id.clone()) {
            return Err(AdminError::new(
                StatusCode::CONFLICT,
                "a recovery check is already in progress for this model route",
            ));
        }
    }
    let outcome = async {
        let probe = with_configuration_store(&state, |store| store.model_route_probe(&route_id))?;
        let (available, http_status) = native_probe(&state.upstream_client, &probe).await;
        record_probe_result(
            &state,
            &probe.route_id,
            probe.quarantine_epoch,
            available,
            http_status,
        );
        let health = if available {
            RouteHealth::Available
        } else {
            RouteHealth::Unavailable
        };
        Ok(no_store_json(
            StatusCode::OK,
            json!({ "health": health.as_str() }),
        ))
    }
    .await;
    if let Ok(mut in_flight) = state.recovery_in_flight.lock() {
        in_flight.remove(&route_id);
    }
    outcome
}

#[derive(Deserialize)]
struct CreatePublishedModelRequest {
    name: String,
    input_price_rmb: serde_json::Value,
    output_price_rmb: serde_json::Value,
    cached_input_price_rmb: serde_json::Value,
}

async fn create_published_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreatePublishedModelRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let input = decimal_request_value(&request.input_price_rmb, "input price", "input_price_rmb")?;
    let output =
        decimal_request_value(&request.output_price_rmb, "output price", "output_price_rmb")?;
    let cached = decimal_request_value(
        &request.cached_input_price_rmb,
        "cached input price",
        "cached_input_price_rmb",
    )?;
    with_configuration_store(&state, |store| {
        store.create_published_model(&request.name, &input, &output, &cached)
    })?;
    Ok(no_store_json(
        StatusCode::CREATED,
        json!({ "created": true, "id": request.name.trim() }),
    ))
}

async fn deprecate_published_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(model_id): Path<String>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    with_configuration_store(&state, |store| store.deprecate_published_model(&model_id))?;
    Ok(no_store_json(StatusCode::OK, json!({ "deprecated": true })))
}

async fn update_published_model_prices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(model_id): Path<String>,
    Json(request): Json<UpdatePricesRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let input =
        decimal_request_value(&request.input_price_rmb, "input price", "input_price_rmb")?;
    let output =
        decimal_request_value(&request.output_price_rmb, "output price", "output_price_rmb")?;
    let cached = decimal_request_value(
        &request.cached_input_price_rmb,
        "cached input price",
        "cached_input_price_rmb",
    )?;
    with_configuration_store(&state, |store| {
        store.update_published_model_prices(&model_id, &input, &output, &cached)
    })?;
    Ok(no_store_json(StatusCode::OK, json!({ "updated": true })))
}

async fn recovery_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let settings = with_store(&state, |store| store.recovery_settings())?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "base_interval_ms": settings.base_interval_ms,
            "doubling_limit": settings.doubling_limit,
            "first_event_timeout_ms": settings.first_event_timeout_ms,
            "stream_idle_timeout_ms": settings.stream_idle_timeout_ms,
            "nonstream_timeout_ms": settings.nonstream_timeout_ms,
            "freshness_interval_ms": settings.freshness_interval_ms,
            "quarantine_threshold": settings.quarantine_threshold,
            "upstream_sync_interval_ms": settings.upstream_sync_interval_ms
        }),
    ))
}

async fn update_recovery_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RecoverySettingsRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    with_configuration_store(&state, |store| {
        store.update_recovery_settings(
            request.base_interval_ms,
            request.doubling_limit,
            request.first_event_timeout_ms,
            request.stream_idle_timeout_ms,
            request.nonstream_timeout_ms,
            request.freshness_interval_ms,
            request.quarantine_threshold,
            request.upstream_sync_interval_ms,
        )
    })?;
    Ok(no_store_json(StatusCode::OK, json!({ "updated": true })))
}

async fn create_relay_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateRelayAccessKeyRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let key = with_configuration_store(&state, |store| {
        store.create_relay_access_key(&request.label, &request.model_route_ids)
    })?;
    Ok(no_store_json(
        StatusCode::CREATED,
        json!({
            "id": key.id,
            "label": key.label,
            "prefix": key.secret_prefix,
            "created_at": key.created_at,
            "model_route_ids": key.model_route_ids,
            "secret": key.secret
        }),
    ))
}

async fn update_relay_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
    Json(request): Json<CreateRelayAccessKeyRequest>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    with_configuration_store(&state, |store| {
        store.update_relay_access_key(&key_id, &request.label, &request.model_route_ids)
    })?;
    Ok(no_store_json(StatusCode::OK, json!({ "updated": true })))
}

async fn list_relay_access_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RelayAccessKeyListQuery>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let keys = with_store(&state, |store| {
        store.relay_access_keys(query.search.as_deref())
    })?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "data": keys.into_iter().map(|key| json!({
                "id": key.id,
                "prefix": key.secret_prefix,
                "label": key.label,
                "created_at": key.created_at,
                "revoked_at": key.revoked_at,
                "secret": key.secret,
                "model_route_ids": key.model_route_ids
            })).collect::<Vec<_>>()
        }),
    ))
}

async fn revoke_relay_access_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    with_configuration_store(&state, |store| store.revoke_relay_access_key(&key_id))?;
    Ok(no_store_json(StatusCode::OK, json!({ "revoked": true })))
}

#[derive(Deserialize)]
struct CallsUsageQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    window: Option<String>,
}

const DEFAULT_CALLS_PAGE_SIZE: i64 = 25;
const MAX_CALLS_PAGE_SIZE: i64 = 100;
const USAGE_WINDOWS: [&str; 6] = ["1h", "5h", "24h", "7d", "14d", "all"];
const DEFAULT_USAGE_WINDOW: &str = "24h";

/// The six fixed usage windows (OPS-008); unknown values fall back to 24h.
fn normalize_usage_window(raw: Option<&str>) -> String {
    let window = raw.unwrap_or(DEFAULT_USAGE_WINDOW);
    if USAGE_WINDOWS.contains(&window) {
        window.to_owned()
    } else {
        DEFAULT_USAGE_WINDOW.to_owned()
    }
}

async fn calls_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallsUsageQuery>,
) -> Result<Response, AdminError> {
    require_active_session(&state, &headers)?;
    let page = query.page.unwrap_or(0).max(0);
    let page_size = query
        .page_size
        .unwrap_or(DEFAULT_CALLS_PAGE_SIZE)
        .clamp(1, MAX_CALLS_PAGE_SIZE);
    let window = normalize_usage_window(query.window.as_deref());
    let result = with_store(&state, |store| store.call_record_page(page, page_size))?;
    let totals = with_store(&state, |store| store.usage_totals(&window, timeutil::now_epoch_ms()))?;
    let usage_integrity = with_store(&state, |store| store.usage_integrity(&window, timeutil::now_epoch_ms()))?;
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "windows": USAGE_WINDOWS,
            "window": window,
            "usage_integrity": {
                "complete": usage_integrity.complete,
                "gaps": usage_integrity.gaps.iter().map(usage_gap_json).collect::<Vec<_>>()
            },
            "totals": {
                "input_tokens": totals.input_tokens,
                "cached_input_tokens": totals.cached_input_tokens,
                "output_tokens": totals.output_tokens,
                "estimated_cost_rmb": totals.estimated_cost_rmb,
                "cache_hit_rate": totals.cache_hit_rate,
                "models": totals.models.iter().map(|model| json!({
                    "published_model_name": model.published_model_name,
                    "input_tokens": model.input_tokens,
                    "cached_input_tokens": model.cached_input_tokens,
                    "output_tokens": model.output_tokens,
                    "estimated_cost_rmb": model.estimated_cost_rmb,
                    "providers": model.providers.iter().map(|provider| json!({
                        "provider_id": provider.provider_id,
                        "provider_name": provider.provider_name,
                        "input_tokens": provider.input_tokens,
                        "cached_input_tokens": provider.cached_input_tokens,
                        "output_tokens": provider.output_tokens,
                        "estimated_cost_rmb": provider.estimated_cost_rmb
                    })).collect::<Vec<_>>()
                })).collect::<Vec<_>>()
            },
            "calls": result.calls.iter().map(|call| json!({
                "id": call.id,
                "created_at_ms": call.created_at_ms,
                "published_model_name": call.published_model_name,
                "protocol": call.protocol,
                "streamed": call.streamed,
                "succeeded": call.succeeded,
                "success_provider_id": call.success_provider_id,
                "success_provider_name": call.success_provider_name,
                "input_tokens": call.input_tokens,
                "cached_input_tokens": call.cached_input_tokens,
                "output_tokens": call.output_tokens,
                "estimated_cost_rmb": call.estimated_cost_rmb,
                "completion_ms": call.completion_ms,
                "first_token_ms": call.first_token_ms,
                "attempts": call.attempts.iter().map(|attempt| json!({
                    "sequence": attempt.sequence,
                    "route_id": attempt.route_id,
                    "provider_id": attempt.provider_id,
                    "provider_name": attempt.provider_name,
                    "started_at_ms": attempt.started_at_ms,
                    "duration_ms": attempt.duration_ms,
                    "http_status": attempt.http_status,
                    "failure_category": attempt.failure_category,
                    "commit_phase": attempt.commit_phase,
                    "outcome": attempt.outcome
                })).collect::<Vec<_>>()
            })).collect::<Vec<_>>(),
            "page": page,
            "page_size": page_size,
            "total": result.total
        }),
    ))
}

async fn list_relay_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, RelayError> {
    let key_id = require_relay_access_key(&state, &headers)?;
    let route_health = with_relay_store(&state, |store| store.eligible_model_route_health(&key_id))?;
    let overrides = overrides_snapshot(&state);
    // A model is listed only while it has at least one route whose effective
    // health is Available (API-003); the in-memory overlay decides when the
    // persisted health row could not be written (DATA-005).
    let mut models: Vec<String> = route_health
        .into_iter()
        .filter(|route| {
            effective_health(&overrides, &route.route_id, route.health) == RouteHealth::Available
        })
        .map(|route| route.model_name)
        .collect();
    models.dedup();
    Ok(no_store_json(
        StatusCode::OK,
        json!({
            "object": "list",
            "data": models.into_iter().map(|id| json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "local-api-relay"
            })).collect::<Vec<_>>()
        }),
    ))
}

/// Replaces `content: null` on client messages with an empty string before the
/// request is forwarded upstream. Reasoning-model clients round-trip an
/// interrupted assistant turn as `{"role":"assistant","content":null,
/// "reasoning_content": ...}` while the model was still thinking, and several
/// upstream gateways reject a null `content` at deserialization with a 400.
/// The value is replaced in place so the client's field order is preserved;
/// non-null `content` (strings or content-block arrays) and messages without a
/// `content` key are left untouched. Empty string is semantically equivalent
/// for conforming upstreams, so this applies unconditionally to the Chat
/// Completions `messages` array and the Responses `input` array.
fn normalize_null_message_content(request: &mut serde_json::Value) {
    let Some(object) = request.as_object_mut() else {
        return;
    };
    for key in ["messages", "input"] {
        let Some(array) = object.get_mut(key).and_then(serde_json::Value::as_array_mut) else {
            continue;
        };
        for item in array {
            let Some(message) = item.as_object_mut() else {
                continue;
            };
            if message.get("content").is_some_and(serde_json::Value::is_null) {
                message.insert("content".to_owned(), serde_json::Value::String(String::new()));
            }
        }
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, RelayError> {
    let key_id = require_relay_access_key(&state, &headers)?;
    let body = body.map_err(|_| {
        RelayError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body is too large",
            "invalid_request_error",
            None,
            None,
        )
    })?;
    let mut request: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        RelayError::new(
            StatusCode::BAD_REQUEST,
            "request body must be valid JSON",
            "invalid_request_error",
            None,
            None,
        )
    })?;
    let object = request.as_object_mut().ok_or_else(|| {
        RelayError::new(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
            "invalid_request_error",
            None,
            None,
        )
    })?;
    let published_model_name = object
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            RelayError::new(
                StatusCode::BAD_REQUEST,
                "model must be a non-empty string",
                "invalid_request_error",
                Some("model"),
                None,
            )
        })?
        .to_owned();
    if !object
        .get("messages")
        .is_some_and(serde_json::Value::is_array)
    {
        return Err(RelayError::new(
            StatusCode::BAD_REQUEST,
            "messages must be an array",
            "invalid_request_error",
            Some("messages"),
            None,
        ));
    }
    let stream = match object.get("stream") {
        Some(serde_json::Value::Bool(stream)) => *stream,
        Some(_) => {
            return Err(RelayError::new(
                StatusCode::BAD_REQUEST,
                "stream must be a boolean",
                "invalid_request_error",
                Some("stream"),
                None,
            ));
        }
        None => {
            object.insert("stream".to_owned(), serde_json::Value::Bool(false));
            false
        }
    };
    // Deepseek-style assistant round-trips may carry `content: null` while the
    // model was still thinking; normalize it before the upstream sees the body.
    normalize_null_message_content(&mut request);
    let candidates = with_relay_store(&state, |store| {
        store.eligible_chat_routes(&key_id, &published_model_name)
    })?;
    let candidates = effective_candidates(&state, candidates);
    if candidates.is_empty() {
        let configured = with_relay_store(&state, |store| {
            store.has_eligible_chat_route(&key_id, &published_model_name)
        })?;
        if configured {
            return Err(RelayError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no eligible model route is currently available",
                "api_error",
                None,
                Some("no_available_route"),
            ));
        }
        return Err(RelayError::new(
            StatusCode::NOT_FOUND,
            "the requested published model is not available for this relay access key",
            "invalid_request_error",
            Some("model"),
            Some("model_not_found"),
        ));
    }
    if stream {
        relay_streaming(
            &state,
            &candidates,
            StreamProtocol::ChatCompletions,
            &published_model_name,
            &mut request,
        )
        .await
    } else {
        relay_non_streaming(
            &state,
            &candidates,
            StreamProtocol::ChatCompletions,
            &published_model_name,
            &mut request,
        )
        .await
    }
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, RelayError> {
    let key_id = require_relay_access_key(&state, &headers)?;
    let body = body.map_err(|_| {
        RelayError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body is too large",
            "invalid_request_error",
            None,
            None,
        )
    })?;
    let mut request: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        RelayError::new(
            StatusCode::BAD_REQUEST,
            "request body must be valid JSON",
            "invalid_request_error",
            None,
            None,
        )
    })?;
    let object = request.as_object_mut().ok_or_else(|| {
        RelayError::new(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
            "invalid_request_error",
            None,
            None,
        )
    })?;
    let published_model_name = object
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            RelayError::new(
                StatusCode::BAD_REQUEST,
                "model must be a non-empty string",
                "invalid_request_error",
                Some("model"),
                None,
            )
        })?
        .to_owned();
    if !object
        .get("input")
        .is_some_and(|input| input.is_string() || input.is_array())
    {
        return Err(RelayError::new(
            StatusCode::BAD_REQUEST,
            "input must be a string or array",
            "invalid_request_error",
            Some("input"),
            None,
        ));
    }
    let stream = match object.get("stream") {
        Some(serde_json::Value::Bool(stream)) => *stream,
        Some(_) => {
            return Err(RelayError::new(
                StatusCode::BAD_REQUEST,
                "stream must be a boolean",
                "invalid_request_error",
                Some("stream"),
                None,
            ));
        }
        None => {
            object.insert("stream".to_owned(), serde_json::Value::Bool(false));
            false
        }
    };
    // Deepseek-style assistant round-trips may carry `content: null` while the
    // model was still thinking; normalize it before the upstream sees the body.
    normalize_null_message_content(&mut request);
    let candidates = with_relay_store(&state, |store| {
        store.eligible_responses_routes(&key_id, &published_model_name)
    })?;
    let candidates = effective_candidates(&state, candidates);
    if candidates.is_empty() {
        let configured = with_relay_store(&state, |store| {
            store.has_eligible_responses_route(&key_id, &published_model_name)
        })?;
        if configured {
            return Err(RelayError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no eligible model route is currently available",
                "api_error",
                None,
                Some("no_available_route"),
            ));
        }
        return Err(RelayError::new(
            StatusCode::NOT_FOUND,
            "the requested published model is not available for this relay access key",
            "invalid_request_error",
            Some("model"),
            Some("model_not_found"),
        ));
    }
    if stream {
        relay_streaming(
            &state,
            &candidates,
            StreamProtocol::Responses,
            &published_model_name,
            &mut request,
        )
        .await
    } else {
        relay_non_streaming(
            &state,
            &candidates,
            StreamProtocol::Responses,
            &published_model_name,
            &mut request,
        )
        .await
    }
}

async fn relay_unauthorized() -> Response {
    RelayError::unauthorized().into_response()
}

fn require_relay_access_key(state: &AppState, headers: &HeaderMap) -> Result<String, RelayError> {
    let Some(authorization) = headers
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
    else {
        return Err(RelayError::unauthorized());
    };
    let Some(secret) = authorization
        .strip_prefix("Bearer ")
        .filter(|secret| !secret.is_empty())
    else {
        return Err(RelayError::unauthorized());
    };
    with_relay_store(state, |store| store.authenticate_relay_access_key(secret))?
        .ok_or_else(RelayError::unauthorized)
}

fn with_relay_store<T>(
    state: &AppState,
    operation: impl FnOnce(&mut Store) -> anyhow::Result<T>,
) -> Result<T, RelayError> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| RelayError::local_state_unavailable())?;
    operation(&mut store).map_err(|_| RelayError::local_state_unavailable())
}

fn relay_upstream_transport_error(error: reqwest::Error) -> RelayError {
    if error.is_timeout() {
        relay_upstream_timeout_error()
    } else {
        RelayError::new(
            StatusCode::BAD_GATEWAY,
            "could not reach the upstream service",
            "api_error",
            None,
            Some("upstream_unavailable"),
        )
    }
}

async fn send_upstream_request(
    state: &AppState,
    stream: bool,
    endpoint: String,
    api_key: &str,
    request: &serde_json::Value,
) -> Result<reqwest::Response, RelayError> {
    let settings = with_relay_store(state, |store| store.recovery_settings())
        .map_err(|_| RelayError::local_state_unavailable())?;
    let upstream_client = if stream {
        &state.streaming_upstream_client
    } else {
        &state.upstream_client
    };
    let upstream_request = upstream_client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(request);
    let head_timeout_ms = if stream {
        settings.first_event_timeout_ms
    } else {
        settings.nonstream_timeout_ms
    };
    let upstream = tokio::time::timeout(
        Duration::from_millis(head_timeout_ms.max(1) as u64),
        upstream_request.send(),
    )
    .await
    .map_err(|_| relay_upstream_timeout_error())?;

    upstream.map_err(relay_upstream_transport_error)
}

fn relay_upstream_timeout_error() -> RelayError {
    RelayError::new(
        StatusCode::GATEWAY_TIMEOUT,
        "upstream request timed out",
        "api_error",
        None,
        Some("upstream_timeout"),
    )
}

/// A pre-commit failure discovered after the upstream response arrived: the
/// shared fallback loop records the attempt, quarantines the current route,
/// and moves on to the next candidate in the original order.
struct PreCommitFailure {
    error: RelayError,
    category: &'static str,
    http_status: Option<i64>,
}

/// The outcome of one success-path handler inside the shared pre-commit
/// fallback loop.
enum PreCommitOutcome {
    /// A downstream response was produced; the recorder was either finalized
    /// or moved into the response's stream.
    Committed(Response),
    /// The attempt failed before commit; the recorder returns to the loop so
    /// it can record the attempt, quarantine the route, and fall through to
    /// the next candidate.
    Fallthrough(PreCommitFailure, CallRecorder),
}

/// Convenience constructor for a fall-through: pairs the failure with the
/// recorder returning to the shared loop, so success-path closures only name
/// the failure itself.
fn precommit_fallthrough(
    recorder: CallRecorder,
    failure: PreCommitFailure,
) -> PreCommitOutcome {
    PreCommitOutcome::Fallthrough(failure, recorder)
}

/// Shared pre-commit fallback loop for the non-streaming and streaming relay
/// paths (ROUTE-011/012/013): begin the attempt, substitute the upstream model
/// name, send the request, and hand a successful response to `on_success`.
/// Every attributable pre-commit failure quarantines the current route and
/// falls through to the next candidate in the original deterministic order; a
/// health-neutral upstream 4xx ends the call without Fallback (ROUTE-009);
/// candidate exhaustion returns the final safe normalized error. `on_success`
/// returns `PreCommitOutcome::Committed` once it produced the downstream
/// response, or `Fallthrough` for the loop to record and continue. Failures
/// after the first event has committed never enter this loop (ROUTE-014/015).
async fn relay_precommit_fallback_loop<F>(
    state: &AppState,
    candidates: &[ModelRouteCandidate],
    protocol: StreamProtocol,
    published_model_name: &str,
    request: &mut serde_json::Value,
    stream: bool,
    mut on_success: F,
) -> Result<Response, RelayError>
where
    F: for<'a> FnMut(
        &'a AppState,
        CallRecorder,
        StatusCode,
        reqwest::Response,
    ) -> futures_util::future::BoxFuture<'a, PreCommitOutcome>,
{
    let mut recorder = CallRecorder::new(state.clone(), published_model_name, protocol, stream);
    let mut final_error: Option<RelayError> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        recorder.begin_model_route_attempt(candidate);
        request["model"] = serde_json::Value::String(candidate.upstream_model_name.clone());
        let endpoint = upstream_endpoint(protocol, &candidate.base_url);
        let outcome = if index + 1 == candidates.len() {
            ModelRouteAttemptOutcome::Failed
        } else {
            ModelRouteAttemptOutcome::Fallback
        };
        let upstream = match send_upstream_request(
            state,
            stream,
            endpoint,
            &candidate.api_key,
            request,
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                let category = relay_error_category(&error);
                recorder.finish_model_route_attempt(
                    None,
                    Some(category.to_owned()),
                    CommitPhase::PreCommit,
                    outcome,
                );
                // A transport failure has no safe HTTP status (OPS-013).
                quarantine_route(state, &candidate.route_id, category, None);
                final_error = Some(error);
                continue;
            }
        };
        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        if !status.is_success() {
            let error = RelayError::new(status, "upstream request failed", "api_error", None, None);
            if is_attributable_upstream_status(status) {
                let category = attributable_http_category(status);
                recorder.finish_model_route_attempt(
                    Some(status.as_u16() as i64),
                    Some(category.to_owned()),
                    CommitPhase::PreCommit,
                    outcome,
                );
                quarantine_route(
                    state,
                    &candidate.route_id,
                    category,
                    Some(status.as_u16() as i64),
                );
                final_error = Some(error);
                continue;
            }
            // Health-neutral upstream 4xx ends the call without Fallback
            // (ROUTE-009); the attempt is still part of the call record.
            recorder.finish_model_route_attempt(
                Some(status.as_u16() as i64),
                Some(health_neutral_http_category().to_owned()),
                CommitPhase::PreCommit,
                ModelRouteAttemptOutcome::Failed,
            );
            let record = recorder.finalize(false);
            write_call_record(state, &record);
            return Err(error);
        }
        match on_success(state, recorder, status, upstream).await {
            PreCommitOutcome::Committed(response) => {
                // A real call succeeded on this route: its consecutive
                // failure counter clears (REL-004).
                if let Ok(mut store) = state.store.lock() {
                    let _ = store.clear_route_failure_count(&candidate.route_id);
                }
                return Ok(response);
            }
            PreCommitOutcome::Fallthrough(failure, mut next_recorder) => {
                next_recorder.finish_model_route_attempt(
                    failure.http_status,
                    Some(failure.category.to_owned()),
                    CommitPhase::PreCommit,
                    outcome,
                );
                quarantine_route(state, &candidate.route_id, failure.category, failure.http_status);
                final_error = Some(failure.error);
                recorder = next_recorder;
            }
        }
    }
    let record = recorder.finalize(false);
    write_call_record(state, &record);
    Err(final_error.unwrap_or_else(relay_candidates_exhausted_error))
}

/// Relays a non-streaming request across the deterministic candidate set.
/// Each attributable pre-commit failure quarantines the current route and
/// tries the next candidate; a successful fully validated response is
/// committed with the published model identity restored (ROUTE-011/012). The
/// whole client call becomes exactly one call record with its ordered attempt
/// chain (OPS-001/OPS-003).
async fn relay_non_streaming(
    state: &AppState,
    candidates: &[ModelRouteCandidate],
    protocol: StreamProtocol,
    published_model_name: &str,
    request: &mut serde_json::Value,
) -> Result<Response, RelayError> {
    let published_model_name = published_model_name.to_owned();
    relay_precommit_fallback_loop(
        state,
        candidates,
        protocol,
        &published_model_name,
        request,
        false,
        |state, mut recorder, status, upstream| {
            let published_model_name = published_model_name.clone();
            let body_deadline = with_relay_store(state, |store| store.recovery_settings())
                .map(|settings| Duration::from_millis(settings.nonstream_timeout_ms.max(1) as u64))
                .unwrap_or_else(|_| Duration::from_secs(120));
            Box::pin(async move {
                let body = match tokio::time::timeout(body_deadline, upstream.bytes()).await {
                    Ok(Ok(body)) => body,
                    Ok(Err(error)) => {
                        let error = relay_upstream_transport_error(error);
                        let category = relay_error_category(&error);
                        return precommit_fallthrough(
                            recorder,
                            PreCommitFailure {
                                error,
                                category,
                                http_status: None,
                            },
                        );
                    }
                    Err(_) => {
                        let error = relay_upstream_timeout_error();
                        let category = relay_error_category(&error);
                        return precommit_fallthrough(
                            recorder,
                            PreCommitFailure {
                                error,
                                category,
                                http_status: None,
                            },
                        );
                    }
                };
                let mut response: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(response) => response,
                    Err(_) => {
                        return precommit_fallthrough(
                            recorder,
                            PreCommitFailure {
                                error: invalid_upstream_response_error(protocol),
                                category: "invalid_upstream_response",
                                http_status: Some(status.as_u16() as i64),
                            },
                        );
                    }
                };
                if !validate_complete_response(protocol, &response) {
                    return precommit_fallthrough(
                        recorder,
                        PreCommitFailure {
                            error: invalid_upstream_response_error(protocol),
                            category: "invalid_upstream_response",
                            http_status: Some(status.as_u16() as i64),
                        },
                    );
                }
                if is_semantic_failure(protocol, &response) {
                    return precommit_fallthrough(
                        recorder,
                        PreCommitFailure {
                            error: RelayError::new(
                                StatusCode::BAD_GATEWAY,
                                "upstream returned a failed Responses response",
                                "api_error",
                                None,
                                Some("upstream_semantic_failure"),
                            ),
                            category: "upstream_semantic_failure",
                            http_status: Some(status.as_u16() as i64),
                        },
                    );
                }
                response
                    .as_object_mut()
                    .expect("validated response is an object")
                    .insert(
                        "model".to_owned(),
                        serde_json::Value::String(published_model_name),
                    );
                recorder.finish_model_route_attempt(
                    Some(status.as_u16() as i64),
                    None,
                    CommitPhase::Committed,
                    ModelRouteAttemptOutcome::Success,
                );
                recorder.note_usage(extract_usage(protocol, &response));
                let record = recorder.finalize(true);
                write_call_record(state, &record);
                PreCommitOutcome::Committed(no_store_json(status, response))
            })
        },
    )
    .await
}

/// Relays a streaming request across the deterministic candidate set. The
/// first native protocol event is validated before committing; pre-commit
/// failures quarantine the route and try the next candidate without leaking
/// bytes. After commit, any failure quarantines the current route and ends
/// the downstream stream without retrying or splicing another generation
/// (ROUTE-013/014/015). The committed attempt's outcome is resolved when the
/// stream ends, and the whole client call becomes exactly one call record.
async fn relay_streaming(
    state: &AppState,
    candidates: &[ModelRouteCandidate],
    protocol: StreamProtocol,
    published_model_name: &str,
    request: &mut serde_json::Value,
) -> Result<Response, RelayError> {
    let published_model_name = published_model_name.to_owned();
    relay_precommit_fallback_loop(
        state,
        candidates,
        protocol,
        &published_model_name,
        request,
        true,
        |state, mut recorder, status, upstream| {
            let published_model_name = published_model_name.clone();
            let settings = with_relay_store(state, |store| store.recovery_settings())
                .unwrap_or_else(|_| crate::store::RecoverySettings::default());
            let first_event_deadline =
                Duration::from_millis(settings.first_event_timeout_ms.max(1) as u64);
            let idle_timeout = Duration::from_millis(settings.stream_idle_timeout_ms.max(1) as u64);
            Box::pin(async move {
                if !upstream
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(is_event_stream_content_type)
                {
                    return precommit_fallthrough(
                        recorder,
                        PreCommitFailure {
                            error: RelayError::new(
                                StatusCode::BAD_GATEWAY,
                                "upstream did not return an SSE stream",
                                "api_error",
                                None,
                                Some("invalid_upstream_response"),
                            ),
                            category: "invalid_upstream_response",
                            http_status: Some(status.as_u16() as i64),
                        },
                    );
                }
                let relay = match tokio::time::timeout(
                    first_event_deadline,
                    SseRelay::prime(upstream, protocol, published_model_name, idle_timeout),
                )
                .await
                {
                    Ok(Ok(relay)) => relay,
                    Ok(Err(error)) => {
                        let category = relay_error_category(&error);
                        return precommit_fallthrough(
                            recorder,
                            PreCommitFailure {
                                error,
                                category,
                                http_status: Some(status.as_u16() as i64),
                            },
                        );
                    }
                    Err(_) => {
                        let error = relay_upstream_timeout_error();
                        let category = relay_error_category(&error);
                        return precommit_fallthrough(
                            recorder,
                            PreCommitFailure {
                                error,
                                category,
                                http_status: Some(status.as_u16() as i64),
                            },
                        );
                    }
                };
                recorder.mark_committed(status.as_u16() as i64);
                let http_status = status.as_u16() as i64;
                let body = Body::from_stream(futures_util::stream::unfold(
                    (relay, recorder, http_status),
                    |(mut relay, mut recorder, http_status)| async move {
                        match relay.next_body_chunk().await {
                            Ok(Some(chunk)) => {
                                recorder.note_first_chunk();
                                recorder.note_usage(relay.captured_usage.take());
                                Some((
                                    Ok::<Bytes, Infallible>(chunk),
                                    (relay, recorder, http_status),
                                ))
                            }
                            Ok(None) => {
                                // The terminal protocol event was relayed intact. A
                                // Responses terminal that reported a semantic failure
                                // is recorded as a failed call and quarantines the
                                // committed route (API-011/ROUTE-008); otherwise the
                                // committed attempt resolves to success.
                                recorder.note_usage(relay.captured_usage.take());
                                if relay.terminal_semantic_failure {
                                    recorder.resolve_committed_model_route_attempt(
                                        ModelRouteAttemptOutcome::StreamTerminated,
                                        Some("upstream_semantic_failure".to_owned()),
                                    );
                                    let record = recorder.finalize(false);
                                    write_call_record(&recorder.state, &record);
                                    if let Some(route_id) = recorder.committed_route_id() {
                                        // The upstream SSE response already carried
                                        // this safe HTTP status (OPS-013).
                                        quarantine_route(
                                            &recorder.state,
                                            route_id,
                                            "upstream_semantic_failure",
                                            Some(http_status),
                                        );
                                    }
                                } else {
                                    recorder.resolve_committed_model_route_attempt(ModelRouteAttemptOutcome::Success, None);
                                    let record = recorder.finalize(true);
                                    write_call_record(&recorder.state, &record);
                                }
                                None
                            }
                            Err(error) => {
                                // Post-commit failure: quarantine the committed route
                                // and end the stream without splicing (ROUTE-014/015).
                                // The upstream SSE response already carried this safe
                                // HTTP status (OPS-013).
                                let category = relay_error_category(&error);
                                recorder.resolve_committed_model_route_attempt(
                                    ModelRouteAttemptOutcome::StreamTerminated,
                                    Some(category.to_owned()),
                                );
                                let record = recorder.finalize(false);
                                write_call_record(&recorder.state, &record);
                                if let Some(route_id) = recorder.committed_route_id() {
                                    // An in-flight stream idle timeout is
                                    // health-neutral (REL-001): the route had
                                    // already produced a valid stream, so a
                                    // pause does not prove an upstream fault.
                                    if category != "upstream_timeout" {
                                        quarantine_route(
                                            &recorder.state,
                                            route_id,
                                            category,
                                            Some(http_status),
                                        );
                                    }
                                }
                                None
                            }
                        }
                    },
                ));
                let mut response = Response::new(body);
                *response.status_mut() = status;
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                PreCommitOutcome::Committed(response)
            })
        },
    )
    .await
}

fn upstream_endpoint(protocol: StreamProtocol, base_url: &str) -> String {
    match protocol {
        StreamProtocol::ChatCompletions => format!("{base_url}/chat/completions"),
        StreamProtocol::Responses => format!("{base_url}/responses"),
    }
}

fn relay_candidates_exhausted_error() -> RelayError {
    RelayError::new(
        StatusCode::BAD_GATEWAY,
        "all eligible upstream attempts failed",
        "api_error",
        None,
        Some("all_upstream_attempts_failed"),
    )
}

fn invalid_upstream_response_error(protocol: StreamProtocol) -> RelayError {
    match protocol {
        StreamProtocol::ChatCompletions => RelayError::new(
            StatusCode::BAD_GATEWAY,
            "upstream returned an invalid Chat Completions response",
            "api_error",
            None,
            None,
        ),
        StreamProtocol::Responses => RelayError::new(
            StatusCode::BAD_GATEWAY,
            "upstream returned an invalid Responses response",
            "api_error",
            None,
            Some("invalid_upstream_response"),
        ),
    }
}

fn validate_complete_response(protocol: StreamProtocol, body: &serde_json::Value) -> bool {
    match protocol {
        StreamProtocol::ChatCompletions => is_complete_chat_completion(body),
        StreamProtocol::Responses => is_complete_responses_response(body),
    }
}

fn is_semantic_failure(protocol: StreamProtocol, body: &serde_json::Value) -> bool {
    protocol == StreamProtocol::Responses && responses_semantic_failure(body)
}

/// The allowlisted upstream statuses that are attributable to the route:
/// `401`, `403`, `404`, `429`, and all `5xx` (ROUTE-007). Any other upstream
/// `4xx` is health-neutral and must not trigger Fallback (ROUTE-009).
fn is_attributable_upstream_status(status: StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 401 | 403 | 404 | 429)
}

fn attributable_http_category(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "upstream_http_401",
        403 => "upstream_http_403",
        404 => "upstream_http_404",
        429 => "upstream_http_429",
        _ => "upstream_http_5xx",
    }
}

fn relay_error_category(error: &RelayError) -> &'static str {
    match error.code {
        Some("upstream_timeout") => "upstream_timeout",
        Some("upstream_unavailable") => "upstream_connection",
        Some("invalid_upstream_response") => "invalid_upstream_response",
        Some("upstream_semantic_failure") => "upstream_semantic_failure",
        _ => "upstream_failure",
    }
}

/// Stable normalized category for health-neutral upstream 4xx responses that
/// must not trigger Fallback or quarantine (ROUTE-009).
fn health_neutral_http_category() -> &'static str {
    "upstream_http_4xx"
}

/// Extracts the tokens a successful route itself reported, mapped from each
/// protocol's native usage shape. Returns None when the upstream reported no
/// usage, which stays unknown in the call record (OPS-004).
fn extract_usage(protocol: StreamProtocol, payload: &serde_json::Value) -> Option<Usage> {
    let usage = payload.get("usage")?;
    let input_tokens = match protocol {
        StreamProtocol::ChatCompletions => usage.get("prompt_tokens"),
        StreamProtocol::Responses => usage.get("input_tokens"),
    }?
    .as_i64()?;
    let output_tokens = match protocol {
        StreamProtocol::ChatCompletions => usage.get("completion_tokens"),
        StreamProtocol::Responses => usage.get("output_tokens"),
    }?
    .as_i64()?;
    let cached_input_tokens = match protocol {
        StreamProtocol::ChatCompletions => usage.pointer("/prompt_tokens_details/cached_tokens"),
        StreamProtocol::Responses => usage.pointer("/input_tokens_details/cached_tokens"),
    }
    .and_then(serde_json::Value::as_i64)
    .unwrap_or(0);
    Some(Usage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
    })
}

/// Best-effort call-record persistence: a failure must not break the relay
/// response (DATA-004). A persistence failure opens a durable usage gap and
/// marks the Storage status; a successful re-write closes the gap (inside the
/// store transaction) and counts toward the OPS-012 recovery condition.
fn write_call_record(state: &AppState, record: &store::NewCallRecord) {
    let result = {
        let Ok(mut store) = state.store.lock() else {
            return;
        };
        match store.record_call(record) {
            Ok(()) => Ok(()),
            Err(error) => {
                eprintln!("local-api-relay: could not record call: {error:#}");
                let _ = store.record_usage_gap(
                    store::OPERATIONAL_CATEGORY_CALL_RECORDS,
                    timeutil::now_epoch_ms(),
                );
                Err(error)
            }
        }
    };
    match result {
        Ok(()) => mark_storage_category_recovered(state, store::OPERATIONAL_CATEGORY_CALL_RECORDS),
        Err(error) => {
            mark_storage_category_failed(
                state,
                store::OPERATIONAL_CATEGORY_CALL_RECORDS,
                &error,
                Some(1),
            );
        }
    }
}

/// Best-effort health transition: the route becomes unavailable in memory even
/// if its health history write fails, so a failed persistence never leaves a
/// known-bad route in the candidate set (DATA-005). The persistence failure
/// marks the Storage status; the in-memory override keeps the route excluded
/// until a successful health write or a restart.
fn quarantine_route(
    state: &AppState,
    route_id: &str,
    failure_category: &str,
    http_status: Option<i64>,
) {
    let result = {
        let Ok(mut store) = state.store.lock() else {
            return;
        };
        store.quarantine_route(route_id, failure_category, http_status)
    };
    match result {
        Ok(()) => {
            if let Ok(mut overrides) = state.route_health_override.lock() {
                overrides.remove(route_id);
            }
            // The quarantine itself is a successful health-history write, so it
            // counts toward the same-category recovery condition (OPS-012).
            mark_storage_category_recovered(state, store::OPERATIONAL_CATEGORY_ROUTE_HEALTH);
        }
        Err(error) => {
            eprintln!(
                "local-api-relay: could not quarantine model route {route_id}: {error:#}"
            );
            if let Ok(mut overrides) = state.route_health_override.lock() {
                overrides.insert(
                    route_id.to_owned(),
                    RouteHealthOverride {
                        state: RouteHealth::Unavailable,
                        failure_category: Some(failure_category.to_owned()),
                    },
                );
            }
            mark_storage_category_failed(
                state,
                store::OPERATIONAL_CATEGORY_ROUTE_HEALTH,
                &error,
                Some(1),
            );
        }
    }
}

/// Best-effort probe-result persistence used by startup, recovery, manual
/// checks, and route/provider edits. The health transition applies in memory
/// first; a persistence failure surfaces as Storage Degraded and leaves the
/// in-memory override until a later write succeeds (DATA-005).
fn record_probe_result(
    state: &AppState,
    route_id: &str,
    quarantine_epoch: i64,
    available: bool,
    http_status: Option<i64>,
) {
    let result = {
        let Ok(mut store) = state.store.lock() else {
            return;
        };
        store.record_probe_result(route_id, quarantine_epoch, available, http_status)
    };
    match result {
        Ok(()) => {
            if let Ok(mut overrides) = state.route_health_override.lock() {
                overrides.remove(route_id);
            }
            mark_storage_category_recovered(state, store::OPERATIONAL_CATEGORY_ROUTE_HEALTH);
        }
        Err(error) => {
            eprintln!(
                "local-api-relay: could not record probe result for route {route_id}: {error:#}"
            );
            if let Ok(mut overrides) = state.route_health_override.lock() {
                overrides.insert(
                    route_id.to_owned(),
                    RouteHealthOverride {
                        state: if available {
                            RouteHealth::Available
                        } else {
                            RouteHealth::Unavailable
                        },
                        failure_category: if available {
                            None
                        } else {
                            Some("native_check_failed".to_owned())
                        },
                    },
                );
            }
            mark_storage_category_failed(
                state,
                store::OPERATIONAL_CATEGORY_ROUTE_HEALTH,
                &error,
                Some(1),
            );
        }
    }
}

/// The candidates whose effective health is Available: the persisted health
/// row overridden in memory where the last health write did not persist
/// (DATA-005).
fn effective_candidates(
    state: &AppState,
    candidates: Vec<store::ModelRouteCandidate>,
) -> Vec<store::ModelRouteCandidate> {
    let overrides = overrides_snapshot(state);
    candidates
        .into_iter()
        .filter(|candidate| {
            effective_health(&overrides, &candidate.route_id, candidate.health)
                == RouteHealth::Available
        })
        .collect()
}

/// Snapshot of the in-memory route health overrides (usually empty), so callers
/// never hold the mutex while building responses.
fn overrides_snapshot(state: &AppState) -> HashMap<String, RouteHealthOverride> {
    state
        .route_health_override
        .lock()
        .map(|overrides| overrides.clone())
        .unwrap_or_default()
}

/// The health a route should report: the persisted row overridden in memory
/// where the last health write did not persist (DATA-005).
fn effective_health(
    overrides: &HashMap<String, RouteHealthOverride>,
    route_id: &str,
    persisted_health: RouteHealth,
) -> RouteHealth {
    overrides
        .get(route_id)
        .map(|override_| override_.state)
        .unwrap_or(persisted_health)
}

fn usage_gap_json(gap: &store::UsageGap) -> serde_json::Value {
    json!({
        "kind": gap.kind,
        "category": gap.category,
        "started_at_ms": gap.started_at_ms,
        "ended_at_ms": gap.ended_at_ms,
        "lost_records": gap.lost_records
    })
}

/// Persists an operational event through the store, which emits it to standard
/// error and the managed log before writing the 14-day history. The
/// emit-plus-persist composition lives in `Store::record_event`; this is only
/// the AppState-to-Store adaptation (OPS-018).
fn persist_operational_event(
    state: &AppState,
    section: &str,
    severity: &str,
    code: &str,
    correlation_id: Option<&str>,
    payload: &serde_json::Value,
) {
    let _ = with_store(state, |store| {
        store.record_event(section, severity, code, correlation_id, payload);
        Ok(())
    });
}

/// Marks `category` as failing to persist: the shared Storage status moves to
/// Degraded (or keeps an existing Degraded/Not ready state) and the category's
/// known lost count grows (OPS-011). A None lost count means unknown. The
/// Healthy-to-Degraded transition emits one `storage.degraded` event so the
/// 14-day history records the episode (OPS-010).
fn mark_storage_category_failed(
    state: &AppState,
    category: &str,
    error: &anyhow::Error,
    lost_records: Option<u64>,
) {
    let now = timeutil::now_epoch();
    let error_text = format!("{error:#}");
    let transitioned_to_degraded = {
        let Ok(mut health) = state.storage_health.lock() else {
            return;
        };
        match health.categories.get_mut(category) {
            Some(failure) => {
                failure.error = error_text.clone();
                failure.lost_records = match (failure.lost_records, lost_records) {
                    (Some(known), Some(lost)) => Some(known + lost),
                    _ => None,
                };
            }
            None => {
                health.categories.insert(
                    category.to_owned(),
                    StorageCategoryFailure {
                        since: now,
                        error: error_text.clone(),
                        lost_records,
                    },
                );
            }
        }
        let was_healthy = health.state == StorageState::Healthy;
        if was_healthy {
            health.state = StorageState::Degraded;
            health.since = now;
        }
        was_healthy
    };
    if transitioned_to_degraded {
        persist_operational_event(
            state,
            crate::log::SECTION_STORAGE,
            crate::log::SEVERITY_ERROR,
            "storage.degraded",
            None,
            &json!({
                "category": category,
                "lost_records": lost_records,
                "error": error_text
            }),
        );
    }
}

/// Marks `category` as persisting successfully again. Only when every degraded
/// category has re-persisted does the current Degraded state clear, and only if
/// a lightweight SQLite integrity check passes; otherwise it becomes Not ready
/// (OPS-012). Historical events and usage gaps are never removed. The
/// Degraded-to-Healthy and Degraded-to-NotReady transitions each emit one
/// event so the 14-day history records the recovery (OPS-010).
fn mark_storage_category_recovered(state: &AppState, category: &str) {
    let all_recovered = {
        let Ok(mut health) = state.storage_health.lock() else {
            return;
        };
        if health.categories.remove(category).is_none() {
            return;
        }
        health.categories.is_empty()
    };
    if !all_recovered {
        return;
    }
    let integrity_ok = state
        .store
        .lock()
        .ok()
        .and_then(|mut store| store.verify_quick_check().ok())
        .is_some();
    let (recovered, not_ready) = {
        let Ok(mut health) = state.storage_health.lock() else {
            return;
        };
        if health.categories.is_empty() {
            health.state = if integrity_ok {
                StorageState::Healthy
            } else {
                StorageState::NotReady
            };
            health.since = timeutil::now_epoch();
        }
        (
            health.state == StorageState::Healthy,
            health.state == StorageState::NotReady,
        )
    };
    if recovered {
        persist_operational_event(
            state,
            crate::log::SECTION_STORAGE,
            crate::log::SEVERITY_INFO,
            "storage.recovered",
            None,
            &json!({ "category": category }),
        );
    } else if not_ready {
        persist_operational_event(
            state,
            crate::log::SECTION_STORAGE,
            crate::log::SEVERITY_ERROR,
            "storage.not_ready",
            None,
            &json!({ "category": category }),
        );
    }
}

impl SseRelay {
    async fn prime(
        upstream: reqwest::Response,
        protocol: StreamProtocol,
        published_model_name: String,
        idle_timeout: Duration,
    ) -> Result<Self, RelayError> {
        let mut relay = Self {
            upstream,
            reader: SseReader::default(),
            queued_events: VecDeque::new(),
            protocol,
            published_model_name,
            idle_timeout,
            captured_usage: None,
            terminal_semantic_failure: false,
            finished: false,
        };
        loop {
            let event = relay.read_next_event().await?.ok_or_else(|| {
                RelayError::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream SSE stream ended before its first event",
                    "api_error",
                    None,
                    Some("invalid_upstream_response"),
                )
            })?;
            let is_protocol_event = event.data.is_some();
            if is_protocol_event {
                validate_first_sse_event(relay.protocol, &event)?;
            }
            relay.queued_events.push_back(event);
            if is_protocol_event {
                return Ok(relay);
            }
        }
    }

    async fn next_body_chunk(&mut self) -> Result<Option<Bytes>, RelayError> {
        if self.finished {
            return Ok(None);
        }
        let event = match self.queued_events.pop_front() {
            Some(event) => event,
            None => match self.read_next_event().await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    self.finished = true;
                    return Err(RelayError::new(
                        StatusCode::BAD_GATEWAY,
                        "upstream SSE stream ended before its terminal event",
                        "api_error",
                        None,
                        Some("invalid_upstream_response"),
                    ));
                }
                Err(error) => return Err(error),
            },
        };
        if event.data.is_some() && !is_valid_sse_protocol_event(self.protocol, &event, true) {
            self.finished = true;
            self.queued_events.clear();
            return Err(RelayError::new(
                StatusCode::BAD_GATEWAY,
                "upstream SSE stream sent an invalid protocol event",
                "api_error",
                None,
                Some("invalid_upstream_response"),
            ));
        }
        if is_terminal_sse_event(self.protocol, &event) {
            self.finished = true;
            self.queued_events.clear();
            // A Responses typed terminal that marks the response as a
            // semantic failure is not a reliable success: the call must not
            // be credited with usage and the route must be quarantined
            // (API-011/ROUTE-008).
            if self.protocol == StreamProtocol::Responses
                && responses_terminal_failure(&event)
            {
                self.terminal_semantic_failure = true;
            }
        }
        if let Some(data) = event.data.as_deref()
            && let Ok(payload) = serde_json::from_str::<serde_json::Value>(data)
            && let Some(usage) = extract_usage(self.protocol, &payload)
        {
            self.captured_usage = Some(usage);
        }
        Ok(Some(render_sse_event(
            self.protocol,
            &event,
            &self.published_model_name,
        )))
    }

    async fn read_next_event(&mut self) -> Result<Option<SseEvent>, RelayError> {
        loop {
            if let Some(event) = self.reader.next_event()? {
                return Ok(Some(event));
            }
            let chunk = tokio::time::timeout(self.idle_timeout, self.upstream.chunk())
                .await
                .map_err(|_| {
                    RelayError::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "upstream SSE stream became idle before its next event",
                        "api_error",
                        None,
                        Some("upstream_timeout"),
                    )
                })?
                .map_err(relay_upstream_transport_error)?;
            let Some(chunk) = chunk else {
                return Ok(None);
            };
            self.reader.push(chunk);
        }
    }
}

impl SseReader {
    fn push(&mut self, chunk: Bytes) {
        self.buffered.extend_from_slice(&chunk);
    }

    fn next_event(&mut self) -> Result<Option<SseEvent>, RelayError> {
        let Some(end) = sse_event_end(&self.buffered) else {
            return Ok(None);
        };
        let raw = Bytes::from(self.buffered.drain(..end).collect::<Vec<_>>());
        let text = std::str::from_utf8(&raw).map_err(|_| {
            RelayError::new(
                StatusCode::BAD_GATEWAY,
                "upstream SSE event is not valid UTF-8",
                "api_error",
                None,
                Some("invalid_upstream_response"),
            )
        })?;
        let mut event_type = None;
        let mut data_lines = Vec::new();
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event_type = Some(value.to_owned()),
                "data" => data_lines.push(value),
                _ => {}
            }
        }
        let data = (!data_lines.is_empty()).then(|| data_lines.join("\n"));
        Ok(Some(SseEvent {
            raw,
            event_type,
            data,
        }))
    }
}

fn sse_event_end(buffer: &[u8]) -> Option<usize> {
    let crlf_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4);
    let short_end = buffer
        .windows(2)
        .position(|window| window == b"\n\n" || window == b"\r\r")
        .map(|index| index + 2);
    match (crlf_end, short_end) {
        (Some(crlf), Some(short)) => Some(crlf.min(short)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

fn is_event_stream_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn validate_first_sse_event(protocol: StreamProtocol, event: &SseEvent) -> Result<(), RelayError> {
    if is_valid_sse_protocol_event(protocol, event, false) {
        Ok(())
    } else {
        Err(RelayError::new(
            StatusCode::BAD_GATEWAY,
            "upstream SSE stream started with an invalid protocol event",
            "api_error",
            None,
            Some("invalid_upstream_response"),
        ))
    }
}

fn is_valid_sse_protocol_event(
    protocol: StreamProtocol,
    event: &SseEvent,
    allow_chat_done: bool,
) -> bool {
    let Some(data) = event.data.as_deref() else {
        return true;
    };
    match protocol {
        StreamProtocol::ChatCompletions => {
            (allow_chat_done && data == "[DONE]")
                || serde_json::from_str::<serde_json::Value>(data)
                    .ok()
                    .is_some_and(|chunk| is_chat_completion_chunk(&chunk))
        }
        StreamProtocol::Responses => serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .is_some_and(|payload| {
                let Some(event_type) = event.event_type.as_deref() else {
                    return false;
                };
                event_type.starts_with("response.")
                    && payload.get("type").and_then(serde_json::Value::as_str) == Some(event_type)
            }),
    }
}

fn is_chat_completion_chunk(chunk: &serde_json::Value) -> bool {
    chunk.get("object").and_then(serde_json::Value::as_str) == Some("chat.completion.chunk")
        && chunk
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && chunk
            .get("created")
            .and_then(serde_json::Value::as_i64)
            .is_some()
        && chunk
            .get("model")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|model| !model.is_empty())
        && chunk
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .is_some()
}

fn is_terminal_sse_event(protocol: StreamProtocol, event: &SseEvent) -> bool {
    match protocol {
        StreamProtocol::ChatCompletions => event.data.as_deref() == Some("[DONE]"),
        StreamProtocol::Responses => {
            matches!(
                event.event_type.as_deref(),
                Some("response.completed" | "response.failed" | "response.incomplete")
            ) && is_valid_sse_protocol_event(protocol, event, true)
        }
    }
}

fn render_sse_event(
    protocol: StreamProtocol,
    event: &SseEvent,
    published_model_name: &str,
) -> Bytes {
    let Some(data) = event.data.as_deref() else {
        return event.raw.clone();
    };
    let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(data) else {
        return event.raw.clone();
    };
    let model = match protocol {
        StreamProtocol::ChatCompletions => payload.get_mut("model"),
        StreamProtocol::Responses => payload
            .get_mut("response")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|response| response.get_mut("model")),
    };
    let Some(serde_json::Value::String(model)) = model else {
        return event.raw.clone();
    };
    if model == published_model_name {
        return event.raw.clone();
    }
    *model = published_model_name.to_owned();
    let Ok(serialized) = serde_json::to_string(&payload) else {
        return event.raw.clone();
    };
    let mut rendered = String::new();
    let mut wrote_data = false;
    let text = std::str::from_utf8(&event.raw).expect("SSE events are parsed as UTF-8");
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "data" || line.starts_with("data:") {
            if !wrote_data {
                rendered.push_str("data: ");
                rendered.push_str(&serialized);
                rendered.push('\n');
                wrote_data = true;
            }
        } else if !line.is_empty() {
            rendered.push_str(line);
            rendered.push('\n');
        }
    }
    rendered.push('\n');
    Bytes::from(rendered)
}

fn is_complete_chat_completion(body: &serde_json::Value) -> bool {
    body.get("object").and_then(serde_json::Value::as_str) == Some("chat.completion")
        && body
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && body
            .get("created")
            .and_then(serde_json::Value::as_i64)
            .is_some()
        && body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|model| !model.is_empty())
        && body
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|choices| {
                !choices.is_empty()
                    && choices.iter().all(|choice| {
                        choice
                            .get("index")
                            .and_then(serde_json::Value::as_i64)
                            .is_some()
                            && choice
                                .get("message")
                                .and_then(serde_json::Value::as_object)
                                .is_some()
                            && choice.get("finish_reason").is_some()
                    })
            })
}

fn require_active_session(state: &AppState, headers: &HeaderMap) -> Result<(), AdminError> {
    let token = read_cookie(headers).ok_or_else(|| {
        AdminError::new(
            StatusCode::UNAUTHORIZED,
            "administrator session is required",
        )
    })?;
    let session = with_store(state, |store| store.session(&token))?.ok_or_else(|| {
        AdminError::new(
            StatusCode::UNAUTHORIZED,
            "administrator session is required",
        )
    })?;
    if session.kind != SessionKind::Active {
        return Err(AdminError::new(
            StatusCode::FORBIDDEN,
            "administrator password change is required",
        ));
    }
    Ok(())
}

fn decimal_request_value(
    value: &serde_json::Value,
    label: &str,
    field: &'static str,
) -> Result<String, AdminError> {
    match value {
        serde_json::Value::String(value) => Ok(value.to_owned()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        _ => Err(AdminError::with_field(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{label} must be a decimal"),
            field,
        )),
    }
}

fn with_store<T>(
    state: &AppState,
    operation: impl FnOnce(&mut Store) -> anyhow::Result<T>,
) -> Result<T, AdminError> {
    let mut store = state.store.lock().map_err(|_| {
        AdminError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "local state is unavailable",
        )
    })?;
    operation(&mut store).map_err(AdminError::internal)
}

/// Maps a configuration-store error to an admin response: field-attributed
/// validation failures (UI-006/CFG-012) become 422 with `error.fields`, every
/// other failure keeps the message-only shape. Shared by the synchronous
/// handlers and the restore's `spawn_blocking` path so the two can't drift.
fn store_error_to_admin(error: anyhow::Error) -> AdminError {
    if let Some(field_error) = error.downcast_ref::<store::FieldError>() {
        AdminError::with_field(
            StatusCode::UNPROCESSABLE_ENTITY,
            field_error.message.clone(),
            field_error.field,
        )
    } else {
        AdminError::new(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
    }
}

fn with_configuration_store<T>(
    state: &AppState,
    operation: impl FnOnce(&mut Store) -> anyhow::Result<T>,
) -> Result<T, AdminError> {
    let mut store = state.store.lock().map_err(|_| {
        AdminError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "local state is unavailable",
        )
    })?;
    operation(&mut store).map_err(store_error_to_admin)
}

/// Runs one native protocol probe. Returns whether the probe succeeded and,
/// when an upstream HTTP response was received (any status), its safe HTTP
/// status for the Operations route row (OPS-013); transport errors have no
/// status and return `None`.
async fn native_probe(
    client: &reqwest::Client,
    probe: &crate::store::ProbeConfiguration,
) -> (bool, Option<i64>) {
    let (endpoint, request_body) = match probe.protocol.as_str() {
        "chat_completions" => (
            format!("{}/chat/completions", probe.base_url),
            json!({
                "model": probe.upstream_model_name,
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1,
                "stream": false
            }),
        ),
        "responses" => (
            format!("{}/responses", probe.base_url),
            json!({
                "model": probe.upstream_model_name,
                "input": "ping",
                "max_output_tokens": 1,
                "stream": false
            }),
        ),
        _ => return (false, None),
    };
    let response = client
        .post(endpoint)
        .bearer_auth(&probe.api_key)
        .json(&request_body)
        .send()
        .await;
    let Ok(response) = response else {
        return (false, None);
    };
    let http_status = Some(response.status().as_u16() as i64);
    if !response.status().is_success() {
        return (false, http_status);
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return (false, http_status);
    };
    let valid = match probe.protocol.as_str() {
        "chat_completions" => is_complete_chat_completion(&body),
        "responses" => is_complete_responses_response(&body) && !responses_semantic_failure(&body),
        _ => false,
    };
    (valid, http_status)
}

fn is_complete_responses_response(body: &serde_json::Value) -> bool {
    body.get("object").and_then(serde_json::Value::as_str) == Some("response")
        && body
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && body
            .get("created_at")
            .and_then(serde_json::Value::as_i64)
            .is_some()
        && body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|model| !model.is_empty())
        && body
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| {
                matches!(status, "completed" | "failed" | "cancelled" | "incomplete")
            })
        && body
            .get("output")
            .and_then(serde_json::Value::as_array)
            .is_some()
}

fn responses_semantic_failure(body: &serde_json::Value) -> bool {
    body.get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| matches!(status, "failed" | "cancelled"))
        || body.get("error").is_some_and(|error| !error.is_null())
}

/// Whether a Responses typed terminal event marks the response as a semantic
/// failure (API-011): the `response.failed` event type, a payload whose status
/// (top-level or nested under `response`) is `failed`/`cancelled`, or a
/// non-null error. `response.incomplete` and a plain `response.completed` are
/// valid native terminators, not failures.
fn responses_terminal_failure(event: &SseEvent) -> bool {
    if event.event_type.as_deref() == Some("response.failed") {
        return true;
    }
    let Some(data) = event.data.as_deref() else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(data) else {
        return false;
    };
    let status = payload
        .get("status")
        .or_else(|| {
            payload
                .get("response")
                .and_then(|response| response.get("status"))
        })
        .and_then(serde_json::Value::as_str);
    status.is_some_and(|status| matches!(status, "failed" | "cancelled"))
        || payload.get("error").is_some_and(|error| !error.is_null())
}

fn read_cookie(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE && !value.is_empty()).then(|| value.to_owned())
    })
}

fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_SECONDS}"
    ))
    .expect("generated session tokens are valid cookie values")
}

fn expired_cookie() -> HeaderValue {
    HeaderValue::from_static("local_api_relay_admin=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

fn session_expiry() -> i64 {
    timeutil::system_epoch_seconds() + SESSION_SECONDS
}

fn no_store_json(status: StatusCode, value: serde_json::Value) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Debug)]
struct AdminError {
    status: StatusCode,
    message: String,
    /// Field-attributed validation failures (UI-006 / CFG-012): a map of wire
    /// field name to an actionable message, rendered as `error.fields` so the
    /// management form can show each error next to its input. `None` for
    /// non-validation errors.
    fields: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug)]
struct RelayError {
    status: StatusCode,
    message: &'static str,
    error_type: &'static str,
    param: Option<&'static str>,
    code: Option<&'static str>,
    authenticate: bool,
}

impl RelayError {
    fn new(
        status: StatusCode,
        message: &'static str,
        error_type: &'static str,
        param: Option<&'static str>,
        code: Option<&'static str>,
    ) -> Self {
        Self {
            status,
            message,
            error_type,
            param,
            code,
            authenticate: false,
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "a valid relay access key is required",
            error_type: "authentication_error",
            param: None,
            code: Some("invalid_api_key"),
            authenticate: true,
        }
    }

    fn local_state_unavailable() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "local state is unavailable",
            "api_error",
            None,
            None,
        )
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let mut response = no_store_json(
            self.status,
            json!({
                "error": {
                    "message": self.message,
                    "type": self.error_type,
                    "param": self.param,
                    "code": self.code
                }
            }),
        );
        if self.authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

impl AdminError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            fields: None,
        }
    }

    fn with_field(status: StatusCode, message: impl Into<String>, field: &'static str) -> Self {
        let message = message.into();
        let mut fields = serde_json::Map::new();
        fields.insert(field.to_owned(), serde_json::Value::String(message.clone()));
        Self {
            status,
            message,
            fields: Some(fields),
        }
    }

    fn internal(error: anyhow::Error) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("local state is unavailable: {error:#}"),
        )
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let mut error = json!({ "message": self.message });
        if let Some(fields) = self.fields {
            error["fields"] = serde_json::Value::Object(fields);
        }
        no_store_json(self.status, json!({ "error": error }))
    }
}

/// Whether the process inherited the given signal as ignored (for example
/// SIGINT for a shell background job or a login-task-launched service). A
/// signal the process was explicitly told to ignore must not be awaited:
/// tokio resolves an ignored-signal listener immediately, which would drain
/// the service on startup instead of on a real stop request (PKG-012).
#[cfg(unix)]
fn signal_is_ignored(signal: libc::c_int) -> bool {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, std::ptr::null(), &mut action) == 0 {
            action.sa_sigaction == libc::SIG_IGN
        } else {
            false
        }
    }
}

/// Waits for the stop signal: SIGINT (Ctrl+C) when the process can catch it,
/// and always SIGTERM (the installed lifecycle stop/restart path). Returning
/// from this future tells axum to stop accepting new calls (PKG-012).
async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("could not install the SIGTERM handler");
        let terminate = async {
            sigterm.recv().await;
        };
        if signal_is_ignored(libc::SIGINT) {
            terminate.await;
        } else {
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("could not install the SIGINT handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = terminate => {}
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
