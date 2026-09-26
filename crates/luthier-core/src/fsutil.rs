//! Filesystem primitives shared by the state store and the installer.

use crate::error::{Error, Result};
use luthier_manifest::Sha256Hash;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Creates `path` and its parents if they do not already exist.
pub fn ensure_dir(path: &Path) -> Result<()> {
  if path.is_dir() {
    return Ok(());
  }
  fs::create_dir_all(path).map_err(|e| Error::io("create directory", path, e))
}

/// Replaces `path` with `bytes` atomically.
///
/// Writes a sibling temporary file, flushes it to disk, then renames over the
/// target. `rename` within a directory is atomic on every filesystem we
/// support, so a reader either sees the whole old file or the whole new one —
/// never a truncated one. The parent directory is synced afterwards so the
/// rename itself survives a crash.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
  let parent = path.parent().ok_or_else(|| {
    Error::io(
      "resolve parent of",
      path,
      std::io::Error::other("path has no parent directory"),
    )
  })?;
  ensure_dir(parent)?;

  let temporary = parent.join(format!(
    ".{}.tmp",
    path.file_name().unwrap_or_default().to_string_lossy()
  ));

  {
    let mut file =
      File::create(&temporary).map_err(|e| Error::io("create temporary file", &temporary, e))?;
    file
      .write_all(bytes)
      .map_err(|e| Error::io("write", &temporary, e))?;
    file
      .sync_all()
      .map_err(|e| Error::io("flush", &temporary, e))?;
  }

  fs::rename(&temporary, path).map_err(|e| Error::io("replace", path, e))?;

  if let Ok(dir) = File::open(parent) {
    // Best effort: not every filesystem supports syncing a directory.
    let _ = dir.sync_all();
  }
  Ok(())
}

/// Computes the SHA-256 of a file without holding it all in memory.
pub fn hash_file(path: &Path) -> Result<Sha256Hash> {
  let mut file = File::open(path).map_err(|e| Error::io("open", path, e))?;
  let mut hasher = Sha256::new();
  let mut buffer = vec![0u8; 128 * 1024];
  loop {
    let n = file
      .read(&mut buffer)
      .map_err(|e| Error::io("read", path, e))?;
    if n == 0 {
      break;
    }
    hasher.update(&buffer[..n]);
  }
  Ok(Sha256Hash::from_bytes(hasher.finalize().into()))
}

/// The permission bits of `path`, on platforms that have them.
pub fn file_mode(path: &Path) -> Result<u32> {
  let meta = fs::symlink_metadata(path).map_err(|e| Error::io("inspect", path, e))?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    Ok(meta.permissions().mode() & 0o7777)
  }
  #[cfg(not(unix))]
  {
    let _ = meta;
    Ok(0o644)
  }
}

/// Recursively copies `source` to `destination`, refusing to follow links.
///
/// Used to stage a bundle next to its final location. Symlinks are refused
/// rather than recreated: extraction already rejects them, so encountering one
/// here means something changed underneath us.
pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
  let meta = fs::symlink_metadata(source).map_err(|e| Error::io("inspect", source, e))?;

  if meta.file_type().is_symlink() {
    return Err(Error::io(
      "copy",
      source,
      std::io::Error::other("refusing to copy a symbolic link"),
    ));
  }

  if meta.is_file() {
    if let Some(parent) = destination.parent() {
      ensure_dir(parent)?;
    }
    fs::copy(source, destination).map_err(|e| Error::io("copy", source, e))?;
    return Ok(());
  }

  if !meta.is_dir() {
    return Err(Error::io(
      "copy",
      source,
      std::io::Error::other("not a regular file or directory"),
    ));
  }

  ensure_dir(destination)?;
  let entries = fs::read_dir(source).map_err(|e| Error::io("list", source, e))?;
  for entry in entries {
    let entry = entry.map_err(|e| Error::io("list", source, e))?;
    copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
  }
  Ok(())
}

/// Moves `source` to `destination`, which must not exist, across filesystems
/// if it has to.
///
/// A rename where one is possible. The cache and the data directory are on
/// different disks as soon as a user moves the cache, and then only a copy
/// can cross; it lands in a sibling of `destination` first and is renamed
/// into place, so a reader never sees half of it.
pub fn move_tree(source: &Path, destination: &Path) -> Result<()> {
  match fs::rename(source, destination) {
    Ok(()) => Ok(()),
    Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
      let temporary = destination.with_file_name(format!(
        ".{}.moving",
        destination
          .file_name()
          .unwrap_or_default()
          .to_string_lossy()
      ));
      remove_any(&temporary)?;
      copy_tree(source, &temporary)?;
      fs::rename(&temporary, destination)
        .map_err(|e| Error::io("move into place", destination, e))?;
      remove_any(source)
    }
    Err(e) => Err(Error::io("move", destination, e)),
  }
}

