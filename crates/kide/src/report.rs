//! Reporting of skipped names and held entries (§5.3), and of conflict
//! copies not made (§5.6): on standard error, once per path while the
//! program runs, whether or not logging is enabled. Status output lists
//! what was reported.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// Why a path is not synchronised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    /// Scanning skipped the name on disk (§5.3, §5.5).
    Skipped,
    /// The device holds the entry instead of writing it (§5.3).
    Held,
    /// A conflict copy's path would be invalid, so none was made (§5.6).
    NotCopied,
    /// Two on-disk entries collide: the platform setting is wrong (§5.3).
    Mismatch,
}

fn reported() -> MutexGuard<'static, BTreeMap<String, Why>> {
    static SEEN: Mutex<BTreeMap<String, Why>> = Mutex::new(BTreeMap::new());
    // A poisoned lock only means another thread panicked; the map is intact.
    SEEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reports `path` once per run; returns whether this call reported it.
pub fn report(why: Why, path: &str, detail: &str) -> bool {
    let mut seen = reported();
    let fresh = !seen.contains_key(path);
    if fresh {
        seen.insert(path.to_string(), why);
    }
    drop(seen);
    if fresh {
        let what = match why {
            Why::Skipped => "skipped",
            Why::Held => "held",
            Why::NotCopied => "not copied",
            Why::Mismatch => "mismatch",
        };
        eprintln!("kide: {what} {path:?}: {detail}");
    }
    fresh
}

/// Every path reported so far for `why`, in byte order.
pub fn reported_paths(why: Why) -> Vec<String> {
    let seen = reported();
    seen.iter()
        .filter(|(_, w)| **w == why)
        .map(|(p, _)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn once_per_path() {
        assert!(report(Why::Skipped, "report-test/a", "invalid"));
        assert!(!report(Why::Skipped, "report-test/a", "invalid"));
        assert!(!report(Why::Held, "report-test/a", "again"));
        assert!(report(Why::Held, "report-test/b", "collides"));
        assert!(reported_paths(Why::Skipped).contains(&"report-test/a".into()));
        assert!(reported_paths(Why::Held).contains(&"report-test/b".into()));
    }
}
