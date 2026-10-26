//! Hashing, placing files (clone, hard link or copy) and identity tags on
//! build outputs.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Extended attribute naming the cache entry a build output came from.
const TAG_ATTR: &str = "user.rcache.id";

pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_mmap(path)?;
    Ok(h.finalize().to_hex().to_string())
}

/// Hash of file contents after rewriting workspace roots (`pats`), so
/// generated files that embed their own absolute path hash the same in every
/// workspace.
pub fn hash_file_normalized(path: &Path, pats: &[(&[u8], &[u8])]) -> io::Result<String> {
    let data = fs::read(path)?;
    let data = crate::paths::replace_roots_bytes(&data, pats);
    Ok(blake3::hash(&data).to_hex().to_string())
}

/// Hash of a directory tree: names, symlink targets and normalized contents.
pub fn hash_tree(root: &Path, pats: &[(&[u8], &[u8])]) -> io::Result<String> {
    let mut h = blake3::Hasher::new();
    if root.is_dir() {
        hash_dir(root, "", pats, &mut h)?;
    }
    Ok(h.finalize().to_hex().to_string())
}

fn hash_dir(
    dir: &Path,
    rel: &str,
    pats: &[(&[u8], &[u8])],
    h: &mut blake3::Hasher,
) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = format!("{rel}/{}", entry.file_name().to_string_lossy());
        let ty = entry.file_type()?;
        let (kind, value) = if ty.is_symlink() {
            (
                "L",
                fs::read_link(entry.path())?.to_string_lossy().into_owned(),
            )
        } else if ty.is_dir() {
            field(h, "D", &name);
            hash_dir(&entry.path(), &name, pats, h)?;
            continue;
        } else {
            ("F", hash_file_normalized(&entry.path(), pats)?)
        };
        field(h, kind, &name);
        field(h, kind, &value);
    }
    Ok(())
}

/// Length-prefixed hash field, so concatenations cannot collide.
pub fn field(h: &mut blake3::Hasher, name: &str, value: &str) {
    h.update(name.as_bytes());
    h.update(&[0]);
    h.update(&(value.len() as u64).to_le_bytes());
    h.update(value.as_bytes());
}

/// How a file ended up at its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// Copy-on-write clone: an independent file that shares data blocks.
    Cloned,
    /// Hard link: the *same* file. It must stay read-only and its mtime must
    /// never be touched, since every link (store, other checkouts) sees it.
    Linked,
    /// Real copy.
    Copied,
}

/// Per-compile tally of placements, for the log.
#[derive(Debug, Default)]
pub struct Placements {
    pub cloned: usize,
    pub linked: usize,
    pub copied: usize,
}

impl Placements {
    pub fn add(&mut self, placed: Placed) {
        match placed {
            Placed::Cloned => self.cloned += 1,
            Placed::Linked => self.linked += 1,
            Placed::Copied => self.copied += 1,
        }
    }
}

impl std::fmt::Display for Placements {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "clone={} link={} copy={}",
            self.cloned, self.linked, self.copied
        )
    }
}

/// `RCACHE_PLACE`: `link` skips cloning (exercises the hard-link path on a
/// copy-on-write filesystem), `copy` uses neither.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Link,
    Copy,
}

fn mode() -> Mode {
    match std::env::var("RCACHE_PLACE").as_deref() {
        Ok("link") => Mode::Link,
        Ok("copy") => Mode::Copy,
        _ => Mode::Auto,
    }
}

/// Places `src` at `dst` (which must not exist), sharing storage when
/// possible: a copy-on-write clone (APFS, btrfs, XFS), else — if `share` — a
/// hard link (ext4 and anything else on the same filesystem), else a copy.
/// Files that are not linked come out owner-writable even if `src` is
/// read-only (a shared store file).
pub fn place(src: &Path, dst: &Path, share: bool) -> io::Result<Placed> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            place(&entry.path(), &dst.join(entry.file_name()), false)?;
        }
        return Ok(Placed::Copied);
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        return Ok(Placed::Copied);
    }
    let mode = mode();
    let placed = if mode == Mode::Auto && clone_file(src, dst)? {
        Placed::Cloned
    } else {
        if share && mode != Mode::Copy {
            match fs::hard_link(src, dst) {
                Ok(()) => return Ok(Placed::Linked),
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
                Err(e) => return Err(e),
            }
        }
        fs::copy(src, dst)?;
        Placed::Copied
    };
    let mode_bits = meta.permissions().mode();
    if mode_bits & 0o200 == 0 {
        fs::set_permissions(dst, fs::Permissions::from_mode(mode_bits | 0o200))?;
    }
    Ok(placed)
}

