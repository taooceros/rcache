//! `RUSTC_WRAPPER` mode: key a rustc compile by its workspace-independent
//! inputs, then either clone a stored result into this workspace's target
//! directory or compile and store the result.
//!
//! Two-level lookup, like ccache's manifests:
//! - the *primary key* covers what is known before compiling: compiler,
//!   normalized arguments, identities of `--extern` crates, Cargo-provided
//!   environment and the build script's `OUT_DIR`;
//! - each candidate entry under a primary key lists the files and `env!`
//!   variables rustc reported in dep-info; the first candidate whose inputs
//!   all match this workspace wins.
//!
//! An entry id is a hash of the primary key plus those inputs, i.e. it names
//! the semantic content of the outputs. Outputs carry the id in an xattr, so a
//! dependent's key is the same in every workspace even though rlibs embed
//! workspace-specific absolute paths.

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::args::{self, Arg, Invocation};
use crate::depinfo;
use crate::fsutil;
use crate::paths::{Roots, replace_roots, replace_roots_bytes};
use crate::store::{self, Input, Manifest, Store, VERSION};

const KEY_VERSION: &str = "rcache-key-1";
/// Candidates checked per lookup; older ones are effectively dead.
const MAX_CANDIDATES: usize = 32;
/// Environment rustc itself reads (not visible in dep-info).
const RUSTC_ENV: &[&str] = &[
    "RUSTC_BOOTSTRAP",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
    "SDKROOT",
    "OUT_DIR",
];

/// Whether argv[1] is a compiler (wrapper mode) rather than a subcommand.
pub fn is_compiler(arg: &OsStr) -> bool {
    let s = arg.to_string_lossy();
    let base = s.rsplit('/').next().unwrap_or(&s);
    s.contains('/') || base.starts_with("rustc") || base.starts_with("clippy-driver")
}

pub fn run(compiler: &OsStr, args: &[OsString]) -> i32 {
    let disabled = env::var_os("RCACHE_DISABLE").is_some_and(|v| !v.is_empty() && v != "0");
    // Anything but plain rustc (e.g. Cargo's workspace wrapper chain
    // `clippy-driver`) goes through untouched.
    if disabled || Path::new(compiler).file_name() != Some(OsStr::new("rustc")) {
        return exec(compiler, args);
    }
    let Some(inv) = args::parse(args) else {
        return exec(compiler, args);
    };
    let Ok(store) = Store::open() else {
        return exec(compiler, args);
    };
    match Job::new(&store, Path::new(compiler), inv) {
        Ok(Some(job)) => job.run(compiler, args),
        Ok(None) => exec(compiler, args),
        Err(e) => {
            store.log("error", "-", 0, 0, &format!("key: {e:#}"));
            exec(compiler, args)
        }
    }
}

fn exec(compiler: &OsStr, args: &[OsString]) -> i32 {
    let err = Command::new(compiler).args(args).exec();
    eprintln!("rcache: cannot run {}: {err}", compiler.to_string_lossy());
    1
}

struct Job<'a> {
    store: &'a Store,
    inv: Invocation,
    roots: Roots,
    out_dir: PathBuf,
    primary: String,
    /// Registry and git checkouts never change, so concurrent misses of the
    /// same unit are worth serializing; workspace crates are not locked.
    immutable: bool,
}

/// Content identity of a finished compile.
struct Identity {
    id: String,
    inputs: Vec<Input>,
    env: Vec<(String, Option<String>)>,
}

