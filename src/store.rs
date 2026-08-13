use crate::{auth, backup, paths, timeutil};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const SCHEMA_VERSION: i64 = 17;

/// Test hook that lowers the schema version this process claims to support, so
/// upgrade/rollback drills can run a genuinely older binary that cannot read a
/// newer live database (PKG-013/PKG-014).
const TEST_SCHEMA_VERSION_VARIABLE: &str = "LOCAL_API_RELAY_TEST_SCHEMA_VERSION";

/// The schema version this process actually supports. The compiled-in
/// `SCHEMA_VERSION` is the real value; the test hook lowers it so a drill can
/// prove that a forward-migrated database is never downgraded, only restored
/// from its pre-migration backup by the previous binary.
pub fn supported_schema() -> i64 {
    if let Ok(value) = std::env::var(TEST_SCHEMA_VERSION_VARIABLE)
        && let Ok(version) = value.parse::<i64>()
        && (1..=SCHEMA_VERSION).contains(&version)
    {
        return version;
    }
    SCHEMA_VERSION
}

/// Test hook that fails the backup-gated migration chain after its last step's
/// DDL, so the single transaction and the preserved old database are provable
/// at the process boundary (DATA-008).
const TEST_FAIL_MIGRATION_VARIABLE: &str = "LOCAL_API_RELAY_TEST_FAIL_MIGRATION";

/// Degradable operational record categories (DATA-004/DATA-005). Call records
/// (with usage and the permanent daily aggregate) and model route health
/// history are written best-effort; a persistence failure marks the matching
/// category in the Storage status instead of failing the completed response.
pub const OPERATIONAL_CATEGORY_CALL_RECORDS: &str = "call_records";
pub const OPERATIONAL_CATEGORY_ROUTE_HEALTH: &str = "route_health";

/// Usage gap kinds reported to the admin surface (OPS-016).
pub const USAGE_GAP_KIND_PERSISTENCE: &str = "persistence";
pub const USAGE_GAP_KIND_MISSING_UPSTREAM_USAGE: &str = "missing_upstream_usage";

/// The typed three-state health of a model route (检测中 / 可用 / 暂不可用),
/// mirroring the persisted `model_route_health.state` values. Keeping the three
/// states in one type avoids stringly-typed Primitive Obsession across the
/// store and the relay/server layers; the JSON wire format and the database
/// values remain the same strings (API-003/ROUTE-003/ROUTE-004/ROUTE-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteHealth {
    /// Startup/restore probe pending (ROUTE-004); excluded from candidates
    /// until the probe completes (ROUTE-005).
    Checking,
    /// Native probe succeeded; enters the candidate set (ROUTE-005).
    Available,
    /// Quarantined after an attributable failure (ROUTE-006) or a failed probe
    /// (ROUTE-005); excluded from candidates until a dedicated recovery probe
    /// succeeds (ROUTE-010).
    Unavailable,
}

impl RouteHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteHealth::Checking => "checking",
            RouteHealth::Available => "available",
            RouteHealth::Unavailable => "unavailable",
        }
    }

    /// Parses a persisted `model_route_health.state` value. The schema
    /// constrains `state` to the three values above
    /// (`CHECK(state IN ('checking', 'available', 'unavailable'))`), so an
    /// unknown value (a corrupt row) is treated as Checking rather than
    /// crashing the process; at every comparison point the old string code
    /// treated any unknown value exactly like Checking (excluded from
    /// candidates, no recovery schedule, probe k = 0).
    pub fn from_persisted(value: &str) -> Self {
        match value {
            "available" => RouteHealth::Available,
            "unavailable" => RouteHealth::Unavailable,
            _ => RouteHealth::Checking,
        }
    }
}

const DECIMAL_SCALE: i64 = 1_000_000;
const DEFAULT_RECOVERY_BASE_INTERVAL_MS: i64 = 30_000;
const DEFAULT_RECOVERY_DOUBLING_LIMIT: i64 = 5;
const MIN_RECOVERY_BASE_INTERVAL_MS: i64 = 100;
const MAX_RECOVERY_BASE_INTERVAL_MS: i64 = 86_400_000;
const MAX_RECOVERY_DOUBLING_LIMIT: i64 = 20;

/// Configurable upstream deadline bounds (REL-001). Values are milliseconds.
const MIN_TIMEOUT_MS: i64 = 1_000;
const MAX_TIMEOUT_MS: i64 = 3_600_000;
pub const DEFAULT_FIRST_EVENT_TIMEOUT_MS: i64 = 120_000;
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: i64 = 30_000;
pub const DEFAULT_NONSTREAM_TIMEOUT_MS: i64 = 120_000;
/// Periodic light-validation sweep for Available routes (REL-003); 0 disables
/// the sweep.
pub const DEFAULT_FRESHNESS_INTERVAL_MS: i64 = 600_000;
const MAX_FRESHNESS_INTERVAL_MS: i64 = 86_400_000;
/// Consecutive attributable failures before a route is quarantined (REL-004).
pub const DEFAULT_QUARANTINE_THRESHOLD: i64 = 2;
const MAX_QUARANTINE_THRESHOLD: i64 = 5;
/// Periodic upstream model-catalog fetch interval (REL-006); 0 disables the
/// periodic fetch.
pub const DEFAULT_UPSTREAM_SYNC_INTERVAL_MS: i64 = 86_400_000;
const MAX_UPSTREAM_SYNC_INTERVAL_MS: i64 = 604_800_000;
/// Test hook: overrides the persisted quarantine threshold so legacy
/// single-failure tests keep their old semantics.
const TEST_QUARANTINE_THRESHOLD_VARIABLE: &str = "LOCAL_API_RELAY_TEST_QUARANTINE_THRESHOLD";

#[derive(Debug, Clone, Copy)]
pub struct RecoverySettings {
    pub base_interval_ms: i64,
    pub doubling_limit: i64,
    /// Deadline for the first protocol event of a streaming call and the
    /// response headers of a non-streaming call (REL-001).
    pub first_event_timeout_ms: i64,
    /// Deadline between two chunks of an in-flight SSE stream (REL-001).
    pub stream_idle_timeout_ms: i64,
    /// Deadline for reading the full body of a non-streaming response
    /// (REL-001).
    pub nonstream_timeout_ms: i64,
    /// Period of the Available-route light-validation sweep (REL-003); 0
    /// disables it.
    pub freshness_interval_ms: i64,
    /// Consecutive attributable failures before quarantine (REL-004).
    pub quarantine_threshold: i64,
    /// Period of the upstream model-catalog fetch (REL-006); 0 disables it.
    pub upstream_sync_interval_ms: i64,
}

impl Default for RecoverySettings {
    fn default() -> Self {
        Self {
            base_interval_ms: crate::store::DEFAULT_RECOVERY_BASE_INTERVAL_MS,
            doubling_limit: crate::store::DEFAULT_RECOVERY_DOUBLING_LIMIT,
            first_event_timeout_ms: DEFAULT_FIRST_EVENT_TIMEOUT_MS,
            stream_idle_timeout_ms: DEFAULT_STREAM_IDLE_TIMEOUT_MS,
            nonstream_timeout_ms: DEFAULT_NONSTREAM_TIMEOUT_MS,
            freshness_interval_ms: DEFAULT_FRESHNESS_INTERVAL_MS,
            quarantine_threshold: DEFAULT_QUARANTINE_THRESHOLD,
            upstream_sync_interval_ms: DEFAULT_UPSTREAM_SYNC_INTERVAL_MS,
        }
    }
}

impl RecoverySettings {
    /// Interval used for the next recovery probe after `failed_probe_count`
    /// failed probes, following `B * 2^min(k, N)` (ROUTE-020).
    pub fn interval_for(&self, failed_probe_count: i64) -> i64 {
        let exponent = failed_probe_count.max(0).min(self.doubling_limit) as u32;
        self.base_interval_ms * 2_i64.saturating_pow(exponent)
    }
}

/// Persistent migration/restore status for the Operations surface (OPS-015):
/// the running/supported schema versions, the outcome of the last migration
/// pre-backup, and the most recent migration or restore operation with its
/// verification result and completion time. The not-ready reason for a failed
/// startup lives in the captured diagnostics instead, because such a process
/// never reaches ready.
#[derive(Debug, Clone)]
pub struct DataOperationsStatus {
    pub running_schema: i64,
    pub supported_schema: i64,
    pub migration_state: String,
    pub migrated_from_schema: Option<i64>,
    pub pre_backup_ok: Option<bool>,
    pub pre_backup_name: Option<String>,
    pub last_phase: String,
    pub last_result: String,
    pub last_completed_at: Option<i64>,
    pub last_failed_stage: Option<String>,
    pub last_failed_reason: Option<String>,
    pub restore_source: Option<String>,
}

/// Coarse stage of an explicit restore for the in-flight progress display
/// (UI-012/OPS-015 "current or recent stage"): verification of the candidate
/// backup, the atomic database switch, and the post-restore route re-check.
/// The durable fine-grained outcome — including the exact failed stage — lives
/// in the `data_operations` migration/restore status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreStage {
    Verify,
    Switch,
    Recheck,
}

impl RestoreStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verify => "verify",
            Self::Switch => "switch",
            Self::Recheck => "recheck",
        }
    }
}

/// Outcome of an explicit restore (DATA-014/015/016). The server uses the
/// probe configurations to re-enter Checking and re-probe every route.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub candidate_name: String,
    pub candidate_schema: i64,
    pub restored_schema: i64,
    pub pre_restore_backup_name: String,
    pub completed_at: i64,
    pub probe_configurations: Vec<ProbeConfiguration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Bootstrap,
    Active,
}

#[derive(Debug, Clone, Copy)]
pub struct Session {
    pub kind: SessionKind,
}

#[derive(Debug, Clone)]
pub struct PublishedModel {
    pub id: String,
    pub name: String,
    pub input_price_rmb: String,
    pub output_price_rmb: String,
    pub cached_input_price_rmb: String,
    /// True when the model was deprecated (REL-007): existing routes keep
    /// serving it, new routes must not reference it.
    pub deprecated: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub api_key_masked: &'static str,
}

#[derive(Debug, Clone)]
pub struct ProviderConfiguration {
    pub id: String,
    pub display_name: String,
    pub base_url: String,
}

#[derive(Debug, Clone)]
pub struct RouteSummary {
    pub id: String,
    pub published_model_id: String,
    pub published_model_name: String,
    pub provider_id: String,
    pub provider_name: String,
    pub upstream_model_name: String,
    pub protocol: String,
    pub cost_multiplier: String,
    pub health: RouteHealth,
    pub last_checked_at: Option<i64>,
    pub failure_category: Option<String>,
    /// The safe HTTP status of the most recent probe or attributable failure
    /// (OPS-013); `None` when no HTTP status has been recorded (unknown).
    pub last_http_status: Option<i64>,
    pub failed_probe_count: i64,
    pub next_probe_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct OperationsSnapshot {
    pub catalog: Vec<PublishedModel>,
    pub providers: Vec<ProviderSummary>,
    pub routes: Vec<RouteSummary>,
}

#[derive(Debug, Clone)]
pub struct ProbeConfiguration {
    pub route_id: String,
    pub provider_id: String,
    pub base_url: String,
    pub api_key: String,
    pub upstream_model_name: String,
    pub protocol: String,
    /// The route's quarantine epoch when this probe was configured; a stale
    /// result is discarded by `record_probe_result` (REL-005).
    pub quarantine_epoch: i64,
}

#[derive(Debug, Clone)]
pub struct CreatedRelayAccessKey {
    pub id: String,
    pub secret: String,
    pub secret_prefix: String,
    pub label: String,
    pub created_at: i64,
    pub model_route_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RelayAccessKeySummary {
    pub id: String,
    pub secret_prefix: String,
    /// The full secret for re-display (REL-010); NULL for keys created by
    /// older binaries, which cannot be recovered.
    pub secret: Option<String>,
    pub label: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub model_route_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModelRouteCandidate {
    pub route_id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub base_url: String,
    pub api_key: String,
    pub upstream_model_name: String,
    /// The persisted health state from `model_route_health`. Routing applies
    /// the in-memory health overlay on top of this (DATA-005).
    pub health: RouteHealth,
}

/// Reliable usage tokens reported by the final successful route attempt.
/// The cache input component is zero when the upstream did not report it
/// (OPS-004/OPS-007: failed attempts never contribute usage).
#[derive(Debug, Clone)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct ModelRouteAttempt {
    pub sequence: i64,
    pub route_id: String,
    pub provider_id: String,
    pub provider_name: String,
    pub started_at_ms: i64,
    pub duration_ms: i64,
    pub http_status: Option<i64>,
    pub failure_category: Option<String>,
    pub commit_phase: String,
    pub outcome: String,
}

#[derive(Debug, Clone)]
pub struct CallRecord {
    pub id: i64,
    pub created_at_ms: i64,
    pub published_model_name: String,
    pub protocol: String,
    pub streamed: bool,
    pub succeeded: bool,
    pub success_provider_id: Option<String>,
    pub success_provider_name: Option<String>,
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub estimated_cost_rmb: Option<f64>,
    pub completion_ms: Option<i64>,
    pub first_token_ms: Option<i64>,
    pub attempts: Vec<ModelRouteAttempt>,
}

/// Metadata-only draft persisted as one call record per client call
/// (OPS-001). Only the final successful attempt's reliable usage enters the
/// record; failed calls keep every metric unknown (OPS-004/OPS-005).
#[derive(Debug, Clone)]
pub struct NewCallRecord {
    pub created_at_ms: i64,
    pub published_model_name: String,
    pub protocol: String,
    pub streamed: bool,
    pub succeeded: bool,
    pub success_provider_id: Option<String>,
    pub success_provider_name: Option<String>,
    pub usage: Option<Usage>,
    pub completion_ms: Option<i64>,
    pub first_token_ms: Option<i64>,
    pub attempts: Vec<ModelRouteAttempt>,
}

pub struct CallRecordPage {
    pub calls: Vec<CallRecord>,
    pub total: i64,
}

/// One provider's share of usage inside a model's distribution (OPS-008).
#[derive(Debug, Clone)]
pub struct ProviderUsageShare {
    pub provider_id: String,
    pub provider_name: String,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub estimated_cost_rmb: f64,
}

/// One published model's share of usage, with its provider breakdown (OPS-008).
#[derive(Debug, Clone)]
pub struct ModelUsageShare {
    pub published_model_name: String,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub estimated_cost_rmb: f64,
    pub providers: Vec<ProviderUsageShare>,
}

/// Aggregated totals for one time window, built only from reliable successful
/// usage (OPS-004/OPS-008). Failed calls and failed route attempts never
/// contribute, and missing values are never estimated.
#[derive(Debug, Clone)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub estimated_cost_rmb: f64,
    pub cache_hit_rate: f64,
    pub models: Vec<ModelUsageShare>,
}

/// One known usage gap (OPS-016). Persistence gaps come from failed local
/// call-record writes and are durable in `usage_gaps`; missing upstream usage
/// gaps are derived per window from successful calls that reported no usage.
/// Gaps are never estimated, backfilled, or hidden by storage recovery.
#[derive(Debug, Clone)]
pub struct UsageGap {
    pub kind: &'static str,
    pub category: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub lost_records: i64,
}

/// A published model name and one of its eligible routes with the persisted
/// health, so the server can apply the in-memory health overlay (API-003).
#[derive(Debug, Clone)]
pub struct EligibleModelRoute {
    pub model_name: String,
    pub route_id: String,
    pub health: RouteHealth,
}

/// Usage completeness for a window: `complete` is false exactly when at least
/// one known gap overlaps the window.
#[derive(Debug, Clone)]
pub struct UsageIntegrity {
    pub complete: bool,
    pub gaps: Vec<UsageGap>,
}

/// One metadata-only operational event (OPS-010/OPS-017/OPS-018): an
/// allowlisted record of a process, route, call, storage, backup, migration,
/// restore, usage, or log-rotation occurrence, retained for 14 days alongside
/// call records (OPS-009). Payloads only carry safe local identifiers, stable
/// codes, and normalized statuses — never request/response content or secrets.
#[derive(Debug, Clone)]
pub struct OperationalEvent {
    pub id: i64,
    pub occurred_at_ms: i64,
    pub section: String,
    pub severity: String,
    pub event_code: String,
    pub version: String,
    pub correlation_id: Option<String>,
    pub payload: serde_json::Value,
}

pub struct OperationalEventPage {
    pub events: Vec<OperationalEvent>,
    pub total: i64,
}

pub struct Store {
    connection: Connection,
    database_path: PathBuf,
    backup_dir: PathBuf,
}

impl Store {
    /// Opens the SQLite database, applying the startup contract (DATA-006 to
    /// DATA-008, DATA-013):
    ///
    /// - a brand-new database is initialized through the full forward-only
    ///   chain in one transaction (no old data to protect);
    /// - an existing database is verified before use, and an unreadable or
    ///   non-relay file is preserved as evidence and blocks ready instead of
    ///   silently creating an empty database over it;
    /// - a newer-than-supported schema is rejected without any write or
    ///   downgrade;
    /// - an older schema first creates and verifies a migration backup, then
    ///   runs the whole migration chain and version update in one transaction
    ///   and only enters ready after a post-migration integrity check passes.
    pub fn open(database_path: impl AsRef<Path>, backup_dir: impl AsRef<Path>) -> Result<Self> {
        let database_path = database_path.as_ref();
        let database_exists = database_path.exists();
        // An existing zero-byte file is never a relay database; opening it would
        // let SQLite write a fresh header over it, so it is rejected untouched
        // (DATA-013).
        if database_exists
            && fs::metadata(database_path)
                .map(|metadata| metadata.len() == 0)
                .unwrap_or(false)
        {
            bail!(
                "SQLite database at {} exists but is empty; it was left untouched \
                 for evidence — restore from a verified backup",
                database_path.display()
            );
        }
        let connection = open_and_configure(database_path, database_exists)?;

        let version = match schema_version(&connection) {
            Ok(version) => version,
            Err(error) if database_exists => {
                return Err(error).context(format!(
                    "SQLite database at {} is corrupted or not a valid local-api-relay \
                     database; it was left untouched for evidence — restore from a \
                     verified backup",
                    database_path.display()
                ));
            }
            Err(error) => return Err(error),
        };
        let mut store = Self {
            connection,
            database_path: database_path.to_path_buf(),
            backup_dir: backup_dir.as_ref().to_path_buf(),
        };

        let Some(version) = version else {
            if database_exists {
                bail!(
                    "SQLite database at {} exists but has no local-api-relay schema; \
                     it was left untouched for evidence — restore from a verified backup",
                    database_path.display()
                );
            }
            // Brand-new database: initialize through the full forward-only
            // chain in one transaction; there is no old data to protect.
            let transaction = store.connection.unchecked_transaction()?;
            run_migrations(&transaction, true)?;
            // Verified inside the transaction so a verification failure rolls
            // the whole initialization back (DATA-008).
            verify_integrity(&transaction)?;
            transaction.commit()?;
            store.ensure_data_operations("fresh", None, None, None)?;
            store.record_event(
                crate::log::SECTION_MIGRATION,
                crate::log::SEVERITY_INFO,
                "migration.fresh",
                None,
                &json!({ "running_schema": supported_schema() }),
            );
            return Ok(store);
        };

        match version {
            version if version > supported_schema() => {
                bail!(
                    "SQLite schema version {version} is newer than this binary supports; \
                     the database was left untouched — use a newer local-api-relay binary"
                );
            }
            version if version < 1 => {
                bail!(
                    "SQLite schema version {version} cannot be migrated by this binary; \
                     the database was left untouched"
                );
            }
            version if version == supported_schema() => {
                verify_integrity(&store.connection)?;
                store.ensure_data_operations("current", None, None, None)?;
            }
            version => {
                // Backup-gated forward migration: the pre-migration snapshot
                // must be created and verified first; any failure keeps the old
                // database and blocks ready (DATA-007/DATA-008).
                let pre_backup = store
                    .create_gate_backup(backup::TriggerKind::Migration, version)
                    .map_err(|failure| {
                        crate::log::emit(
                            crate::log::SECTION_MIGRATION,
                            crate::log::SEVERITY_ERROR,
                            "migration.failed",
                            None,
                            &json!({ "stage": "pre_backup", "migrated_from": version, "reason": failure.reason }),
                        );
                        anyhow!(
                            "migration pre-backup failed: {failure}; the database was left \
                             untouched — restore from a verified backup"
                        )
                    })?;
                let transaction = store.connection.unchecked_transaction()?;
                let migrated = run_migrations(&transaction, false)
                    .and_then(|_| verify_integrity(&transaction));
                if let Err(error) = migrated {
                    drop(transaction);
                    crate::log::emit(
                        crate::log::SECTION_MIGRATION,
                        crate::log::SEVERITY_ERROR,
                        "migration.failed",
                        None,
                        &json!({ "stage": "migrate", "migrated_from": version, "reason": format!("{error:#}") }),
                    );
                    return Err(error).context(
                        "the migration failed and was rolled back; the database was left \
                         untouched — restore from a verified backup",
                    );
                }
                // Verified inside the transaction so a post-migration
                // verification failure rolls the chain back and keeps the old
                // database (DATA-008).
                transaction.commit()?;
                store.ensure_data_operations("migrated", Some(version), Some(true), Some(&pre_backup.name))?;
                store.record_operation(
                    "migration",
                    "ok",
                    timeutil::now_epoch(),
                    None,
                    None,
                    None,
                )?;
                store.record_event(
                    crate::log::SECTION_MIGRATION,
                    crate::log::SEVERITY_INFO,
                    "migration.completed",
                    None,
                    &json!({
                        "migrated_from": version,
                        "pre_backup": pre_backup.name,
                        "pre_backup_ok": true,
                        "running_schema": supported_schema()
                    }),
                );
            }
        }
        Ok(store)
    }

