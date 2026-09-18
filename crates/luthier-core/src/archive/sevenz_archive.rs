//! 7z unpacking.
//!
//! As with tar and zip, this does not call the crate's own extraction helper;
//! entries are inspected and then handed to [`SafeExtractor`], so the policy
//! that governs the other two formats governs this one unchanged.
//!
//! 7z exists here for one reason: LSP Plugins, the largest free plugin suite
//! on Linux, publishes its binaries in no other container.

use super::safe::SafeExtractor;
use crate::error::{ArchiveError, UnsafeEntry};
use std::io::{Read, Seek};

/// `S_IFMT` and friends, as for zip. 7z stores Windows attributes; an archive
/// written on a unix-like system sets bit 15 and packs the Unix mode into the
/// high half, which is the same convention zip uses.
const ATTR_UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// The Unix mode an entry claims, if it carries one.
fn unix_mode(attributes: u32) -> Option<u32> {
  (attributes & ATTR_UNIX_EXTENSION != 0).then_some(attributes >> 16)
}

pub fn extract<R: Read + Seek>(
  reader: R,
  extractor: &mut SafeExtractor,
) -> Result<(), ArchiveError> {
  let mut archive = sevenz_rust2::ArchiveReader::new(reader, sevenz_rust2::Password::empty())
    .map_err(|e| ArchiveError::Corrupt(format!("cannot open 7z: {e}")))?;

  // The reader hands each entry to this closure in archive order. Returning
  // an error stops the walk, which is what makes a refusal abort the whole
  // extraction rather than skipping one entry.
  let mut failure: Option<ArchiveError> = None;
  let result = archive.for_each_entries(|entry, rest| {
    let name = entry.name().to_owned();
    let mode = unix_mode(entry.windows_attributes());

    if let Some(mode) = mode {
      match mode & S_IFMT {
        // A symlink is the classic archive escape, in any container.
        S_IFLNK => {
          failure = Some(extractor.reject(&name, UnsafeEntry::Symlink));
          return Ok(false);
        }
        0 | S_IFREG | S_IFDIR => {}
        _ => {
          failure = Some(extractor.reject(&name, UnsafeEntry::SpecialFile));
          return Ok(false);
        }
      }
    }

    let outcome = if entry.is_directory() {
      extractor.directory(&name)
    } else {
      extractor.file(&name, mode, rest)
    };

    match outcome {
      Ok(()) => Ok(true),
      Err(e) => {
        failure = Some(e);
        Ok(false)
      }
    }
  });

  if let Some(error) = failure {
    return Err(error);
  }
  result.map_err(|e| ArchiveError::Corrupt(format!("cannot read 7z: {e}")))?;
  Ok(())
}
