//! Zip unpacking.
//!
//! As with tar, this does not call the crate's own `extract`; entries are
//! inspected and then handed to [`SafeExtractor`].

use super::safe::{SafeExtractor, claimed_mode};
use crate::error::{ArchiveError, UnsafeEntry};
use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};
use zip::result::ZipError;
use zip::{CompressionMethod, ZipArchive, ZipReadOptions};

/// The signature that opens each record of a zip's central directory.
const CENTRAL_RECORD: u32 = 0x0201_4b50;

pub fn extract<R: Read + Seek>(
  reader: R,
  extractor: &mut SafeExtractor,
) -> Result<(), ArchiveError> {
  // A name listed twice in the central directory is read once, the last
  // copy winning without a word, so a review of the archive would not
  // tell which lands on disk. Refused, as tar refuses a duplicate. The
  // crate keeps one entry per name, so the records are read here.
  let archive = open(reader)?;
  let start = archive.central_directory_start();
  let mut reader = archive.into_inner();
  if let Some(name) = repeated_name(&mut reader, start)? {
    return Err(extractor.reject(&name, UnsafeEntry::Duplicate));
  }
  let mut archive = open(reader)?;

  for index in 0..archive.len() {
    // A name that is valid UTF-8 is read as such, and any other as CP437,
    // which maps every byte: this does not fail today. Refused rather than
    // unwrapped should a later release start failing it.
    let name = archive
      .name_for_index(index)
      .ok_or_else(|| ArchiveError::Corrupt(format!("zip entry {index} is missing")))?
      .map_err(|e| {
        ArchiveError::Corrupt(format!("cannot read the name of zip entry {index}: {e}"))
      })?
      .into_owned();

    // A local header that says something else than the central directory
    // is what a reader streaming the local headers — bsdtar does — would
    // install instead. Told apart from an entry that cannot be read at
    // all by reading it again without the check.
    let checked = ZipReadOptions::new().enforce_header_consistency(true);
    if let Err(e) = archive.by_index_with_options(index, checked).map(drop) {
      return Err(match archive.by_index(index) {
        Ok(_) => extractor.reject(&name, UnsafeEntry::HeadersDisagree),
        Err(_) => entry_error(index, e),
      });
    }
    let mut file = archive.by_index(index).map_err(|e| entry_error(index, e))?;

    // The mode from the upper half of the external attributes, whatever
    // system the entry says made it, as zip 8 read it: zip 9 builds one
    // from the DOS attributes for an entry marked as made on DOS, which
    // would pass a link for a file and drop an executable bit. Only with
    // nothing there is the crate asked.
    let mode = claimed_mode(file.external_attributes()).or_else(|| file.unix_mode());
    extractor.check_type(&name, mode)?;

    if file.is_dir() {
      extractor.directory(&name)?;
    } else {
      extractor.file(&name, mode, &mut file)?;
    }
  }
  Ok(())
}

/// Why entry `index` could not be opened. A method the crate has no decoder
/// for is a limit of this build, told apart from damage as 7z tells it.
fn entry_error(index: usize, error: ZipError) -> ArchiveError {
  match error {
    ZipError::CompressionMethodNotSupported(id) => ArchiveError::UnsupportedMethod {
      method: match CompressionMethod::name_from_u16(id) {
        "Unknown" => format!("method {id}"),
        name => name.to_owned(),
      },
    },
    other => ArchiveError::Corrupt(format!("cannot read zip entry {index}: {other}")),
  }
}

fn open<R: Read + Seek>(reader: R) -> Result<ZipArchive<R>, ArchiveError> {
  ZipArchive::new(reader).map_err(|e| ArchiveError::Corrupt(format!("cannot open zip: {e}")))
}

/// The first name the central directory at `start` lists twice, if any.
fn repeated_name<R: Read + Seek>(
  reader: &mut R,
  start: u64,
) -> Result<Option<String>, ArchiveError> {
  let corrupt =
    |e: io::Error| ArchiveError::Corrupt(format!("cannot read the zip's central directory: {e}"));
  reader.seek(SeekFrom::Start(start)).map_err(corrupt)?;
  let mut reader = io::BufReader::new(reader);
  let mut seen = HashSet::new();
  // The fixed part of a record: its signature first, and the lengths of
  // its name, extra field and comment at 28, 30 and 32.
  let mut record = [0u8; 46];
  loop {
    match reader.read_exact(&mut record) {
      Ok(()) => {}
      Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
      Err(e) => return Err(corrupt(e)),
    }
    if u32::from_le_bytes([record[0], record[1], record[2], record[3]]) != CENTRAL_RECORD {
      return Ok(None);
    }
    let length = |at: usize| u64::from(u16::from_le_bytes([record[at], record[at + 1]]));
    let mut name = Vec::new();
    (&mut reader)
      .take(length(28))
      .read_to_end(&mut name)
      .map_err(corrupt)?;
    io::copy(
      &mut (&mut reader).take(length(30) + length(32)),
      &mut io::sink(),
    )
    .map_err(corrupt)?;
    if !seen.insert(name.clone()) {
      return Ok(Some(String::from_utf8_lossy(&name).into_owned()));
    }
  }
}
