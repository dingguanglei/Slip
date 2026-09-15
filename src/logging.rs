//! Structured, redacting, file-based diagnostics — off unless `SLIP_LOG` is
//! set. Slip handles credentials and plaintext message bodies, so the logger
//! is built to make leaking them hard:
//!
//! 1. Call sites log metadata only (addresses, ids, sizes, counts) — never a
//!    password, auth code, key, or message body.
//! 2. Every secret passed to [`register_secret`] is scrubbed from every line
//!    as a second line of defence, so an accidental log of a known secret is
//!    still masked to `***`.
//!
//! `SLIP_LOG` selects the level: `error|warn|info|debug|trace`, or a truthy
//! value (`1`/`true`/`on`) for `info`. `SLIP_LOG_FILE` overrides the path
//! (default `<SLIP_HOME>/slip.log`, else `~/.slip/slip.log`).

use chrono::Local;
use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// Parse the `SLIP_LOG` value into a max level. `None` means logging is off.
fn parse_level(raw: &str) -> Option<Level> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "off" | "false" | "no" => None,
        "error" => Some(Level::Error),
        "warn" | "warning" => Some(Level::Warn),
        "1" | "true" | "on" | "yes" | "info" => Some(Level::Info),
        "debug" => Some(Level::Debug),
        "trace" => Some(Level::Trace),
        _ => Some(Level::Info),
    }
}

struct Logger {
    level: Level,
    sink: Mutex<Box<dyn Write + Send>>,
}

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();
/// Secrets to mask in every line. Separate from `LOGGER` so it can be
/// populated (at login) before or after `init`, and so a disabled logger
/// still records what to scrub if it is later enabled in-process.
static SECRETS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Initialise from the environment. Idempotent: the first call wins, so both
/// binaries can call it unconditionally at start-up.
pub fn init() {
    LOGGER.get_or_init(|| {
        let level = std::env::var("SLIP_LOG")
            .ok()
            .and_then(|raw| parse_level(&raw))?;
        let path = log_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(Logger {
            level,
            sink: Mutex::new(Box::new(file)),
        })
    });
}

fn log_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("SLIP_LOG_FILE") {
        return PathBuf::from(explicit);
    }
    let home = std::env::var_os("SLIP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".slip")))
        .unwrap_or_else(|| PathBuf::from(".slip-cache"));
    home.join("slip.log")
}

/// Register a secret to be masked in all subsequent log output. No-op for
/// short/empty values (masking those would redact innocuous text).
pub fn register_secret(secret: &str) {
    if secret.len() < 4 {
        return;
    }
    if let Ok(mut secrets) = SECRETS.write()
        && !secrets.iter().any(|existing| existing == secret)
    {
        secrets.push(secret.to_string());
    }
}

/// True if a message at `level` would be recorded.
pub fn enabled(level: Level) -> bool {
    matches!(LOGGER.get(), Some(Some(logger)) if level <= logger.level)
}

/// Emit one structured line: `<ts> <LEVEL> <target> <msg> k=v k=v`.
pub fn event(level: Level, target: &str, msg: &str, fields: &[(&str, &dyn Display)]) {
    let Some(Some(logger)) = LOGGER.get() else {
        return;
    };
    if level > logger.level {
        return;
    }
    let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
    let mut line = format!("{ts} {:5} {target} {msg}", level.label());
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&quote_value(&value.to_string()));
    }
    let line = scrub(line);
    if let Ok(mut sink) = logger.sink.lock() {
        let _ = writeln!(sink, "{line}");
        let _ = sink.flush();
    }
}

pub fn error(target: &str, msg: &str, fields: &[(&str, &dyn Display)]) {
    event(Level::Error, target, msg, fields);
}
pub fn warn(target: &str, msg: &str, fields: &[(&str, &dyn Display)]) {
    event(Level::Warn, target, msg, fields);
}
pub fn info(target: &str, msg: &str, fields: &[(&str, &dyn Display)]) {
    event(Level::Info, target, msg, fields);
}
pub fn debug(target: &str, msg: &str, fields: &[(&str, &dyn Display)]) {
    event(Level::Debug, target, msg, fields);
}

/// Quote a field value if it contains whitespace, `=`, or a quote, so lines
/// stay parseable as `key=value` pairs.
fn quote_value(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || c == '=' || c == '"')
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

/// Replace every registered secret with `***`. The last-resort guard against
/// a call site accidentally including a password/key/body in a field.
fn scrub(mut line: String) -> String {
    if let Ok(secrets) = SECRETS.read() {
        for secret in secrets.iter() {
            if line.contains(secret.as_str()) {
                line = line.replace(secret.as_str(), "***");
            }
        }
    }
    line
}
