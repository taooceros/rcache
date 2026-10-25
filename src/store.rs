//! On-disk cache layout:
//!
//! ```text
//! entries/<id>/manifest.json   inputs, outputs, origin roots (mtime = last use)
//! entries/<id>/{stdout,stderr} rustc output to replay
//! entries/<id>/out/…           build outputs (copy-on-write clones)
//! index/<primary>/<id>         candidates for a workspace-independent key (mtime = last use)
//! locks/<primary>              serializes misses of immutable (registry/git) crates
//! rustc/<id>                   cached `rustc -vV` per compiler binary
//! log                          one line per wrapped compile
//! ```

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil;

pub const VERSION: u32 = 1;
const DEFAULT_MAX_SIZE: u64 = 40 << 30;
const GC_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const LOG_ROTATE_SIZE: u64 = 16 << 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// Workspace-independent name (see `paths::Roots::name_of`).
    pub path: String,
    pub size: u64,
    pub hash: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub primary: String,
    pub crate_name: String,
    /// Roots of the workspace that produced the entry, for rewriting
    /// dep-info and diagnostics into the consuming workspace.
    pub cwd: String,
    pub target_root: String,
    pub inputs: Vec<Input>,
    pub env: Vec<(String, Option<String>)>,
    pub outputs: Vec<String>,
    pub dep_info: String,
    pub compile_ms: u64,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open() -> Result<Store> {
        let root = default_root()?;
        for dir in ["entries", "index", "locks", "rustc", "tmp"] {
            fs::create_dir_all(root.join(dir))
                .with_context(|| format!("creating {}", root.join(dir).display()))?;
        }
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn entry_dir(&self, id: &str) -> PathBuf {
        self.root.join("entries").join(id)
    }

    /// Entry ids recorded for `primary`, most recently used first.
    pub fn candidates(&self, primary: &str) -> Vec<String> {
        let Ok(dir) = fs::read_dir(self.root.join("index").join(primary)) else {
            return Vec::new();
        };
        let mut ids: Vec<_> = dir
            .flatten()
            .filter_map(|e| {
                let used = e.metadata().ok()?.modified().ok()?;
                Some((used, e.file_name().into_string().ok()?))
            })
            .collect();
        ids.sort_by(|a, b| b.cmp(a));
        ids.into_iter().map(|(_, id)| id).collect()
    }

    pub fn load(&self, id: &str) -> Result<Manifest> {
        let text = fs::read(self.entry_dir(id).join("manifest.json"))?;
        let manifest: Manifest = serde_json::from_slice(&text)?;
        anyhow::ensure!(manifest.version == VERSION, "manifest version");
        Ok(manifest)
    }

    pub fn touch(&self, m: &Manifest) {
        let now = SystemTime::now();
        let _ = fsutil::set_mtime(&self.index_file(&m.primary, &m.id), now);
        let _ = fsutil::set_mtime(&self.entry_dir(&m.id).join("manifest.json"), now);
    }

    /// Stores clones of `outputs` from `out_dir`. Concurrent inserts of the
    /// same id are fine: the first rename wins and the rest are dropped.
    pub fn insert(&self, m: &Manifest, out_dir: &Path, stdout: &[u8], stderr: &[u8]) -> Result<()> {
        let dest = self.entry_dir(&m.id);
        if !dest.exists() {
            let tmp = self.root.join("tmp").join(format!(
                "{}-{}-{}",
                m.id,
                std::process::id(),
                unix_ms()
            ));
            let staged = (|| -> Result<()> {
                fs::create_dir_all(tmp.join("out"))?;
                for name in &m.outputs {
                    fsutil::copy_tree(&out_dir.join(name), &tmp.join("out").join(name))
                        .with_context(|| format!("storing {name}"))?;
                }
                fs::write(tmp.join("stdout"), stdout)?;
                fs::write(tmp.join("stderr"), stderr)?;
                fs::write(tmp.join("manifest.json"), serde_json::to_vec(m)?)?;
                Ok(())
            })();
            let renamed = staged.and_then(|()| fs::rename(&tmp, &dest).map_err(Into::into));
            if let Err(e) = renamed {
                let _ = fsutil::remove_tree(&tmp);
                if !dest.exists() {
                    return Err(e);
                }
            }
        }
        fs::create_dir_all(self.root.join("index").join(&m.primary))?;
        File::create(self.index_file(&m.primary, &m.id))?;
        self.maybe_gc();
        Ok(())
    }

    fn index_file(&self, primary: &str, id: &str) -> PathBuf {
        self.root.join("index").join(primary).join(id)
    }

    /// Blocks until no other process is compiling the same unit.
    pub fn lock(&self, primary: &str) -> Result<File> {
        let file = File::create(self.root.join("locks").join(primary))?;
        file.lock()?;
        Ok(file)
    }

    /// `rustc -vV` for the compiler binary, cached by its path and stat.
    pub fn rustc_version(&self, compiler: &Path) -> Result<String> {
        let cache = fs::metadata(compiler).ok().map(|meta| {
            let mut h = blake3::Hasher::new();
            fsutil::field(&mut h, "path", &compiler.to_string_lossy());
            fsutil::field(&mut h, "len", &meta.len().to_string());
            fsutil::field(&mut h, "mtime", &fsutil::mtime_ns(&meta).to_string());
            self.root.join("rustc").join(&h.finalize().to_hex()[..32])
        });
        if let Some(text) = cache.as_ref().and_then(|p| fs::read_to_string(p).ok()) {
            return Ok(text);
        }
        let out = Command::new(compiler).arg("-vV").output()?;
        anyhow::ensure!(out.status.success(), "rustc -vV failed");
        let text = String::from_utf8(out.stdout)?;
        if let Some(cache) = cache {
            let tmp = cache.with_extension(format!("tmp{}", std::process::id()));
            if fs::write(&tmp, &text).is_ok() {
                let _ = fs::rename(&tmp, &cache);
            }
        }
        Ok(text)
    }

    pub fn log(&self, kind: &str, crate_name: &str, ms: u64, saved_ms: u64, detail: &str) {
        let detail = detail.replace(['\t', '\n'], " ");
        let line = format!(
            "{}\t{kind}\t{crate_name}\t{ms}\t{saved_ms}\t{detail}\n",
            unix_ms()
        );
        if let Ok(mut f) = File::options()
            .create(true)
            .append(true)
            .open(self.log_path())
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn log_path(&self) -> PathBuf {
        self.root.join("log")
    }

    fn maybe_gc(&self) {
        let stamp = self.root.join("gc.stamp");
        let due = fs::metadata(&stamp)
            .and_then(|m| m.modified())
            .map_or(true, |t| t.elapsed().unwrap_or_default() > GC_INTERVAL);
        if !due {
            return;
        }
        let Ok(lock) = File::create(self.root.join("locks").join("gc")) else {
            return;
        };
        if lock.try_lock().is_ok() {
            let _ = File::create(&stamp);
            let _ = self.gc(max_size());
        }
    }

    /// Evicts least recently used entries until the logical size fits.
    /// Logical size over-counts disk use: entries share blocks with the
    /// target directories they were cloned from or into.
    pub fn gc(&self, max_bytes: u64) -> Result<GcReport> {
        let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(self.root.join("entries"))?.flatten() {
            let path = entry.path();
            let used = fs::metadata(path.join("manifest.json"))
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            entries.push((used, fsutil::tree_size(&path), path));
        }
        entries.sort();
        let mut report = GcReport {
            entries: entries.len(),
            bytes: entries.iter().map(|e| e.1).sum(),
            ..GcReport::default()
        };
        for (_, size, path) in &entries {
            if report.bytes <= max_bytes {
                break;
            }
            let id = path.file_name().unwrap().to_string_lossy().into_owned();
            let primary = self.load(&id).ok().map(|m| m.primary);
            // Rename first so a concurrent reader sees the entry vanish atomically.
            let doomed = self
                .root
                .join("tmp")
                .join(format!("gc-{id}-{}", std::process::id()));
            if fs::rename(path, &doomed).is_ok() {
                if let Some(primary) = primary {
                    let _ = fs::remove_file(self.index_file(&primary, &id));
                }
                let _ = fsutil::remove_tree(&doomed);
                report.bytes -= size;
                report.entries -= 1;
                report.evicted += 1;
            }
        }
        // Leftovers of crashed inserts.
        for entry in fs::read_dir(self.root.join("tmp"))?.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(86400));
            if old {
                let _ = fsutil::remove_tree(&entry.path());
            }
        }
        if fs::metadata(self.log_path()).is_ok_and(|m| m.len() > LOG_ROTATE_SIZE) {
            let _ = fs::rename(self.log_path(), self.root.join("log.old"));
        }
        Ok(report)
    }

    /// Removes every entry and index.
    pub fn clear(&self) -> Result<()> {
        for dir in ["entries", "index", "rustc"] {
            let doomed = self
                .root
                .join("tmp")
                .join(format!("clear-{dir}-{}", unix_ms()));
            fs::rename(self.root.join(dir), &doomed)?;
            fs::create_dir(self.root.join(dir))?;
            fsutil::remove_tree(&doomed)?;
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub entries: usize,
    pub bytes: u64,
    pub evicted: usize,
}

fn default_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RCACHE_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Caches/rcache")
    } else if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        PathBuf::from(xdg).join("rcache")
    } else {
        home.join(".cache/rcache")
    })
}

pub fn max_size() -> u64 {
    std::env::var("RCACHE_MAX_SIZE")
        .ok()
        .and_then(|s| parse_size(&s))
        .unwrap_or(DEFAULT_MAX_SIZE)
}

/// Parses `123`, `500M`, `40G`, `40GiB`, `1.5T` (binary units).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().ok()?;
    let shift = match unit
        .trim()
        .to_ascii_uppercase()
        .trim_end_matches("IB")
        .trim_end_matches('B')
    {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return None,
    };
    Some((num * (1u64 << shift) as f64) as u64)
}

pub fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("123"), Some(123));
        assert_eq!(parse_size("500M"), Some(500 << 20));
        assert_eq!(parse_size("40G"), Some(40 << 30));
        assert_eq!(parse_size("40GiB"), Some(40 << 30));
        assert_eq!(parse_size("2gb"), Some(2 << 30));
        assert_eq!(parse_size("1.5K"), Some(1536));
        assert_eq!(parse_size("12X"), None);
    }
}
