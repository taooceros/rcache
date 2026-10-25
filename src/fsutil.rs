//! Hashing, copy-on-write copies and identity tags on build outputs.

use std::fs;
use std::io;
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

/// Copies a file or directory tree. `std::fs::copy` clones on APFS
/// (`fclonefileat`) and reflinks on Linux when the filesystem supports it,
/// so a copy costs no data blocks until one side is rewritten.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else {
        fs::copy(src, dst).map(drop)
    }
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

pub fn set_mtime(path: &Path, time: SystemTime) -> io::Result<()> {
    fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(time)
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
