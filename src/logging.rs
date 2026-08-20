//! Dual-output logger (stderr + optional log file) with UTC timestamps.
//!
//! Replaces `env_logger` so logs can be written to a file as well as the
//! terminal — handy for debugging client (e.g. KIO) issues after the fact.
//! The level comes from `RUST_LOG` (a bare level such as `debug`), falling
//! back to `--verbose` (debug) or `info`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record};

struct NzkLogger {
    level: LevelFilter,
    file: Option<Mutex<std::fs::File>>,
}

impl NzkLogger {
    fn format(record: &Record<'_>) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!(
            "{} [{:<5}] {}: {}\n",
            utc_timestamp(now),
            record.level().as_str(),
            record.target(),
            record.args()
        )
    }
}

impl Log for NzkLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = Self::format(record);
        eprint!("{line}");
        if let Some(file) = &self.file
            && let Ok(mut f) = file.lock()
        {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    fn flush(&self) {
        if let Some(file) = &self.file
            && let Ok(mut f) = file.lock()
        {
            let _ = f.flush();
        }
    }
}

/// Install the logger. `verbose` enables debug level when `RUST_LOG` is unset;
/// `log_file` additionally appends logs to the given path (stderr is always
/// used too).
pub fn init(verbose: bool, log_file: Option<&Path>) {
    let level = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| level_from_str(&s))
        .unwrap_or_else(|| {
            if verbose {
                LevelFilter::Debug
            } else {
                LevelFilter::Info
            }
        });

    let file = match log_file {
        Some(p) => match OpenOptions::new().create(true).append(true).open(p) {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => {
                eprintln!("warning: cannot open log file {}: {e}", p.display());
                None
            }
        },
        None => None,
    };

    let _ = log::set_boxed_logger(Box::new(NzkLogger { level, file }));
    log::set_max_level(level);
}

fn level_from_str(s: &str) -> LevelFilter {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Info,
    }
}

/// Format a unix timestamp as UTC `YYYY-MM-DD HH:MM:SS`.
fn utc_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Howard Hinnant's civil-from-days algorithm (days since 1970-01-01).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
