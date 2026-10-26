//! Two checkouts of the same Cargo workspace, each with its own target
//! directory, sharing one rcache store.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// `RCACHE_PLACE` for every build (`link` forces the ext4 code path).
    place: Option<&'static str>,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        // Canonical so paths match what Cargo reports (/var → /private/var).
        let root = dir.path().canonicalize().unwrap();
        Sandbox {
            _dir: dir,
            root,
            place: None,
        }
    }

    /// Hard links instead of clones, as on a filesystem without reflinks.
    fn linking() -> Sandbox {
        Sandbox {
            place: Some("link"),
            ..Sandbox::new()
        }
    }

    fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    /// A workspace whose library has a build script generating code that
    /// embeds the absolute OUT_DIR, like most code generators do, and depends
    /// on `ext`, a crate outside every workspace. Like registry crates, Cargo
    /// compiles it with its own directory as cwd; like many of them it has a
    /// build script, which reruns in every new checkout.
    fn workspace(&self, name: &str, greeting: &str, print_manifest_dir: bool) -> PathBuf {
        write(
            &self.root,
            "ext/Cargo.toml",
            "[package]\nname = \"ext\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &self.root,
            "ext/build.rs",
            "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        );
        write(
            &self.root,
            "ext/src/lib.rs",
            "pub fn suffix() -> &'static str { \"gen\" }\n",
        );
        let ws = self.root.join(name);
        write(
            &ws,
            "Cargo.toml",
            "[workspace]\nmembers = [\"lib\", \"app\"]\nresolver = \"2\"\n",
        );
        // cdylib: Cargo gives the unit stable names (no `-C extra-filename`).
        write(
            &ws,
            "lib/Cargo.toml",
            "[package]\nname = \"demo_lib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [lib]\ncrate-type = [\"lib\", \"cdylib\"]\n\
             [dependencies]\next = { path = \"../../ext\" }\n",
        );
        write(
            &ws,
            "lib/build.rs",
            r#"fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let code = format!("// generated in {out}\npub const GENERATED: &str = \"gen\";\n");
    std::fs::write(format!("{out}/gen.rs"), code).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
"#,
        );
        write(
            &ws,
            "lib/src/lib.rs",
            &format!(
                "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n\
                 pub fn greet() -> String {{ assert_eq!(ext::suffix(), GENERATED); format!(\"{greeting} {{GENERATED}}\") }}\n"
            ),
        );
        write(
            &ws,
            "app/Cargo.toml",
            "[package]\nname = \"demo_app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\ndemo_lib = { path = \"../lib\" }\n",
        );
        let main = if print_manifest_dir {
            "fn main() { println!(\"{} {}\", demo_lib::greet(), env!(\"CARGO_MANIFEST_DIR\")); }\n"
        } else {
            "fn main() { println!(\"{}\", demo_lib::greet()); }\n"
        };
        write(&ws, "app/src/main.rs", main);
        ws
    }

    fn cargo(&self, ws: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new(env!("CARGO"));
        // Drop the outer `cargo test` environment so the nested build is clean.
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy();
            if (name.starts_with("CARGO_") && name != "CARGO_HOME")
                || name.starts_with("RUSTC")
                || name.starts_with("RCACHE")
            {
                cmd.env_remove(&*name);
            }
        }
        cmd.args(args)
            .current_dir(ws)
            .env("RUSTC_WRAPPER", env!("CARGO_BIN_EXE_rcache"))
            .env("RCACHE_DIR", self.store());
        if let Some(place) = self.place {
            cmd.env("RCACHE_PLACE", place);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "cargo {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn run_app(&self, ws: &Path) -> String {
        self.cargo(ws, &["run", "-q", "-p", "demo_app"])
    }

    /// `(kind, crate)` for every wrapped compile so far.
    fn log(&self) -> Vec<(String, String)> {
        fs::read_to_string(self.store().join("log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (f[1].to_owned(), f[2].to_owned())
            })
            .collect()
    }

    fn kinds_since(&self, from: usize, crate_name: &str) -> Vec<String> {
        self.log()[from..]
            .iter()
            .filter(|(_, c)| c == crate_name)
            .map(|(k, _)| k.clone())
            .collect()
    }
}

