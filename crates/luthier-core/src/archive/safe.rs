//! The extraction policy.
//!
//! Everything an archive asks us to do goes through [`SafeExtractor`]. The
//! extractors for each container format decide *what* entries exist; this type
//! decides whether any of them are allowed to touch the disk. Keeping that
//! decision in one place means adding a fourth container format cannot
//! accidentally introduce a fourth extraction policy.

use crate::error::{ArchiveError, UnsafeEntry};
use luthier_manifest::{ArchivePath, PathError};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Ceilings that make a decompression bomb fail fast instead of filling the disk.
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
  pub max_entries: usize,
  pub max_total_bytes: u64,
  pub max_entry_bytes: u64,
}

impl Default for ExtractLimits {
  fn default() -> Self {
    // Generous next to real packages — the full Surge XT tarball expands to
    // a few hundred megabytes — but far below anything that could exhaust a
    // disk before we noticed.
    Self {
      max_entries: 200_000,
      max_total_bytes: 8 * 1024 * 1024 * 1024,
      max_entry_bytes: 4 * 1024 * 1024 * 1024,
    }
  }
}

impl ExtractLimits {
  /// How far an archive may expand past its own compressed size.
  ///
  /// Sample content is the demanding case and it is also the least
  /// compressible: a kit of WAVs expands by well under two. A bomb expands by
  /// thousands, so four separates them with room to spare.
  const EXPANSION: u64 = 4;

  /// Limits for an artifact of a known download size.
  ///
  /// The defaults are a floor rather than a ceiling here: an archive that is
  /// itself larger than the default allowance must be allowed to expand, or a
  /// 5 GiB sample library cannot be installed at all. What the limit still
  /// catches is the archive that expands out of all proportion to its size,
  /// which is what a decompression bomb is. Running out of actual disk is a
  /// different failure with a different check — see `Session::plan_install`.
  pub fn for_download(bytes: u64) -> Self {
    let default = Self::default();
    Self {
      max_entries: default.max_entries,
      max_total_bytes: default
        .max_total_bytes
        .max(bytes.saturating_mul(Self::EXPANSION)),
      max_entry_bytes: default.max_entry_bytes.max(bytes),
    }
  }

  /// Tight limits for tests.
  pub fn small() -> Self {
    Self {
      max_entries: 64,
      max_total_bytes: 1 << 20,
      max_entry_bytes: 1 << 20,
    }
  }
}

/// What an extraction produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
  pub entries: usize,
  pub bytes: u64,
}

/// Guards every write made while unpacking one archive.
pub struct SafeExtractor {
  root: PathBuf,
  limits: ExtractLimits,
  seen: HashSet<String>,
  entries: usize,
  bytes: u64,
}

impl SafeExtractor {
  /// `root` must already exist and be a directory we created.
  pub fn new(root: impl Into<PathBuf>, limits: ExtractLimits) -> Self {
    Self {
      root: root.into(),
      limits,
      seen: HashSet::new(),
      entries: 0,
      bytes: 0,
    }
  }

  pub fn report(&self) -> ExtractReport {
    ExtractReport {
      entries: self.entries,
      bytes: self.bytes,
    }
  }

  /// Validates an archive-supplied name and returns where it may be written.
  ///
  /// Reuses [`ArchivePath`], so the rules applied to a path coming out of an
  /// archive are exactly the rules applied to a path written in a manifest —
  /// there is no second, weaker implementation to drift.
  ///
  /// Returns `Ok(None)` for an entry that names the archive root itself,
  /// which is not something to write but is not an attack either.
  fn resolve(&mut self, raw_name: &str) -> Result<Option<PathBuf>, ArchiveError> {
    let unsafe_entry = |reason| ArchiveError::Unsafe {
      name: raw_name.to_owned(),
      reason,
    };

    let Some(normalised) = normalise(raw_name) else {
      // `./` or `.`: GNU tar writes this as the first entry of any
      // archive built with `tar -C dir .`, which is how Surge XT and many
      // other real releases are packaged. It refers to the extraction
      // directory we already created.
      return Ok(None);
    };

    let path = ArchivePath::new(&normalised).map_err(|e| {
      unsafe_entry(match e {
        PathError::Traversal(_) | PathError::Absolute(_) | PathError::Prefix(_) => {
          UnsafeEntry::PathEscape
        }
        _ => UnsafeEntry::MalformedPath,
      })
    })?;

    if !self.seen.insert(path.as_str().to_owned()) {
      return Err(unsafe_entry(UnsafeEntry::Duplicate));
    }

    self.entries += 1;
    if self.entries > self.limits.max_entries {
      return Err(ArchiveError::TooManyEntries {
        limit: self.limits.max_entries,
      });
    }

    Ok(Some(path.resolve_under(&self.root)))
  }

