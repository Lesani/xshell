//! Host aliases from `~/.ssh/config` (and its `Include`s), for the SSH target suggestions.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 4;

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// Split `Keyword args` or `Keyword=args`.
fn split_kw(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let kw = line[..end].to_ascii_lowercase();
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    Some((kw, rest))
}

fn words(s: &str) -> Vec<&str> {
    // Quoted words with spaces are rare in Host lines; split on whitespace and unquote.
    s.split_whitespace().map(unquote).collect()
}

/// `(aliases, includes)` declared in one config file's text. Patterns (`*`, `?`, `!`) are
/// not aliases.
pub fn parse_hosts(text: &str) -> (Vec<String>, Vec<String>) {
    let mut hosts = Vec::new();
    let mut includes = Vec::new();
    for line in text.lines() {
        let Some((kw, rest)) = split_kw(line) else {
            continue;
        };
        match kw.as_str() {
            "host" => {
                for w in words(rest) {
                    if !w.is_empty() && !w.contains(['*', '?', '!']) {
                        hosts.push(w.to_string());
                    }
                }
            }
            "include" => includes.extend(words(rest).into_iter().map(String::from)),
            _ => {}
        }
    }
    (hosts, includes)
}

fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob_match(&pat[1..], s) || (!s.is_empty() && glob_match(pat, &s[1..])),
        (Some(b'?'), Some(_)) => glob_match(&pat[1..], &s[1..]),
        (Some(p), Some(c)) if p == c => glob_match(&pat[1..], &s[1..]),
        _ => false,
    }
}

/// The files an `Include` argument names: `~/` and relative paths resolve like ssh does
/// (relative to `~/.ssh`); a glob is allowed in the last path component.
fn resolve_include(arg: &str, home: &Path) -> Vec<PathBuf> {
    let p = if let Some(rest) = arg.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        home.join(".ssh").join(arg)
    };
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![p];
    }
    let Some(dir) = p.parent() else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| glob_match(name.as_bytes(), e.file_name().to_string_lossy().as_bytes()))
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn collect(file: &Path, home: &Path, depth: usize, out: &mut BTreeSet<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let (hosts, includes) = parse_hosts(&text);
    out.extend(hosts);
    for inc in includes {
        for f in resolve_include(&inc, home) {
            collect(&f, home, depth + 1, out);
        }
    }
}

/// Every alias in `home/.ssh/config` and the files it includes, deduplicated and sorted.
pub fn list_hosts(home: &Path) -> Vec<String> {
    let mut out = BTreeSet::new();
    collect(&home.join(".ssh").join("config"), home, 0, &mut out);
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_skip_patterns() {
        let text = "# comment\nHost a b *.x !c\n  HostName 1.2.3.4\nhost=d\nMatch host e\n  User u\nHOST \"f\"\n";
        let (hosts, inc) = parse_hosts(text);
        assert_eq!(hosts, ["a", "b", "d", "f"]);
        assert!(inc.is_empty());
    }

    #[test]
    fn follows_includes_bounded() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Include conf.d/*\nHost main\nInclude ~/.ssh/loop\n",
        )
        .unwrap();
        std::fs::write(ssh.join("conf.d/one"), "Host one dup\n").unwrap();
        std::fs::write(ssh.join("conf.d/two.conf"), "Host two dup\n").unwrap();
        // Includes itself forever: the depth bound stops it.
        std::fs::write(ssh.join("loop"), "Host looped\nInclude loop\n").unwrap();
        assert_eq!(
            list_hosts(home.path()),
            ["dup", "looped", "main", "one", "two"]
        );
        assert!(list_hosts(&home.path().join("missing")).is_empty());
    }

    #[test]
    fn globbing() {
        assert!(glob_match(b"*.conf", b"a.conf"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"*.conf", b"a.cfg"));
        assert!(glob_match(b"*", b""));
    }
}
