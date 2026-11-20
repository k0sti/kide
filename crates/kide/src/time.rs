//! Modification times as the format keeps them: nanoseconds since the Unix
//! epoch, negative before it (§2.1).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn to_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

pub fn from_ns(ns: i64) -> SystemTime {
    let d = Duration::from_nanos(ns.unsigned_abs());
    match ns >= 0 {
        true => UNIX_EPOCH + d,
        false => UNIX_EPOCH - d,
    }
}

/// The current time in nanoseconds since the Unix epoch.
pub fn now() -> i64 {
    to_ns(SystemTime::now())
}

/// The current time in whole seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // Windows keeps times in steps of 100 ns.
        for ns in [0, 100, -100, 1_761_134_400_123_456_700, -86_400_000_000_100] {
            assert_eq!(to_ns(from_ns(ns)), ns);
        }
    }
}