  /// Creates the parent chain for `target`, refusing to traverse a symlink.
  ///
  /// We reject symlink entries outright, so no symlink should exist inside
  /// the root — this check is the belt to that braces. It also means we never
  /// call `create_dir_all`, which happily walks through an existing link.
  fn ensure_parents(&self, target: &Path) -> Result<(), ArchiveError> {
    let Some(parent) = target.parent() else {
      return Ok(());
    };
    let relative = parent.strip_prefix(&self.root).unwrap_or(Path::new(""));

    let mut current = self.root.clone();
    for component in relative.components() {
      current.push(component);
      match fs::symlink_metadata(&current) {
        Ok(meta) if meta.file_type().is_symlink() => {
          return Err(ArchiveError::Unsafe {
            name: current.display().to_string(),
            reason: UnsafeEntry::SymlinkedParent,
          });
        }
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
          return Err(ArchiveError::Unsafe {
            name: current.display().to_string(),
            reason: UnsafeEntry::SpecialFile,
          });
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
          fs::create_dir(&current).map_err(|source| ArchiveError::Io {
            operation: "create directory",
            path: current.clone(),
            source,
          })?;
        }
        Err(source) => {
          return Err(ArchiveError::Io {
            operation: "inspect",
            path: current.clone(),
            source,
          });
        }
      }
    }
    Ok(())
  }

  /// Records a directory entry.
  pub fn directory(&mut self, name: &str) -> Result<(), ArchiveError> {
    let Some(target) = self.resolve(name)? else {
      return Ok(());
    };
    self.ensure_parents(&target)?;
    match fs::symlink_metadata(&target) {
      Ok(meta) if meta.is_dir() => Ok(()),
      Ok(_) => Err(ArchiveError::Unsafe {
        name: name.to_owned(),
        reason: UnsafeEntry::Duplicate,
      }),
      Err(e) if e.kind() == io::ErrorKind::NotFound => {
        fs::create_dir(&target).map_err(|source| ArchiveError::Io {
          operation: "create directory",
          path: target,
          source,
        })
      }
      Err(source) => Err(ArchiveError::Io {
        operation: "inspect",
        path: target,
        source,
      }),
    }
  }

  /// Streams one regular file into place.
  ///
  /// `mode` is the archive's claimed permission bits; only the executable bit
  /// is honoured. setuid, setgid and sticky are always dropped — an archive
  /// has no business asking for them, and a setuid binary dropped into a
  /// plugin directory is a local privilege-escalation primitive.
  pub fn file(
    &mut self,
    name: &str,
    mode: Option<u32>,
    reader: &mut dyn Read,
  ) -> Result<(), ArchiveError> {
    let target = self.resolve(name)?.ok_or_else(|| ArchiveError::Unsafe {
      name: name.to_owned(),
      reason: UnsafeEntry::MalformedPath,
    })?;
    self.ensure_parents(&target)?;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(sanitise_mode(mode));
    }

    // `create_new` maps to O_EXCL, which refuses to follow a symlink at the
    // final component. A hostile archive therefore cannot use a link
    // planted by an earlier entry to redirect this write.
    let mut out = options.open(&target).map_err(|source| ArchiveError::Io {
      operation: "create file",
      path: target.clone(),
      source,
    })?;

    let written = self.copy_bounded(name, reader, &mut out, &target)?;
    out.flush().map_err(|source| ArchiveError::Io {
      operation: "write",
      path: target.clone(),
      source,
    })?;
    self.bytes += written;
    Ok(())
  }

  fn copy_bounded(
    &self,
    name: &str,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    path: &Path,
  ) -> Result<u64, ArchiveError> {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
      let n = reader
        .read(&mut buffer)
        .map_err(|source| ArchiveError::Io {
          operation: "read from archive",
          path: path.to_path_buf(),
          source,
        })?;
      if n == 0 {
        break;
      }
      written += n as u64;
      if written > self.limits.max_entry_bytes {
        return Err(ArchiveError::EntryTooLarge {
          name: name.to_owned(),
          limit: self.limits.max_entry_bytes,
        });
      }
      if self.bytes + written > self.limits.max_total_bytes {
        return Err(ArchiveError::TooLarge {
          limit: self.limits.max_total_bytes,
        });
      }
      writer
        .write_all(&buffer[..n])
        .map_err(|source| ArchiveError::Io {
          operation: "write",
          path: path.to_path_buf(),
          source,
        })?;
    }
    Ok(written)
  }

  /// Materialises a hard-link entry by copying the file it names.
  ///
  /// A tar hard link names another entry in the same archive. Creating a real
  /// link would put two paths on one inode, and a link naming a file outside
  /// the root would alias something we do not own — which is why links were
  /// refused outright. Copying removes the aliasing question entirely: the
  /// result is an independent file, so nothing a later entry does through one
  /// path can reach the other.
  ///
  /// Safety rests on one rule: the target must be an entry *this extraction
  /// has already written*. Anything already written was itself put through
  /// [`SafeExtractor::resolve`], so it is inside the root and was vetted. A
  /// link naming a path the archive never produced is refused rather than
  /// resolved against the filesystem, which is what stops
  /// `link -> ../../../etc/passwd` before it touches the disk.
  ///
  /// Real packages need this: DPF-Plugins ships its preset collection with
  /// hard links for the files repeated across plugins.
  pub fn hard_link(
    &mut self,
    name: &str,
    link_target: &str,
    mode: Option<u32>,
  ) -> Result<(), ArchiveError> {
    let refuse = || ArchiveError::Unsafe {
      name: name.to_owned(),
      reason: UnsafeEntry::HardLink,
    };

    // The target is archive-supplied, so it goes through the same rules as
    // any entry name rather than a second, weaker check.
    let normalised = normalise(link_target).ok_or_else(refuse)?;
    let target = ArchivePath::new(&normalised).map_err(|_| refuse())?;

    if !self.seen.contains(target.as_str()) {
      return Err(refuse());
    }

    let source = target.resolve_under(&self.root);
    let mut input = fs::File::open(&source).map_err(|source_err| ArchiveError::Io {
      operation: "open the hard link's target",
      path: source,
      source: source_err,
    })?;
    self.file(name, mode, &mut input)
  }

  /// Rejects an entry type we will not handle, naming why.
  pub fn reject(&self, name: &str, reason: UnsafeEntry) -> ArchiveError {
    ArchiveError::Unsafe {
      name: name.to_owned(),
      reason,
    }
  }
}