    pub fn initialize_administrator(&mut self) -> Result<String> {
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM administrator_credentials WHERE id = 1)",
            [],
            |row| row.get(0),
        )?;
        if exists {
            bail!("administrator has already been initialized");
        }

        let credential = auth::generate_secret();
        let password_hash = auth::hash_password(&credential)?;
        let now = timeutil::system_epoch_seconds();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO administrator_credentials (id, password_hash, must_change, created_at)
             VALUES (1, ?1, 1, ?2)",
            params![password_hash, now],
        )?;
        transaction.commit()?;
        Ok(credential)
    }

    pub fn login(
        &mut self,
        password: &str,
        expires_at: i64,
    ) -> Result<Option<(String, SessionKind)>> {
        let record: Option<(String, bool)> = self
            .connection
            .query_row(
                "SELECT password_hash, must_change FROM administrator_credentials WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((password_hash, must_change)) = record else {
            return Ok(None);
        };
        if !auth::verify_password(password, &password_hash)? {
            return Ok(None);
        }

        let kind = if must_change {
            SessionKind::Bootstrap
        } else {
            SessionKind::Active
        };
        let token = auth::generate_secret();
        self.insert_session(&token, kind, expires_at)?;
        Ok(Some((token, kind)))
    }

    pub fn session(&mut self, token: &str) -> Result<Option<Session>> {
        let now = timeutil::system_epoch_seconds();
        self.connection.execute(
            "DELETE FROM administrator_sessions WHERE expires_at <= ?1",
            [now],
        )?;
        let token_hash = auth::hash_token(token);
        let kind: Option<String> = self
            .connection
            .query_row(
                "SELECT kind FROM administrator_sessions WHERE token_hash = ?1 AND expires_at > ?2",
                params![token_hash.as_slice(), now],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match kind.as_deref() {
            Some("bootstrap") => Some(Session {
                kind: SessionKind::Bootstrap,
            }),
            Some("active") => Some(Session {
                kind: SessionKind::Active,
            }),
            Some(_) => return Err(anyhow!("database contains an unsupported session state")),
            None => None,
        })
    }

    pub fn change_password(
        &mut self,
        bootstrap_token: &str,
        password: &str,
        expires_at: i64,
    ) -> Result<String> {
        let password_hash = auth::hash_password(password)?;
        let now = timeutil::system_epoch_seconds();
        let old_token_hash = auth::hash_token(bootstrap_token);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bootstrap_session: Option<String> = transaction
            .query_row(
                "SELECT kind FROM administrator_sessions WHERE token_hash = ?1 AND expires_at > ?2",
                params![old_token_hash.as_slice(), now],
                |row| row.get(0),
            )
            .optional()?;
        if bootstrap_session.as_deref() != Some("bootstrap") {
            bail!("administrator password change requires the bootstrap session");
        }
        transaction.execute(
            "UPDATE administrator_credentials SET password_hash = ?1, must_change = 0 WHERE id = 1",
            [password_hash],
        )?;
        transaction.execute("DELETE FROM administrator_sessions", [])?;
        let token = auth::generate_secret();
        let token_hash = auth::hash_token(&token);
        transaction.execute(
            "INSERT INTO administrator_sessions (token_hash, kind, expires_at, created_at)
             VALUES (?1, 'active', ?2, ?3)",
            params![token_hash.as_slice(), expires_at, now],
        )?;
        transaction.commit()?;
        Ok(token)
    }

    pub fn logout(&mut self, token: &str) -> Result<()> {
        let token_hash = auth::hash_token(token);
        self.connection.execute(
            "DELETE FROM administrator_sessions WHERE token_hash = ?1",
            [token_hash.as_slice()],
        )?;
        Ok(())
    }

    pub fn create_provider(
        &mut self,
        display_name: &str,
        base_url: &str,
        api_key: &str,
    ) -> Result<ProviderSummary> {
        let display_name = validate_display_name(display_name)?;
        let base_url = validate_base_url(base_url)?;
        let api_key = validate_api_key(api_key)?;
        let provider = ProviderSummary {
            id: new_id(),
            display_name,
            api_key_masked: "********",
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO upstream_providers (id, display_name, base_url, api_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                provider.id,
                provider.display_name,
                base_url,
                api_key,
                timeutil::system_epoch_seconds()
            ],
        )?;
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(provider)
    }

    pub fn provider_configuration(&mut self, provider_id: &str) -> Result<ProviderConfiguration> {
        self.connection
            .query_row(
                "SELECT id, display_name, base_url FROM upstream_providers WHERE id = ?1",
                [provider_id],
                |row| {
                    Ok(ProviderConfiguration {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        base_url: row.get(2)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("upstream provider does not exist"))
    }

    pub fn update_provider(
        &mut self,
        provider_id: &str,
        display_name: &str,
        base_url: &str,
        api_key: &str,
    ) -> Result<Vec<ProbeConfiguration>> {
        let display_name = validate_display_name(display_name)?;
        let base_url = validate_base_url(base_url)?;
        let api_key = validate_api_key(api_key)?;
        let v14 = self.health_v14()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = transaction
            .query_row(
                "SELECT base_url, api_key FROM upstream_providers WHERE id = ?1",
                [provider_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((existing_base_url, existing_api_key)) = existing else {
            bail!("upstream provider does not exist");
        };
        let connection_changed = existing_base_url != base_url || existing_api_key != api_key;
        transaction.execute(
            "UPDATE upstream_providers
             SET display_name = ?1, base_url = ?2, api_key = ?3
             WHERE id = ?4",
            params![display_name, base_url, api_key, provider_id],
        )?;
        let probes = if connection_changed {
            let routes = {
                let mut statement = transaction.prepare(
                    "SELECT r.id, r.upstream_model_name, r.protocol, h.quarantine_epoch
                     FROM model_routes r
                     JOIN model_route_health h ON h.model_route_id = r.id
                     WHERE r.upstream_provider_id = ?1
                     ORDER BY r.id",
                )?;
                statement
                    .query_map([provider_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let reset_extra = if v14 {
                ", consecutive_failures = 0"
            } else {
                ""
            };
            transaction.execute(
                &format!("UPDATE model_route_health
                 SET state = 'checking', checked_at = NULL, failure_category = NULL,
                     failed_probe_count = 0, next_probe_at_ms = NULL{reset_extra}
                 WHERE model_route_id IN (
                     SELECT id FROM model_routes WHERE upstream_provider_id = ?1
                 )"),
                [provider_id],
            )?;
            routes
                .into_iter()
                .map(
                    |(route_id, upstream_model_name, protocol, quarantine_epoch)| ProbeConfiguration {
                        route_id,
                        provider_id: provider_id.to_owned(),
                        base_url: base_url.clone(),
                        api_key: api_key.clone(),
                        upstream_model_name,
                        protocol,
                        quarantine_epoch,
                    },
                )
                .collect()
        } else {
            Vec::new()
        };
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(probes)
    }

    pub fn create_model_route(
        &mut self,
        published_model_id: &str,
        provider_id: &str,
        upstream_model_name: &str,
        protocol: &str,
        cost_multiplier: &str,
    ) -> Result<ProbeConfiguration> {
        let upstream_model_name = validate_upstream_model_name(upstream_model_name)?;
        validate_protocol(protocol)?;
        let cost_multiplier =
            parse_positive_decimal(cost_multiplier, "cost multiplier", "cost_multiplier")?;
        let route_id = new_id();
        // The deprecated_at column exists at schema >= 16; the schema-version
        // test hook serves older fixtures with this binary, so the deprecation
        // guard degrades to the plain existence check there.
        let schema = schema_version(&self.connection)?.unwrap_or(SCHEMA_VERSION);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider: Option<(String, String)> = transaction
            .query_row(
                "SELECT base_url, api_key FROM upstream_providers WHERE id = ?1",
                [provider_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((base_url, api_key)) = provider else {
            return Err(field_error("provider_id", "selected upstream provider does not exist").into());
        };
        if schema >= 16 {
            let model_deprecated: Option<Option<i64>> = transaction
                .query_row(
                    "SELECT deprecated_at FROM published_models WHERE id = ?1",
                    [published_model_id],
                    |row| row.get(0),
                )
                .optional()?;
            match model_deprecated {
                None => {
                    return Err(field_error(
                        "published_model_id",
                        "selected published model does not exist",
                    )
                    .into());
                }
                Some(Some(_)) => {
                    return Err(field_error(
                        "published_model_id",
                        "selected published model is deprecated; choose an active model",
                    )
                    .into());
                }
                Some(None) => {}
            }
        } else {
            let model_exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM published_models WHERE id = ?1)",
                [published_model_id],
                |row| row.get(0),
            )?;
            if !model_exists {
                return Err(field_error(
                    "published_model_id",
                    "selected published model does not exist",
                )
                .into());
            }
        }
        if route_identity_conflict(
            &transaction,
            published_model_id,
            provider_id,
            &upstream_model_name,
            protocol,
            None,
        )? {
            bail!(DUPLICATE_ROUTE_IDENTITY_MESSAGE);
        }
        transaction.execute(
            "INSERT INTO model_routes
             (id, published_model_id, upstream_provider_id, upstream_model_name, protocol, cost_multiplier_micros, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                route_id,
                published_model_id,
                provider_id,
                upstream_model_name,
                protocol,
                cost_multiplier,
                timeutil::system_epoch_seconds()
            ],
        )?;
        transaction.execute(
            "INSERT INTO model_route_health (model_route_id, state, checked_at, failure_category)
             VALUES (?1, 'checking', NULL, NULL)",
            [route_id.as_str()],
        )?;
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(ProbeConfiguration {
            route_id,
            provider_id: provider_id.to_owned(),
            base_url,
            api_key,
            upstream_model_name,
            protocol: protocol.to_owned(),
            quarantine_epoch: 0,
        })
    }

    pub fn update_model_route(
        &mut self,
        route_id: &str,
        published_model_id: &str,
        provider_id: &str,
        upstream_model_name: &str,
        protocol: &str,
        cost_multiplier: &str,
    ) -> Result<Option<ProbeConfiguration>> {
        let upstream_model_name = validate_upstream_model_name(upstream_model_name)?;
        validate_protocol(protocol)?;
        let cost_multiplier =
            parse_positive_decimal(cost_multiplier, "cost multiplier", "cost_multiplier")?;
        let v14 = self.health_v14()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider: Option<(String, String)> = transaction
            .query_row(
                "SELECT base_url, api_key FROM upstream_providers WHERE id = ?1",
                [provider_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((base_url, api_key)) = provider else {
            return Err(field_error("provider_id", "selected upstream provider does not exist").into());
        };
        let model_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM published_models WHERE id = ?1)",
            [published_model_id],
            |row| row.get(0),
        )?;
        if !model_exists {
            return Err(field_error(
                "published_model_id",
                "selected published model does not exist",
            )
            .into());
        }
        let existing: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT upstream_provider_id, upstream_model_name, protocol
                 FROM model_routes WHERE id = ?1",
                [route_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((existing_provider_id, existing_model_name, existing_protocol)) = existing else {
            bail!("model route does not exist");
        };
        if route_identity_conflict(
            &transaction,
            published_model_id,
            provider_id,
            &upstream_model_name,
            protocol,
            Some(route_id),
        )? {
            bail!(DUPLICATE_ROUTE_IDENTITY_MESSAGE);
        }
        transaction.execute(
            "UPDATE model_routes
             SET published_model_id = ?1,
                 upstream_provider_id = ?2,
                 upstream_model_name = ?3,
                 protocol = ?4,
                 cost_multiplier_micros = ?5
             WHERE id = ?6",
            params![
                published_model_id,
                provider_id,
                upstream_model_name,
                protocol,
                cost_multiplier,
                route_id
            ],
        )?;
        let needs_check = existing_provider_id != provider_id
            || existing_model_name != upstream_model_name
            || existing_protocol != protocol;
        if needs_check {
            let reset_extra = if v14 {
                ", consecutive_failures = 0"
            } else {
                ""
            };
            transaction.execute(
                &format!("UPDATE model_route_health
                 SET state = 'checking', checked_at = NULL, failure_category = NULL,
                     failed_probe_count = 0, next_probe_at_ms = NULL{reset_extra}
                 WHERE model_route_id = ?1"),
                [route_id],
            )?;
        }
        fail_config_commit_if_requested()?;
        let quarantine_epoch: i64 = if needs_check {
            transaction.query_row(
                "SELECT quarantine_epoch FROM model_route_health WHERE model_route_id = ?1",
                [route_id],
                |row| row.get(0),
            )?
        } else {
            0
        };
        transaction.commit()?;
        Ok(needs_check.then_some(ProbeConfiguration {
            route_id: route_id.to_owned(),
            provider_id: provider_id.to_owned(),
            base_url,
            api_key,
            upstream_model_name,
            protocol: protocol.to_owned(),
            quarantine_epoch,
        }))
    }

    pub fn model_route_probe(&mut self, route_id: &str) -> Result<ProbeConfiguration> {
        let epoch = self.probe_epoch_sql()?;
        let epoch_subquery = if epoch == "0" {
            "0".to_owned()
        } else {
            "COALESCE((SELECT quarantine_epoch FROM model_route_health WHERE model_route_id = r.id), 0)".to_owned()
        };
        self.connection
            .query_row(
                &format!("SELECT r.id, u.base_url, u.api_key, r.upstream_model_name, r.protocol, u.id, {epoch_subquery} FROM model_routes r JOIN upstream_providers u ON u.id = r.upstream_provider_id WHERE r.id = ?1"),
                [route_id],
                |row| {
                    Ok(ProbeConfiguration {
                        route_id: row.get(0)?,
                        base_url: row.get(1)?,
                        api_key: row.get(2)?,
                        upstream_model_name: row.get(3)?,
                        protocol: row.get(4)?,
                        provider_id: row.get(5)?,
                        quarantine_epoch: row.get(6)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("model route does not exist"))
    }

    pub fn recovery_settings(&mut self) -> Result<RecoverySettings> {
        // The settings row grew column by column across migrations; the
        // schema-version test hook simulates an old binary serving an old
        // fixture, so the SELECT must only reference columns the live schema
        // actually has, degrading to defaults otherwise.
        let version = schema_version(&self.connection)?.unwrap_or(SCHEMA_VERSION);
        let mut columns = vec!["base_interval_ms", "doubling_limit"];
        if version >= 12 {
            columns.extend_from_slice(&[
                "first_event_timeout_ms",
                "stream_idle_timeout_ms",
                "nonstream_timeout_ms",
            ]);
        }
        if version >= 13 {
            columns.push("freshness_interval_ms");
        }
        if version >= 14 {
            columns.push("quarantine_threshold");
        }
        if version >= 15 {
            columns.push("upstream_sync_interval_ms");
        }
        let sql = format!(
            "SELECT {} FROM recovery_settings WHERE id = 1",
            columns.join(", ")
        );
        let values: Vec<i64> = self
            .connection
            .query_row(&sql, [], |row| {
                let mut values = Vec::new();
                for index in 0..columns.len() {
                    values.push(row.get(index)?);
                }
                Ok(values)
            })?;
        let get = |name: &str, default: i64| -> i64 {
            columns
                .iter()
                .position(|column| *column == name)
                .map(|index| values[index])
                .unwrap_or(default)
        };
        let persisted_threshold = get("quarantine_threshold", DEFAULT_QUARANTINE_THRESHOLD);
        Ok(RecoverySettings {
            base_interval_ms: get("base_interval_ms", DEFAULT_RECOVERY_BASE_INTERVAL_MS),
            doubling_limit: get("doubling_limit", DEFAULT_RECOVERY_DOUBLING_LIMIT),
            first_event_timeout_ms: get("first_event_timeout_ms", DEFAULT_FIRST_EVENT_TIMEOUT_MS),
            stream_idle_timeout_ms: get("stream_idle_timeout_ms", DEFAULT_STREAM_IDLE_TIMEOUT_MS),
            nonstream_timeout_ms: get("nonstream_timeout_ms", DEFAULT_NONSTREAM_TIMEOUT_MS),
            freshness_interval_ms: get("freshness_interval_ms", DEFAULT_FRESHNESS_INTERVAL_MS),
            quarantine_threshold: std::env::var(TEST_QUARANTINE_THRESHOLD_VARIABLE)
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|value| (1..=MAX_QUARANTINE_THRESHOLD).contains(value))
                .unwrap_or(persisted_threshold),
            upstream_sync_interval_ms: get(
                "upstream_sync_interval_ms",
                DEFAULT_UPSTREAM_SYNC_INTERVAL_MS
            ),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_recovery_settings(
        &mut self,
        base_interval_ms: i64,
        doubling_limit: i64,
        first_event_timeout_ms: Option<i64>,
        stream_idle_timeout_ms: Option<i64>,
        nonstream_timeout_ms: Option<i64>,
        freshness_interval_ms: Option<i64>,
        quarantine_threshold: Option<i64>,
        upstream_sync_interval_ms: Option<i64>,
    ) -> Result<()> {
        let current = self.recovery_settings()?;
        let first_event_timeout_ms = first_event_timeout_ms.unwrap_or(current.first_event_timeout_ms);
        let stream_idle_timeout_ms = stream_idle_timeout_ms.unwrap_or(current.stream_idle_timeout_ms);
        let nonstream_timeout_ms = nonstream_timeout_ms.unwrap_or(current.nonstream_timeout_ms);
        let freshness_interval_ms = freshness_interval_ms.unwrap_or(current.freshness_interval_ms);
        let quarantine_threshold = quarantine_threshold.unwrap_or(current.quarantine_threshold);
        let upstream_sync_interval_ms = upstream_sync_interval_ms.unwrap_or(current.upstream_sync_interval_ms);
        if !(MIN_RECOVERY_BASE_INTERVAL_MS..=MAX_RECOVERY_BASE_INTERVAL_MS)
            .contains(&base_interval_ms)
        {
            bail!(
                "base recovery interval must be between {} and {} milliseconds",
                MIN_RECOVERY_BASE_INTERVAL_MS,
                MAX_RECOVERY_BASE_INTERVAL_MS
            );
        }
        if !(0..=MAX_RECOVERY_DOUBLING_LIMIT).contains(&doubling_limit) {
            bail!(
                "doubling limit must be between 0 and {}",
                MAX_RECOVERY_DOUBLING_LIMIT
            );
        }
        if !(0..=MAX_FRESHNESS_INTERVAL_MS).contains(&freshness_interval_ms) {
            bail!(
                "freshness interval must be between 0 and {} milliseconds",
                MAX_FRESHNESS_INTERVAL_MS
            );
        }
        if !(1..=MAX_QUARANTINE_THRESHOLD).contains(&quarantine_threshold) {
            bail!(
                "quarantine threshold must be between 1 and {}",
                MAX_QUARANTINE_THRESHOLD
            );
        }
        if !(0..=MAX_UPSTREAM_SYNC_INTERVAL_MS).contains(&upstream_sync_interval_ms) {
            bail!(
                "upstream sync interval must be between 0 and {} milliseconds",
                MAX_UPSTREAM_SYNC_INTERVAL_MS
            );
        }
        for (label, timeout_ms) in [
            ("first-event timeout", first_event_timeout_ms),
            ("stream idle timeout", stream_idle_timeout_ms),
            ("non-streaming timeout", nonstream_timeout_ms),
        ] {
            if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
                bail!(
                    "{label} must be between {} and {} milliseconds",
                    MIN_TIMEOUT_MS,
                    MAX_TIMEOUT_MS
                );
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE recovery_settings SET base_interval_ms = ?1, doubling_limit = ?2,
                     first_event_timeout_ms = ?3, stream_idle_timeout_ms = ?4, nonstream_timeout_ms = ?5,
                     freshness_interval_ms = ?6, quarantine_threshold = ?7, upstream_sync_interval_ms = ?8
             WHERE id = 1",
            params![
                base_interval_ms,
                doubling_limit,
                first_event_timeout_ms,
                stream_idle_timeout_ms,
                nonstream_timeout_ms,
                freshness_interval_ms,
                quarantine_threshold,
                upstream_sync_interval_ms
            ],
        )?;
        // Recompute every unavailable route's next probe so the displayed
        // interval and schedule always follow the current settings.
        let now_ms = timeutil::recovery_clock_now_ms().unwrap_or_else(timeutil::system_epoch_millis);
        let mut statement = transaction.prepare(
            "SELECT model_route_id, failed_probe_count
             FROM model_route_health
             WHERE state = 'unavailable'",
        )?;
        let unavailable: Vec<(String, i64)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let settings = RecoverySettings {
            base_interval_ms,
            doubling_limit,
            first_event_timeout_ms,
            stream_idle_timeout_ms,
            nonstream_timeout_ms,
            freshness_interval_ms,
            quarantine_threshold,
            upstream_sync_interval_ms,
        };
        for (route_id, failed_probe_count) in unavailable {
            transaction.execute(
                "UPDATE model_route_health SET next_probe_at_ms = ?1 WHERE model_route_id = ?2",
                params![now_ms + settings.interval_for(failed_probe_count), route_id],
            )?;
        }
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(())
    }

    /// Resets every configured model route to Checking at process start,
    /// discarding persisted health for candidate selection (ROUTE-004), and
    /// returns the concurrent startup probe configurations for all routes.
    /// Whether the live database carries the v14 health columns
    /// (`quarantine_epoch`, `consecutive_failures`). The schema-version test
    /// hook simulates an old binary serving an old fixture, so health SQL
    /// must degrade to the pre-v14 shape instead of failing.
    fn health_v14(&self) -> Result<bool> {
        let version = schema_version(&self.connection)?.unwrap_or(SCHEMA_VERSION);
        Ok(version >= 14)
    }

    /// The SQL expression selecting a probe's quarantine epoch: pre-v14
    /// databases lack the column, and the schema-version test hook simulates
    /// an old binary serving an old fixture, so the expression degrades to a
    /// constant instead of failing (REL-005).
    fn probe_epoch_sql(&self) -> Result<String> {
        let version = schema_version(&self.connection)?.unwrap_or(SCHEMA_VERSION);
        Ok(if version >= 14 {
            "h.quarantine_epoch".to_owned()
        } else {
            "0".to_owned()
        })
    }

    pub fn startup_probe_configurations(&mut self) -> Result<Vec<ProbeConfiguration>> {
        let reset_extra = if self.health_v14()? {
            ", consecutive_failures = 0"
        } else {
            ""
        };
        self.connection.execute_batch(&format!(
            "UPDATE model_route_health
             SET state = 'checking', checked_at = NULL, failure_category = NULL,
                 failed_probe_count = 0, next_probe_at_ms = NULL{reset_extra}"
        ))?;
        let epoch = self.probe_epoch_sql()?;
        let mut statement = self.connection.prepare(&format!(
            "SELECT r.id, u.base_url, u.api_key, r.upstream_model_name, r.protocol, u.id, {epoch}
             FROM model_routes r
             JOIN upstream_providers u ON u.id = r.upstream_provider_id
             JOIN model_route_health h ON h.model_route_id = r.id
             ORDER BY r.id"
        ))?;
        statement
            .query_map([], |row| {
                Ok(ProbeConfiguration {
                    route_id: row.get(0)?,
                    base_url: row.get(1)?,
                    api_key: row.get(2)?,
                    upstream_model_name: row.get(3)?,
                    protocol: row.get(4)?,
                    provider_id: row.get(5)?,
                    quarantine_epoch: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Routes currently due for a recovery probe. The scheduler enforces that
    /// at most one recovery probe is in flight per route (ROUTE-018).
    pub fn recovery_due_probes(&mut self, now_ms: i64) -> Result<Vec<ProbeConfiguration>> {
        let epoch = self.probe_epoch_sql()?;
        let mut statement = self.connection.prepare(&format!(
            "SELECT r.id, u.base_url, u.api_key, r.upstream_model_name, r.protocol, u.id, {epoch}
             FROM model_route_health h
             JOIN model_routes r ON r.id = h.model_route_id
             JOIN upstream_providers u ON u.id = r.upstream_provider_id
             WHERE h.state = 'unavailable'
               AND h.next_probe_at_ms IS NOT NULL
               AND h.next_probe_at_ms <= ?1
             ORDER BY h.next_probe_at_ms ASC, r.id ASC",
        ))?;
        statement
            .query_map([now_ms], |row| {
                Ok(ProbeConfiguration {
                    route_id: row.get(0)?,
                    base_url: row.get(1)?,
                    api_key: row.get(2)?,
                    upstream_model_name: row.get(3)?,
                    protocol: row.get(4)?,
                    provider_id: row.get(5)?,
                    quarantine_epoch: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Applies one probe result (startup, recovery, freshness, or manual
    /// check). REL-005: the result only applies when `quarantine_epoch` still
    /// matches the route's current epoch — a probe that started before a
    /// quarantine is discarded instead of restoring the route. Success also
    /// clears the consecutive-failure counter (REL-004).
    pub fn record_probe_result(
        &mut self,
        route_id: &str,
        quarantine_epoch: i64,
        available: bool,
        http_status: Option<i64>,
    ) -> Result<()> {
        fail_operational_write_if_requested(OPERATIONAL_CATEGORY_ROUTE_HEALTH)?;
        let now_ms = timeutil::recovery_clock_now_ms().unwrap_or_else(timeutil::system_epoch_millis);
        let v14 = self.health_v14()?;
        if available {
            let changed = if v14 {
                self.connection.execute(
                    "UPDATE model_route_health
                     SET state = 'available', checked_at = ?1, failure_category = NULL,
                         failed_probe_count = 0, next_probe_at_ms = NULL, last_http_status = ?2,
                         consecutive_failures = 0
                     WHERE model_route_id = ?3 AND quarantine_epoch = ?4",
                    params![timeutil::system_epoch_seconds(), http_status, route_id, quarantine_epoch],
                )?
            } else {
                self.connection.execute(
                    "UPDATE model_route_health
                     SET state = 'available', checked_at = ?1, failure_category = NULL,
                         failed_probe_count = 0, next_probe_at_ms = NULL
                     WHERE model_route_id = ?2",
                    params![timeutil::system_epoch_seconds(), route_id],
                )?
            };
            if changed == 1 {
                self.record_event(
                    crate::log::SECTION_ROUTES,
                    crate::log::SEVERITY_INFO,
                    "routes.check",
                    None,
                    &json!({ "route_id": route_id, "result": "available" }),
                );
            } else if !self.route_health_row_exists(route_id)? {
                bail!("model route does not exist");
            }
            return Ok(());
        }
        let settings = self.recovery_settings()?;
        if v14 {
            let current: Option<(String, i64, i64)> = self
                .connection
                .query_row(
                    "SELECT state, failed_probe_count, quarantine_epoch
                     FROM model_route_health WHERE model_route_id = ?1",
                    [route_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((state, failed_probe_count, current_epoch)) = current else {
                bail!("model route does not exist");
            };
            if current_epoch != quarantine_epoch {
                // Stale probe: the route was quarantined (or re-checked) after
                // this probe started. Discard the result (REL-005).
                return Ok(());
            }
            self.apply_failed_probe(
                route_id,
                state,
                failed_probe_count,
                http_status,
                now_ms,
                settings,
                true,
                quarantine_epoch,
            )?;
            return Ok(());
        }
        let current: Option<(String, i64)> = self
            .connection
            .query_row(
                "SELECT state, failed_probe_count
                 FROM model_route_health WHERE model_route_id = ?1",
                [route_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((state, failed_probe_count)) = current else {
            bail!("model route does not exist");
        };
        self.apply_failed_probe(route_id, state, failed_probe_count, http_status, now_ms, settings, false, 0)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_failed_probe(
        &mut self,
        route_id: &str,
        state: String,
        failed_probe_count: i64,
        http_status: Option<i64>,
        now_ms: i64,
        settings: RecoverySettings,
        v14: bool,
        quarantine_epoch: i64,
    ) -> Result<()> {
        // Only a failed recovery probe advances k; entering Temporarily
        // unavailable (startup probe, manual check, or a runtime failure)
        // starts the schedule at k = 0 so the first recovery probe runs after
        // B. The k-th failure then schedules the next probe after B * 2^min(k,N)
        // (ROUTE-020).
        let failed_probe_count = if RouteHealth::from_persisted(&state) == RouteHealth::Unavailable {
            failed_probe_count + 1
        } else {
            0
        };
        let changed = if v14 {
            self.connection.execute(
                "UPDATE model_route_health
                 SET state = 'unavailable', checked_at = ?1, failure_category = 'native_check_failed',
                     failed_probe_count = ?2, next_probe_at_ms = ?3, last_http_status = ?4,
                     consecutive_failures = 0
                 WHERE model_route_id = ?5 AND quarantine_epoch = ?6",
                params![
                    timeutil::system_epoch_seconds(),
                    failed_probe_count,
                    now_ms + settings.interval_for(failed_probe_count),
                    http_status,
                    route_id,
                    quarantine_epoch
                ],
            )?
        } else {
            self.connection.execute(
                "UPDATE model_route_health
                 SET state = 'unavailable', checked_at = ?1, failure_category = 'native_check_failed',
                     failed_probe_count = ?2, next_probe_at_ms = ?3
                 WHERE model_route_id = ?4",
                params![
                    timeutil::system_epoch_seconds(),
                    failed_probe_count,
                    now_ms + settings.interval_for(failed_probe_count),
                    route_id
                ],
            )?
        };
        if changed != 1 {
            bail!("model route does not exist");
        }
        self.record_event(
            crate::log::SECTION_ROUTES,
            crate::log::SEVERITY_INFO,
            "routes.check",
            None,
            &json!({
                "route_id": route_id,
                "result": "unavailable",
                "failed_probe_count": failed_probe_count
            }),
        );
        Ok(())
    }

    fn route_health_row_exists(&mut self, route_id: &str) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM model_route_health WHERE model_route_id = ?1)",
                [route_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Replaces a provider's cached upstream model catalog (REL-006). The
    /// cache records what `GET /v1/models` actually returned; it never feeds
    /// candidate selection directly.
    pub fn cache_upstream_models(
        &mut self,
        provider_id: &str,
        models: &[String],
        fetched_at_ms: i64,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM upstream_model_cache WHERE upstream_provider_id = ?1",
            [provider_id],
        )?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO upstream_model_cache (upstream_provider_id, model_name, fetched_at)
                 VALUES (?1, ?2, ?3)",
            )?;
            for model in models {
                statement.execute(params![provider_id, model, fetched_at_ms])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every provider connection (id, Base URL, upstream API key) for the
    /// catalog sync scheduler and refresh endpoint (REL-006).
    pub fn provider_connections(&mut self) -> Result<Vec<(String, String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT id, base_url, api_key FROM upstream_providers ORDER BY id",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// One provider's connection (Base URL, upstream API key) for the
    /// catalog refresh endpoint (REL-006).
    pub fn provider_connection(
        &mut self,
        provider_id: &str,
    ) -> Result<Option<(String, String)>> {
        self.connection
            .query_row(
                "SELECT base_url, api_key FROM upstream_providers WHERE id = ?1",
                [provider_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(Into::into)
    }

    /// The time of the provider's most recent successful catalog fetch.
    pub fn cached_upstream_models_fetched_at(
        &mut self,
        provider_id: &str,
    ) -> Result<Option<i64>> {
        // The aggregate always returns one row; an empty cache yields NULL.
        self.connection
            .query_row(
                "SELECT MAX(fetched_at) FROM upstream_model_cache WHERE upstream_provider_id = ?1",
                [provider_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(Into::into)
    }

    /// The last fetched model catalog for a provider, oldest first, or an
    /// empty list when nothing was fetched yet.
    pub fn cached_upstream_models(&mut self, provider_id: &str) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT model_name FROM upstream_model_cache
             WHERE upstream_provider_id = ?1 ORDER BY model_name ASC",
        )?;
        statement
            .query_map([provider_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Puts one route back into Checking without touching the persisted
    /// recovery schedule of any other route (REL-003 freshness sweep).
    pub fn reset_route_checking(&mut self, route_id: &str) -> Result<()> {
        let reset_extra = if self.health_v14()? {
            ", consecutive_failures = 0"
        } else {
            ""
        };
        self.connection.execute(
            &format!("UPDATE model_route_health
             SET state = 'checking', checked_at = NULL, failure_category = NULL,
                 failed_probe_count = 0, next_probe_at_ms = NULL{reset_extra}
             WHERE model_route_id = ?1"),
            [route_id],
        )?;
        Ok(())
    }

    /// Probe configurations for every Available route, used by the periodic
    /// light-validation sweep (REL-003).
    pub fn available_route_probe_configurations(&mut self) -> Result<Vec<ProbeConfiguration>> {
        let epoch = self.probe_epoch_sql()?;
        let mut statement = self.connection.prepare(&format!(
            "SELECT r.id, u.base_url, u.api_key, r.upstream_model_name, r.protocol, u.id, {epoch}
             FROM model_route_health h
             JOIN model_routes r ON r.id = h.model_route_id
             JOIN upstream_providers u ON u.id = r.upstream_provider_id
             WHERE h.state = 'available'
             ORDER BY r.id"
        ))?;
        statement
            .query_map([], |row| {
                Ok(ProbeConfiguration {
                    route_id: row.get(0)?,
                    base_url: row.get(1)?,
                    api_key: row.get(2)?,
                    upstream_model_name: row.get(3)?,
                    protocol: row.get(4)?,
                    provider_id: row.get(5)?,
                    quarantine_epoch: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Creates a published model (REL-007): the client-visible name is the
    /// stable id, and prices are RMB per million tokens. Deprecated models
    /// cannot be recreated under the same name while they still exist.
    pub fn create_published_model(
        &mut self,
        name: &str,
        input_price_rmb: &str,
        output_price_rmb: &str,
        cached_input_price_rmb: &str,
    ) -> Result<()> {
        let name = validate_published_model_name(name)?;
        let input =
            parse_non_negative_decimal(input_price_rmb, "input price", "input_price_rmb")?;
        let output = parse_non_negative_decimal(
            output_price_rmb,
            "output price",
            "output_price_rmb",
        )?;
        let cached = parse_non_negative_decimal(
            cached_input_price_rmb,
            "cached input price",
            "cached_input_price_rmb",
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM published_models WHERE name = ?1)",
            [&name],
            |row| row.get(0),
        )?;
        if exists {
            bail!("a published model with this name already exists");
        }
        transaction.execute(
            "INSERT INTO published_models (id, name, input_price_micrormb_per_million,
                    output_price_micrormb_per_million, cached_input_price_micrormb_per_million, deprecated_at)
             VALUES (?1, ?1, ?2, ?3, ?4, NULL)",
            params![name, input, output, cached],
        )?;
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(())
    }

    /// Deprecates a published model (REL-007): existing routes keep serving
    /// it, new routes must not reference it. Idempotent.
    pub fn deprecate_published_model(&mut self, model_id: &str) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE published_models SET deprecated_at = COALESCE(deprecated_at, ?1)
             WHERE id = ?2",
            params![timeutil::system_epoch_seconds(), model_id],
        )?;
        if changed != 1 {
            bail!("published model does not exist");
        }
        Ok(())
    }

    pub fn update_published_model_prices(
        &mut self,
        model_id: &str,
        input_price_rmb: &str,
        output_price_rmb: &str,
        cached_input_price_rmb: &str,
    ) -> Result<()> {
        let input = parse_non_negative_decimal(input_price_rmb, "input price", "input_price_rmb")?;
        let output =
            parse_non_negative_decimal(output_price_rmb, "output price", "output_price_rmb")?;
        let cached = parse_non_negative_decimal(
            cached_input_price_rmb,
            "cached input price",
            "cached_input_price_rmb",
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE published_models
             SET input_price_micrormb_per_million = ?1,
                 output_price_micrormb_per_million = ?2,
                 cached_input_price_micrormb_per_million = ?3
             WHERE id = ?4",
            params![input, output, cached, model_id],
        )?;
        if changed != 1 {
            bail!("published model does not exist");
        }
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_relay_access_key(
        &mut self,
        label: &str,
        model_route_ids: &[String],
    ) -> Result<CreatedRelayAccessKey> {
        let label = validate_relay_key_label(label)?;
        let unique_route_ids = valid_eligible_model_route_ids(model_route_ids)?;

        let secret = format!("lar_{}", auth::generate_secret());
        let secret_prefix = secret[..12].to_owned();
        let secret_hash = auth::hash_token(&secret);
        let key = CreatedRelayAccessKey {
            id: new_id(),
            secret,
            secret_prefix,
            label,
            created_at: timeutil::system_epoch_seconds(),
            model_route_ids: unique_route_ids,
        };
        // The secret column exists at schema >= 17 (REL-010); the
        // schema-version test hook serves older fixtures, which keep the
        // hash-only historical storage.
        let store_secret = schema_version(&self.connection)?.unwrap_or(SCHEMA_VERSION) >= 17;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for route_id in &key.model_route_ids {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM model_routes WHERE id = ?1)",
                [route_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(field_error(
                    "model_route_ids",
                    "selected eligible model route does not exist",
                )
                .into());
            }
        }
        if store_secret {
            transaction.execute(
                "INSERT INTO relay_access_keys (id, secret_prefix, secret_hash, label, created_at, revoked_at, secret)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
                params![
                    key.id,
                    key.secret_prefix,
                    secret_hash.as_slice(),
                    key.label,
                    key.created_at,
                    key.secret
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO relay_access_keys (id, secret_prefix, secret_hash, label, created_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                params![
                    key.id,
                    key.secret_prefix,
                    secret_hash.as_slice(),
                    key.label,
                    key.created_at
                ],
            )?;
        }
        for route_id in &key.model_route_ids {
            transaction.execute(
                "INSERT INTO relay_key_route_eligibility (relay_access_key_id, model_route_id)
                 VALUES (?1, ?2)",
                params![key.id, route_id],
            )?;
        }
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(key)
    }

    pub fn update_relay_access_key(
        &mut self,
        key_id: &str,
        label: &str,
        model_route_ids: &[String],
    ) -> Result<()> {
        let label = validate_relay_key_label(label)?;
        let unique_route_ids = valid_eligible_model_route_ids(model_route_ids)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM relay_access_keys WHERE id = ?1)",
            [key_id],
            |row| row.get(0),
        )?;
        if !key_exists {
            bail!("relay access key does not exist");
        }
        for route_id in &unique_route_ids {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM model_routes WHERE id = ?1)",
                [route_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(field_error(
                    "model_route_ids",
                    "selected eligible model route does not exist",
                )
                .into());
            }
        }
        transaction.execute(
            "UPDATE relay_access_keys SET label = ?1 WHERE id = ?2",
            params![label, key_id],
        )?;
        transaction.execute(
            "DELETE FROM relay_key_route_eligibility WHERE relay_access_key_id = ?1",
            [key_id],
        )?;
        for route_id in unique_route_ids {
            transaction.execute(
                "INSERT INTO relay_key_route_eligibility (relay_access_key_id, model_route_id)
                 VALUES (?1, ?2)",
                params![key_id, route_id],
            )?;
        }
        fail_config_commit_if_requested()?;
        transaction.commit()?;
        Ok(())
    }

    pub fn relay_access_keys(
        &mut self,
        search: Option<&str>,
    ) -> Result<Vec<RelayAccessKeySummary>> {
        let search = search.unwrap_or("").trim();
        let pattern = format!("%{search}%");
        // The secret column exists at schema >= 17 (REL-010); older schemas
        // (test hook) list keys without a recoverable secret.
        let with_secret = schema_version(&self.connection)?.unwrap_or(SCHEMA_VERSION) >= 17;
        let keys = {
            let select = if with_secret {
                "SELECT id, secret_prefix, label, created_at, revoked_at, secret
                 FROM relay_access_keys
                 WHERE label LIKE ?1 COLLATE NOCASE OR secret_prefix LIKE ?1 COLLATE NOCASE
                 ORDER BY created_at DESC, id"
            } else {
                "SELECT id, secret_prefix, label, created_at, revoked_at, NULL
                 FROM relay_access_keys
                 WHERE label LIKE ?1 COLLATE NOCASE OR secret_prefix LIKE ?1 COLLATE NOCASE
                 ORDER BY created_at DESC, id"
            };
            let mut statement = self.connection.prepare(select)?;
            statement
                .query_map([pattern], |row| {
                    Ok(RelayAccessKeySummary {
                        id: row.get(0)?,
                        secret_prefix: row.get(1)?,
                        label: row.get(2)?,
                        created_at: row.get(3)?,
                        revoked_at: row.get(4)?,
                        secret: row.get(5)?,
                        model_route_ids: Vec::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut keys_with_scope = Vec::with_capacity(keys.len());
        for mut key in keys {
            let mut statement = self.connection.prepare(
                "SELECT model_route_id FROM relay_key_route_eligibility
                 WHERE relay_access_key_id = ?1 ORDER BY model_route_id",
            )?;
            key.model_route_ids = statement
                .query_map([key.id.as_str()], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys_with_scope.push(key);
        }
        Ok(keys_with_scope)
    }

    pub fn revoke_relay_access_key(&mut self, key_id: &str) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE relay_access_keys
             SET revoked_at = COALESCE(revoked_at, ?1)
             WHERE id = ?2",
            params![timeutil::system_epoch_seconds(), key_id],
        )?;
        if changed != 1 {
            bail!("relay access key does not exist");
        }
        Ok(())
    }

    pub fn authenticate_relay_access_key(&mut self, secret: &str) -> Result<Option<String>> {
        let secret_hash = auth::hash_token(secret);
        self.connection
            .query_row(
                "SELECT id FROM relay_access_keys
                 WHERE secret_hash = ?1 AND revoked_at IS NULL",
                [secret_hash.as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Model names and the persisted health of every route eligible for a key,
    /// so the server can apply the in-memory health overlay (API-003/DATA-005).
    pub fn eligible_model_route_health(&mut self, key_id: &str) -> Result<Vec<EligibleModelRoute>> {
        let mut statement = self.connection.prepare(
            "SELECT p.name, r.id, h.state
             FROM relay_key_route_eligibility e
             JOIN model_routes r ON r.id = e.model_route_id
             JOIN model_route_health h ON h.model_route_id = r.id
             JOIN published_models p ON p.id = r.published_model_id
             WHERE e.relay_access_key_id = ?1
             ORDER BY p.name, r.id",
        )?;
        statement
            .query_map([key_id], |row| {
                let health: String = row.get(2)?;
                Ok(EligibleModelRoute {
                    model_name: row.get(0)?,
                    route_id: row.get(1)?,
                    health: RouteHealth::from_persisted(&health),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn eligible_chat_routes(
        &mut self,
        key_id: &str,
        published_model_name: &str,
    ) -> Result<Vec<ModelRouteCandidate>> {
        // The persisted health state is returned (not filtered) so the server
        // can apply the in-memory health overlay on top of it (DATA-005): a
        // route whose health history write failed must still transition in
        // memory immediately.
        let mut statement = self.connection.prepare(
            "SELECT r.id, u.id, u.display_name, u.base_url, u.api_key,
                    r.upstream_model_name, h.state
             FROM relay_key_route_eligibility e
             JOIN model_routes r ON r.id = e.model_route_id
             JOIN model_route_health h ON h.model_route_id = r.id
             JOIN published_models p ON p.id = r.published_model_id
             JOIN upstream_providers u ON u.id = r.upstream_provider_id
             WHERE e.relay_access_key_id = ?1
               AND p.name = ?2
               AND r.protocol = 'chat_completions'
             ORDER BY r.cost_multiplier_micros ASC, r.id ASC",
        )?;
        let routes = statement
            .query_map(params![key_id, published_model_name], |row| {
                let health: String = row.get(6)?;
                Ok(ModelRouteCandidate {
                    route_id: row.get(0)?,
                    provider_id: row.get(1)?,
                    provider_name: row.get(2)?,
                    base_url: row.get(3)?,
                    api_key: row.get(4)?,
                    upstream_model_name: row.get(5)?,
                    health: RouteHealth::from_persisted(&health),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(routes)
    }

    pub fn has_eligible_chat_route(
        &mut self,
        key_id: &str,
        published_model_name: &str,
    ) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1
                    FROM relay_key_route_eligibility e
                    JOIN model_routes r ON r.id = e.model_route_id
                    JOIN published_models p ON p.id = r.published_model_id
                    WHERE e.relay_access_key_id = ?1
                      AND p.name = ?2
                      AND r.protocol = 'chat_completions'
                 )",
                params![key_id, published_model_name],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn eligible_responses_routes(
        &mut self,
        key_id: &str,
        published_model_name: &str,
    ) -> Result<Vec<ModelRouteCandidate>> {
        let mut statement = self.connection.prepare(
            "SELECT r.id, u.id, u.display_name, u.base_url, u.api_key,
                    r.upstream_model_name, h.state
             FROM relay_key_route_eligibility e
             JOIN model_routes r ON r.id = e.model_route_id
             JOIN model_route_health h ON h.model_route_id = r.id
             JOIN published_models p ON p.id = r.published_model_id
             JOIN upstream_providers u ON u.id = r.upstream_provider_id
             WHERE e.relay_access_key_id = ?1
               AND p.name = ?2
               AND r.protocol = 'responses'
             ORDER BY r.cost_multiplier_micros ASC, r.id ASC",
        )?;
        let routes = statement
            .query_map(params![key_id, published_model_name], |row| {
                let health: String = row.get(6)?;
                Ok(ModelRouteCandidate {
                    route_id: row.get(0)?,
                    provider_id: row.get(1)?,
                    provider_name: row.get(2)?,
                    base_url: row.get(3)?,
                    api_key: row.get(4)?,
                    upstream_model_name: row.get(5)?,
                    health: RouteHealth::from_persisted(&health),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(routes)
    }

    /// Marks a single Available model route unavailable after one attributable
    /// failure and schedules the first recovery probe after the base interval.
    /// Success of an already in-flight request never restores a route; only a
    /// dedicated recovery check may return it to Available (ROUTE-010).
    /// Records one attributable failure against a route (REL-004). The
    /// consecutive counter advances; only when it reaches the configured
    /// quarantine threshold does the route actually quarantine (with a fresh
    /// recovery schedule and a bumped quarantine epoch, REL-005). Below the
    /// threshold the route stays available and only the counter changes.
    pub fn quarantine_route(
        &mut self,
        route_id: &str,
        failure_category: &str,
        http_status: Option<i64>,
    ) -> Result<()> {
        fail_operational_write_if_requested(OPERATIONAL_CATEGORY_ROUTE_HEALTH)?;
        let settings = self.recovery_settings()?;
        let v14 = self.health_v14()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !v14 {
            // Pre-v14 health shape (schema-version test hook): quarantine
            // immediately on one attributable failure, the historical
            // ROUTE-006 semantics.
            let changed = transaction.execute(
                "UPDATE model_route_health
                 SET state = 'unavailable', checked_at = ?1, failure_category = ?2,
                     failed_probe_count = 0, next_probe_at_ms = ?3
                 WHERE model_route_id = ?4 AND state = 'available'",
                params![
                    timeutil::system_epoch_seconds(),
                    failure_category,
                    timeutil::recovery_clock_now_ms()
                        .unwrap_or_else(timeutil::system_epoch_millis)
                        + settings.interval_for(0),
                    route_id
                ],
            )?;
            transaction.commit()?;
            if changed == 1 {
                self.record_event(
                    crate::log::SECTION_ROUTES,
                    crate::log::SEVERITY_WARNING,
                    "routes.quarantined",
                    None,
                    &json!({ "route_id": route_id, "failure_category": failure_category }),
                );
            }
            return Ok(());
        }
        transaction.execute(
            "UPDATE model_route_health
             SET consecutive_failures = consecutive_failures + 1, last_http_status = ?1
             WHERE model_route_id = ?2",
            params![http_status, route_id],
        )?;
        let (count, state): (i64, String) = transaction.query_row(
            "SELECT consecutive_failures, state FROM model_route_health WHERE model_route_id = ?1",
            [route_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count < settings.quarantine_threshold || state != "available" {
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "UPDATE model_route_health
             SET state = 'unavailable', checked_at = ?1, failure_category = ?2,
                 failed_probe_count = 0, next_probe_at_ms = ?3, last_http_status = ?4,
                 consecutive_failures = 0, quarantine_epoch = quarantine_epoch + 1
             WHERE model_route_id = ?5",
            params![
                timeutil::system_epoch_seconds(),
                failure_category,
                timeutil::recovery_clock_now_ms().unwrap_or_else(timeutil::system_epoch_millis)
                    + settings.interval_for(0),
                http_status,
                route_id
            ],
        )?;
        transaction.commit()?;
        self.record_event(
            crate::log::SECTION_ROUTES,
            crate::log::SEVERITY_WARNING,
            "routes.quarantined",
            None,
            &json!({ "route_id": route_id, "failure_category": failure_category }),
        );
        Ok(())
    }

    /// Clears the consecutive-failure counter after a successful call or a
    /// successful probe (REL-004).
    pub fn clear_route_failure_count(&mut self, route_id: &str) -> Result<()> {
        if !self.health_v14()? {
            return Ok(());
        }
        self.connection.execute(
            "UPDATE model_route_health SET consecutive_failures = 0 WHERE model_route_id = ?1",
            [route_id],
        )?;
        Ok(())
    }

    pub fn has_eligible_responses_route(
        &mut self,
        key_id: &str,
        published_model_name: &str,
    ) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1
                    FROM relay_key_route_eligibility e
                    JOIN model_routes r ON r.id = e.model_route_id
                    JOIN published_models p ON p.id = r.published_model_id
                    WHERE e.relay_access_key_id = ?1
                      AND p.name = ?2
                      AND r.protocol = 'responses'
                 )",
                params![key_id, published_model_name],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn operations_snapshot(&mut self) -> Result<OperationsSnapshot> {
        let catalog = {
            let mut statement = self.connection.prepare(
                "SELECT id, name, input_price_micrormb_per_million,
                        output_price_micrormb_per_million, cached_input_price_micrormb_per_million,
                        deprecated_at IS NOT NULL
                 FROM published_models ORDER BY name",
            )?;
            statement
                .query_map([], |row| {
                    Ok(PublishedModel {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        input_price_rmb: format_decimal(row.get(2)?),
                        output_price_rmb: format_decimal(row.get(3)?),
                        cached_input_price_rmb: format_decimal(row.get(4)?),
                        deprecated: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let providers = {
            let mut statement = self.connection.prepare(
                "SELECT id, display_name FROM upstream_providers ORDER BY display_name, id",
            )?;
            statement
                .query_map([], |row| {
                    Ok(ProviderSummary {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        api_key_masked: "********",
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let routes = {
            let mut statement = self.connection.prepare(
                "SELECT r.id, p.id, p.name, u.id, u.display_name, r.upstream_model_name,
                        r.protocol, r.cost_multiplier_micros, h.state, h.checked_at,
                        h.failure_category, h.last_http_status, h.failed_probe_count,
                        h.next_probe_at_ms
                 FROM model_routes r
                 JOIN published_models p ON p.id = r.published_model_id
                 JOIN upstream_providers u ON u.id = r.upstream_provider_id
                 JOIN model_route_health h ON h.model_route_id = r.id
                 ORDER BY p.name, r.id",
            )?;
            statement
                .query_map([], |row| {
                    let health: String = row.get(8)?;
                    Ok(RouteSummary {
                        id: row.get(0)?,
                        published_model_id: row.get(1)?,
                        published_model_name: row.get(2)?,
                        provider_id: row.get(3)?,
                        provider_name: row.get(4)?,
                        upstream_model_name: row.get(5)?,
                        protocol: row.get(6)?,
                        cost_multiplier: format_decimal(row.get(7)?),
                        health: RouteHealth::from_persisted(&health),
                        last_checked_at: row.get(9)?,
                        failure_category: row.get(10)?,
                        last_http_status: row.get(11)?,
                        failed_probe_count: row.get(12)?,
                        next_probe_at_ms: row.get(13)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(OperationsSnapshot {
            catalog,
            providers,
            routes,
        })
    }

    /// Persists one metadata-only call record with its ordered attempt chain
    /// (OPS-001/OPS-003). Reliable facts are written transactionally; the relay
    /// calls this best-effort so a persistence failure never fails an already
    /// successful response (DATA-004). The estimated cost is computed here
    /// from the successful attempt's route multiplier and the published
    /// model's prices (OPS-006), and the same reliable usage feeds the
    /// permanent daily aggregate (OPS-009).
    pub fn record_call(&mut self, record: &NewCallRecord) -> Result<()> {
        fail_operational_write_if_requested(OPERATIONAL_CATEGORY_CALL_RECORDS)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cost = if record.succeeded {
            compute_call_cost(&transaction, record)?
        } else {
            None
        };
        transaction.execute(
            "INSERT INTO call_records
             (created_at_ms, published_model_name, protocol, streamed, succeeded,
              success_provider_id, success_provider_name, input_tokens,
              cached_input_tokens, output_tokens, estimated_cost_rmb,
              completion_ms, first_token_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.created_at_ms,
                record.published_model_name,
                record.protocol,
                record.streamed as i64,
                record.succeeded as i64,
                record.success_provider_id,
                record.success_provider_name,
                record.usage.as_ref().map(|usage| usage.input_tokens),
                record.usage.as_ref().map(|usage| usage.cached_input_tokens),
                record.usage.as_ref().map(|usage| usage.output_tokens),
                cost,
                record.completion_ms,
                record.first_token_ms,
            ],
        )?;
        let call_record_id = transaction.last_insert_rowid();
        for attempt in &record.attempts {
            transaction.execute(
                "INSERT INTO call_attempts
                 (call_record_id, sequence, route_id, provider_id, provider_name,
                  started_at_ms, duration_ms, http_status, failure_category,
                  commit_phase, outcome)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    call_record_id,
                    attempt.sequence,
                    attempt.route_id,
                    attempt.provider_id,
                    attempt.provider_name,
                    attempt.started_at_ms,
                    attempt.duration_ms,
                    attempt.http_status,
                    attempt.failure_category,
                    attempt.commit_phase,
                    attempt.outcome,
                ],
            )?;
        }
        // The permanent daily aggregate keeps model/provider token and cost
        // totals after the per-call records expire; it never stores call IDs
        // or attempt details (OPS-009).
        if let Some(cost) = cost
            && let Some(usage) = record.usage.as_ref()
            && let Some(success) = record
                .attempts
                .iter()
                .find(|attempt| attempt.outcome == "success")
        {
            transaction.execute(
                "INSERT INTO daily_usage
                 (day, published_model_name, provider_id, provider_name,
                  input_tokens, cached_input_tokens, output_tokens, estimated_cost_rmb)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(day, published_model_name, provider_id) DO UPDATE SET
                    input_tokens = input_tokens + excluded.input_tokens,
                    cached_input_tokens = cached_input_tokens + excluded.cached_input_tokens,
                    output_tokens = output_tokens + excluded.output_tokens,
                    estimated_cost_rmb = estimated_cost_rmb + excluded.estimated_cost_rmb",
                params![
                    timeutil::date_key(record.created_at_ms),
                    record.published_model_name,
                    success.provider_id,
                    success.provider_name,
                    usage.input_tokens,
                    usage.cached_input_tokens,
                    usage.output_tokens,
                    cost,
                ],
            )?;
        }
        // A successful re-write of the degraded category closes the matching
        // open usage gap; the historical gap stays as a permanent
        // incompleteness marker (OPS-012/OPS-016).
        let closed_gaps = transaction.execute(
            "UPDATE usage_gaps
             SET ended_at_ms = ?1
             WHERE category = ?2 AND ended_at_ms IS NULL",
            params![record.created_at_ms, OPERATIONAL_CATEGORY_CALL_RECORDS],
        )?;
        transaction.commit()?;
        if closed_gaps > 0 {
            self.record_event(
                crate::log::SECTION_USAGE,
                crate::log::SEVERITY_INFO,
                "usage.gap_closed",
                None,
                &json!({ "category": OPERATIONAL_CATEGORY_CALL_RECORDS }),
            );
        }
        Ok(())
    }

    /// Returns one page of call records newest-first with each record's ordered
    /// attempt chain (UI-010/UI-011). Page numbers are zero-based.
    pub fn call_record_page(&mut self, page: i64, page_size: i64) -> Result<CallRecordPage> {
        let total: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM call_records", [], |row| row.get(0))?;
        let offset = page.saturating_mul(page_size);
        let records = {
            let mut statement = self.connection.prepare(
                "SELECT id, created_at_ms, published_model_name, protocol, streamed, succeeded,
                        success_provider_id, success_provider_name, input_tokens,
                        cached_input_tokens, output_tokens, estimated_cost_rmb,
                        completion_ms, first_token_ms
                 FROM call_records
                 ORDER BY created_at_ms DESC, id DESC
                 LIMIT ?1 OFFSET ?2",
            )?;
            statement
                .query_map(params![page_size, offset], |row| {
                    Ok(CallRecord {
                        id: row.get(0)?,
                        created_at_ms: row.get(1)?,
                        published_model_name: row.get(2)?,
                        protocol: row.get(3)?,
                        streamed: row.get::<_, i64>(4)? != 0,
                        succeeded: row.get::<_, i64>(5)? != 0,
                        success_provider_id: row.get(6)?,
                        success_provider_name: row.get(7)?,
                        input_tokens: row.get(8)?,
                        cached_input_tokens: row.get(9)?,
                        output_tokens: row.get(10)?,
                        estimated_cost_rmb: row.get(11)?,
                        completion_ms: row.get(12)?,
                        first_token_ms: row.get(13)?,
                        attempts: Vec::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut calls = Vec::with_capacity(records.len());
        for mut record in records {
            let mut statement = self.connection.prepare(
                "SELECT sequence, route_id, provider_id, provider_name, started_at_ms,
                        duration_ms, http_status, failure_category, commit_phase, outcome
                 FROM call_attempts
                 WHERE call_record_id = ?1
                 ORDER BY sequence ASC",
            )?;
            record.attempts = statement
                .query_map([record.id], |row| {
                    Ok(ModelRouteAttempt {
                        sequence: row.get(0)?,
                        route_id: row.get(1)?,
                        provider_id: row.get(2)?,
                        provider_name: row.get(3)?,
                        started_at_ms: row.get(4)?,
                        duration_ms: row.get(5)?,
                        http_status: row.get(6)?,
                        failure_category: row.get(7)?,
                        commit_phase: row.get(8)?,
                        outcome: row.get(9)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            calls.push(record);
        }
        Ok(CallRecordPage { calls, total })
    }

    /// Aggregates reliable successful usage for one time window (OPS-008).
    /// `now_ms` is supplied by the caller so a test clock can control the
    /// window boundary. Windows up to 14 days read per-call records; the
    /// all-time window reads the permanent daily aggregate (OPS-009).
    pub fn usage_totals(&mut self, window: &str, now_ms: i64) -> Result<UsageTotals> {
        let rows = match window_span_ms(window) {
            Some(span) => {
                let mut statement = self.connection.prepare(
                    "SELECT published_model_name, success_provider_id, success_provider_name,
                            SUM(input_tokens), SUM(cached_input_tokens), SUM(output_tokens),
                            COALESCE(SUM(estimated_cost_rmb), 0)
                     FROM call_records
                     WHERE succeeded = 1 AND input_tokens IS NOT NULL AND created_at_ms >= ?1
                     GROUP BY published_model_name, success_provider_id, success_provider_name",
                )?;
                statement
                    .query_map([now_ms - span], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, f64>(6)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut statement = self.connection.prepare(
                    "SELECT published_model_name, provider_id, provider_name,
                            SUM(input_tokens), SUM(cached_input_tokens), SUM(output_tokens),
                            COALESCE(SUM(estimated_cost_rmb), 0)
                     FROM daily_usage
                     GROUP BY published_model_name, provider_id, provider_name",
                )?;
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, f64>(6)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        let mut input_tokens = 0_i64;
        let mut cached_input_tokens = 0_i64;
        let mut output_tokens = 0_i64;
        let mut estimated_cost_rmb = 0.0_f64;
        let mut models: Vec<ModelUsageShare> = Vec::new();
        for (model_name, provider_id, provider_name, input, cached, output, cost) in rows {
            input_tokens += input;
            cached_input_tokens += cached;
            output_tokens += output;
            estimated_cost_rmb += cost;
            if let Some(model) = models
                .iter_mut()
                .find(|model| model.published_model_name == model_name)
            {
                model.input_tokens += input;
                model.cached_input_tokens += cached;
                model.output_tokens += output;
                model.estimated_cost_rmb += cost;
                model.providers.push(ProviderUsageShare {
                    provider_id,
                    provider_name,
                    input_tokens: input,
                    cached_input_tokens: cached,
                    output_tokens: output,
                    estimated_cost_rmb: cost,
                });
            } else {
                models.push(ModelUsageShare {
                    published_model_name: model_name,
                    input_tokens: input,
                    cached_input_tokens: cached,
                    output_tokens: output,
                    estimated_cost_rmb: cost,
                    providers: vec![ProviderUsageShare {
                        provider_id,
                        provider_name,
                        input_tokens: input,
                        cached_input_tokens: cached,
                        output_tokens: output,
                        estimated_cost_rmb: cost,
                    }],
                });
            }
        }
        let cache_hit_rate = if input_tokens > 0 {
            cached_input_tokens as f64 / input_tokens as f64
        } else {
            0.0
        };
        Ok(UsageTotals {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            estimated_cost_rmb,
            cache_hit_rate,
            models,
        })
    }

    /// Records one lost call-record write as a usage gap: the first failure of
    /// a category opens a gap, later failures widen it (known lost count),
    /// until a successful re-write closes it (OPS-012/OPS-016). Durable so the
    /// incompleteness marker survives storage recovery and restarts.
    pub fn record_usage_gap(&mut self, category: &str, now_ms: i64) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE usage_gaps
             SET lost_records = lost_records + 1
             WHERE category = ?1 AND ended_at_ms IS NULL",
            [category],
        )?;
        if changed == 0 {
            self.connection.execute(
                "INSERT INTO usage_gaps (category, started_at_ms, ended_at_ms, lost_records)
                 VALUES (?1, ?2, NULL, 1)",
                params![category, now_ms],
            )?;
            self.record_event(
                crate::log::SECTION_USAGE,
                crate::log::SEVERITY_WARNING,
                "usage.gap_opened",
                None,
                &json!({ "category": category }),
            );
        }
        Ok(())
    }

    /// Every persisted usage gap, oldest first. Used by the Operations usage
    /// completeness area (OPS-010/OPS-016).
    pub fn persisted_usage_gaps(&mut self) -> Result<Vec<UsageGap>> {
        let mut statement = self.connection.prepare(
            "SELECT category, started_at_ms, ended_at_ms, lost_records
             FROM usage_gaps
             ORDER BY started_at_ms ASC, id ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(category, started_at_ms, ended_at_ms, lost_records)| UsageGap {
                kind: USAGE_GAP_KIND_PERSISTENCE,
                category,
                started_at_ms,
                ended_at_ms,
                lost_records,
            })
            .collect())
    }

    /// Known usage gaps overlapping `window` (OPS-016): durable persistence
    /// gaps plus, per window, successful calls whose upstream reported no
    /// usage. `complete` is false exactly when any gap overlaps; gaps are never
    /// estimated or backfilled.
    pub fn usage_integrity(&mut self, window: &str, now_ms: i64) -> Result<UsageIntegrity> {
        let gaps = self.collect_usage_gaps(window_span_ms(window), now_ms)?;
        Ok(UsageIntegrity {
            complete: gaps.is_empty(),
            gaps,
        })
    }

    /// Usage completeness across all time, for the Operations status area.
    pub fn usage_integrity_all(&mut self) -> Result<UsageIntegrity> {
        let gaps = self.collect_usage_gaps(None, 0)?;
        Ok(UsageIntegrity {
            complete: gaps.is_empty(),
            gaps,
        })
    }

    fn collect_usage_gaps(&mut self, span: Option<i64>, now_ms: i64) -> Result<Vec<UsageGap>> {
        let mut gaps = self.persisted_usage_gaps()?;
        if let Some(span) = span {
            let window_start = now_ms - span;
            gaps.retain(|gap| {
                gap.started_at_ms < now_ms
                    && (gap.ended_at_ms.is_none() || gap.ended_at_ms >= Some(window_start))
            });
        }
        let window_filter = match span {
            Some(_) => " AND created_at_ms >= ?1",
            None => "",
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT created_at_ms
             FROM call_records
             WHERE succeeded = 1 AND input_tokens IS NULL{window_filter}
             ORDER BY created_at_ms ASC, id ASC"
        ))?;
        let rows = match span {
            Some(span) => statement
                .query_map([now_ms - span], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => statement
                .query_map([], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        gaps.extend(rows.into_iter().map(|created_at_ms| UsageGap {
            kind: USAGE_GAP_KIND_MISSING_UPSTREAM_USAGE,
            category: "upstream_usage".to_owned(),
            started_at_ms: created_at_ms,
            ended_at_ms: Some(created_at_ms),
            lost_records: 1,
        }));
        gaps.sort_by_key(|gap| (gap.started_at_ms, gap.category.clone()));
        Ok(gaps)
    }

    /// True when any per-call record or daily aggregate exists, so the
    /// Operations usage area can distinguish "no data" from "complete".
    pub fn usage_data_present(&mut self) -> Result<bool> {
        let calls: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM call_records", [], |row| row.get(0))?;
        let days: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM daily_usage", [], |row| row.get(0))?;
        Ok(calls > 0 || days > 0)
    }

    /// Lightweight SQLite integrity check (`PRAGMA quick_check`) used as the
    /// recovery condition for the Storage Degraded state (OPS-012).
    pub fn verify_quick_check(&mut self) -> Result<()> {
        let result: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result != "ok" {
            bail!("SQLite quick check failed");
        }
        Ok(())
    }

    /// Deletes per-call records (and their attempt chains, via cascade) older
    /// than the retention window while leaving the permanent daily aggregate
    /// untouched (OPS-009).
    pub fn delete_expired_call_records(&mut self, now_ms: i64, retention_ms: i64) -> Result<i64> {
        let deleted = self.connection.execute(
            "DELETE FROM call_records WHERE created_at_ms < ?1",
            [now_ms - retention_ms],
        )?;
        Ok(deleted as i64)
    }

    /// Persists one metadata-only operational event (OPS-017/OPS-018). The
    /// event is part of the 14-day diagnostic history and is best-effort: a
    /// failure never fails the surrounding operation, because the same event
    /// already flowed to standard error and the managed log (OPS-019).
    pub fn record_operational_event(
        &mut self,
        section: &str,
        severity: &str,
        event_code: &str,
        correlation_id: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let payload_json = serde_json::to_string(payload)
            .context("operational event payload must be serializable")?;
        self.connection.execute(
            "INSERT INTO operational_events
             (occurred_at_ms, section, severity, event_code, version, correlation_id, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                crate::timeutil::now_epoch_ms(),
                section,
                severity,
                event_code,
                crate::log::process_version(),
                correlation_id,
                payload_json,
            ],
        )?;
        Ok(())
    }

    /// Returns one page of operational events newest-first, optionally filtered
    /// to one section, with the total count for pagination. Used by the
    /// Operations status drill-down (OPS-010).
    pub fn operational_event_page(
        &mut self,
        section: Option<&str>,
        page: i64,
        page_size: i64,
    ) -> Result<OperationalEventPage> {
        let total: i64 = match section {
            Some(section) => self.connection.query_row(
                "SELECT COUNT(*) FROM operational_events WHERE section = ?1",
                [section],
                |row| row.get(0),
            )?,
            None => self
                .connection
                .query_row("SELECT COUNT(*) FROM operational_events", [], |row| {
                    row.get(0)
                })?,
        };
        let offset = page.saturating_mul(page_size);
        let mut statement = match section {
            Some(_) => self.connection.prepare(
                "SELECT id, occurred_at_ms, section, severity, event_code, version,
                        correlation_id, payload_json
                 FROM operational_events
                 WHERE section = ?1
                 ORDER BY occurred_at_ms DESC, id DESC
                 LIMIT ?2 OFFSET ?3",
            )?,
            None => self.connection.prepare(
                "SELECT id, occurred_at_ms, section, severity, event_code, version,
                        correlation_id, payload_json
                 FROM operational_events
                 ORDER BY occurred_at_ms DESC, id DESC
                 LIMIT ?1 OFFSET ?2",
            )?,
        };
        let rows = match section {
            Some(section) => statement
                .query_map(params![section, page_size, offset], event_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => statement
                .query_map(params![page_size, offset], event_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(OperationalEventPage {
            events: rows,
            total,
        })
    }

    /// Deletes operational events older than the retention window, leaving the
    /// current status, managed backup metadata, and permanent daily aggregates
    /// untouched (OPS-009).
    pub fn delete_expired_operational_events(
        &mut self,
        now_ms: i64,
        retention_ms: i64,
    ) -> Result<i64> {
        let deleted = self.connection.execute(
            "DELETE FROM operational_events WHERE occurred_at_ms < ?1",
            [now_ms - retention_ms],
        )?;
        Ok(deleted as i64)
    }

    /// Records an operational event to standard error and the 14-day
    /// diagnostic history in one step (OPS-017/OPS-018). The persistence is
    /// best-effort: the event already flowed to stderr and the managed log, so
    /// a history write failure never fails the surrounding operation.
    pub(crate) fn record_event(
        &mut self,
        section: &str,
        severity: &str,
        code: &str,
        correlation_id: Option<&str>,
        payload: &serde_json::Value,
    ) {
        crate::log::emit(section, severity, code, correlation_id, payload);
        let _ = self.record_operational_event(section, severity, code, correlation_id, payload);
    }

    /// Creates and verifies a consistent full backup artifact, then rotates the
    /// managed set to the most recent backups. A failure at any stage removes
    /// only the new artifact; existing verified backups are never deleted.
    pub fn create_backup(
        &mut self,
        trigger: backup::TriggerKind,
    ) -> Result<backup::BackupArtifact> {
        let now = timeutil::now_epoch();
        let mut state = backup::read_state(&self.backup_dir)?;
        let artifact = match backup::create_artifact(
            &self.connection,
            &self.backup_dir,
            trigger,
            now,
            state.next_sequence,
            supported_schema(),
        ) {
            Ok(artifact) => artifact,
            Err(failure) => {
                self.record_backup_failure(&mut state, failure.stage, &failure.reason, now);
                bail!("{failure}");
            }
        };
        state.next_sequence += 1;
        if trigger == backup::TriggerKind::Auto {
            state.last_auto_backup_at = Some(now);
        }
        state.last_snapshot_writes = Some(self.data_change_writes()?);
        state.last_failure_stage = None;
        state.last_failure_reason = None;
        state.last_failure_at = None;
        if let Err(error) = backup::write_state(&self.backup_dir, &state) {
            let _ = fs::remove_file(&artifact.path);
            self.record_backup_failure(&mut state, "verify", "could not record backup state", now);
            return Err(error);
        }
        if let Err(failure) = backup::rotate(&self.backup_dir, backup::RETENTION) {
            self.record_backup_failure(&mut state, failure.stage, &failure.reason, now);
            bail!("{failure}");
        }
        self.record_event(
            crate::log::SECTION_BACKUPS,
            crate::log::SEVERITY_INFO,
            "backup.created",
            None,
            &json!({
                "name": artifact.name,
                "trigger": trigger.as_str(),
                "schema_version": supported_schema(),
                "size": artifact.size
            }),
        );
        Ok(artifact)
    }

    fn record_backup_failure(
        &mut self,
        state: &mut backup::BackupState,
        stage: &str,
        reason: &str,
        now: i64,
    ) {
        state.last_failure_stage = Some(stage.to_owned());
        state.last_failure_reason = Some(reason.to_owned());
        state.last_failure_at = Some(now);
        let _ = backup::write_state(&self.backup_dir, state);
        self.record_event(
            crate::log::SECTION_BACKUPS,
            crate::log::SEVERITY_ERROR,
            "backup.failed",
            None,
            &json!({ "stage": stage, "reason": reason }),
        );
    }

    /// Creates an automatic backup when durable data changed since the last
    /// snapshot and no automatic backup exists within the current 24-hour
    /// window. Returns whether a backup was created.
    pub fn maybe_create_auto_backup(&mut self) -> Result<bool> {
        let now = timeutil::now_epoch();
        let state = backup::read_state(&self.backup_dir)?;
        if Some(self.data_change_writes()?) == state.last_snapshot_writes {
            return Ok(false);
        }
        if let Some(last_auto_backup_at) = state.last_auto_backup_at
            && now.saturating_sub(last_auto_backup_at) < backup::AUTO_BACKUP_INTERVAL_SECONDS
        {
            return Ok(false);
        }
        self.create_backup(backup::TriggerKind::Auto)?;
        Ok(true)
    }

    pub fn list_backups(&mut self) -> Result<Vec<backup::BackupArtifact>> {
        backup::list_artifacts(&self.backup_dir)
    }

    pub fn backup_status(&mut self) -> Result<backup::BackupStatus> {
        let state = backup::read_state(&self.backup_dir)?;
        let artifacts = backup::list_artifacts(&self.backup_dir)?;
        let latest = artifacts.first();
        let changed = Some(self.data_change_writes()?) != state.last_snapshot_writes;
        let now = timeutil::now_epoch();
        let next_auto_backup_at = if changed {
            let base = state.last_auto_backup_at.unwrap_or(0);
            Some((base + backup::AUTO_BACKUP_INTERVAL_SECONDS).max(now))
        } else {
            None
        };
        let degraded = match (state.last_failure_at, latest) {
            (Some(failure_at), Some(artifact)) => failure_at >= artifact.created_at,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let state_label = if degraded {
            "degraded"
        } else if latest.is_some() {
            "ok"
        } else {
            "none"
        };
        Ok(backup::BackupStatus {
            state: state_label.to_owned(),
            last_backup_at: latest.as_ref().map(|artifact| artifact.created_at),
            last_trigger: latest.as_ref().map(|artifact| artifact.trigger.clone()),
            schema_version: latest.as_ref().map(|artifact| artifact.schema_version),
            last_size: latest.as_ref().map(|artifact| artifact.size),
            next_auto_backup_at,
            count: artifacts.len(),
            retention: backup::RETENTION,
            last_failed_stage: state.last_failure_stage,
            last_failed_reason: state.last_failure_reason,
        })
    }

    fn data_change_writes(&self) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT writes FROM data_change_signal WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .context("data change signal is missing")
    }

    /// Creates the backup-gated snapshot required before a forward migration or
    /// an explicit restore (DATA-007/DATA-015). Unlike `create_backup` it works
    /// for schemas that predate `backup_metadata`/`data_change_signal` and
    /// never consults the data-change counter; `source_schema` is the version
    /// actually snapshotted. Failures are recorded in the backup state so the
    /// Operations backups card shows the normalized stage and reason, and a
    /// verified snapshot rotates the managed set like any other backup
    /// (DATA-012).
    fn create_gate_backup(
        &mut self,
        trigger: backup::TriggerKind,
        source_schema: i64,
    ) -> Result<backup::BackupArtifact, backup::BackupFailure> {
        let now = timeutil::now_epoch();
        let mut state = backup::read_state(&self.backup_dir).map_err(|_| backup::BackupFailure {
            stage: "create",
            reason: "could not read the backup state".to_owned(),
        })?;
        let artifact = match backup::create_artifact(
            &self.connection,
            &self.backup_dir,
            trigger,
            now,
            state.next_sequence,
            source_schema,
        ) {
            Ok(artifact) => artifact,
            Err(failure) => {
                self.record_backup_failure(&mut state, failure.stage, &failure.reason, now);
                return Err(failure);
            }
        };
        state.next_sequence += 1;
        state.last_failure_stage = None;
        state.last_failure_reason = None;
        state.last_failure_at = None;
        if let Err(_error) = backup::write_state(&self.backup_dir, &state) {
            let _ = fs::remove_file(&artifact.path);
            return Err(backup::BackupFailure {
                stage: "verify",
                reason: "could not record backup state".to_owned(),
            });
        }
        if let Err(failure) = backup::rotate(&self.backup_dir, backup::RETENTION) {
            self.record_backup_failure(&mut state, failure.stage, &failure.reason, now);
            return Err(failure);
        }
        self.record_event(
            crate::log::SECTION_BACKUPS,
            crate::log::SEVERITY_INFO,
            "backup.created",
            None,
            &json!({
                "name": artifact.name,
                "trigger": trigger.as_str(),
                "schema_version": source_schema,
                "size": artifact.size
            }),
        );
        Ok(artifact)
    }

    /// Ensures the single `data_operations` row exists and records the
    /// running/supported schema and the given migration state. Any prior
    /// operation record (migration or restore) is preserved, so a routine
    /// startup never hides the most recent migration or restore (OPS-015).
    fn ensure_data_operations(
        &mut self,
        migration_state: &str,
        migrated_from_schema: Option<i64>,
        pre_backup_ok: Option<bool>,
        pre_backup_name: Option<&str>,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO data_operations
                 (id, running_schema, supported_schema, migration_state, migrated_from_schema,
                  pre_backup_ok, pre_backup_name, last_phase, last_result)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, 'none', 'none')
             ON CONFLICT(id) DO UPDATE SET
                running_schema = excluded.running_schema,
                supported_schema = excluded.supported_schema,
                migration_state = excluded.migration_state,
                migrated_from_schema = excluded.migrated_from_schema,
                pre_backup_ok = excluded.pre_backup_ok,
                pre_backup_name = excluded.pre_backup_name",
            params![
                supported_schema(),
                supported_schema(),
                migration_state,
                migrated_from_schema,
                pre_backup_ok,
                pre_backup_name
            ],
        )?;
        Ok(())
    }

    /// Records the outcome of the most recent migration or restore operation.
    fn record_operation(
        &mut self,
        phase: &str,
        result: &str,
        completed_at: i64,
        failed_stage: Option<&str>,
        failed_reason: Option<&str>,
        restore_source: Option<&str>,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE data_operations
             SET last_phase = ?1, last_result = ?2, last_completed_at = ?3,
                 last_failed_stage = ?4, last_failed_reason = ?5, restore_source = ?6
             WHERE id = 1",
            params![
                phase,
                result,
                completed_at,
                failed_stage,
                failed_reason,
                restore_source
            ],
        )?;
        Ok(())
    }

    pub fn data_operations_status(&mut self) -> Result<DataOperationsStatus> {
        let record = self
            .connection
            .query_row(
                "SELECT running_schema, supported_schema, migration_state, migrated_from_schema,
                        pre_backup_ok, pre_backup_name, last_phase, last_result, last_completed_at,
                        last_failed_stage, last_failed_reason, restore_source
                 FROM data_operations WHERE id = 1",
                [],
                |row| {
                    Ok(DataOperationsStatus {
                        running_schema: row.get(0)?,
                        supported_schema: row.get(1)?,
                        migration_state: row.get(2)?,
                        migrated_from_schema: row.get(3)?,
                        pre_backup_ok: row.get(4)?,
                        pre_backup_name: row.get(5)?,
                        last_phase: row.get(6)?,
                        last_result: row.get(7)?,
                        last_completed_at: row.get(8)?,
                        last_failed_stage: row.get(9)?,
                        last_failed_reason: row.get(10)?,
                        restore_source: row.get(11)?,
                    })
                },
            )
            .optional()?;
        Ok(record.unwrap_or(DataOperationsStatus {
            running_schema: supported_schema(),
            supported_schema: supported_schema(),
            migration_state: "current".to_owned(),
            migrated_from_schema: None,
            pre_backup_ok: None,
            pre_backup_name: None,
            last_phase: "none".to_owned(),
            last_result: "none".to_owned(),
            last_completed_at: None,
            last_failed_stage: None,
            last_failed_reason: None,
            restore_source: None,
        }))
    }

    /// Explicit restore (DATA-014/015/016). The candidate is selected from the
    /// managed backup set by artifact name, the current database is preserved
    /// through a restore-gate backup, the candidate is verified in isolation
    /// (integrity, identity, schema) and upgraded under the same forward-only
    /// contract when older, and only then switched into place. A failure at any
    /// pre-switch stage keeps the current database selected. On success every
    /// model route returns to Checking so restored health never influences
    /// candidates (OPS-015), and the caller re-probes them. `progress` is
    /// invoked at each stage transition so the management surface can show the
    /// in-flight stage (UI-012/OPS-015) while the synchronous restore runs.
    pub fn restore_from_backup(
        &mut self,
        backup_name: &str,
        progress: &mut dyn FnMut(RestoreStage),
    ) -> Result<RestoreOutcome> {
        let now = timeutil::now_epoch();
        progress(RestoreStage::Verify);
        let current_schema: i64 = self
            .connection
            .query_row("SELECT version FROM schema_metadata WHERE id = 1", [], |row| {
                row.get(0)
            })
            .context("could not read the current schema version")?;

        let Some(candidate) = backup::list_artifacts(&self.backup_dir)?
            .into_iter()
            .find(|artifact| artifact.name == backup_name)
        else {
            let reason = format!("backup {backup_name} is not in the managed backup set");
            record_restore_failure(self, now, "select", &reason, None);
            bail!("{reason}");
        };

        // Preserve the current database before any switch (DATA-015).
        let pre_restore_backup =
            match self.create_gate_backup(backup::TriggerKind::Restore, current_schema) {
                Ok(artifact) => artifact,
                Err(failure) => {
                    let reason = format!("could not preserve the current database: {failure}");
                    record_restore_failure(self, now, "backup_current", &reason, Some(&candidate.name));
                    bail!("{reason}");
                }
            };

        // Stage a private copy of the candidate so the artifact itself is
        // never modified: verification and any migration run on the staged
        // file, and only the staged file is switched into place (DATA-014,
        // DATA-017). The artifact keeps its provenance, so a second restore of
        // the same backup re-verifies and re-migrates it identically.
        let directory = self
            .database_path
            .parent()
            .context("database path has no parent")?;
        let staging = directory.join("relay.sqlite3.restore-tmp");
        let (_, candidate_schema) =
            match stage_verified_candidate(&self.backup_dir, backup_name, &staging) {
                Ok(pair) => pair,
                Err(failure) => {
                    let _ = fs::remove_file(&staging);
                    record_restore_failure(
                        self,
                        now,
                        failure.stage(),
                        failure.reason(),
                        failure.source(&candidate.name),
                    );
                    bail!("{failure}");
                }
            };
        progress(RestoreStage::Switch);

        if let Err(error) = self.swap_database(&staging) {
            let reason = format!("could not switch to the candidate: {error}");
            record_restore_failure(self, now, "switch", &reason, Some(&candidate.name));
            bail!("{reason}");
        }

        // Record the successful restore in the restored database, then re-enter
        // Checking for every model route (DATA-016/OPS-015).
        let restored_state = if candidate_schema < supported_schema() {
            "migrated"
        } else {
            "current"
        };
        self.ensure_data_operations(
            restored_state,
            (candidate_schema < supported_schema()).then_some(candidate_schema),
            None,
            None,
        )?;
        self.record_operation("restore", "ok", now, None, None, Some(&candidate.name))?;
        self.record_event(
            crate::log::SECTION_MIGRATION,
            crate::log::SEVERITY_INFO,
            "restore.completed",
            None,
            &json!({
                "source": candidate.name,
                "from_schema": candidate_schema,
                "running_schema": supported_schema(),
                "pre_restore_backup": pre_restore_backup.name
            }),
        );
        let probes = self.startup_probe_configurations()?;
        // The recheck stage completes once every restored route is reset to
        // Checking and the native re-probe configurations are read (DATA-016);
        // the probes themselves run asynchronously after the restore returns.
        progress(RestoreStage::Recheck);

        Ok(RestoreOutcome {
            candidate_name: candidate.name,
            candidate_schema,
            restored_schema: supported_schema(),
            pre_restore_backup_name: pre_restore_backup.name,
            completed_at: now,
            probe_configurations: probes,
        })
    }

    /// Switches the live database to the fully verified staged candidate file
    /// (already copied next to the live database and migrated/verified by the
    /// caller). The current database is moved aside until the switch completes;
    /// any failure before the atomic rename restores it and reconnects the
    /// store, so a failed switch leaves the current database selected and
    /// usable (DATA-015).
    fn swap_database(&mut self, staged_path: &Path) -> Result<()> {
        let _ = self.connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        let old_connection = std::mem::replace(
            &mut self.connection,
            Connection::open_in_memory().context("could not open an in-memory placeholder")?,
        );
        if let Err((_connection, error)) = old_connection.close() {
            let _ = fs::remove_file(staged_path);
            self.reopen_live_database()?;
            bail!("could not close the live database: {error}");
        }

        let directory = self
            .database_path
            .parent()
            .with_context(|| format!("database path {} has no parent", self.database_path.display()))?;
        let previous = directory.join("relay.sqlite3.pre-restore");

        // Move the current database aside until the switch fully succeeds.
        if let Err(error) = fs::rename(&self.database_path, &previous) {
            let _ = fs::remove_file(staged_path);
            self.reopen_live_database()?;
            return Err(error).with_context(|| {
                format!("could not preserve the current database at {}", self.database_path.display())
            });
        }
        let restore_previous = || -> Result<()> {
            fs::rename(&previous, &self.database_path).with_context(|| {
                format!("could not restore the current database to {}", self.database_path.display())
            })
        };

        let _ = fs::remove_file(self.database_path.with_extension("sqlite3-wal"));
        let _ = fs::remove_file(self.database_path.with_extension("sqlite3-shm"));
        if let Err(error) = fs::rename(staged_path, &self.database_path) {
            let _ = restore_previous();
            let _ = self.reopen_live_database();
            return Err(error).context("could not switch to the candidate backup");
        }

        // The switch is committed at the file level; reconnect to the candidate.
        self.connection = match Connection::open(&self.database_path) {
            Ok(connection) => connection,
            Err(error) => {
                // Extremely unlikely after full verification; restore the
                // previous database and its connection so the process keeps
                // serving it.
                let _ = restore_previous();
                let _ = self.reopen_live_database();
                return Err(error).context("the candidate could not be opened after the switch");
            }
        };
        paths::restrict_file(&self.database_path)?;
        configure_connection(&self.connection)?;
        verify_integrity(&self.connection)?;

        // The switch succeeded; the current database is preserved by the
        // restore-gate backup artifact.
        let _ = fs::remove_file(&previous);
        Ok(())
    }

    /// Reconnects the store to the live database file, used to keep the current
    /// database selected and usable when a restore switch fails.
    fn reopen_live_database(&mut self) -> Result<()> {
        self.connection = Connection::open(&self.database_path).with_context(|| {
            format!("could not reopen the database at {}", self.database_path.display())
        })?;
        paths::restrict_file(&self.database_path)?;
        configure_connection(&self.connection)?;
        Ok(())
    }

    fn insert_session(&mut self, token: &str, kind: SessionKind, expires_at: i64) -> Result<()> {
        let token_hash = auth::hash_token(token);
        let kind = match kind {
            SessionKind::Bootstrap => "bootstrap",
            SessionKind::Active => "active",
        };
        self.connection.execute(
            "INSERT INTO administrator_sessions (token_hash, kind, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![token_hash.as_slice(), kind, expires_at, timeutil::system_epoch_seconds()],
        )?;
        Ok(())
    }
}

/// Opens a database connection and applies the persistence prerequisites
/// (foreign keys, WAL, full durability, DATA-001). An existing file that fails
/// to configure is reported as corruption so it is preserved as evidence and
/// never silently re-initialized (DATA-013).
fn open_and_configure(database_path: &Path, existed: bool) -> Result<Connection> {
    let connection = Connection::open(database_path).with_context(|| {
        format!("could not open SQLite database at {}", database_path.display())
    })?;
    paths::restrict_file(database_path)?;
    configure_connection(&connection).with_context(|| {
        if existed {
            format!(
                "SQLite database at {} is corrupted or not a valid local-api-relay \
                 database; it was left untouched for evidence — restore from a \
                 verified backup",
                database_path.display()
            )
        } else {
            format!("could not configure SQLite database at {}", database_path.display())
        }
    })?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;",
    )?;
    let foreign_keys: i64 = connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        bail!("SQLite foreign keys could not be enabled");
    }
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        bail!("SQLite WAL mode could not be enabled");
    }
    Ok(())
}

/// Reads the persisted integer schema version, or None when the database has
/// no `schema_metadata` table (a brand-new file or an existing foreign or
/// truncated file, which the caller must disambiguate).
fn schema_version(connection: &Connection) -> Result<Option<i64>> {
    let metadata_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_metadata')",
        [],
        |row| row.get(0),
    )?;
    if !metadata_exists {
        return Ok(None);
    }
    let version: i64 = connection.query_row(
        "SELECT version FROM schema_metadata WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(Some(version))
}

/// Records a failed restore attempt in the current database's operation
/// history, best-effort: the original failure is what the caller surfaces.
fn record_restore_failure(
    store: &mut Store,
    now: i64,
    stage: &str,
    reason: &str,
    source: Option<&str>,
) {
    let _ = store.record_operation("restore", "failed", now, Some(stage), Some(reason), source);
    store.record_event(
        crate::log::SECTION_MIGRATION,
        crate::log::SEVERITY_ERROR,
        "restore.failed",
        None,
        &json!({ "stage": stage, "reason": reason, "source": source }),
    );
}

/// Reads the persisted schema version of an existing database through a
/// read-only connection — no migration and no write ever touches the live
/// database. Used by the offline `check` and `restore` commands so an upgrade
/// pre-flight or a rollback can inspect a database this binary may not be able
/// to open through `Store::open` (for example a newer schema, PKG-013/PKG-014).
pub fn read_live_schema(database_path: &Path) -> Result<Option<i64>> {
    if !database_path.exists() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("could not open {} for inspection", database_path.display()))?;
    schema_version(&connection).with_context(|| {
        format!(
            "SQLite database at {} is corrupted or not a valid local-api-relay database",
            database_path.display()
        )
    })
}

/// Creates a verified managed backup without opening the database through
/// `Store::open`, so no migration or write ever touches the live database. This
/// is the upgrade pre-flight's pre-migration snapshot (PKG-013): it must exist
/// and verify before the stable entry is switched, and the rollback restores it
/// to undo a committed forward migration with the previous binary (PKG-014).
pub fn create_backup_at_paths(
    database_path: &Path,
    backup_dir: &Path,
    trigger: backup::TriggerKind,
) -> Result<backup::BackupArtifact> {
    let now = timeutil::now_epoch();
    let mut state = backup::read_state(backup_dir)?;
    let connection = Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("could not open {} for backup", database_path.display()))?;
    let Some(source_schema) = schema_version(&connection)? else {
        bail!(
            "database {} has no local-api-relay schema; nothing to back up",
            database_path.display()
        );
    };
    let artifact = match backup::create_artifact(
        &connection,
        backup_dir,
        trigger,
        now,
        state.next_sequence,
        source_schema,
    ) {
        Ok(artifact) => artifact,
        Err(failure) => {
            record_backup_failure_at_paths(backup_dir, &mut state, failure.stage, &failure.reason, now);
            bail!("{failure}");
        }
    };
    state.next_sequence += 1;
    // The store-free path does not read the data-change counter; unknown writes
    // must never suppress the 24-hour automatic snapshot.
    state.last_snapshot_writes = None;
    state.last_failure_stage = None;
    state.last_failure_reason = None;
    state.last_failure_at = None;
    if let Err(error) = backup::write_state(backup_dir, &state) {
        let _ = fs::remove_file(&artifact.path);
        return Err(error);
    }
    if let Err(failure) = backup::rotate(backup_dir, backup::RETENTION) {
        record_backup_failure_at_paths(backup_dir, &mut state, failure.stage, &failure.reason, now);
        bail!("{failure}");
    }
    crate::log::emit(
        crate::log::SECTION_BACKUPS,
        crate::log::SEVERITY_INFO,
        "backup.created",
        None,
        &json!({
            "name": artifact.name,
            "trigger": trigger.as_str(),
            "schema_version": source_schema,
            "size": artifact.size
        }),
    );
    Ok(artifact)
}

/// Records a backup failure in the managed set's bookkeeping without a store,
/// mirroring `Store::record_backup_failure`.
fn record_backup_failure_at_paths(
    backup_dir: &Path,
    state: &mut backup::BackupState,
    stage: &str,
    reason: &str,
    now: i64,
) {
    state.last_failure_stage = Some(stage.to_owned());
    state.last_failure_reason = Some(reason.to_owned());
    state.last_failure_at = Some(now);
    let _ = backup::write_state(backup_dir, state);
    crate::log::emit(
        crate::log::SECTION_BACKUPS,
        crate::log::SEVERITY_ERROR,
        "backup.failed",
        None,
        &json!({ "stage": stage, "reason": reason }),
    );
}

/// A failure during the staged-candidate phase of a restore, tagged with the
/// OPS-015 stage it occurred at so both restore paths record the same detail.
#[derive(Debug)]
enum RestoreStagingFailure {
    /// The candidate is not in the managed backup set.
    Select(String),
    /// The candidate copy could not be staged, or failed integrity, identity,
    /// or schema verification.
    Verify(String),
    /// The staged candidate's forward migration failed and rolled back.
    Migrate(String),
    /// The staged candidate could not be closed cleanly before the switch.
    Close(String),
}

impl RestoreStagingFailure {
    fn stage(&self) -> &'static str {
        match self {
            Self::Select(_) => "select",
            Self::Verify(_) => "verify_candidate",
            Self::Migrate(_) => "migrate_candidate",
            Self::Close(_) => "switch",
        }
    }

    fn reason(&self) -> &str {
        match self {
            Self::Select(reason)
            | Self::Verify(reason)
            | Self::Migrate(reason)
            | Self::Close(reason) => reason,
        }
    }

    /// The restore failure's restore-source field: the select stage has no
    /// candidate, every later stage names the requested backup.
    fn source<'a>(&self, backup_name: &'a str) -> Option<&'a str> {
        match self {
            Self::Select(_) => None,
            _ => Some(backup_name),
        }
    }
}

impl std::fmt::Display for RestoreStagingFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.reason())
    }
}

impl std::error::Error for RestoreStagingFailure {}

/// Stages the selected backup artifact and verifies it in isolation for a
/// restore: copies the artifact to `staging`, checks its integrity, identity,
/// and schema, rejects candidates newer than this binary supports or below
/// schema 1, migrates an older candidate under the same forward-only contract
/// in a transaction (DATA-008/DATA-014), and checkpoints and closes it so the
/// staged file is complete. The artifact itself is never modified; the caller
/// owns the swap and the failure recording.
fn stage_verified_candidate(
    backup_dir: &Path,
    backup_name: &str,
    staging: &Path,
) -> Result<(String, i64), RestoreStagingFailure> {
    let Some(candidate) = backup::list_artifacts(backup_dir)
        .map_err(|_| RestoreStagingFailure::Select("could not list the managed backups".to_owned()))?
        .into_iter()
        .find(|artifact| artifact.name == backup_name)
    else {
        return Err(RestoreStagingFailure::Select(format!(
            "backup {backup_name} is not in the managed backup set"
        )));
    };
    if let Err(error) = fs::copy(&candidate.path, staging) {
        return Err(RestoreStagingFailure::Verify(format!(
            "could not stage the candidate backup: {error}"
        )));
    }
    let (candidate_connection, candidate_schema) =
        backup::open_candidate(staging).map_err(|failure| {
            let _ = fs::remove_file(staging);
            RestoreStagingFailure::Verify(format!("candidate failed verification: {failure}"))
        })?;
    if candidate_schema > supported_schema() {
        drop(candidate_connection);
        let _ = fs::remove_file(staging);
        return Err(RestoreStagingFailure::Verify(format!(
            "backup {backup_name} has schema v{candidate_schema}, newer than this binary supports"
        )));
    }
    if candidate_schema < 1 {
        drop(candidate_connection);
        let _ = fs::remove_file(staging);
        return Err(RestoreStagingFailure::Verify(format!(
            "backup {backup_name} has an unsupported schema version {candidate_schema}"
        )));
    }
    if candidate_schema < supported_schema() {
        let migrated = (|| -> Result<()> {
            let transaction = candidate_connection.unchecked_transaction()?;
            run_migrations(&transaction, false)?;
            // Verified inside the transaction so a failed candidate migration
            // rolls back (DATA-008/DATA-014).
            verify_integrity(&transaction)?;
            transaction.commit()?;
            Ok(())
        })();
        if let Err(error) = migrated {
            drop(candidate_connection);
            let _ = fs::remove_file(staging);
            return Err(RestoreStagingFailure::Migrate(format!(
                "candidate migration failed: {error}"
            )));
        }
    }
    let _ = candidate_connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    if let Err((_connection, error)) = candidate_connection.close() {
        let _ = fs::remove_file(staging);
        return Err(RestoreStagingFailure::Close(format!(
            "candidate could not be closed cleanly: {error}"
        )));
    }
    Ok((candidate.name, candidate_schema))
}

/// Restores a managed backup artifact into the live database without opening
/// the live database through `Store::open`. This is what makes a rollback to a
/// previous version possible when the live database is newer than the previous
/// binary supports: `Store::open` would reject it (DATA-008) before any restore
/// could run, so the whole restore operates at the file level with the same
/// contract as `Store::restore_from_backup` (DATA-014/015/016) — preserve the
/// current database, verify and (if older) migrate a staged candidate in
/// isolation, and only then switch it into place. On success the restored
/// database's model routes all return to Checking (DATA-016).
pub fn restore_database_at_paths(
    database_path: &Path,
    backup_dir: &Path,
    backup_name: &str,
) -> Result<RestoreOutcome> {
    let now = timeutil::now_epoch();

    // The live database is inspected read-only: it may be newer than this
    // binary supports, and nothing about the restore may modify it before the
    // switch.
    let live_connection = Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("could not open {} for restore", database_path.display()))?;
    let Some(live_schema) = schema_version(&live_connection)? else {
        bail!(
            "database {} has no local-api-relay schema",
            database_path.display()
        );
    };

    // Preserve the current database before any switch (DATA-015). The snapshot
    // bound is the live schema itself: the current database may be newer than
    // this binary supports, and the artifact must still verify as internally
    // consistent so it stays restorable later.
    let pre_restore_backup = match create_preserve_backup_at_paths(
        &live_connection,
        backup_dir,
        live_schema,
        now,
    ) {
        Ok(artifact) => artifact,
        Err(failure) => {
            let reason = format!("could not preserve the current database: {failure}");
            record_restore_failure_at_paths(
                database_path,
                now,
                "backup_current",
                &reason,
                Some(backup_name),
            );
            bail!("{reason}");
        }
    };

    // Stage a private copy so the artifact itself is never modified; all
    // verification and any migration run on the staged file (DATA-014).
    let staging = database_path.with_extension("sqlite3.restore-tmp");
    let (candidate_name, candidate_schema) =
        match stage_verified_candidate(backup_dir, backup_name, &staging) {
            Ok(pair) => pair,
            Err(failure) => {
                let _ = fs::remove_file(&staging);
                record_restore_failure_at_paths(
                    database_path,
                    now,
                    failure.stage(),
                    failure.reason(),
                    failure.source(backup_name),
                );
                bail!("{failure}");
            }
        };

    if let Err(error) = swap_database_files(database_path, &staging) {
        let reason = format!("could not switch to the candidate: {error}");
        record_restore_failure_at_paths(database_path, now, "switch", &reason, Some(&candidate_name));
        bail!("{reason}");
    }

    // The switch is committed at the file level; reopen the restored database
    // and record the operation, then return every model route to Checking
    // (DATA-016/OPS-015). The restored candidate was migrated to the supported
    // schema during staging, so `Store::open` accepts it without a migration.
    let mut restored = Store::open(database_path, backup_dir)?;
    let restored_state = if candidate_schema < supported_schema() {
        "migrated"
    } else {
        "current"
    };
    restored.ensure_data_operations(
        restored_state,
        (candidate_schema < supported_schema()).then_some(candidate_schema),
        None,
        None,
    )?;
    restored.record_operation("restore", "ok", now, None, None, Some(&candidate_name))?;
    restored.record_event(
        crate::log::SECTION_MIGRATION,
        crate::log::SEVERITY_INFO,
        "restore.completed",
        None,
        &json!({
            "source": candidate_name,
            "from_schema": candidate_schema,
            "running_schema": supported_schema(),
            "pre_restore_backup": pre_restore_backup.name
        }),
    );
    let probes = restored.startup_probe_configurations()?;

    Ok(RestoreOutcome {
        candidate_name,
        candidate_schema,
        restored_schema: supported_schema(),
        pre_restore_backup_name: pre_restore_backup.name,
        completed_at: now,
        probe_configurations: probes,
    })
}

/// Creates the preserve-before-restore snapshot of the current database at the
/// file level, mirroring `Store::create_gate_backup` with the live schema as
/// the artifact's own verification bound so a newer-than-supported database is
/// still preserved as a restorable artifact (DATA-015).
fn create_preserve_backup_at_paths(
    connection: &Connection,
    backup_dir: &Path,
    live_schema: i64,
    now: i64,
) -> Result<backup::BackupArtifact, backup::BackupFailure> {
    let mut state = backup::read_state(backup_dir).map_err(|_| backup::BackupFailure {
        stage: "create",
        reason: "could not read the backup state".to_owned(),
    })?;
    let artifact = match backup::create_artifact(
        connection,
        backup_dir,
        backup::TriggerKind::Restore,
        now,
        state.next_sequence,
        live_schema,
    ) {
        Ok(artifact) => artifact,
        Err(failure) => {
            record_backup_failure_at_paths(backup_dir, &mut state, failure.stage, &failure.reason, now);
            return Err(failure);
        }
    };
    state.next_sequence += 1;
    state.last_failure_stage = None;
    state.last_failure_reason = None;
    state.last_failure_at = None;
    if let Err(_error) = backup::write_state(backup_dir, &state) {
        let _ = fs::remove_file(&artifact.path);
        return Err(backup::BackupFailure {
            stage: "verify",
            reason: "could not record backup state".to_owned(),
        });
    }
    if let Err(failure) = backup::rotate(backup_dir, backup::RETENTION) {
        record_backup_failure_at_paths(backup_dir, &mut state, failure.stage, &failure.reason, now);
        return Err(failure);
    }
    Ok(artifact)
}

/// Atomically switches the live database file to a fully verified staged file,
/// preserving the current file aside until the switch completes; any failure
/// before the atomic rename restores it, so the current database stays selected
/// (DATA-015). On success the current database has already been preserved by
/// the restore-gate backup artifact.
fn swap_database_files(database_path: &Path, staged_path: &Path) -> Result<()> {
    let previous = database_path.with_extension("sqlite3.pre-restore");
    if let Err(error) = fs::rename(database_path, &previous) {
        let _ = fs::remove_file(staged_path);
        return Err(error).with_context(|| {
            format!(
                "could not preserve the current database at {}",
                database_path.display()
            )
        });
    }
    let restore_previous = || -> Result<()> {
        fs::rename(&previous, database_path).with_context(|| {
            format!(
                "could not restore the current database to {}",
                database_path.display()
            )
        })
    };
    let _ = fs::remove_file(database_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(database_path.with_extension("sqlite3-shm"));
    if let Err(error) = fs::rename(staged_path, database_path) {
        let _ = restore_previous();
        return Err(error).context("could not switch to the candidate backup");
    }
    // The switch succeeded; the current database is preserved by the restore-
    // gate backup artifact.
    let _ = fs::remove_file(&previous);
    paths::restrict_file(database_path)?;
    Ok(())
}

/// Records a failed restore attempt at the file level, best-effort: the
/// original failure is what the caller surfaces, and the event always flows to
/// stderr and the managed log (OPS-017).
fn record_restore_failure_at_paths(
    database_path: &Path,
    now: i64,
    stage: &str,
    reason: &str,
    source: Option<&str>,
) {
    let _ = (|| -> Result<()> {
        let connection = Connection::open(database_path)?;
        let has_operations: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'data_operations')",
            [],
            |row| row.get(0),
        )?;
        if has_operations {
            connection.execute(
                "UPDATE data_operations
                 SET last_phase = 'restore', last_result = 'failed', last_completed_at = ?1,
                     last_failed_stage = ?2, last_failed_reason = ?3, restore_source = ?4
                 WHERE id = 1",
                params![now, stage, reason, source],
            )?;
        }
        Ok(())
    })();
    crate::log::emit(
        crate::log::SECTION_MIGRATION,
        crate::log::SEVERITY_ERROR,
        "restore.failed",
        None,
        &json!({ "stage": stage, "reason": reason, "source": source }),
    );
}

fn fail_migration_if_requested() -> Result<()> {
    if std::env::var(TEST_FAIL_MIGRATION_VARIABLE).ok().as_deref() == Some("1") {
        bail!("injected migration failure");
    }
    Ok(())
}

/// Applies the ordered, forward-only migration chain in a single transaction
/// (DATA-006/DATA-007). `fresh` means a brand-new database, whose initial
/// schema (version 1) is created first. Any failure rolls back the whole
/// chain, leaving the old database untouched (DATA-008).
fn run_migrations(transaction: &rusqlite::Transaction<'_>, fresh: bool) -> Result<i64> {
    if fresh {
        transaction.execute_batch(
            "CREATE TABLE schema_metadata (
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
             INSERT INTO schema_metadata (id, version) VALUES (1, 1);",
        )?;
    }

    let mut version: i64 = transaction.query_row(
        "SELECT version FROM schema_metadata WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    let target = supported_schema();
    while version < target {
        match version {
            1 => {
                transaction.execute_batch(
                    "CREATE TABLE upstream_providers (
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
                         failure_category TEXT
                     );",
                )?;
                seed_published_models(transaction)?;
            }
            2 => {
                transaction.execute_batch(
                    "CREATE TABLE relay_access_keys (
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
                     );",
                )?;
            }
            3 => {
                // Idempotent because a migration backup of a pre-v3 database
                // already carries the identity `backup_metadata` injected into
                // the artifact copy (DATA-009): restoring such a candidate must
                // migrate cleanly rather than collide with the injected table.
                transaction.execute_batch(
                    "CREATE TABLE IF NOT EXISTS backup_metadata (
                         id INTEGER PRIMARY KEY CHECK (id = 1),
                         application TEXT NOT NULL,
                         created_at INTEGER NOT NULL,
                         trigger TEXT NOT NULL CHECK (trigger IN ('seed', 'auto', 'manual', 'migration', 'restore')),
                         source_schema_version INTEGER NOT NULL
                     );
                     CREATE TABLE IF NOT EXISTS data_change_signal (
                         id INTEGER PRIMARY KEY CHECK (id = 1),
                         writes INTEGER NOT NULL DEFAULT 0
                     );
                     INSERT INTO backup_metadata (id, application, created_at, trigger, source_schema_version)
                         VALUES (1, 'local-api-relay', 0, 'seed', 0)
                         ON CONFLICT(id) DO NOTHING;
                     INSERT INTO data_change_signal (id, writes) VALUES (1, 0)
                         ON CONFLICT(id) DO NOTHING;",
                )?;
                for table in [
                    "administrator_credentials",
                    "upstream_providers",
                    "published_models",
                    "model_routes",
                    "model_route_health",
                    "relay_access_keys",
                    "relay_key_route_eligibility",
                ] {
                    for (kind, action) in [
                        ("insert", "INSERT"),
                        ("update", "UPDATE"),
                        ("delete", "DELETE"),
                    ] {
                        transaction.execute_batch(&format!(
                            "CREATE TRIGGER data_change_{table}_{kind} AFTER {action} ON {table}
                             BEGIN UPDATE data_change_signal SET writes = writes + 1 WHERE id = 1; END;"
                        ))?;
                    }
                }
            }
            4 => {
                transaction.execute_batch(&format!(
                    "ALTER TABLE model_route_health ADD COLUMN failed_probe_count INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE model_route_health ADD COLUMN next_probe_at_ms INTEGER;
                     CREATE TABLE recovery_settings (
                         id INTEGER PRIMARY KEY CHECK (id = 1),
                         base_interval_ms INTEGER NOT NULL CHECK(base_interval_ms > 0),
                         doubling_limit INTEGER NOT NULL CHECK(doubling_limit >= 0)
                     );
                     INSERT INTO recovery_settings (id, base_interval_ms, doubling_limit)
                         VALUES (1, {DEFAULT_RECOVERY_BASE_INTERVAL_MS}, {DEFAULT_RECOVERY_DOUBLING_LIMIT});"
                ))?;
                for (kind, action) in [
                    ("insert", "INSERT"),
                    ("update", "UPDATE"),
                    ("delete", "DELETE"),
                ] {
                    transaction.execute_batch(&format!(
                        "CREATE TRIGGER data_change_recovery_settings_{kind} AFTER {action} ON recovery_settings
                         BEGIN UPDATE data_change_signal SET writes = writes + 1 WHERE id = 1; END;"
                    ))?;
                }
            }
            5 => {
                transaction.execute_batch(
                    "CREATE TABLE call_records (
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
                     CREATE INDEX call_attempts_record ON call_attempts (call_record_id);",
                )?;
                // Call records are insert-only durable data; new writes must mark the
                // data as changed so the automatic snapshot boundary stays honest
                // (DATA-011).
                for (table, action) in [("call_records", "INSERT"), ("call_attempts", "INSERT")] {
                    transaction.execute_batch(&format!(
                        "CREATE TRIGGER data_change_{table}_{action} AFTER {action} ON {table}
                         BEGIN UPDATE data_change_signal SET writes = writes + 1 WHERE id = 1; END;"
                    ))?;
                }
            }
            6 => {
                // The permanent daily usage aggregate survives per-call record expiry
                // and never carries call IDs or attempt details (OPS-009).
                transaction.execute_batch(
                    "CREATE TABLE daily_usage (
                         day TEXT NOT NULL,
                         published_model_name TEXT NOT NULL,
                         provider_id TEXT NOT NULL,
                         provider_name TEXT NOT NULL,
                         input_tokens INTEGER NOT NULL DEFAULT 0,
                         cached_input_tokens INTEGER NOT NULL DEFAULT 0,
                         output_tokens INTEGER NOT NULL DEFAULT 0,
                         estimated_cost_rmb REAL NOT NULL DEFAULT 0,
                         PRIMARY KEY (day, published_model_name, provider_id)
                     );",
                )?;
                // Backfill the daily aggregate from existing reliable per-call
                // records so all-time totals survive the migration unchanged
                // (OPS-009/DATA-007).
                let rows = {
                    let mut statement = transaction.prepare(
                        "SELECT created_at_ms, published_model_name, success_provider_id,
                                success_provider_name, input_tokens, cached_input_tokens,
                                output_tokens, estimated_cost_rmb
                         FROM call_records
                         WHERE succeeded = 1 AND input_tokens IS NOT NULL
                           AND success_provider_id IS NOT NULL AND estimated_cost_rmb IS NOT NULL",
                    )?;
                    statement
                        .query_map([], |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, i64>(5)?,
                                row.get::<_, i64>(6)?,
                                row.get::<_, f64>(7)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                for (created_at_ms, model, provider_id, provider_name, input, cached, output, cost) in rows
                {
                    transaction.execute(
                        "INSERT INTO daily_usage
                         (day, published_model_name, provider_id, provider_name,
                          input_tokens, cached_input_tokens, output_tokens, estimated_cost_rmb)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(day, published_model_name, provider_id) DO UPDATE SET
                            input_tokens = input_tokens + excluded.input_tokens,
                            cached_input_tokens = cached_input_tokens + excluded.cached_input_tokens,
                            output_tokens = output_tokens + excluded.output_tokens,
                            estimated_cost_rmb = estimated_cost_rmb + excluded.estimated_cost_rmb",
                        params![
                            timeutil::date_key(created_at_ms),
                            model,
                            provider_id,
                            provider_name,
                            input,
                            cached,
                            output,
                            cost,
                        ],
                    )?;
                }
            }
            7 => {
                // Known usage gaps caused by failed call-record persistence. They are
                // durable, never estimated or backfilled, and survive storage recovery
                // as permanent incompleteness markers (OPS-012/OPS-016).
                transaction.execute_batch(
                    "CREATE TABLE usage_gaps (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         category TEXT NOT NULL,
                         started_at_ms INTEGER NOT NULL,
                         ended_at_ms INTEGER,
                         lost_records INTEGER NOT NULL DEFAULT 1
                     );
                     CREATE INDEX usage_gaps_started ON usage_gaps (started_at_ms);",
                )?;
            }
            8 => {
                // Persistent migration/restore status for the Operations surface
                // (OPS-015). The single row is maintained by Store::open after the
                // migration chain commits, so a crash between the migration and the
                // status write is self-healing on the next start.
                transaction.execute_batch(
                    "CREATE TABLE data_operations (
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
                     );",
                )?;
            }
            9 => {
                // Metadata-only operational events behind the Operations status
                // drill-down (OPS-010/OPS-017/OPS-018). They are diagnostic
                // history with a 14-day retention (OPS-009) and are deliberately
                // excluded from the data-change signal: diagnostic history must
                // not drive automatic snapshot eligibility the way configuration
                // and usage do (DATA-011).
                transaction.execute_batch(
                    "CREATE TABLE operational_events (
                         id INTEGER PRIMARY KEY AUTOINCREMENT,
                         occurred_at_ms INTEGER NOT NULL,
                         section TEXT NOT NULL CHECK (section IN ('process','routes','calls','storage','backups','migration','usage','logs')),
                         severity TEXT NOT NULL CHECK (severity IN ('info','warning','error')),
                         event_code TEXT NOT NULL,
                         version TEXT NOT NULL,
                         correlation_id TEXT,
                         payload_json TEXT NOT NULL
                     );
                     CREATE INDEX operational_events_section_time
                         ON operational_events (section, occurred_at_ms DESC, id DESC);",
                )?;
                fail_migration_if_requested()?;
            }
            10 => {
                // The most recent safe HTTP status of a probe or attributable
                // failure, shown on the Operations route row (OPS-013). NULL
                // means no HTTP status has been recorded yet (unknown, not
                // zero); a transport failure with no HTTP status also records
                // NULL. The column is nullable so rows written by older
                // binaries remain valid after the forward migration.
                transaction.execute_batch(
                    "ALTER TABLE model_route_health ADD COLUMN last_http_status INTEGER;",
                )?;
            }
            11 => {
                // Configurable upstream deadlines (REL-001): first-event
                // deadline, in-flight stream idle deadline, and non-streaming
                // body deadline. Defaults preserve generous direct-like
                // behavior; older binaries' rows keep the new defaults.
                transaction.execute_batch(&format!(
                    "ALTER TABLE recovery_settings ADD COLUMN first_event_timeout_ms INTEGER NOT NULL DEFAULT {DEFAULT_FIRST_EVENT_TIMEOUT_MS};
                     ALTER TABLE recovery_settings ADD COLUMN stream_idle_timeout_ms INTEGER NOT NULL DEFAULT {DEFAULT_STREAM_IDLE_TIMEOUT_MS};
                     ALTER TABLE recovery_settings ADD COLUMN nonstream_timeout_ms INTEGER NOT NULL DEFAULT {DEFAULT_NONSTREAM_TIMEOUT_MS};"
                ))?;
            }
            12 => {
                // Available-route light-validation sweep (REL-003) and the
                // upstream model catalog cache (REL-006). The cache is a
                // diagnostic projection of upstream reality, so it is
                // deliberately excluded from the data-change signal like
                // operational events (DATA-011).
                transaction.execute_batch(&format!(
                    "ALTER TABLE recovery_settings ADD COLUMN freshness_interval_ms INTEGER NOT NULL DEFAULT {DEFAULT_FRESHNESS_INTERVAL_MS};
                     CREATE TABLE upstream_model_cache (
                         upstream_provider_id TEXT NOT NULL REFERENCES upstream_providers(id) ON DELETE CASCADE,
                         model_name TEXT NOT NULL,
                         fetched_at INTEGER NOT NULL,
                         PRIMARY KEY (upstream_provider_id, model_name)
                     );"
                ))?;
            }
            13 => {
                // Quarantine epoch (REL-005: stale probe results must not restore
                // a freshly quarantined route) and the consecutive-failure
                // counter/threshold (REL-004: quarantine only after the
                // configured number of attributable failures).
                transaction.execute_batch(&format!(
                    "ALTER TABLE model_route_health ADD COLUMN quarantine_epoch INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE model_route_health ADD COLUMN consecutive_failures INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE recovery_settings ADD COLUMN quarantine_threshold INTEGER NOT NULL DEFAULT {DEFAULT_QUARANTINE_THRESHOLD};"
                ))?;
            }
            14 => {
                // Periodic upstream model-catalog fetch interval (REL-006).
                transaction.execute_batch(&format!(
                    "ALTER TABLE recovery_settings ADD COLUMN upstream_sync_interval_ms INTEGER NOT NULL DEFAULT {DEFAULT_UPSTREAM_SYNC_INTERVAL_MS};"
                ))?;
            }
            15 => {
                // Published-model deprecation (REL-007): deprecated models
                // keep serving their existing routes but refuse new ones.
                transaction.execute_batch(
                    "ALTER TABLE published_models ADD COLUMN deprecated_at INTEGER;",
                )?;
            }
            16 => {
                // Relay access keys become re-displayable for a personal
                // relay (REL-010): the full secret is stored in plaintext
                // (owner-only files) so the management surface can show it
                // again. Keys created by older binaries keep NULL here and
                // cannot be recovered.
                transaction.execute_batch(
                    "ALTER TABLE relay_access_keys ADD COLUMN secret TEXT;",
                )?;
            }
            _ => bail!("SQLite schema version {version} cannot be migrated by this binary"),
        }
        version += 1;
        transaction.execute("UPDATE schema_metadata SET version = ?1 WHERE id = 1", [version])?;
    }
    Ok(version)
}

/// Estimated cost for a successful call following OPS-006:
///
/// `(uncached_input * input_price + cached_input * cached_input_price
///  + output * output_price) / 1,000,000 * cost_multiplier`,
///
/// using the prices and multiplier in effect at record time. The cache
/// component is zero when the upstream reported no cached tokens. Returns None
/// when any input the formula needs is missing (no usage, no successful
/// attempt, or the route or model was deleted), in which case the call keeps an
/// unknown cost rather than inventing one (OPS-004/OPS-005).
fn compute_call_cost(
    transaction: &rusqlite::Transaction<'_>,
    record: &NewCallRecord,
) -> Result<Option<f64>> {
    let Some(usage) = record.usage.as_ref() else {
        return Ok(None);
    };
    let Some(success) = record
        .attempts
        .iter()
        .find(|attempt| attempt.outcome == "success")
    else {
        return Ok(None);
    };
    let Some(multiplier_micros) = transaction
        .query_row(
            "SELECT cost_multiplier_micros FROM model_routes WHERE id = ?1",
            [&success.route_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let Some((input_price, output_price, cached_price)) = transaction
        .query_row(
            "SELECT input_price_micrormb_per_million, output_price_micrormb_per_million,
                    cached_input_price_micrormb_per_million
             FROM published_models
             WHERE name = ?1",
            [&record.published_model_name],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let uncached = (usage.input_tokens - usage.cached_input_tokens).max(0);
    // Prices are micro-RMB per million tokens and the multiplier is stored in
    // micros, so the whole product converts to RMB with 1e18.
    let cost_rmb = (uncached as f64 * input_price as f64
        + usage.cached_input_tokens as f64 * cached_price as f64
        + usage.output_tokens as f64 * output_price as f64)
        * multiplier_micros as f64
        / 1e18;
    Ok(Some(cost_rmb))
}

/// The look-back span of one of the six fixed usage windows (OPS-008); the
/// all-time window reads the permanent daily aggregate and has no span.
fn window_span_ms(window: &str) -> Option<i64> {
    match window {
        "1h" => Some(3_600_000),
        "5h" => Some(5 * 3_600_000),
        "24h" => Some(24 * 3_600_000),
        "7d" => Some(7 * 86_400_000),
        "14d" => Some(14 * 86_400_000),
        "all" => None,
        _ => None,
    }
}

fn seed_published_models(transaction: &rusqlite::Transaction<'_>) -> Result<()> {
    for (id, name, input, output, cached) in [
        ("gpt-5.6-sol", "gpt-5.6-sol", 5_000_000, 30_000_000, 500_000),
        (
            "gpt-5.6-terra",
            "gpt-5.6-terra",
            2_000_000,
            12_000_000,
            200_000,
        ),
        (
            "deepseek-v4-flash",
            "deepseek-v4-flash",
            1_000_000,
            2_000_000,
            20_000,
        ),
    ] {
        transaction.execute(
            "INSERT INTO published_models
             (id, name, input_price_micrormb_per_million, output_price_micrormb_per_million, cached_input_price_micrormb_per_million)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, input, output, cached],
        )?;
    }
    Ok(())
}

fn verify_integrity(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        bail!("SQLite integrity check failed");
    }
    Ok(())
}

/// A configuration validation failure attributed to a specific form field so
/// the management surface can render an actionable message next to the
/// offending input (UI-006 / CFG-012). `field` is the wire/API field name of
/// the form input; the message keeps the full human-readable sentence so the
/// top-level `error.message` stays self-contained.
///
/// Field errors must reach the handler boundary as the anyhow root cause:
/// `with_configuration_store` in `server.rs` downcasts it from the error
/// chain. Adding `.context(...)` between the store and the boundary would
/// silently demote a field error to a generic 422 without `fields`.
#[derive(Debug)]
pub struct FieldError {
    pub field: &'static str,
    pub message: String,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FieldError {}

fn field_error(field: &'static str, message: impl Into<String>) -> FieldError {
    FieldError {
        field,
        message: message.into(),
    }
}

fn validate_display_name(value: &str) -> Result<String, FieldError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(field_error(
            "display_name",
            "provider name must be between 1 and 128 characters",
        ));
    }
    Ok(value.to_owned())
}

fn validate_base_url(value: &str) -> Result<String, FieldError> {
    let value = value.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(value)
        .map_err(|_| field_error("base_url", "base URL must be a valid HTTP URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(field_error(
            "base_url",
            "base URL must be a valid HTTP URL without credentials, query, or fragment",
        ));
    }
    Ok(value.to_owned())
}

fn validate_api_key(value: &str) -> Result<String, FieldError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 8_192 {
        return Err(field_error(
            "api_key",
            "upstream API key must be between 1 and 8192 characters",
        ));
    }
    Ok(value.to_owned())
}

fn validate_published_model_name(value: &str) -> Result<String, FieldError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(field_error(
            "name",
            "published model name must be between 1 and 128 characters",
        ));
    }
    Ok(value.to_owned())
}

fn validate_upstream_model_name(value: &str) -> Result<String, FieldError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 {
        return Err(field_error(
            "upstream_model_name",
            "upstream model name must be between 1 and 256 characters",
        ));
    }
    Ok(value.to_owned())
}

fn validate_relay_key_label(value: &str) -> Result<String, FieldError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(field_error(
            "label",
            "relay access key label must be between 1 and 128 characters",
        ));
    }
    Ok(value.to_owned())
}

fn valid_eligible_model_route_ids(model_route_ids: &[String]) -> Result<Vec<String>, FieldError> {
    if model_route_ids.is_empty() {
        return Err(field_error(
            "model_route_ids",
            "at least one eligible model route is required",
        ));
    }
    let mut unique_route_ids = model_route_ids.to_vec();
    unique_route_ids.sort();
    unique_route_ids.dedup();
    if unique_route_ids.len() != model_route_ids.len() {
        return Err(field_error(
            "model_route_ids",
            "eligible model routes must not contain duplicates",
        ));
    }
    Ok(unique_route_ids)
}

/// Shared user-facing message for a duplicate model route identity (CFG-008).
/// The create and edit paths must present the same actionable text so the two
/// cannot drift.
const DUPLICATE_ROUTE_IDENTITY_MESSAGE: &str =
    "a model route with this published model, provider, upstream model, and protocol already exists";

/// True when another model route already occupies the CFG-008 identity
/// (published model, provider, upstream model name, protocol) inside this
/// transaction. `excluded_route_id` skips the route being edited so an edit
/// cannot conflict with its own row; the create path passes `None`.
fn route_identity_conflict(
    transaction: &Transaction<'_>,
    published_model_id: &str,
    provider_id: &str,
    upstream_model_name: &str,
    protocol: &str,
    excluded_route_id: Option<&str>,
) -> Result<bool> {
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM model_routes
                 WHERE published_model_id = ?1
                   AND upstream_provider_id = ?2
                   AND upstream_model_name = ?3
                   AND protocol = ?4
                   AND (?5 IS NULL OR id != ?5)
             )",
            params![
                published_model_id,
                provider_id,
                upstream_model_name,
                protocol,
                excluded_route_id
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn validate_protocol(protocol: &str) -> Result<(), FieldError> {
    if !matches!(protocol, "chat_completions" | "responses") {
        return Err(field_error(
            "protocol",
            "protocol must be chat_completions or responses",
        ));
    }
    Ok(())
}

fn parse_positive_decimal(
    value: &str,
    label: &str,
    field: &'static str,
) -> Result<i64, FieldError> {
    let parsed = parse_decimal(value, label, field)?;
    if parsed <= 0 {
        return Err(field_error(field, format!("{label} must be greater than zero")));
    }
    Ok(parsed)
}

fn parse_non_negative_decimal(
    value: &str,
    label: &str,
    field: &'static str,
) -> Result<i64, FieldError> {
    let parsed = parse_decimal(value, label, field)?;
    if parsed < 0 {
        return Err(field_error(field, format!("{label} must not be negative")));
    }
    Ok(parsed)
}

fn parse_decimal(value: &str, label: &str, field: &'static str) -> Result<i64, FieldError> {
    let value = value.trim();
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, ""), |(whole, fraction)| (whole, fraction));
    let negative = whole.starts_with('-');
    let whole_digits = whole.strip_prefix('-').unwrap_or(whole);
    if whole_digits.is_empty()
        || !whole_digits.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(field_error(
            field,
            format!("{label} must be a decimal with at most six decimal places"),
        ));
    }
    let whole: i64 = whole_digits
        .parse()
        .map_err(|_| field_error(field, format!("{label} is outside the supported range")))?;
    let fraction = format!("{fraction:0<6}")
        .parse::<i64>()
        .map_err(|_| field_error(field, format!("{label} must be a decimal")))?;
    let scaled_whole = whole
        .checked_mul(DECIMAL_SCALE)
        .ok_or_else(|| field_error(field, format!("{label} is outside the supported range")))?;
    if negative {
        scaled_whole
            .checked_neg()
            .ok_or_else(|| field_error(field, format!("{label} is outside the supported range")))?
            .checked_sub(fraction)
            .ok_or_else(|| field_error(field, format!("{label} is outside the supported range")))
    } else {
        scaled_whole
            .checked_add(fraction)
            .ok_or_else(|| field_error(field, format!("{label} is outside the supported range")))
    }
}

fn format_decimal(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let absolute = value.unsigned_abs();
    let whole = absolute / DECIMAL_SCALE as u64;
    let fraction = absolute % DECIMAL_SCALE as u64;
    if fraction == 0 {
        return format!("{sign}{whole}");
    }
    format!("{sign}{whole}.{fraction:06}")
        .trim_end_matches('0')
        .to_owned()
}

/// Row mapper for `operational_events` (allowlisted metadata only).
fn event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationalEvent> {
    let payload_json: String = row.get(7)?;
    Ok(OperationalEvent {
        id: row.get(0)?,
        occurred_at_ms: row.get(1)?,
        section: row.get(2)?,
        severity: row.get(3)?,
        event_code: row.get(4)?,
        version: row.get(5)?,
        correlation_id: row.get(6)?,
        payload: serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null),
    })
}

fn fail_config_commit_if_requested() -> Result<()> {
    if std::env::var_os("LOCAL_API_RELAY_TEST_FAIL_CONFIG_COMMIT").is_some() {
        bail!("injected configuration commit failure");
    }
    Ok(())
}

/// Test fault injection for degradable operational writes (DATA-004/DATA-005).
/// `LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE=<category>` fails every write
/// of that category; the `_ONCE` variant fails only the first matching write so
/// a test can observe same-category recovery (OPS-012). Values are the
/// operational category names or `all`.
static OPERATIONAL_WRITE_FAILED_ONCE: AtomicBool = AtomicBool::new(false);

fn fail_operational_write_if_requested(category: &str) -> Result<()> {
    if operational_write_injection_matches(
        "LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE",
        category,
    ) {
        bail!("injected operational write failure: {category}");
    }
    if operational_write_injection_matches(
        "LOCAL_API_RELAY_TEST_FAIL_OPERATIONAL_WRITE_ONCE",
        category,
    ) && !OPERATIONAL_WRITE_FAILED_ONCE.swap(true, Ordering::SeqCst)
    {
        bail!("injected one-time operational write failure: {category}");
    }
    Ok(())
}

fn operational_write_injection_matches(variable: &str, category: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| {
        let value = value.to_string_lossy();
        value == "all" || value == category
    })
}

fn new_id() -> String {
    auth::generate_secret()
}
