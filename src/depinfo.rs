//! rustc `--emit=dep-info` files (Makefile syntax plus `# env-dep:` lines).

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, PartialEq, Eq)]
pub struct DepInfo {
    /// Every prerequisite path, as written by rustc (relative paths are
    /// relative to rustc's working directory).
    pub files: Vec<String>,
    /// Environment variables read through `env!`/`option_env!`, with the value
    /// escaped like rustc does (`None` = unset).
    pub env: Vec<(String, Option<String>)>,
}

pub fn parse(text: &str) -> DepInfo {
    let mut files = BTreeSet::new();
    let mut env = BTreeMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            match rest.split_once('=') {
                Some((name, value)) => env.insert(name.to_owned(), Some(value.to_owned())),
                None => env.insert(rest.to_owned(), None),
            };
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(colon) = rule_colon(line) {
            files.extend(split_escaped(&line[colon + 1..]));
        }
    }
    DepInfo {
        files: files.into_iter().collect(),
        env: env.into_iter().collect(),
    }
}

/// Escapes an environment value the way rustc writes it into dep-info.
pub fn escape_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\\' => out.push_str(r"\\"),
            _ => out.push(c),
        }
    }
    out
}

/// Position of the `target: prerequisites` separator, skipping escaped bytes.
fn rule_colon(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b':' if bytes.get(i + 1).is_none_or(|&b| b == b' ') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits on unescaped spaces; rustc escapes spaces in paths as `\ `.
fn split_escaped(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(' ') => cur.push(' '),
                Some(other) => {
                    cur.push('\\');
                    cur.push(other);
                }
                None => cur.push('\\'),
            },
            ' ' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_prerequisites_and_env_deps() {
        let text = "\
/t/debug/deps/libfoo-1.rmeta: src/lib.rs src/a\\ b.rs /t/debug/build/foo-2/out/gen.rs

/t/debug/deps/foo-1.d: src/lib.rs src/a\\ b.rs /t/debug/build/foo-2/out/gen.rs

src/lib.rs:
src/a\\ b.rs:
/t/debug/build/foo-2/out/gen.rs:

# env-dep:CARGO_PKG_NAME=foo
# env-dep:MULTI=a\\nb
# env-dep:UNSET
";
        let info = parse(text);
        assert_eq!(
            info.files,
            [
                "/t/debug/build/foo-2/out/gen.rs",
                "src/a b.rs",
                "src/lib.rs"
            ]
        );
        assert_eq!(
            info.env,
            [
                ("CARGO_PKG_NAME".to_owned(), Some("foo".to_owned())),
                ("MULTI".to_owned(), Some(r"a\nb".to_owned())),
                ("UNSET".to_owned(), None),
            ]
        );
        assert_eq!(escape_env("a\nb"), r"a\nb");
        assert_eq!(escape_env(r"c:\x"), r"c:\\x");
    }
}
