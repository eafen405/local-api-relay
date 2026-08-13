use crate::paths;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const RETENTION: usize = 10;
pub const AUTO_BACKUP_INTERVAL_SECONDS: i64 = 24 * 60 * 60;

const APPLICATION_IDENTITY: &str = "local-api-relay";
const STATE_FILE: &str = "backup-state.json";
const TEST_FAILURE_STAGE_VARIABLE: &str = "LOCAL_API_RELAY_TEST_FAIL_BACKUP_STAGE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Auto,
    Manual,
    /// The pre-migration snapshot required before any forward schema migration
    /// (DATA-007/DATA-008).
    Migration,
    /// The pre-restore snapshot of the current database taken before an
    /// explicit restore switches to a candidate backup (DATA-015).
    Restore,
}

impl TriggerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Auto => "auto",
            TriggerKind::Manual => "manual",
            TriggerKind::Migration => "migration",
            TriggerKind::Restore => "restore",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackupArtifact {
    pub name: String,
    pub path: PathBuf,
    pub sequence: i64,
    pub created_at: i64,
    pub trigger: String,
    pub schema_version: i64,
    pub size: i64,
}

#[derive(Debug, Clone)]
pub struct BackupStatus {
    pub state: String,
    pub last_backup_at: Option<i64>,
    pub last_trigger: Option<String>,
    pub schema_version: Option<i64>,
    pub last_size: Option<i64>,
    pub next_auto_backup_at: Option<i64>,
    pub count: usize,
    pub retention: usize,
    pub last_failed_stage: Option<String>,
    pub last_failed_reason: Option<String>,
}

/// Bookkeeping for the managed backup set. This deliberately lives outside
/// SQLite: the backup flow never writes the source database, so the
/// "has durable data changed since the last snapshot" write counter stays
/// exact and the snapshot itself never marks data as changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupState {
    pub next_sequence: i64,
    pub last_auto_backup_at: Option<i64>,
    pub last_snapshot_writes: Option<i64>,
    pub last_failure_stage: Option<String>,
    pub last_failure_reason: Option<String>,
    pub last_failure_at: Option<i64>,
}

/// A normalized backup failure. `stage` is one of `create`, `verify`, or
/// `rotate`; `reason` is a bounded, secret-free description suitable for the
/// management surface.
#[derive(Debug)]
pub struct BackupFailure {
    pub stage: &'static str,
    pub reason: String,
}

impl std::fmt::Display for BackupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "backup failed at {}: {}",
            self.stage, self.reason
        )
    }
}

impl std::error::Error for BackupFailure {}

pub fn ensure_backup_dir(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    #[cfg(unix)]
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not secure {}", directory.display()))?;
    Ok(())
}

pub fn read_state(directory: &Path) -> Result<BackupState> {
    let path = directory.join(STATE_FILE);
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackupState::default());
        }
        Err(error) => return Err(error).context("could not read the backup state"),
    };
    serde_json::from_slice(&contents).context("backup state is not valid")
}

/// Persists the backup-set bookkeeping atomically and with owner-only access.
pub fn write_state(directory: &Path, state: &BackupState) -> Result<()> {
    ensure_backup_dir(directory)?;
    let path = directory.join(STATE_FILE);
    let temporary = directory.join(format!("{STATE_FILE}.tmp"));
    let contents = serde_json::to_vec_pretty(state)?;
    fs::write(&temporary, contents).context("could not write the backup state")?;
    fs::rename(&temporary, &path).context("could not replace the backup state")?;
    paths::restrict_file(&path)
}

