//! Tar unpacking.
//!
//! Deliberately does not use [`tar::Archive::unpack`]. That helper applies its
//! own idea of what is safe; we apply ours, through [`SafeExtractor`], so there
//! is exactly one policy to audit and to test.

use super::safe::SafeExtractor;
use crate::error::{ArchiveError, UnsafeEntry};
use std::io::Read;
use tar::EntryType;

pub fn extract<R: Read>(reader: R, extractor: &mut SafeExtractor) -> Result<(), ArchiveError> {
  let mut archive = tar::Archive::new(reader);
  let entries = archive
    .entries()
    .map_err(|e| ArchiveError::Corrupt(format!("cannot read tar entries: {e}")))?;

  for entry in entries {
    let mut entry =
      entry.map_err(|e| ArchiveError::Corrupt(format!("cannot read tar entry: {e}")))?;

    let raw = entry.path_bytes().into_owned();
    let name = String::from_utf8(raw).map_err(|_| ArchiveError::Unsafe {
      name: "<non-utf8>".into(),
      reason: UnsafeEntry::MalformedPath,
    })?;

    match entry.header().entry_type() {
      EntryType::Directory => extractor.directory(&name)?,
      EntryType::Regular | EntryType::Continuous => {
        let mode = entry.header().mode().ok();
        extractor.file(&name, mode, &mut entry)?;
      }
      // A symlink is the classic archive escape: plant `link -> /home/u`,
      // then write `link/.ssh/authorized_keys` in a later entry.
      EntryType::Symlink => return Err(extractor.reject(&name, UnsafeEntry::Symlink)),
      // A hard link is allowed only when it names an entry this archive
      // already wrote, and is then materialised as a copy. See
      // `SafeExtractor::hard_link`.
      EntryType::Link => {
        let link = entry
          .link_name()
          .ok()
          .flatten()
          .map(|p| p.to_string_lossy().into_owned());
        match link {
          Some(target) => {
            let mode = entry.header().mode().ok();
            extractor.hard_link(&name, &target, mode)?;
          }
          None => return Err(extractor.reject(&name, UnsafeEntry::HardLink)),
        }
      }
      EntryType::Char | EntryType::Block | EntryType::Fifo => {
        return Err(extractor.reject(&name, UnsafeEntry::SpecialFile));
      }
      // Sparse files would have to be expanded by hand; no plugin ships one.
      EntryType::GNUSparse => {
        return Err(extractor.reject(&name, UnsafeEntry::SpecialFile));
      }
      // Metadata records consumed by the tar crate itself.
      EntryType::XGlobalHeader | EntryType::XHeader => continue,
      EntryType::GNULongName | EntryType::GNULongLink => continue,
      _ => return Err(extractor.reject(&name, UnsafeEntry::SpecialFile)),
    }
  }
  Ok(())
}
