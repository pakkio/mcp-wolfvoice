//! Runtime-controllable logging.
//!
//! `env_logger` bakes its level filter in at `init()` with no reload — the level
//! is fixed for the life of the process. This module replaces it with a tiny
//! `log::Log` backed by an atomic, so the admin endpoint in `main.rs` can raise
//! or lower verbosity live, without a restart. Output keeps the exact
//! `[<ts> <LEVEL> <target>] <msg>` bracket shape env_logger produced, since
//! `wolfvoice-mcp`'s log parser (`LOG_PREFIX_RE`/`parse_since` in mcp/server.py)
//! depends on it.
//!
//! A second, independent atomic (`MicLogMode`) controls whether the per-tick
//! mic start/stop transitions in `room::Session::update_level` get logged at
//! all, split by whether the room had another participant to mix against at
//! that moment — a solo mic check floods the log with the same "started/stopped
//! speaking" line the log-level knob alone can't selectively silence without
//! also hiding every other INFO line.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

/// Default: quiet. Console/journal noise is opt-in via the admin endpoint, not
/// the starting state of a production process.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(LevelFilter::Error as u8);

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MicLogMode {
    Off = 0,
    MixedOnly = 1,
    SoloOnly = 2,
    All = 3,
}

impl MicLogMode {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => MicLogMode::Off,
            1 => MicLogMode::MixedOnly,
            2 => MicLogMode::SoloOnly,
            _ => MicLogMode::All,
        }
    }

    fn name(self) -> &'static str {
        match self {
            MicLogMode::Off => "off",
            MicLogMode::MixedOnly => "mixed",
            MicLogMode::SoloOnly => "solo",
            MicLogMode::All => "all",
        }
    }
}

/// Default: mixed-only. A lone participant's own mic check is the loudest,
/// least useful source of "started/stopped speaking" spam (nobody else is
/// there to hear it); a real multi-party room is what's worth seeing.
static MIC_LOG_MODE: AtomicU8 = AtomicU8::new(MicLogMode::MixedOnly as u8);

fn level_filter_from_u8(v: u8) -> LevelFilter {
    match v {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

fn parse_level(s: &str) -> Result<LevelFilter, String> {
    match s.to_ascii_lowercase().as_str() {
        "off" => Ok(LevelFilter::Off),
        "error" => Ok(LevelFilter::Error),
        "warn" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        other => Err(format!(
            "unknown level {other:?}: expected off|error|warn|info|debug|trace"
        )),
    }
}

fn parse_mic_mode(s: &str) -> Result<MicLogMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "off" => Ok(MicLogMode::Off),
        "mixed" => Ok(MicLogMode::MixedOnly),
        "solo" => Ok(MicLogMode::SoloOnly),
        "all" => Ok(MicLogMode::All),
        other => Err(format!("unknown mic-log mode {other:?}: expected off|mixed|solo|all")),
    }
}

pub fn set_level(s: &str) -> Result<&'static str, String> {
    let lvl = parse_level(s)?;
    LOG_LEVEL.store(lvl as u8, Ordering::Relaxed);
    Ok(level_name(lvl))
}

pub fn get_level() -> &'static str {
    level_name(level_filter_from_u8(LOG_LEVEL.load(Ordering::Relaxed)))
}

fn level_name(l: LevelFilter) -> &'static str {
    match l {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}

pub fn set_mic_mode(s: &str) -> Result<&'static str, String> {
    let mode = parse_mic_mode(s)?;
    MIC_LOG_MODE.store(mode as u8, Ordering::Relaxed);
    Ok(mode.name())
}

pub fn get_mic_mode() -> &'static str {
    MicLogMode::from_u8(MIC_LOG_MODE.load(Ordering::Relaxed)).name()
}

/// Whether `room::Session::update_level` should log the speaking transition it
/// just detected, given whether the room had another member to mix against.
pub fn mic_log_allowed(mixed: bool) -> bool {
    match MicLogMode::from_u8(MIC_LOG_MODE.load(Ordering::Relaxed)) {
        MicLogMode::Off => false,
        MicLogMode::MixedOnly => mixed,
        MicLogMode::SoloOnly => !mixed,
        MicLogMode::All => true,
    }
}

struct RuntimeLogger;

impl Log for RuntimeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= level_filter_from_u8(LOG_LEVEL.load(Ordering::Relaxed))
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level: Level = record.level();
        let _ = writeln!(
            std::io::stderr(),
            "[{} {level:<5} {}] {}",
            iso_timestamp(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`, computed by hand (Howard Hinnant's
/// `civil_from_days`) rather than pulling in a datetime crate for one call
/// site. `wolfvoice-mcp`'s `parse_since()` requires this calendar shape to
/// window-filter by recency — a raw epoch number would silently defeat it.
fn iso_timestamp() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, m, s) = (sod / 3600, (sod / 60) % 60, sod % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_num = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m_num <= 2 { y + 1 } else { y };

    format!("{year:04}-{m_num:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

static LOGGER: RuntimeLogger = RuntimeLogger;

pub fn init() {
    // Never block at the crate-wide cap; `RuntimeLogger::enabled` does the
    // real, live-adjustable check. `set_logger` (not `set_boxed_logger`) so
    // this doesn't need the `log` crate's `alloc`/`std` features on top of
    // the bare default.
    log::set_max_level(LevelFilter::Trace);
    log::set_logger(&LOGGER).expect("logger already installed");
}