fn write(ws: &Path, rel: &str, text: &str) {
    let path = ws.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// The second checkout reuses every unit and Cargo accepts the placed files
/// as up to date — in that checkout and, untouched, in the first one.
fn reuses_every_unit_and_stays_fresh(sb: &Sandbox) -> (PathBuf, PathBuf) {
    let a = sb.workspace("a", "A", false);
    let b = sb.workspace("b", "A", false);
    assert_eq!(sb.run_app(&a), "A gen");

    let before = sb.log().len();
    assert_eq!(sb.run_app(&b), "A gen");
    for krate in ["ext", "demo_lib", "demo_app"] {
        assert_eq!(
            sb.kinds_since(before, krate),
            ["hit"],
            "{krate} in b: {:?}",
            sb.log()
        );
    }
    // ext's and demo_lib's.
    assert_eq!(sb.kinds_since(before, "build_script_build"), ["hit", "hit"]);

    let after = sb.log().len();
    sb.cargo(&b, &["build", "-q"]);
    sb.cargo(&a, &["build", "-q"]);
    assert_eq!(sb.log().len(), after, "rebuilt: {:?}", &sb.log()[after..]);
    (a, b)
}

#[test]
fn identical_workspace_reuses_every_unit_and_stays_fresh() {
    reuses_every_unit_and_stays_fresh(&Sandbox::new());
}

#[test]
fn link_mode_shares_rlibs_and_keeps_freshness_files_private() {
    let sb = Sandbox::linking();
    let (_, b) = reuses_every_unit_and_stays_fresh(&sb);
    let deps = b.join("target/debug/deps");
    let unit = |prefix: &str, ext: &str| {
        let path = fs::read_dir(&deps)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with(prefix) && name.ends_with(ext)
            })
            .unwrap_or_else(|| panic!("no {prefix}*{ext}"));
        fs::metadata(path).unwrap()
    };
    let private = |meta: fs::Metadata| {
        assert_eq!(meta.nlink(), 1);
        assert_ne!(meta.permissions().mode() & 0o200, 0);
    };
    // Each rlib is one file for a, the store and b, read-only so nothing
    // writes through it.
    for prefix in ["libext-", "libdemo_lib."] {
        let rlib = unit(prefix, ".rlib");
        assert!(rlib.nlink() >= 3, "{prefix} rlib links: {}", rlib.nlink());
        assert_eq!(rlib.permissions().mode() & 0o222, 0);
    }
    // Every unit keeps an output Cargo tracks that carries b's own mtime when
    // a dependency is newer in b than the store's copy: ext's build script
    // ran in b, so its .rmeta is private; demo_lib's cdylib (Cargo emits no
    // .rmeta for a lib that also links one) always is.
    private(unit("libext-", ".rmeta"));
    private(unit("libdemo_lib.", std::env::consts::DLL_EXTENSION));
}

#[test]
fn link_mode_rebuild_never_writes_through_a_link() {
    let sb = Sandbox::linking();
    let (a, b) = reuses_every_unit_and_stays_fresh(&sb);
    let stored = store_hashes(&sb.store());

    // b's lib (whose rlib is linked to a's and the store's) recompiles.
    sb.workspace("b", "B", false);
    assert_eq!(sb.run_app(&b), "B gen");
    assert_eq!(sb.run_app(&a), "A gen");
    let now = store_hashes(&sb.store());
    for (path, hash) in &stored {
        assert_eq!(now.get(path), Some(hash), "{} changed", path.display());
    }
}