/// Creates a consistent full SQLite snapshot through the online backup API,
/// embeds the application identity and backup provenance in the artifact, and
/// verifies its integrity, identity, and schema before returning it.
///
/// Any failure before a verified artifact exists removes the partial file;
/// existing verified backups are never touched here. The source database is
/// only read, never written.
pub fn create_artifact(
    source: &Connection,
    directory: &Path,
    trigger: TriggerKind,
    now: i64,
    sequence: i64,
    schema_version: i64,
) -> Result<BackupArtifact, BackupFailure> {
    ensure_backup_dir(directory).map_err(|error| BackupFailure {
        stage: "create",
        reason: normalize_directory_error(&error),
    })?;
    fail_if_requested("create")?;

    let nonce = format!("{:016x}", rand::random::<u64>());
    let name = format!("backup-{sequence:06}-{nonce}.sqlite3");
    let path = directory.join(&name);

    let outcome = (|| -> Result<(), BackupFailure> {
        let mut destination = Connection::open(&path).map_err(|_| BackupFailure {
            stage: "create",
            reason: "could not create the backup file".to_owned(),
        })?;
        rusqlite::backup::Backup::new(source, &mut destination)
            .and_then(|backup| backup.run_to_completion(32, Duration::from_millis(50), None))
            .map_err(|_| BackupFailure {
                stage: "create",
                reason: "online backup of the live database failed".to_owned(),
            })?;
        // Schemas older than v3 predate `backup_metadata`; the artifact still
        // carries identity and provenance, created only inside the copy so the
        // source is never written during a snapshot (DATA-009).
        destination
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS backup_metadata (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     application TEXT NOT NULL,
                     created_at INTEGER NOT NULL,
                     trigger TEXT NOT NULL,
                     source_schema_version INTEGER NOT NULL
                 );",
            )
            .map_err(|_| BackupFailure {
                stage: "verify",
                reason: "could not finalize the backup metadata".to_owned(),
            })?;
        destination
            .execute(
                "INSERT INTO backup_metadata
                     (id, application, created_at, trigger, source_schema_version)
                 VALUES (1, ?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                    application = excluded.application,
                    created_at = excluded.created_at,
                    trigger = excluded.trigger,
                    source_schema_version = excluded.source_schema_version",
                params![APPLICATION_IDENTITY, now, trigger.as_str(), schema_version],
            )
            .map_err(|_| BackupFailure {
                stage: "verify",
                reason: "could not finalize the backup metadata".to_owned(),
            })?;
        fail_if_requested("verify")?;
        verify_destination(&destination, schema_version).map_err(|error| BackupFailure {
            stage: "verify",
            reason: normalize_backup_error(&error),
        })?;
        Ok(())
    })();

    if let Err(failure) = outcome {
        let _ = fs::remove_file(&path);
        return Err(failure);
    }

    paths::restrict_file(&path).map_err(|_| {
        let _ = fs::remove_file(&path);
        BackupFailure {
            stage: "verify",
            reason: "could not secure the backup file".to_owned(),
        }
    })?;
    let size = fs::metadata(&path)
        .map(|metadata| metadata.len() as i64)
        .map_err(|_| {
            let _ = fs::remove_file(&path);
            BackupFailure {
                stage: "verify",
                reason: "could not read the backup file".to_owned(),
            }
        })?;
    Ok(BackupArtifact {
        name,
        path,
        sequence,
        created_at: now,
        trigger: trigger.as_str().to_owned(),
        schema_version,
        size,
    })
}

/// Lists managed backup artifacts newest-first, reading provenance from each
/// file. Unreadable or non-matching files are not part of the managed set.
pub fn list_artifacts(directory: &Path) -> Result<Vec<BackupArtifact>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("could not list backups"),
    };
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry.context("could not read a backup directory entry")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(sequence) = backup_sequence(&name) else {
            continue;
        };
        let size = entry
            .metadata()
            .map(|metadata| metadata.len() as i64)
            .unwrap_or(0);
        if let Some(artifact) = read_artifact_metadata(&entry.path(), name, sequence, size) {
            artifacts.push(artifact);
        }
    }
    artifacts.sort_by(|left, right| {
        right
            .sequence
            .cmp(&left.sequence)
            .then_with(|| right.name.cmp(&left.name))
    });
    Ok(artifacts)
}

/// Rotates the managed set to the `keep` most recent artifacts. Rotation only
/// deletes artifacts outside the retained set, and a rotation failure stops
/// before deleting anything else.
pub fn rotate(directory: &Path, keep: usize) -> Result<(), BackupFailure> {
    let artifacts = list_artifacts(directory).map_err(|_| BackupFailure {
        stage: "rotate",
        reason: "could not list backups".to_owned(),
    })?;
    if artifacts.len() <= keep {
        return Ok(());
    }
    fail_if_requested("rotate")?;
    for artifact in &artifacts[keep..] {
        fs::remove_file(&artifact.path).map_err(|_| BackupFailure {
            stage: "rotate",
            reason: "could not remove an outdated backup".to_owned(),
        })?;
    }
    Ok(())
}

