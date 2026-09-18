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

  for index in 0..archive.len() {
    let mut file = archive
      .by_index(index)
      .map_err(|e| ArchiveError::Corrupt(format!("cannot read zip entry {index}: {e}")))?;

    let name = file.name().to_owned();
    let mode = file.unix_mode();

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