/// Strips an archive's decorative path prefixes.
///
/// Returns `None` when the name refers to the archive root itself. Only a
/// *leading* `./` is removed: an interior `.` component is left in place so
/// that [`ArchivePath`] still rejects it, since no real archive contains one
/// and normalising it away would mean trusting our own rewriting.
fn normalise(raw: &str) -> Option<String> {
  let mut rest = raw.trim_end_matches('/');
  while let Some(stripped) = rest.strip_prefix("./") {
    rest = stripped.trim_start_matches('/');
  }
  if rest.is_empty() || rest == "." {
    return None;
  }
  Some(rest.to_owned())
}

/// Keeps only the executable bit, and only for the owner triad's benefit.
fn sanitise_mode(mode: Option<u32>) -> u32 {
  let executable = mode.is_some_and(|m| m & 0o111 != 0);
  if executable { 0o755 } else { 0o644 }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_leading_current_directory_is_stripped_not_rejected() {
    // GNU tar writes `./` entries for any archive built with `tar -C dir .`;
    // Surge XT's release tarball is packaged exactly that way.
    assert_eq!(normalise("./"), None);
    assert_eq!(normalise("."), None);
    assert_eq!(
      normalise("./Surge XT.clap"),
      Some("Surge XT.clap".to_string())
    );
    assert_eq!(
      normalise("./lib/vst3/Surge XT.vst3/"),
      Some("lib/vst3/Surge XT.vst3".to_string())
    );
    // An interior `.` is left for ArchivePath to reject.
    assert_eq!(normalise("a/./b"), Some("a/./b".to_string()));
    // Stripping must not open a route to traversal.
    assert_eq!(normalise("./../evil"), Some("../evil".to_string()));
  }

  #[test]
  fn permission_bits_are_reduced_to_two_shapes() {
    assert_eq!(sanitise_mode(Some(0o777)), 0o755);
    assert_eq!(sanitise_mode(Some(0o644)), 0o644);
    assert_eq!(sanitise_mode(None), 0o644);
    // The cases that matter: setuid, setgid and sticky never survive.
    assert_eq!(sanitise_mode(Some(0o4755)), 0o755);
    assert_eq!(sanitise_mode(Some(0o6755)), 0o755);
    assert_eq!(sanitise_mode(Some(0o1777)), 0o755);
    for mode in [0o4755, 0o6755, 0o2755, 0o1777] {
      assert_eq!(
        sanitise_mode(Some(mode)) & 0o7000,
        0,
        "mode {mode:o} kept a special bit"
      );
    }
  }

  #[test]
  fn a_large_artifact_raises_the_expansion_ceiling_but_a_small_one_does_not() {
    let default = ExtractLimits::default();
    // A plugin tarball is nowhere near the floor, so nothing moves.
    let small = ExtractLimits::for_download(333 * 1024 * 1024);
    assert_eq!(small.max_total_bytes, default.max_total_bytes);
    assert_eq!(small.max_entry_bytes, default.max_entry_bytes);

    // CrocellKit is larger than the default entry ceiling on its own.
    let kit = ExtractLimits::for_download(5_646_502_341);
    assert_eq!(kit.max_total_bytes, 5_646_502_341 * 4);
    assert_eq!(kit.max_entry_bytes, 5_646_502_341);
    assert_eq!(kit.max_entries, default.max_entries);

    // The ratio is what catches a bomb: a 1 MiB archive still may not
    // expand to a gigabyte.
    assert_eq!(
      ExtractLimits::for_download(1 << 20).max_total_bytes,
      default.max_total_bytes
    );
  }
}