enum Saved {
    Stored(fsutil::Placements),
    Skipped(&'static str),
}

impl<'a> Job<'a> {
    fn new(store: &'a Store, compiler: &Path, inv: Invocation) -> Result<Option<Job<'a>>> {
        let cwd = env::current_dir()?;
        let out_dir = cwd.join(&inv.out_dir);
        // Cargo tags both `target/` and `target/<triple>/`; the outermost one
        // also covers host artifacts (`target/release/deps`) of cross builds.
        let Some(target) = out_dir
            .ancestors()
            .filter(|d| d.join("CACHEDIR.TAG").is_file())
            .last()
        else {
            return Ok(None);
        };
        let Some(roots) = Roots::new(&cwd, target) else {
            return Ok(None);
        };
        let pats = byte_pats(&roots);

        let mut h = blake3::Hasher::new();
        fsutil::field(&mut h, "version", KEY_VERSION);
        fsutil::field(&mut h, "rustc", &store.rustc_version(compiler)?);
        for arg in &inv.args {
            let text = match arg {
                Arg::Value(flag, v) if flag == "-C" && v.starts_with("incremental=") => continue,
                Arg::Value(flag, _) if flag == "--diagnostic-width" => continue,
                Arg::Value(flag, v) if flag == "--extern" => match v.split_once('=') {
                    Some((name, path)) => {
                        format!(
                            "--extern {name}={}",
                            extern_identity(&roots.absolute(Path::new(path)))?
                        )
                    }
                    None => format!("--extern {v}"),
                },
                Arg::Value(flag, v) => format!("{flag} {}", roots.normalize(v)),
                Arg::Flag(f) => roots.normalize(f),
                Arg::Input(i) => format!("<input> {}", roots.normalize(i)),
            };
            fsutil::field(&mut h, "arg", &text);
        }
        let mut vars: Vec<(String, String)> = Vec::new();
        for (name, value) in env::vars_os() {
            let Some(name) = name.to_str() else { continue };
            if cargo_set(name) || RUSTC_ENV.contains(&name) {
                let value = value
                    .to_str()
                    .with_context(|| format!("non-UTF-8 {name}"))?;
                vars.push((name.to_owned(), roots.normalize(value)));
            }
        }
        vars.sort();
        for (name, value) in &vars {
            fsutil::field(&mut h, "env", &format!("{name}={value}"));
        }
        // Build-script products (static libs to bundle, generated code) are
        // not all listed in dep-info; hash the whole OUT_DIR.
        if let Some(out) = env::var_os("OUT_DIR") {
            fsutil::field(
                &mut h,
                "out_dir",
                &fsutil::hash_tree(Path::new(&out), &pats)?,
            );
        }
        let primary = h.finalize().to_hex()[..32].to_owned();

        let cargo_home = env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")));
        let immutable = match (env::var_os("CARGO_MANIFEST_DIR"), cargo_home) {
            (Some(dir), Some(home)) => {
                let dir = Path::new(&dir);
                dir.starts_with(home.join("registry")) || dir.starts_with(home.join("git"))
            }
            _ => false,
        };
        Ok(Some(Job {
            store,
            inv,
            roots,
            out_dir,
            primary,
            immutable,
        }))
    }

    fn run(self, compiler: &OsStr, args: &[OsString]) -> i32 {
        let crate_name = self.inv.crate_name.clone();
        let _lock = self.immutable.then(|| self.store.lock(&self.primary).ok());
        let t0 = Instant::now();
        if let Some(m) = self.lookup() {
            match self.materialize(&m) {
                Ok(placed) => {
                    self.replay(&m);
                    self.store.touch(&m);
                    let ms = t0.elapsed().as_millis() as u64;
                    self.store
                        .log("hit", &crate_name, ms, m.compile_ms, &placed.to_string());
                    return 0;
                }
                Err(e) => self.store.log(
                    "error",
                    &crate_name,
                    0,
                    0,
                    &format!("materialize {}: {e:#}", m.id),
                ),
            }
        }

        self.break_links();
        let start = SystemTime::now();
        let t0 = Instant::now();
        let early = Mutex::new(None::<String>);
        let compiled = compile(compiler, args, |line| {
            self.on_stderr_line(line, start, &early)
        });
        let (code, stdout, stderr) = match compiled {
            Ok(r) => r,
            Err(e) => {
                eprintln!("rcache: cannot run {}: {e}", compiler.to_string_lossy());
                return 1;
            }
        };
        let compile_ms = t0.elapsed().as_millis() as u64;
        let early = early.into_inner().unwrap();
        if code != 0 {
            self.untag_outputs();
            return code;
        }
        match self.save(start, compile_ms, &stdout, &stderr, early.as_deref()) {
            Ok(Saved::Stored(placed)) => {
                self.store
                    .log("miss", &crate_name, compile_ms, 0, &placed.to_string())
            }
            Ok(Saved::Skipped(why)) => {
                self.untag_outputs();
                self.store.log("nostore", &crate_name, compile_ms, 0, why);
            }
            Err(e) => {
                self.untag_outputs();
                self.store.log(
                    "error",
                    &crate_name,
                    compile_ms,
                    0,
                    &format!("store: {e:#}"),
                );
            }
        }
        code
    }

    fn lookup(&self) -> Option<Manifest> {
        let mut seen: HashMap<String, Option<(u64, String)>> = HashMap::new();
        self.store
            .candidates(&self.primary)
            .into_iter()
            .take(MAX_CANDIDATES)
            .filter_map(|id| self.store.load(&id).ok())
            .find(|m| self.matches(m, &mut seen))
    }

    fn matches(&self, m: &Manifest, seen: &mut HashMap<String, Option<(u64, String)>>) -> bool {
        let env_ok = m.env.iter().all(|(name, want)| {
            let have = env::var_os(name).map(|v| v.to_str().map(|v| self.env_value(v)));
            match (want, have) {
                (None, None) => true,
                (Some(want), Some(Some(have))) => *want == have,
                _ => false,
            }
        });
        env_ok
            && m.inputs.iter().all(|input| {
                let have = seen
                    .entry(input.path.clone())
                    .or_insert_with(|| self.current_input(&input.path).ok());
                have.as_ref()
                    .is_some_and(|(size, hash)| *size == input.size && *hash == input.hash)
            })
    }

    /// Size and hash of an input in this workspace. Files under the target
    /// directory (build-script output) are hashed with roots normalized.
    fn current_input(&self, name: &str) -> io::Result<(u64, String)> {
        let path = self.roots.resolve(name);
        let size = fs::metadata(&path)?.len();
        let hash = if name.starts_with("$T/") {
            fsutil::hash_file_normalized(&path, &byte_pats(&self.roots))?
        } else {
            fsutil::hash_file(&path)?
        };
        Ok((size, hash))
    }

    /// Identity of the compile from its dep-info. `None` when an input changed
    /// after the compile started: the outputs may not match what we would hash.
    fn identity(&self, start: SystemTime) -> Result<Option<Identity>> {
        let text = fs::read_to_string(self.out_dir.join(format!("{}.d", self.inv.stem())))?;
        let info = depinfo::parse(&text);
        let mut inputs = Vec::with_capacity(info.files.len());
        for file in &info.files {
            let path = self.roots.absolute(Path::new(file));
            let meta = fs::metadata(&path).with_context(|| format!("input {file}"))?;
            if meta.modified()? > start {
                return Ok(None);
            }
            let name = self
                .roots
                .name_of(&path)
                .with_context(|| format!("non-UTF-8 input {}", path.display()))?;
            let (size, hash) = self.current_input(&name)?;
            inputs.push(Input {
                path: name,
                size,
                hash,
            });
        }
        let mut h = blake3::Hasher::new();
        fsutil::field(&mut h, "primary", &self.primary);
        for i in &inputs {
            fsutil::field(&mut h, "input", &i.path);
            fsutil::field(&mut h, "hash", &i.hash);
        }
        let env: Vec<_> = info
            .env
            .into_iter()
            .map(|(name, value)| {
                // dep-info values are escaped already; only normalize.
                let value = value.map(|v| self.roots.normalize_target(&v));
                (name, value)
            })
            .collect();
        for (name, value) in &env {
            fsutil::field(&mut h, "env", name);
            fsutil::field(&mut h, "value", value.as_deref().unwrap_or("\0unset"));
        }
        Ok(Some(Identity {
            id: h.finalize().to_hex()[..32].to_owned(),
            inputs,
            env,
        }))
    }

    /// An environment value as recorded in a manifest: escaped like rustc's
    /// dep-info, target directory normalized.
    fn env_value(&self, raw: &str) -> String {
        self.roots.normalize_target(&depinfo::escape_env(raw))
    }

    /// Tags the `.rmeta` as soon as rustc announces it, before Cargo starts
    /// pipelined dependents, so their keys use the identity, not the bytes.
    fn on_stderr_line(&self, line: &[u8], start: SystemTime, early: &Mutex<Option<String>>) {
        if !(line.starts_with(b"{") && contains(line, br#""emit":"metadata""#)) {
            return;
        }
        let Some(path) = artifact_path(line) else {
            return;
        };
        let mut early = early.lock().unwrap();
        if early.is_none()
            && let Ok(Some(ident)) = self.identity(start)
            && fsutil::tag(&path, &ident.id).is_ok()
        {
            *early = Some(ident.id);
        }
    }

    fn save(
        &self,
        start: SystemTime,
        compile_ms: u64,
        stdout: &[u8],
        stderr: &[u8],
        early: Option<&str>,
    ) -> Result<Saved> {
        let Some(ident) = self.identity(start)? else {
            return Ok(Saved::Skipped("input modified during compile"));
        };
        if early.is_some_and(|e| e != ident.id) {
            return Ok(Saved::Skipped("inputs changed between metadata and link"));
        }
        let outputs = self.outputs_since(start)?;
        let dep_info = format!("{}.d", self.inv.stem());
        if !outputs.contains(&dep_info) {
            return Ok(Saved::Skipped("dep-info not rewritten"));
        }
        for line in stderr.split(|&b| b == b'\n') {
            if line.starts_with(b"{")
                && contains(line, br#""artifact""#)
                && let Some(path) = artifact_path(line)
            {
                let name = path.file_name().and_then(OsStr::to_str);
                if !name.is_some_and(|n| outputs.iter().any(|o| o == n)) {
                    return Ok(Saved::Skipped("artifact outside the output set"));
                }
            }
        }
        let manifest = Manifest {
            version: VERSION,
            id: ident.id,
            primary: self.primary.clone(),
            crate_name: self.inv.crate_name.clone(),
            cwd: self.roots.cwd.clone(),
            target_root: self.roots.target.clone(),
            inputs: ident.inputs,
            env: ident.env,
            outputs,
            dep_info,
            compile_ms,
        };
        // Tag first: a stored output may come back hard-linked and read-only.
        for name in &manifest.outputs {
            let path = self.out_dir.join(name);
            if path.is_file() {
                fsutil::tag(&path, &manifest.id)?;
            }
        }
        let placed = self
            .store
            .insert(&manifest, &self.out_dir, stdout, stderr, self.immutable)?;
        Ok(Saved::Stored(placed))
    }

    /// This unit's files in `--out-dir` written by the compile that just ran.
    fn outputs_since(&self, start: SystemTime) -> Result<Vec<String>> {
        let stem = self.inv.stem();
        let lib = format!("lib{stem}");
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.out_dir)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !(is_unit_file(&name, &stem) || is_unit_file(&name, &lib)) {
                continue;
            }
            if entry.metadata()?.modified()? >= start {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// This unit's existing files in `--out-dir`.
    fn unit_files(&self) -> Vec<PathBuf> {
        let stem = self.inv.stem();
        let lib = format!("lib{stem}");
        fs::read_dir(&self.out_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                is_unit_file(&name, &stem) || is_unit_file(&name, &lib)
            })
            .map(|e| e.path())
            .collect()
    }

    /// Unlinks this unit's outputs that are shared with the store, so rustc
    /// and the linker only ever write fresh files, never through a link.
    fn break_links(&self) {
        for path in self.unit_files() {
            if fs::symlink_metadata(&path).is_ok_and(|m| fsutil::is_shared(&m)) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    fn untag_outputs(&self) {
        for path in self.unit_files() {
            // A shared file's tag belongs to its store entry.
            if fs::symlink_metadata(&path).is_ok_and(|m| !fsutil::is_shared(&m)) {
                fsutil::untag(&path);
            }
        }
    }

    /// Places the entry's outputs into `--out-dir`: shareable ones hard-linked
    /// when the filesystem cannot clone (the same file as the store's, mtime
    /// untouched), the rest as private files stamped with the current time
    /// like a fresh compile; dep-info rewritten to this workspace.
    fn materialize(&self, m: &Manifest) -> Result<fsutil::Placements> {
        let src = self.store.entry_dir(&m.id).join("out");
        let now = SystemTime::now();
        let share = self.share_set(m, &src);
        let mut placements = fsutil::Placements::default();
        for name in &m.outputs {
            let dst = self.out_dir.join(name);
            let tmp = self
                .out_dir
                .join(format!(".rcache-{}-{name}", std::process::id()));
            let placed = (|| -> Result<fsutil::Placed> {
                let placed = if *name == m.dep_info {
                    let text = fs::read_to_string(src.join(name))?;
                    fs::write(&tmp, self.rewrite_dep_info(m, &text))?;
                    fsutil::Placed::Copied
                } else {
                    fsutil::place(&src.join(name), &tmp, share.contains(name.as_str()))?
                };
                let private = placed != fsutil::Placed::Linked && tmp.is_file();
                if private {
                    fsutil::set_mtime(&tmp, now)?;
                }
                if dst.is_dir() {
                    fsutil::remove_tree(&dst)?;
                }
                fs::rename(&tmp, &dst)?;
                // Renaming a link onto another link of the same file is a
                // no-op that leaves `tmp` behind (POSIX).
                if fs::symlink_metadata(&tmp).is_ok() {
                    fs::remove_file(&tmp)?;
                }
                if private {
                    fsutil::tag(&dst, &m.id)?;
                }
                Ok(placed)
            })();
            match placed {
                Ok(placed) => placements.add(placed),
                Err(e) => {
                    let _ = fsutil::remove_tree(&tmp);
                    return Err(e.context(format!("placing {name}")));
                }
            }
        }
        Ok(placements)
    }

    /// Outputs to hard-link. Cargo marks a unit stale when one of its
    /// dependencies' outputs is newer than the unit's newest output; a linked
    /// output keeps the store's mtime (when it was first compiled), which
    /// can be older than a dependency produced in this checkout — typically a
    /// build script that just ran here. Then the `.rmeta` (else every output
    /// Cargo tracks) stays private, stamped now.
    fn share_set<'m>(&self, m: &'m Manifest, src: &Path) -> HashSet<&'m str> {
        let mut share: HashSet<&str> = m
            .outputs
            .iter()
            .filter(|o| store::shareable(o, self.immutable))
            .map(String::as_str)
            .collect();
        let tracked: Vec<&str> = m
            .outputs
            .iter()
            .map(String::as_str)
            .filter(|o| !o.ends_with(".d") && !o.ends_with(".o"))
            .collect();
        if tracked.is_empty() || !tracked.iter().all(|o| share.contains(o)) {
            return share;
        }
        let newest = tracked
            .iter()
            .filter_map(|o| fs::metadata(src.join(o)).and_then(|m| m.modified()).ok())
            .max()
            .unwrap_or(UNIX_EPOCH);
        if self.dependencies_mtime() > newest {
            match tracked.iter().find(|o| o.ends_with(".rmeta")) {
                Some(rmeta) => {
                    share.remove(rmeta);
                }
                None => share.retain(|o| !tracked.contains(o)),
            }
        }
        share
    }

    /// Newest output, in this checkout, of anything Cargo checks this unit
    /// against: every `--extern` crate (all files of its unit) and the build
    /// script run. Unreadable means "now", which keeps a private output.
    fn dependencies_mtime(&self) -> SystemTime {
        let mut newest = UNIX_EPOCH;
        let mut see = |path: &Path| match fs::metadata(path).and_then(|m| m.modified()) {
            Ok(t) => newest = newest.max(t),
            Err(_) => newest = SystemTime::now(),
        };
        for arg in &self.inv.args {
            let Arg::Value(flag, v) = arg else { continue };
            let Some((_, path)) = v.split_once('=').filter(|_| flag == "--extern") else {
                continue;
            };
            let path = self.roots.absolute(Path::new(path));
            let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
                continue;
            };
            let stem = stem.to_string_lossy();
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                if is_unit_file(&entry.file_name().to_string_lossy(), &stem) {
                    see(&entry.path());
                }
            }
        }
        if let Some(out) = env::var_os("OUT_DIR") {
            let out = PathBuf::from(out);
            see(&out);
            if let Some(run) = out.parent() {
                see(&run.join("output"));
            }
        }
        newest
    }

    fn rewrite_dep_info(&self, m: &Manifest, text: &str) -> String {
        let escaped = |s: &str| s.replace(' ', "\\ ");
        let (old_t, old_c) = (escaped(&m.target_root), escaped(&m.cwd));
        let (new_t, new_c) = (escaped(&self.roots.target), escaped(&self.roots.cwd));
        replace_roots(text, &ordered(&[(&old_t, &new_t), (&old_c, &new_c)]))
    }

    fn replay(&self, m: &Manifest) {
        let dir = self.store.entry_dir(&m.id);
        let pats: Vec<(&[u8], &[u8])> = ordered(&[
            (&m.target_root, &self.roots.target),
            (&m.cwd, &self.roots.cwd),
        ])
        .into_iter()
        .map(|(a, b)| (a.as_bytes(), b.as_bytes()))
        .collect();
        if let Ok(out) = fs::read(dir.join("stdout")) {
            let _ = io::stdout().write_all(&replace_roots_bytes(&out, &pats));
        }
        if let Ok(err) = fs::read(dir.join("stderr")) {
            let _ = io::stderr().write_all(&replace_roots_bytes(&err, &pats));
        }
    }
}

/// Longest source prefix first, so nested roots resolve to the inner one.
fn ordered<'s>(pats: &[(&'s str, &'s str)]) -> Vec<(&'s str, &'s str)> {
    let mut v = pats.to_vec();
    v.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    v
}

fn byte_pats(roots: &Roots) -> Vec<(&[u8], &[u8])> {
    ordered(&[(&roots.target, "$T"), (&roots.cwd, "$CWD")])
        .into_iter()
        .map(|(a, b)| (a.as_bytes(), b.as_bytes()))
        .collect()
}

/// Environment Cargo sets for the crate being compiled. Proc macros may read
/// these without rustc tracking it, so they are part of the key; arbitrary
/// shell variables are not (only `env!` reads, tracked via dep-info).
fn cargo_set(name: &str) -> bool {
    name == "CARGO"
        || name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_MANIFEST_")
        || name.starts_with("CARGO_BIN_")
        || matches!(
            name,
            "CARGO_CRATE_NAME"
                | "CARGO_PRIMARY_PACKAGE"
                | "CARGO_TARGET_TMPDIR"
                | "CARGO_RUSTC_CURRENT_DIR"
        )
}

/// `e:<entry id>` for a tagged cache output, else `c:<content hash>`.
fn extern_identity(path: &Path) -> Result<String> {
    let meta = fs::metadata(path).with_context(|| format!("extern {}", path.display()))?;
    if let Some(id) = fsutil::read_tag(path, &meta) {
        return Ok(format!("e:{id}"));
    }
    Ok(format!("c:{}", fsutil::hash_file(path)?))
}

fn is_unit_file(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn artifact_path(line: &[u8]) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    Some(PathBuf::from(v.get("artifact")?.as_str()?))
}

/// Runs rustc, forwarding stdout/stderr line by line as they arrive (Cargo
/// relies on timely artifact notifications for pipelining) and capturing them.
fn compile(
    compiler: &OsStr,
    args: &[OsString],
    on_stderr_line: impl Fn(&[u8]) + Sync,
) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    let mut child = Command::new(compiler)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let child_out = child.stdout.take().expect("piped");
    let child_err = child.stderr.take().expect("piped");
    let (stdout, stderr) = std::thread::scope(|s| {
        let out = s.spawn(|| tee(child_out, io::stdout(), |_| {}));
        let err = s.spawn(|| tee(child_err, io::stderr(), &on_stderr_line));
        (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        )
    });
    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok((code, stdout, stderr))
}

fn tee(src: impl Read, mut dst: impl Write, on_line: impl Fn(&[u8])) -> Vec<u8> {
    let mut reader = BufReader::new(src);
    let mut all = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                on_line(&line);
                let _ = dst.write_all(&line);
                let _ = dst.flush();
                all.extend_from_slice(&line);
            }
        }
    }
    all
}
