//! Zip unpacking.
//!
//! As with tar, this does not call the crate's own `extract`; entries are
//! inspected and then handed to [`SafeExtractor`].

use super::safe::SafeExtractor;
use crate::error::{ArchiveError, UnsafeEntry};
use std::io::{Read, Seek};

/// `S_IFMT` and `S_IFLNK` from `<sys/stat.h>`: zip stores a Unix mode in the
/// external attributes, and a symlink is encoded there rather than as a
/// distinct entry type.
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

pub fn extract<R: Read + Seek>(
  reader: R,
  extractor: &mut SafeExtractor,
) -> Result<(), ArchiveError> {
  let mut archive = zip::ZipArchive::new(reader)
    .map_err(|e| ArchiveError::Corrupt(format!("cannot open zip: {e}")))?;

  // A name listed twice in the central directory would be read once, the
  // last copy winning without a word, and a local header that says
  // something else than the central directory is what a reader streaming
  // the local headers — bsdtar does — would install instead. Either way
  // a review of the archive would not tell what lands on disk, so both
  // are refused, as tar refuses a duplicate. Every Linux zip sampled from
  // the Open Audio Stack registry passes.
  archive
    .validate_header_consistency()
    .map_err(|e| ArchiveError::Corrupt(format!("zip headers disagree: {e}")))?;

  for index in 0..archive.len() {
    let mut file = archive
      .by_index(index)
      .map_err(|e| ArchiveError::Corrupt(format!("cannot read zip entry {index}: {e}")))?;

    // A name that is valid UTF-8 is read as such, and any other as CP437,
    // which maps every byte: this does not fail today. Refused rather than
    // unwrapped should a later release start failing it.
    let name = file
      .name()
      .map_err(|e| {
        ArchiveError::Corrupt(format!("cannot read the name of zip entry {index}: {e}"))
      })?
      .into_owned();

    // The Unix mode is read here, from the upper half of the external
    // attributes, whatever system the entry says it was made on — as zip 8
    // read it, and as `sevenz_archive` reads its own. zip 9 trusts the
    // marking: for an entry marked as made on DOS it builds a mode from the
    // DOS attributes, which would let a link through as a plain file and
    // drop an executable bit. Only with nothing there is the crate asked.
    let mode = match file.external_attributes() >> 16 {
      0 => file.unix_mode(),
      marked => Some(marked),
    };

    if let Some(mode) = mode {
      match mode & S_IFMT {
        S_IFLNK => return Err(extractor.reject(&name, UnsafeEntry::Symlink)),
        0 | S_IFREG | S_IFDIR => {}
        _ => return Err(extractor.reject(&name, UnsafeEntry::SpecialFile)),
      }
    }

    if file.is_dir() {
      extractor.directory(&name)?;
    } else {
      extractor.file(&name, mode, &mut file)?;
    }
  }
  Ok(())
}
