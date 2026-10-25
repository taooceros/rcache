//! Parsing of the rustc command lines Cargo produces.

use std::ffi::OsString;
use std::path::PathBuf;

/// Flags whose value may be passed as the following argument.
const VALUE_FLAGS: &[&str] = &[
    "--crate-name",
    "--edition",
    "--crate-type",
    "--emit",
    "--out-dir",
    "-o",
    "-C",
    "--codegen",
    "-L",
    "-l",
    "--extern",
    "--cfg",
    "--check-cfg",
    "--cap-lints",
    "--error-format",
    "--json",
    "--target",
    "--sysroot",
    "--print",
    "-A",
    "-W",
    "-D",
    "-F",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--force-warn",
    "--remap-path-prefix",
    "--diagnostic-width",
    "--color",
    "--explain",
    "-Z",
    "--env-set",
    "--crate-attr",
];

/// Short flags that also take a joined value (`-Copt-level=3`, `-Ldependency=…`).
const JOINED: &[&str] = &["-C", "-L", "-l", "-A", "-W", "-D", "-F", "-Z", "-o"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// A flag without a value (`-O`, `--test`).
    Flag(String),
    /// A flag and its value; `--codegen` is folded into `-C`.
    Value(String, String),
    /// The crate root source file.
    Input(String),
}

#[derive(Debug)]
pub struct Invocation {
    pub args: Vec<Arg>,
    pub crate_name: String,
    pub extra_filename: String,
    pub out_dir: PathBuf,
}

impl Invocation {
    /// File stem shared by every artifact of this unit (`foo-1a2b3c…`; just
    /// `foo` for units Cargo gives stable names, like cdylibs).
    pub fn stem(&self) -> String {
        format!("{}{}", self.crate_name, self.extra_filename)
    }
}

/// Parses a cacheable compile. `None` means "run rustc untouched": queries
/// (`-vV`, `--print`), stdin input, explicit `-o`, emits other than
/// link/metadata/dep-info, or anything without dep-info to learn inputs from.
pub fn parse(raw: &[OsString]) -> Option<Invocation> {
    let mut args = Vec::with_capacity(raw.len());
    let mut it = raw.iter();
    while let Some(a) = it.next() {
        let a = a.to_str()?;
        let long_joined = a
            .split_once('=')
            .filter(|(flag, _)| flag.starts_with("--") && VALUE_FLAGS.contains(flag));
        let arg = if let Some((flag, value)) = long_joined {
            Arg::Value(canonical(flag), value.to_owned())
        } else if VALUE_FLAGS.contains(&a) {
            Arg::Value(canonical(a), it.next()?.to_str()?.to_owned())
        } else if a.len() > 2 && a.is_char_boundary(2) && JOINED.contains(&&a[..2]) {
            Arg::Value(a[..2].to_owned(), a[2..].to_owned())
        } else if a == "-" {
            return None;
        } else if a.starts_with('-') {
            Arg::Flag(a.to_owned())
        } else {
            Arg::Input(a.to_owned())
        };
        args.push(arg);
    }

    let mut crate_name = None;
    let mut out_dir = None;
    let mut extra_filename = None;
    let mut inputs = 0;
    let mut dep_info = false;
    for arg in &args {
        match arg {
            Arg::Input(_) => inputs += 1,
            Arg::Flag(_) => {}
            Arg::Value(flag, value) => match flag.as_str() {
                "--crate-name" => crate_name = Some(value.clone()),
                "--out-dir" => out_dir = Some(PathBuf::from(value)),
                "-o" | "--print" => return None,
                "--emit" => {
                    for kind in value.split(',') {
                        match kind {
                            "dep-info" => dep_info = true,
                            "link" | "metadata" => {}
                            _ => return None,
                        }
                    }
                }
                "-C" => {
                    if let Some(extra) = value.strip_prefix("extra-filename=") {
                        extra_filename = Some(extra.to_owned());
                    }
                }
                _ => {}
            },
        }
    }
    if inputs != 1 || !dep_info {
        return None;
    }
    Some(Invocation {
        args,
        crate_name: crate_name?,
        extra_filename: extra_filename.unwrap_or_default(),
        out_dir: out_dir?,
    })
}

fn canonical(flag: &str) -> String {
    if flag == "--codegen" { "-C" } else { flag }.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_cargo_lib_compile() {
        let inv = parse(&os(&[
            "--crate-name",
            "serde",
            "--edition=2018",
            "/r/serde-1.0/src/lib.rs",
            "--error-format=json",
            "--crate-type",
            "lib",
            "--emit=dep-info,metadata,link",
            "-C",
            "opt-level=3",
            "-Cextra-filename=-0a1b",
            "--out-dir",
            "/t/release/deps",
            "-L",
            "dependency=/t/release/deps",
            "--extern",
            "serde_derive=/t/release/deps/libserde_derive-9.dylib",
            "-O",
        ]))
        .unwrap();
        assert_eq!(inv.stem(), "serde-0a1b");
        assert_eq!(inv.out_dir, PathBuf::from("/t/release/deps"));
        assert!(
            inv.args
                .contains(&Arg::Value("-C".into(), "opt-level=3".into()))
        );
        assert!(inv.args.contains(&Arg::Value(
            "-L".into(),
            "dependency=/t/release/deps".into()
        )));
        assert!(inv.args.contains(&Arg::Flag("-O".into())));
        assert!(
            inv.args
                .contains(&Arg::Input("/r/serde-1.0/src/lib.rs".into()))
        );
    }

    #[test]
    fn rejects_queries_and_uncacheable_compiles() {
        assert!(parse(&os(&["-vV"])).is_none());
        let base = [
            "--crate-name",
            "x",
            "src/lib.rs",
            "--out-dir",
            "/t",
            "-C",
            "extra-filename=-1",
        ];
        let with = |extra: &[&str]| {
            let mut v = base.to_vec();
            v.extend_from_slice(extra);
            parse(&os(&v))
        };
        assert!(with(&["--emit=dep-info,link"]).is_some());
        // No dep-info: inputs cannot be learned.
        assert!(with(&["--emit=link"]).is_none());
        assert!(with(&["--emit=dep-info,asm"]).is_none());
        assert!(with(&["--emit=dep-info,link=/x/out"]).is_none());
        assert!(with(&["--emit=dep-info,link", "--print", "cfg"]).is_none());
        assert!(with(&["--emit=dep-info,link", "-o", "/x"]).is_none());
        assert!(with(&["--emit=dep-info,link", "src/other.rs"]).is_none());
        assert!(with(&["--emit=dep-info,link", "-"]).is_none());
    }
}