#[cfg(target_vendor = "apple")]
fn clone_file(src: &Path, dst: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let (s, d) = (
        CString::new(src.as_os_str().as_bytes())?,
        CString::new(dst.as_os_str().as_bytes())?,
    );
    // SAFETY: both arguments are valid NUL-terminated paths.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => Ok(false),
        _ => Err(err),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn clone_file(src: &Path, dst: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    /// `_IOW(0x94, 9, int)`: reflink the whole file (btrfs, XFS, bcachefs…).
    const FICLONE: u32 = 0x4004_9409;
    let from = fs::File::open(src)?;
    let to = fs::File::options().write(true).create_new(true).open(dst)?;
    // SAFETY: both descriptors are open for the duration of the call.
    if unsafe { libc::ioctl(to.as_raw_fd(), FICLONE as _, from.as_raw_fd()) } == 0 {
        fs::set_permissions(dst, from.metadata()?.permissions())?;
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    drop(to);
    fs::remove_file(dst)?;
    match err.raw_os_error() {
        // ext4 and friends: no reflink support.
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY | libc::EPERM) => {
            Ok(false)
        }
        _ => Err(err),
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn clone_file(_: &Path, _: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Clears every write bit: a hard-linked store file must never be rewritten
/// in place, by rcache or anyone else.
pub fn make_readonly(path: &Path) -> io::Result<()> {
    let mode = fs::metadata(path)?.permissions().mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o222))
}

/// Whether `meta` is a file shared through hard links (or a read-only
/// leftover of one) that must be unlinked before anything writes that path.
pub fn is_shared(meta: &fs::Metadata) -> bool {
    meta.is_file() && (meta.nlink() > 1 || meta.permissions().mode() & 0o222 == 0)
}

pub fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| tree_size(&e.path()))
        .sum()
}

pub fn mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// Sets the mtime through a read-only descriptor (the owner may set times on
/// files it cannot write).
pub fn set_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    fs::File::open(path)?.set_modified(time)
}

/// Records that `path` (as it is right now) is the output of cache entry `id`.
/// The tag carries size and mtime so a later rewrite of the file voids it.
pub fn tag(path: &Path, id: &str) -> io::Result<()> {
    let meta = fs::metadata(path)?;
    let value = format!("{id}:{}:{}", meta.len(), mtime_ns(&meta));
    xattr::set(path, TAG_ATTR, value.as_bytes())
}

pub fn untag(path: &Path) {
    let _ = xattr::remove(path, TAG_ATTR);
}

/// Entry id from a still-valid tag.
pub fn read_tag(path: &Path, meta: &fs::Metadata) -> Option<String> {
    let value = xattr::get(path, TAG_ATTR).ok()??;
    let value = String::from_utf8(value).ok()?;
    let mut parts = value.split(':');
    let (id, len, mtime) = (parts.next()?, parts.next()?, parts.next()?);
    (len.parse::<u64>().ok()? == meta.len() && mtime.parse::<u128>().ok()? == mtime_ns(meta))
        .then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_void_after_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("libx.rlib");
        fs::write(&f, b"one").unwrap();
        tag(&f, "abc").unwrap();
        assert_eq!(
            read_tag(&f, &fs::metadata(&f).unwrap()).as_deref(),
            Some("abc")
        );
        fs::write(&f, b"two!").unwrap();
        assert_eq!(read_tag(&f, &fs::metadata(&f).unwrap()), None);
    }

    #[test]
    fn normalized_tree_hash_ignores_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        for (root, body) in [(&a, "/w/a/target/out/x.o"), (&b, "/w/b/t/out/x.o")] {
            fs::create_dir_all(root.join("sub")).unwrap();
            fs::write(root.join("sub/obj.o"), body).unwrap();
        }
        let ha = hash_tree(&a, &[(b"/w/a/target", b"$T")]).unwrap();
        let hb = hash_tree(&b, &[(b"/w/b/t", b"$T")]).unwrap();
        assert_eq!(ha, hb);
        fs::write(b.join("sub/obj.o"), "/w/b/t/out/y.o").unwrap();
        assert_ne!(ha, hash_tree(&b, &[(b"/w/b/t", b"$T")]).unwrap());
    }
}