/// Removes a file or directory tree, ignoring a missing target.
pub fn remove_any(path: &Path) -> Result<()> {
  match fs::symlink_metadata(path) {
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(e) => Err(Error::io("inspect", path, e)),
    Ok(meta) if meta.is_dir() => {
      fs::remove_dir_all(path).map_err(|e| Error::io("remove directory", path, e))
    }
    Ok(_) => fs::remove_file(path).map_err(|e| Error::io("remove", path, e)),
  }
}

/// Lists every regular file under `root`, as paths relative to it, sorted.
pub fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
  fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))?;
    for entry in entries {
      let entry = entry.map_err(|e| Error::io("list", dir, e))?;
      let path = entry.path();
      let meta = fs::symlink_metadata(&path).map_err(|e| Error::io("inspect", &path, e))?;
      if meta.is_dir() {
        walk(base, &path, out)?;
      } else if meta.is_file() {
        out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf());
      }
    }
    Ok(())
  }
  let mut out = Vec::new();
  if root.is_dir() {
    walk(root, root, &mut out)?;
  }
  out.sort();
  Ok(out)
}

/// The nearest ancestor of `path` that exists, `path` included.
///
/// A destination directory is often not there yet — the first install creates
/// it — and a question about free space is really a question about the
/// filesystem it will land on, which its nearest existing ancestor identifies.
pub fn nearest_existing(path: &Path) -> Option<PathBuf> {
  let mut current = path;
  loop {
    if current.exists() {
      return Some(current.to_path_buf());
    }
    current = current.parent()?;
  }
}

/// Which filesystem `path` is on, for grouping several paths by device.
///
/// `None` when the path cannot be inspected, which is not an error here: it
/// only means the caller has to treat this path as its own filesystem.
pub fn filesystem_id(path: &Path) -> Option<u64> {
  use std::os::unix::fs::MetadataExt;
  let existing = nearest_existing(path)?;
  fs::metadata(existing).ok().map(|m| m.dev())
}

/// Bytes an unprivileged process can still write to the filesystem holding
/// `path`.
///
/// `None` when it cannot be determined. A missing answer must never block an
/// install: not knowing how much room there is is not the same as knowing
/// there is none.
pub fn available_bytes(path: &Path) -> Option<u64> {
  let existing = nearest_existing(path)?;
  let stat = rustix::fs::statvfs(&existing).ok()?;
  // `f_bavail` rather than `f_bfree`: the reserved blocks are not ours.
  stat.f_bavail.checked_mul(stat.f_frsize)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn atomic_write_replaces_content_and_leaves_no_temporary() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("state.json");
    write_atomic(&target, b"first").unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"first");
    write_atomic(&target, b"second").unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"second");

    let leftovers: Vec<_> = fs::read_dir(dir.path())
      .unwrap()
      .flatten()
      .map(|e| e.file_name().to_string_lossy().into_owned())
      .filter(|n| n != "state.json")
      .collect();
    assert!(
      leftovers.is_empty(),
      "temporary files left behind: {leftovers:?}"
    );
  }

  #[test]
  fn hashing_matches_the_known_digest_of_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty");
    fs::write(&path, b"").unwrap();
    assert_eq!(
      hash_file(&path).unwrap().to_string(),
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
  }

  #[test]
  fn copy_tree_reproduces_a_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Plugin.vst3");
    let nested = source.join("Contents/x86_64-linux");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("Plugin.so"), b"binary").unwrap();
    fs::write(source.join("Contents/moduleinfo.json"), b"{}").unwrap();

    let destination = dir.path().join("copy.vst3");
    copy_tree(&source, &destination).unwrap();
    assert_eq!(
      fs::read(destination.join("Contents/x86_64-linux/Plugin.so")).unwrap(),
      b"binary"
    );
    assert_eq!(walk_files(&destination).unwrap().len(), 2);
  }

  #[cfg(unix)]
  #[test]
  fn copy_tree_refuses_to_follow_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink("/etc", &link).unwrap();
    assert!(copy_tree(&link, &dir.path().join("out")).is_err());
  }

  #[test]
  fn removing_a_missing_path_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    remove_any(&dir.path().join("nope")).unwrap();
  }

  #[test]
  fn free_space_is_read_through_a_directory_that_does_not_exist_yet() {
    // The first install creates `~/.clap`, so the question has to be
    // answerable before the directory is there.
    let dir = tempfile::tempdir().unwrap();
    let future = dir.path().join("not/created/yet");
    assert_eq!(nearest_existing(&future).as_deref(), Some(dir.path()));

    let available = available_bytes(&future).expect("a tempdir is on a real filesystem");
    assert!(available > 0, "a writable tempdir should report free space");

    // Everything under one root is one filesystem, which is what lets the
    // requirement be summed per device rather than counted twice.
    assert_eq!(
      filesystem_id(&future),
      filesystem_id(&dir.path().join("elsewhere"))
    );
  }

  #[test]
  fn a_path_with_no_existing_ancestor_answers_nothing_rather_than_erroring() {
    assert_eq!(available_bytes(Path::new("relative/nowhere")), None);
  }
}
