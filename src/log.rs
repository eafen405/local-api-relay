//! Structured operational events and the managed rotating log (OPS-017 to
//! OPS-020).
//!
//! Every event carries only allowlisted metadata: time, severity, a stable
//! event code, the process version, an optional locally generated correlation
//! identifier, and safe section-specific fields. Raw request/response bodies,
//! prompts, tool arguments, headers, query strings, complete Base URLs,
//! upstream API keys, relay access keys, and backup contents never enter
//! events or logs (OPS-020/OPS-021).
//!
//! Events are written as one JSON object per line to standard error (captured
//! by the installed launcher, OPS-017) and mirrored into a managed rotating
//! log under the XDG state directory. The managed log rotates at the earlier
//! of the local calendar-day boundary or a 20 MiB size limit, retains no file
//! older than 14 days, and never exceeds a 200 MiB total cap, deleting the
//! oldest files first (OPS-019).

use crate::timeutil::{MILLIS_PER_DAY, date_key, days_from_civil, now_epoch_ms};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::UNIX_EPOCH,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Event sections. The five Operations status areas are `storage`, `routes`,
/// `backups`, `migration`, and `usage`; `process`, `calls`, and `logs` cover
/// the remaining OPS-017 event scope.
pub const SECTION_PROCESS: &str = "process";
pub const SECTION_ROUTES: &str = "routes";
pub const SECTION_CALLS: &str = "calls";
pub const SECTION_STORAGE: &str = "storage";
pub const SECTION_BACKUPS: &str = "backups";
pub const SECTION_MIGRATION: &str = "migration";
pub const SECTION_USAGE: &str = "usage";
pub const SECTION_LOGS: &str = "logs";

pub const SEVERITY_INFO: &str = "info";
pub const SEVERITY_WARNING: &str = "warning";
pub const SEVERITY_ERROR: &str = "error";

/// Rotation and retention boundaries (OPS-019).
pub const ROTATION_SIZE_BYTES: u64 = 20 * 1024 * 1024;
pub const RETENTION_DAYS: i64 = 14;
pub const TOTAL_CAP_BYTES: u64 = 200 * 1024 * 1024;

/// Test hooks that shrink the rotation size and total cap so the boundaries
/// are observable at the process boundary without writing 20 MiB of events.
const TEST_SIZE_LIMIT_VARIABLE: &str = "LOCAL_API_RELAY_TEST_LOG_SIZE_LIMIT";
const TEST_CAP_VARIABLE: &str = "LOCAL_API_RELAY_TEST_LOG_SIZE_CAP";

const CURRENT_LOG_NAME: &str = "relay.log";

/// The managed log state, initialized once by `init` before the listener
/// binds. Standard-error events always flow even when the managed mirror is
/// unavailable.
struct Logger {
    directory: PathBuf,
    current: Option<LogFile>,
}

