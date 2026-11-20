//! Logging to stderr, enabled by `KIDE_LOG=1`.

use std::sync::OnceLock;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("KIDE_LOG").is_some_and(|v| v == "1"))
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => {
        if $crate::log::enabled() {
            eprintln!("kide: {}", format_args!($($t)*));
        }
    };
}
