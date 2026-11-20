//! Which paths a device takes part in (§5.8): `ignore` and `sync` rules,
//! in the order of the configuration. A rule is a path pattern whose
//! components may hold `*`, matching any run of characters within one
//! component; it applies to the paths it matches and everything below them.
//! The last rule that applies decides; a path no rule applies to is synced.
//! So `ignore = .obsidian` then `sync = .obsidian/plugins/*/data.json`
//! syncs only each plugin's settings there.

use crate::format::path::check_path;

/// One rule: whether it syncs, and its pattern's components.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    sync: bool,
    pattern: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules(Vec<Rule>);

impl Rules {
    /// Adds a rule; the pattern must be a valid path once `*` is read as a
    /// character, and a trailing `/` is dropped.
    pub fn add(&mut self, sync: bool, pattern: &str) -> Result<(), String> {
        let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
        check_path(pattern).map_err(|e| e.to_string())?;
        let pattern = pattern.split('/').map(str::to_string).collect();
        self.0.push(Rule { sync, pattern });
        Ok(())
    }

    /// Whether `path` is ignored: the last rule that applies is `ignore`.
    pub fn ignores(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.strip_suffix('/').unwrap_or(path).split('/').collect();
        let applies = |r: &&Rule| {
            r.pattern.len() <= parts.len() && r.pattern.iter().zip(&parts).all(|(p, c)| glob(p, c))
        };
        self.0.iter().rev().find(applies).is_some_and(|r| !r.sync)
    }

    /// Whether anything below the directory `dir` may be synced, so that a
    /// walk or a watch must look inside: it is not ignored, or a `sync`
    /// rule reaches below it.
    pub fn looks_below(&self, dir: &str) -> bool {
        let parts: Vec<&str> = dir.split('/').collect();
        let reaches = |r: &Rule| {
            r.sync
                && r.pattern.len() > parts.len()
                && r.pattern.iter().zip(&parts).all(|(p, c)| glob(p, c))
        };
        !self.ignores(dir) || self.0.iter().any(reaches)
    }
}

/// Whether `name` matches `pattern`, where `*` matches any run of
/// characters, the empty one too.
fn glob(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let middle: Vec<&str> = parts.collect();
    let Some((last, middle)) = middle.split_last() else {
        return rest.is_empty(); // no `*`: the whole name
    };
    for m in middle {
        match rest.find(m) {
            Some(i) => rest = &rest[i + m.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &[(bool, &str)]) -> Rules {
        let mut r = Rules::default();
        for (sync, p) in lines {
            r.add(*sync, p).unwrap();
        }
        r
    }

    #[test]
    fn globs() {
        for (p, n) in [
            ("*", "x"),
            ("*", ""),
            ("a*", "abc"),
            ("*.json", "app.json"),
            ("w*e*.json", "workspace.json"),
            ("a*a", "aa"),
        ] {
            assert!(glob(p, n), "{p} {n}");
        }
        for (p, n) in [
            ("a*", "ba"),
            ("*.json", "app.js"),
            ("a*a", "a"),
            ("x", "xy"),
        ] {
            assert!(!glob(p, n), "{p} {n}");
        }
    }

    #[test]
    fn the_last_rule_that_applies_decides() {
        let r = rules(&[
            (false, ".obsidian"),
            (true, ".obsidian/*.json"),
            (true, ".obsidian/plugins/*/data.json"),
            (false, ".obsidian/workspace*.json"),
        ]);
        for p in [
            ".obsidian/app.json",
            ".obsidian/plugins/tasks/data.json",
            "notes/a.md",
            ".obsidianx",
        ] {
            assert!(!r.ignores(p), "{p} syncs");
        }
        for p in [
            ".obsidian",
            ".obsidian/workspace.json",
            ".obsidian/workspace-mobile.json",
            ".obsidian/plugins/tasks/cache.db",
            ".obsidian/plugins/tasks/",
            ".obsidian/themes/x.css",
        ] {
            assert!(r.ignores(p), "{p} is ignored");
        }
        assert!(r.looks_below(".obsidian") && r.looks_below(".obsidian/plugins/tasks"));
        assert!(
            !r.looks_below(".obsidian/themes"),
            "nothing below is synced"
        );
        assert!(rules(&[]).looks_below("notes"));
    }

    #[test]
    fn invalid_patterns_are_refused() {
        let mut r = Rules::default();
        for p in ["../x", "a//b", ""] {
            assert!(r.add(false, p).is_err(), "{p:?}");
        }
    }
}
