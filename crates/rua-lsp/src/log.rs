//! What the server says about itself.
//!
//! An editor keeps the server's standard error — fresh files it under
//! `~/.local/state/fresh/logs/lsp/`, VS Code puts it in an output channel —
//! and it is the only window onto a process nobody can attach to. A server
//! that prints nothing looks identical to one that is broken.
//!
//! Standard output is the protocol, so every word here goes to stderr.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

const OFF: u8 = 0;
const ERROR: u8 = 1;
const WARN: u8 = 2;
const INFO: u8 = 3;
const DEBUG: u8 = 4;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

/// Read `RUA_LSP_LOG` once, at startup. `off`, `error`, `warn`, `info` or
/// `debug`; anything else is `info`, since a typo in a log setting should not
/// silence the log.
pub fn init() {
    let level = match std::env::var("RUA_LSP_LOG").unwrap_or_default().to_ascii_lowercase().as_str()
    {
        "off" | "none" | "0" => OFF,
        "error" => ERROR,
        "warn" | "warning" => WARN,
        "debug" | "trace" => DEBUG,
        _ => INFO,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

/// Seconds since the process started, which is what matters when reading a
/// log of one session — a wall clock would only repeat what the editor's own
/// log already stamps.
fn elapsed() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub fn write(level: u8, tag: &str, message: &str) {
    if !enabled(level) {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "[{:8.3}] {tag:<5} {message}", elapsed());
    let _ = err.flush();
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::write(1, "error", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log::write(2, "warn", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::write(3, "info", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => { $crate::log::write(4, "debug", &format!($($arg)*)) };
}

/// The tail of a uri, which is the part a reader recognises.
pub fn short(uri: &lsp_types::Url) -> String {
    let path = uri.path();
    match path.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_string(),
        _ => path.to_string(),
    }
}