#[test]
fn gc_evicts_entries_no_checkout_links_first() {
    let sb = Sandbox::linking();
    let b = sb.workspace("b", "B", false);
    assert_eq!(sb.run_app(&b), "B gen");
    let a = sb.workspace("a", "A", false);
    assert_eq!(sb.run_app(&a), "A gen");
    // a's own lib and app entries are now only in the store; b's lib (the
    // least recently used) and the shared ext are still linked from b.
    fs::remove_dir_all(a.join("target")).unwrap();
    let linked: u64 = fs::read_dir(sb.store().join("entries"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|entry| {
            fs::read_dir(entry.join("out"))
                .unwrap()
                .any(|f| f.unwrap().metadata().unwrap().nlink() > 1)
        })
        .map(|entry| tree_size(&entry))
        .sum();
    let gc = Command::new(env!("CARGO_BIN_EXE_rcache"))
        .args(["gc", "--max-size", &linked.to_string()])
        .env("RCACHE_DIR", sb.store())
        .output()
        .unwrap();
    assert!(gc.status.success());

    let c = sb.workspace("c", "B", false);
    let before = sb.log().len();
    assert_eq!(sb.run_app(&c), "B gen");
    assert_eq!(sb.kinds_since(before, "ext"), ["hit"]);
    assert_eq!(sb.kinds_since(before, "demo_lib"), ["hit"]);
}

/// Content hash of every file under the store's entries.
fn store_hashes(store: &Path) -> std::collections::HashMap<PathBuf, blake3::Hash> {
    let mut out = std::collections::HashMap::new();
    let mut dirs = vec![store.join("entries")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                out.insert(path.clone(), blake3::hash(&fs::read(&path).unwrap()));
            }
        }
    }
    out
}

fn tree_size(path: &Path) -> u64 {
    let meta = fs::symlink_metadata(path).unwrap();
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .unwrap()
        .map(|e| tree_size(&e.unwrap().path()))
        .sum()
}

#[test]
fn edited_workspace_never_gets_another_workspaces_build() {
    let sb = Sandbox::new();
    // b is edited *before* a builds: mtime-based freshness (what a shared
    // target dir relies on) would hand b the output of a.
    let b = sb.workspace("b", "B", false);
    let a = sb.workspace("a", "A", false);
    assert_eq!(sb.run_app(&a), "A gen");

    let before = sb.log().len();
    assert_eq!(sb.run_app(&b), "B gen");
    assert_eq!(sb.kinds_since(before, "build_script_build"), ["hit", "hit"]);
    assert_eq!(sb.kinds_since(before, "demo_lib"), ["miss"]);
    assert_eq!(sb.kinds_since(before, "demo_app"), ["miss"]);
    assert_eq!(sb.run_app(&a), "A gen");
}

#[test]
fn manifest_dir_reads_stay_per_workspace() {
    let sb = Sandbox::new();
    let a = sb.workspace("a", "A", true);
    let b = sb.workspace("b", "A", true);
    assert_eq!(sb.run_app(&a), format!("A gen {}", a.join("app").display()));

    let before = sb.log().len();
    assert_eq!(sb.run_app(&b), format!("A gen {}", b.join("app").display()));
    assert_eq!(sb.kinds_since(before, "demo_lib"), ["hit"]);
    assert_eq!(sb.kinds_since(before, "demo_app"), ["miss"]);
}

#[test]
fn gc_to_zero_empties_the_store_and_builds_still_work() {
    let sb = Sandbox::new();
    let a = sb.workspace("a", "A", false);
    assert_eq!(sb.run_app(&a), "A gen");
    let gc = Command::new(env!("CARGO_BIN_EXE_rcache"))
        .args(["gc", "--max-size", "0"])
        .env("RCACHE_DIR", sb.store())
        .output()
        .unwrap();
    assert!(gc.status.success());
    assert_eq!(fs::read_dir(sb.store().join("entries")).unwrap().count(), 0);

    let b = sb.workspace("b", "A", false);
    let before = sb.log().len();
    assert_eq!(sb.run_app(&b), "A gen");
    assert_eq!(sb.kinds_since(before, "demo_lib"), ["miss"]);
}

#[test]
fn explicit_target_build_is_shared() {
    // `--target` splits outputs into `target/<triple>/` (tagged separately)
    // and host artifacts like build scripts in `target/<profile>/`.
    let rustc = Command::new("rustc").arg("-vV").output().unwrap();
    let rustc = String::from_utf8(rustc.stdout).unwrap();
    let host = rustc
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap();
    let sb = Sandbox::new();
    let a = sb.workspace("a", "A", false);
    let b = sb.workspace("b", "A", false);
    sb.cargo(&a, &["build", "-q", "--target", host]);
    let before = sb.log().len();
    sb.cargo(&b, &["build", "-q", "--target", host]);
    let kinds: Vec<_> = sb.log()[before..]
        .iter()
        .map(|(k, c)| format!("{k} {c}"))
        .collect();
    assert!(kinds.iter().all(|k| k.starts_with("hit ")), "{kinds:?}");
    assert_eq!(kinds.len(), 5, "{kinds:?}");
}
