//! Workspace-independent spelling of paths: the target directory becomes `$T`
//! and rustc's working directory becomes `$CWD`.

use std::path::{Path, PathBuf};

pub struct Roots {
    pub cwd: String,
    pub target: String,
}

impl Roots {
    pub fn new(cwd: &Path, target: &Path) -> Option<Roots> {
        Some(Roots {
            cwd: cwd.to_str()?.to_owned(),
            target: target.to_str()?.to_owned(),
        })
    }

    /// Replaces this workspace's roots inside arbitrary text (arguments,
    /// environment values).
    pub fn normalize(&self, text: &str) -> String {
        replace_roots(text, &[(&self.target, "$T"), (&self.cwd, "$CWD")])
    }

    /// Like [`Roots::normalize`] but only for the target directory. Used for
    /// `env!` values: `OUT_DIR` is almost always `include!`d (the included
    /// file is tracked separately), while `CARGO_MANIFEST_DIR` often locates
    /// files at runtime, so a workspace path must keep its identity.
    pub fn normalize_target(&self, text: &str) -> String {
        replace_roots(text, &[(&self.target, "$T")])
    }

    /// Workspace-independent name of a file (absolute or relative to cwd).
    pub fn name_of(&self, path: &Path) -> Option<String> {
        let abs = self.absolute(path);
        if let Ok(rel) = abs.strip_prefix(&self.target) {
            return Some(format!("$T/{}", rel.to_str()?));
        }
        if let Ok(rel) = abs.strip_prefix(&self.cwd) {
            return Some(format!("$CWD/{}", rel.to_str()?));
        }
        abs.to_str().map(str::to_owned)
    }

    /// Inverse of [`Roots::name_of`] for this workspace.
    pub fn resolve(&self, name: &str) -> PathBuf {
        if let Some(rel) = name.strip_prefix("$T/") {
            Path::new(&self.target).join(rel)
        } else if let Some(rel) = name.strip_prefix("$CWD/") {
            Path::new(&self.cwd).join(rel)
        } else {
            PathBuf::from(name)
        }
    }

    pub fn absolute(&self, path: &Path) -> PathBuf {
        Path::new(&self.cwd).join(path)
    }
}

/// One-pass replacement of path prefixes. A prefix only matches as a whole
/// path component run: `/a/rust` matches in `dep=/a/rust/x` but not in
/// `/a/rust-b` or `/z/a/rust`. Longer prefixes should come first.
pub fn replace_roots(text: &str, pats: &[(&str, &str)]) -> String {
    String::from_utf8(replace_roots_bytes(
        text.as_bytes(),
        &pats
            .iter()
            .map(|(from, to)| (from.as_bytes(), to.as_bytes()))
            .collect::<Vec<_>>(),
    ))
    .expect("replacing UTF-8 with UTF-8 at char boundaries")
}

pub fn replace_roots_bytes(data: &[u8], pats: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    'scan: while i < data.len() {
        let left_ok = i == 0 || !(is_path_byte(data[i - 1]) || data[i - 1] == b'/');
        if left_ok {
            for (from, to) in pats {
                if !from.is_empty() && data[i..].starts_with(from) {
                    let end = i + from.len();
                    if data.get(end).is_none_or(|&b| !is_path_byte(b)) {
                        out.extend_from_slice(to);
                        i = end;
                        continue 'scan;
                    }
                }
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+' | b'@' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_whole_roots_only() {
        let roots = Roots {
            cwd: "/w/a/rust".into(),
            target: "/w/a/rust/target".into(),
        };
        assert_eq!(
            roots.normalize("dependency=/w/a/rust/target/debug/deps"),
            "dependency=$T/debug/deps"
        );
        assert_eq!(roots.normalize("/w/a/rust"), "$CWD");
        assert_eq!(roots.normalize("/w/a/rust/core/x.rs"), "$CWD/core/x.rs");
        assert_eq!(roots.normalize("/w/a/rust-b/x"), "/w/a/rust-b/x");
        assert_eq!(roots.normalize("/z/w/a/rust/x"), "/z/w/a/rust/x");
        assert_eq!(roots.normalize("/w/a/rust=."), "$CWD=.");
    }

    #[test]
    fn names_round_trip_across_workspaces() {
        let a = Roots {
            cwd: "/w/a".into(),
            target: "/w/a/target".into(),
        };
        let b = Roots {
            cwd: "/w/b".into(),
            target: "/cache/b-target".into(),
        };
        let generated = a
            .name_of(Path::new("/w/a/target/debug/build/x/out/g.rs"))
            .unwrap();
        assert_eq!(generated, "$T/debug/build/x/out/g.rs");
        assert_eq!(
            b.resolve(&generated),
            PathBuf::from("/cache/b-target/debug/build/x/out/g.rs")
        );
        let src = a.name_of(Path::new("core/src/lib.rs")).unwrap();
        assert_eq!(src, "$CWD/core/src/lib.rs");
        assert_eq!(b.resolve(&src), PathBuf::from("/w/b/core/src/lib.rs"));
        let reg = a
            .name_of(Path::new("/home/.cargo/registry/src/x/lib.rs"))
            .unwrap();
        assert_eq!(
            b.resolve(&reg),
            PathBuf::from("/home/.cargo/registry/src/x/lib.rs")
        );
    }
}
