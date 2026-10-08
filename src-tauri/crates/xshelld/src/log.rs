//! Diagnostics go to stderr as `<rfc3339> <LEVEL> <message>`. `serve` runs with stderr
//! redirected to `~/.xshell/log/xshelld.log`; for `connect`, stderr is the ssh error text.

#[doc(hidden)]
pub fn write(level: &str, msg: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let now = xshell_core::time::system_time_to_iso(std::time::SystemTime::now());
    // One write per line, so lines from concurrent writers never interleave.
    let line = format!("{now} {level} {msg}\n");
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! log {
    ($level:literal, $($arg:tt)*) => {
        $crate::log::write($level, format_args!($($arg)*))
    };
}
