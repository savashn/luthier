//! 7z unpacking.
//!
//! As with tar and zip, this does not call the crate's own extraction helper;
//! entries are inspected and then handed to [`SafeExtractor`], so the policy
//! that governs the other two formats governs this one unchanged.
//!
//! 7z exists here for one reason: LSP Plugins, the largest free plugin suite
//! on Linux, publishes its binaries in no other container.

use super::safe::{SafeExtractor, claimed_mode};
use crate::error::ArchiveError;
use std::io::{Read, Seek};

/// 7z stores Windows attributes; an archive written on a unix-like system
/// sets bit 15 and packs the Unix mode into the high half, which is the same
/// convention zip uses.
const ATTR_UNIX_EXTENSION: u32 = 0x8000;

/// The permissions an entry carries, where it says it carries a Unix mode.
/// Windows keeps attributes of its own in the high half too (pinned,
/// recall-on-access), and read as a mode those would turn executable bits on.
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
    let attributes = entry.windows_attributes();

    // The type from whatever the high half claims, with or without bit
    // 15, as zip reads it: a link marked as Windows-made is still a link.
    // Of the Windows attributes only FILE_ATTRIBUTE_STRICTLY_SEQUENTIAL
    // lands in the type bits, and an entry carrying it is refused as a
    // special file: refusing a type that is unclear beats guessing it.
    if let Err(refusal) = extractor.check_type(&name, claimed_mode(attributes)) {
      failure = Some(refusal);
      return Ok(false);
    }
    let mode = unix_mode(attributes);

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