/// Shared integrity + identity verification for a snapshot artifact: SQLite
/// integrity and the application identity embedded in `backup_metadata`.
fn verify_snapshot(connection: &Connection) -> Result<()> {
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|_| anyhow!("backup integrity check failed"))?;
    if integrity != "ok" {
        bail!("backup integrity check failed");
    }
    let application: String = connection
        .query_row(
            "SELECT application FROM backup_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| anyhow!("backup identity check failed"))?;
    if application != APPLICATION_IDENTITY {
        bail!("backup identity check failed");
    }
    Ok(())
}

fn verify_destination(destination: &Connection, expected_schema_version: i64) -> Result<()> {
    verify_snapshot(destination)?;
    let provenance_schema: i64 = destination
        .query_row(
            "SELECT source_schema_version FROM backup_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| anyhow!("backup identity check failed"))?;
    let database_schema: i64 = destination
        .query_row(
            "SELECT version FROM schema_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| anyhow!("backup schema is inconsistent"))?;
    if provenance_schema != database_schema {
        bail!("backup schema is inconsistent");
    }
    if !(1..=expected_schema_version).contains(&provenance_schema) {
        bail!("backup schema is unsupported");
    }
    Ok(())
}

/// Opens a backup artifact as an isolated candidate connection and verifies
/// its SQLite integrity and application identity, returning the schema version
/// it holds. The caller decides how to handle the version (newer is rejected,
/// older is migrated under the same forward-only contract, DATA-014). The
/// returned connection is the only handle to the candidate, so its close is the
/// checkpoint that makes the staged file complete before any switch.
pub fn open_candidate(path: &Path) -> Result<(Connection, i64), BackupFailure> {
    let connection = Connection::open(path).map_err(|_| BackupFailure {
        stage: "verify",
        reason: "could not open the backup".to_owned(),
    })?;
    verify_snapshot(&connection).map_err(|error| BackupFailure {
        stage: "verify",
        reason: normalize_backup_error(&error),
    })?;
    let schema_version: i64 = connection
        .query_row(
            "SELECT version FROM schema_metadata WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| BackupFailure {
            stage: "verify",
            reason: "backup schema is inconsistent".to_owned(),
        })?;
    Ok((connection, schema_version))
}

fn backup_sequence(name: &str) -> Option<i64> {
    let base = name.strip_prefix("backup-")?.strip_suffix(".sqlite3")?;
    let sequence = base.split('-').next()?;
    if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    sequence.parse().ok()
}

fn read_artifact_metadata(
    path: &Path,
    name: String,
    sequence: i64,
    size: i64,
) -> Option<BackupArtifact> {
    let connection = Connection::open(path).ok()?;
    let (created_at, trigger, schema_version) = connection
        .query_row(
            "SELECT created_at, trigger, source_schema_version FROM backup_metadata WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()?;
    Some(BackupArtifact {
        name,
        path: path.to_path_buf(),
        sequence,
        created_at,
        trigger,
        schema_version,
        size,
    })
}

fn fail_if_requested(stage: &'static str) -> Result<(), BackupFailure> {
    if std::env::var(TEST_FAILURE_STAGE_VARIABLE).ok().as_deref() == Some(stage) {
        return Err(BackupFailure {
            stage,
            reason: format!("injected {stage} failure"),
        });
    }
    Ok(())
}

fn normalize_directory_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("could not create") {
        "could not create the backup directory".to_owned()
    } else if message.contains("could not secure") {
        "could not secure the backup directory".to_owned()
    } else {
        "could not prepare the backup directory".to_owned()
    }
}

fn normalize_backup_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    for normalized in [
        "backup integrity check failed",
        "backup identity check failed",
        "backup schema is inconsistent",
        "backup schema is unsupported",
    ] {
        if message.contains(normalized) {
            return normalized.to_owned();
        }
    }
    "backup verification failed".to_owned()
}
