//! Shared date and clock helpers.
//!
//! Civil-date conversion, UTC date keys, and the testable epoch clock each have
//! exactly one implementation here, so the storage layer, the managed log, the
//! backup clock, and the management server cannot drift apart.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds per UTC day, the grain shared by the daily usage aggregate
/// (OPS-009), the managed log's calendar-day rotation, and retention sweeps.
pub const MILLIS_PER_DAY: i64 = 86_400_000;

/// Environment variable that pins the process-wide epoch (seconds) so tests can
/// observe day boundaries, retention, and the backup boundary at the process
/// boundary. The same variable drives the backup clock (backup.rs), the managed
/// log's calendar-day rotation (log.rs), and every timestamp that honors the
/// test clock in the management server (server.rs).
pub const TEST_CLOCK_EPOCH_VARIABLE: &str = "LOCAL_API_RELAY_TEST_CLOCK_EPOCH";

/// Environment variable that points the recovery scheduler at a test-owned
/// clock file. When set, every recovery-schedule timestamp — the scheduler's
/// due check and the failure/quarantine/settings anchors in store.rs — reads
/// "now" from the file instead of the wall clock. The file holds a single
/// integer: the epoch-millisecond instant the schedule should treat as now.
/// Tests advance the file to drive recovery probes deterministically
/// (ROUTE-019..021) instead of asserting wall-clock tolerances.
pub const RECOVERY_CLOCK_FILE_VARIABLE: &str = "LOCAL_API_RELAY_TEST_RECOVERY_CLOCK_FILE";

/// The injectable recovery-clock "now" in epoch milliseconds. `Some` when
/// `RECOVERY_CLOCK_FILE_VARIABLE` is set: the file is authoritative, and an
/// unreadable or malformed file freezes the schedule (`i64::MIN`, so nothing is
/// ever due and every new anchor stays far in the past). `None` when unset, and
/// each call site falls back to its existing clock.
pub fn recovery_clock_now_ms() -> Option<i64> {
    std::env::var_os(RECOVERY_CLOCK_FILE_VARIABLE).map(|path| {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(i64::MIN)
    })
}

/// The optional fixed epoch (seconds) injected through the environment.
pub fn test_clock_epoch() -> Option<i64> {
    std::env::var(TEST_CLOCK_EPOCH_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
}

/// The current epoch in seconds, honoring the shared test clock.
pub fn now_epoch() -> i64 {
    test_clock_epoch().unwrap_or_else(system_epoch_seconds)
}

/// The current epoch in milliseconds, honoring the shared test clock.
pub fn now_epoch_ms() -> i64 {
    test_clock_epoch()
        .map(|epoch| epoch.saturating_mul(1000))
        .unwrap_or_else(system_epoch_millis)
}

/// The current epoch in seconds from the system clock, ignoring the test clock.
pub fn system_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// The current epoch in milliseconds from the system clock, ignoring the test
/// clock.
pub fn system_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// UTC civil date `YYYY-MM-DD` for an epoch-millisecond instant, the grain of
/// the permanent daily usage aggregate (OPS-009) and the managed log's
/// calendar-day rotation boundary.
pub fn date_key(epoch_ms: i64) -> String {
    let (year, month, day) = civil_from_days(epoch_ms.div_euclid(MILLIS_PER_DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
pub fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
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
    (year, month as u32, day as u32)
}

/// The inverse of `civil_from_days`: (y, m, d) to days since 1970-01-01.
pub fn days_from_civil((year, month, day): (i64, u32, u32)) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
