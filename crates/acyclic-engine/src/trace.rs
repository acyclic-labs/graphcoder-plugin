//! Code-path tracing, on when `<NAME>_TRACE` (see `product::TRACE_ENV`) is set.
//!
//! Every stage a request passes through (client connect, daemon dispatch,
//! pipeline branch, watcher drain, capture, index write, publish) emits one
//! line naming the branch taken and its cost. Off by default and free when
//! off: the check is one cached boolean. Lines go to stderr, which for the
//! daemon is `daemon.log` in the store and for the CLI is the terminal.
//!
//! Format: `<name>-trace +<ms since process start> <scope> <message>`.

use std::sync::OnceLock;
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Whether tracing is on for this process.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os(crate::product::TRACE_ENV).is_some_and(|v| !v.is_empty() && v != "0"))
}

/// Emits one trace line. Prefer the [`trace!`] macro, which skips the
/// formatting when tracing is off.
pub fn emit(scope: &str, args: std::fmt::Arguments<'_>) {
    let start = START.get_or_init(Instant::now);
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let thread = std::thread::current();
    eprintln!(
        "{}-trace +{ms:>9.1}ms [{}] {scope}: {args}",
        crate::product::NAME,
        thread.name().unwrap_or("?")
    );
}

/// Milliseconds elapsed since `since`, for trace messages.
pub fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[macro_export]
macro_rules! trace {
    ($scope:expr, $($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::emit($scope, format_args!($($arg)*));
        }
    };
}