struct LogFile {
    path: PathBuf,
    /// UTC calendar day (`YYYY-MM-DD`) the file was opened for.
    day: String,
    bytes: u64,
    file: fs::File,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

/// The version of the running binary, included in every event (OPS-018).
/// Test hook that overrides the version this process reports in `--version`
/// and every structured event, so upgrade/rollback drills can install the same
/// binary under two distinct versions side by side (PKG-013). Declared here so
/// the CLI's `--version` and the event envelope cannot drift apart.
pub const TEST_VERSION_VARIABLE: &str = "LOCAL_API_RELAY_TEST_VERSION";

/// The version this process reports in every structured event. The compiled-in
/// crate version is the real value; the test hook overrides it so upgrade
/// drills can run the same binary under two distinct versions (PKG-013).
pub fn process_version() -> String {
    std::env::var(TEST_VERSION_VARIABLE)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

/// Initializes the managed rotating log directory. Called once before the
/// loopback listener binds; a failure only disables the managed file mirror —
/// standard-error events still flow.
pub fn init(directory: PathBuf) {
    let logger = Logger {
        directory,
        current: None,
    };
    let _ = LOGGER.set(Mutex::new(logger));
}

/// Emits one structured operational event: a single JSON line on standard
/// error, mirrored into the managed rotating log when available. Payloads must
/// stay within the metadata allowlist; the helper never redacts, so callers
/// own that contract (OPS-018/OPS-020).
pub fn emit(
    section: &str,
    severity: &str,
    code: &str,
    correlation_id: Option<&str>,
    payload: &serde_json::Value,
) {
    let Some(line) = event_line(section, severity, code, correlation_id, payload) else {
        return;
    };
    let _ = writeln!(std::io::stderr(), "{line}");
    write_managed(&line);
}

/// Builds the single-line JSON envelope for one event. Every event carries
/// exactly the metadata allowlist (OPS-018): time, severity, stable event
/// code, process version, section, optional correlation identifier, and the
/// safe payload.
fn event_line(
    section: &str,
    severity: &str,
    code: &str,
    correlation_id: Option<&str>,
    payload: &serde_json::Value,
) -> Option<String> {
    serde_json::to_string(&json!({
        "ts": now_epoch_ms(),
        "severity": severity,
        "event": code,
        "version": process_version(),
        "section": section,
        "correlation": correlation_id,
        "payload": payload,
    }))
    .ok()
}

fn write_managed(line: &str) {
    let Some(mutex) = LOGGER.get() else {
        return;
    };
    let Ok(mut logger) = mutex.lock() else {
        return;
    };
    let write_result = append(&mut logger, line);
    if let Err(error) = write_result {
        // The managed mirror failed (rotation or write); report a structured
        // rotation/write failure on standard error only, never recursing into
        // the managed file (OPS-017 log-rotation failures).
        let failure = event_line(
            SECTION_LOGS,
            SEVERITY_ERROR,
            "logs.rotation_failed",
            None,
            &json!({ "reason": format!("{error}") }),
        )
        .unwrap_or_else(|| r#"{"severity":"error","event":"logs.rotation_failed"}"#.to_owned());
        let _ = writeln!(std::io::stderr(), "{failure}");
    }
}

/// Appends one line to the active managed file, rotating first at the earlier
/// of the calendar-day boundary or the size limit, then re-enforcing the total
/// cap so the whole managed set never exceeds it (OPS-019).
fn append(logger: &mut Logger, line: &str) -> Result<(), std::io::Error> {
    let day = date_key(now_epoch_ms());
    let needs_rotation = logger
        .current
        .as_ref()
        .is_some_and(|current| current.day != day)
        || logger
            .current
            .as_ref()
            .is_some_and(|current| current.bytes + line.len() as u64 + 1 > rotation_size_limit());
    if needs_rotation {
        rotate(logger)?;
        ensure_current(logger, &day)?;
    } else if logger.current.is_none() {
        ensure_current(logger, &day)?;
    }
    let current = logger.current.as_mut().expect("managed log is ensured");
    current.file.write_all(line.as_bytes())?;
    current.file.write_all(b"\n")?;
    current.bytes += line.len() as u64 + 1;
    sweep(logger);
    Ok(())
}

/// Renames the active file to a dated rotated name so a fresh active file can
/// start (OPS-019).
fn rotate(logger: &mut Logger) -> Result<(), std::io::Error> {
    if let Some(current) = logger.current.take() {
        let rotated = next_rotated_path(&logger.directory, &current.day);
        fs::rename(&current.path, &rotated)?;
    }
    Ok(())
}

/// Opens (or reopens) the active `relay.log` for the given day, sweeping
/// retention afterwards. A leftover active file from a previous run on an
/// older day is rotated away first so its content is covered by retention.
fn ensure_current(logger: &mut Logger, day: &str) -> Result<(), std::io::Error> {
    fs::create_dir_all(&logger.directory)?;
    #[cfg(unix)]
    fs::set_permissions(&logger.directory, fs::Permissions::from_mode(0o700))?;
    let path = logger.directory.join(CURRENT_LOG_NAME);
    if path.exists()
        && let Ok(metadata) = fs::metadata(&path)
        && let Some(leftover_day) = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| date_key(duration.as_millis() as i64))
        && leftover_day != day
    {
        let rotated = next_rotated_path(&logger.directory, &leftover_day);
        fs::rename(&path, &rotated)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    let file = options.open(&path)?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let bytes = file.metadata()?.len();
    logger.current = Some(LogFile {
        path,
        day: day.to_owned(),
        bytes,
        file,
    });
    sweep(logger);
    Ok(())
}

/// The next unused rotated file name for a day: `relay.log.<day>`, then
/// `relay.log.<day>.1`, `.2`, and so on (deterministic across processes).
fn next_rotated_path(directory: &Path, day: &str) -> PathBuf {
    let base = directory.join(format!("{CURRENT_LOG_NAME}.{day}"));
    if !base.exists() {
        return base;
    }
    for index in 1.. {
        let candidate = directory.join(format!("{CURRENT_LOG_NAME}.{day}.{index}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the rotated-name space is unbounded")
}

/// Enforces retention over the managed rotated files: nothing older than 14
/// days survives, and when the whole managed set (including the active file)
/// exceeds the cap the oldest rotated files are deleted first (OPS-019). The
/// active `relay.log` itself is never deleted; in practice it never alone
/// exceeds the cap because the rotation limit is well below it.
fn sweep(logger: &Logger) {
    let Ok(entries) = fs::read_dir(&logger.directory) else {
        return;
    };
    let mut rotated: Vec<(PathBuf, u64, i64, usize)> = Vec::new();
    let today_day_number = now_epoch_ms().div_euclid(MILLIS_PER_DAY);
    let mut total: u64 = logger.current.as_ref().map(|current| current.bytes).unwrap_or(0);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(day) = name.strip_prefix(&format!("{CURRENT_LOG_NAME}.")) else {
            continue;
        };
        let Some(date) = day.get(..10).filter(|date| is_valid_date(date)) else {
            continue;
        };
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        let suffix_index = if day.len() > 10 {
            day[10..].trim_start_matches('.').parse::<usize>().unwrap_or(0)
        } else {
            0
        };
        let day_number = days_from_civil(parse_date(date));
        let age_days = today_day_number - day_number;
        if age_days > RETENTION_DAYS {
            let _ = fs::remove_file(&path);
            continue;
        }
        total += metadata.len();
        rotated.push((path, metadata.len(), day_number, suffix_index));
    }
    rotated.sort_by_key(|(_, _, day_number, suffix_index)| (*day_number, *suffix_index));
    for (path, size, _, _) in rotated {
        if total <= total_cap_limit() {
            break;
        }
        let _ = fs::remove_file(&path);
        total = total.saturating_sub(size);
    }
}

fn rotation_size_limit() -> u64 {
    std::env::var(TEST_SIZE_LIMIT_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(ROTATION_SIZE_BYTES)
}

fn total_cap_limit() -> u64 {
    std::env::var(TEST_CAP_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(TOTAL_CAP_BYTES)
}

fn is_valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

/// Parses `YYYY-MM-DD` into (year, month, day); callers only pass validated
/// dates.
fn parse_date(value: &str) -> (i64, u32, u32) {
    let year: i64 = value[0..4].parse().unwrap_or(0);
    let month: u32 = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    (year, month, day)
}
