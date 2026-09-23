//! Reading untrusted archives.
//!
//! Everything downloaded is hostile until proven otherwise (§41). The rules are
//! stated once in [`safe::SafeExtractor`] and applied identically to every
//! container format; the per-format modules only decide what entries exist.

mod safe;
mod sevenz_archive;
mod tar_archive;
mod zip_archive;

pub use safe::{ExtractLimits, ExtractReport, SafeExtractor};

use crate::error::ArchiveError;
use luthier_manifest::ArchiveFormat;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Identifies a container from its leading bytes.
///
/// Filenames lie — plenty of real releases name a tarball `.zip` by accident —
/// and the manifest is data we are checking, not data we trust. Sniffing gives
/// an independent second opinion, and a disagreement is reported rather than
/// silently resolved.
pub fn sniff(path: &Path) -> Result<Option<ArchiveFormat>, ArchiveError> {
  let mut file = File::open(path).map_err(|source| ArchiveError::Io {
    operation: "open",
    path: path.to_path_buf(),
    source,
  })?;
  let mut magic = [0u8; 8];
  let read = read_up_to(&mut file, &mut magic).map_err(|source| ArchiveError::Io {
    operation: "read",
    path: path.to_path_buf(),
    source,
  })?;
  Ok(sniff_bytes(&magic[..read]))
}

/// The magic-number table, separated so it can be unit tested without files.
pub fn sniff_bytes(magic: &[u8]) -> Option<ArchiveFormat> {
  const GZIP: &[u8] = &[0x1f, 0x8b];
  const XZ: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
  const SEVEN_Z: &[u8] = &[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c];

  if magic.starts_with(GZIP) {
    return Some(ArchiveFormat::TarGz);
  }
  if magic.starts_with(XZ) {
    return Some(ArchiveFormat::TarXz);
  }
  if magic.starts_with(SEVEN_Z) {
    return Some(ArchiveFormat::SevenZ);
  }
  // Zip local file header, plus the empty and spanned variants.
  if magic.starts_with(b"PK\x03\x04")
    || magic.starts_with(b"PK\x05\x06")
    || magic.starts_with(b"PK\x07\x08")
  {
    return Some(ArchiveFormat::Zip);
  }
  None
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
  let mut filled = 0;
  while filled < buffer.len() {
    match file.read(&mut buffer[filled..])? {
      0 => break,
      n => filled += n,
    }
  }
  file.seek(SeekFrom::Start(0))?;
  Ok(filled)
}

/// Makes a downloaded artifact available under `destination` as a tree,
/// whatever it was published as.
///
/// An archive is unpacked. A bare file (`none`) is placed as the one entry
/// of a tree, under `file_name` — the name it was published under, since the
/// cache knows it only by digest. Either way what follows sees a directory,
/// so derivation and install rules have one shape to read.
pub fn unpack(
  source: &Path,
  declared: &ArchiveFormat,
  file_name: &str,
  destination: &Path,
  limits: ExtractLimits,
) -> Result<ExtractReport, ArchiveError> {
  match declared {
    ArchiveFormat::None => place(source, file_name, destination, limits),
    _ => extract(source, declared, destination, limits),
  }
}

/// Places a bare file into `destination` as `name`.
///
/// Through [`SafeExtractor`] like any archive entry, so the name is held to
/// the same rules, the write refuses to follow a link, and the permission
/// bits are the same sanitised ones. `name` must be one component: it comes
/// from a URL, and a bare file has no directories to recreate.
///
/// The bytes are sniffed as they are for an archive. A file declared bare
/// that is really a zip would otherwise be placed whole and installed as the
/// plugin it is not.
pub fn place(
  source: &Path,
  name: &str,
  destination: &Path,
  limits: ExtractLimits,
) -> Result<ExtractReport, ArchiveError> {
  if let Some(detected) = sniff(source)? {
    return Err(ArchiveError::FormatMismatch {
      declared: ArchiveFormat::None.to_string(),
      detected: detected.to_string(),
    });
  }
  let mut extractor = SafeExtractor::new(destination, limits);
  if name.contains('/') {
    return Err(extractor.reject(name, crate::error::UnsafeEntry::MalformedPath));
  }
  let mut file = File::open(source).map_err(|e| ArchiveError::Io {
    operation: "open",
    path: source.to_path_buf(),
    source: e,
  })?;
  extractor.file(name, None, &mut file)?;
  Ok(extractor.report())
}

/// Unpacks `source` into `destination`, which must already exist and be empty.
///
/// `declared` is what the manifest says the container is. It is cross-checked
/// against the bytes, and a mismatch aborts rather than being worked around.
pub fn extract(
  source: &Path,
  declared: &ArchiveFormat,
  destination: &Path,
  limits: ExtractLimits,
) -> Result<ExtractReport, ArchiveError> {
  if !matches!(
    declared,
    ArchiveFormat::TarGz | ArchiveFormat::TarXz | ArchiveFormat::Zip | ArchiveFormat::SevenZ
  ) {
    return Err(ArchiveError::UnsupportedFormat(declared.to_string()));
  }

  if let Some(detected) = sniff(source)?
    && &detected != declared
  {
    return Err(ArchiveError::FormatMismatch {
      declared: declared.to_string(),
      detected: detected.to_string(),
    });
  }

  let file = File::open(source).map_err(|e| ArchiveError::Io {
    operation: "open",
    path: source.to_path_buf(),
    source: e,
  })?;
  let reader = BufReader::new(file);
  let mut extractor = SafeExtractor::new(destination, limits);

  match declared {
    ArchiveFormat::TarGz => {
      tar_archive::extract(flate2::read::GzDecoder::new(reader), &mut extractor)?;
    }
    ArchiveFormat::TarXz => {
      tar_archive::extract(liblzma::read::XzDecoder::new(reader), &mut extractor)?;
    }
    ArchiveFormat::Zip => {
      zip_archive::extract(reader, &mut extractor)?;
    }
    ArchiveFormat::SevenZ => {
      sevenz_archive::extract(reader, &mut extractor)?;
    }
    other => return Err(ArchiveError::UnsupportedFormat(other.to_string())),
  }

  Ok(extractor.report())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn magic_numbers_identify_each_container() {
    assert_eq!(sniff_bytes(&[0x1f, 0x8b, 0x08]), Some(ArchiveFormat::TarGz));
    assert_eq!(
      sniff_bytes(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]),
      Some(ArchiveFormat::TarXz)
    );
    assert_eq!(sniff_bytes(b"PK\x03\x04rest"), Some(ArchiveFormat::Zip));
    assert_eq!(
      sniff_bytes(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]),
      Some(ArchiveFormat::SevenZ)
    );
    assert_eq!(sniff_bytes(b"not an archive"), None);
    assert_eq!(sniff_bytes(b""), None);
  }

  #[test]
  fn the_validator_and_the_extractor_agree() {
    // "Archives this build opens" is stated twice: as a constant in
    // luthier-manifest, which the registry validator uses and which cannot
    // depend on this crate, and as the dispatch in `extract`. If the two
    // drift, a manifest either validates and then fails at install time,
    // or is rejected for a container that would have worked.
    let dir = tempfile::tempdir().unwrap();
    for format in luthier_manifest::validate::SUPPORTED_ARCHIVES {
      if format == &ArchiveFormat::None {
        continue; // A bare file is copied, not extracted.
      }
      let file = dir.path().join("empty");
      std::fs::write(&file, b"").unwrap();
      let err = extract(&file, format, dir.path(), ExtractLimits::default()).unwrap_err();
      assert!(
        !matches!(err, ArchiveError::UnsupportedFormat(_)),
        "{format} is in SUPPORTED_ARCHIVES but the extractor refuses it"
      );
    }
  }

  #[test]
  fn a_truncated_seven_zip_is_reported_as_corrupt_not_unsupported() {
    // 7z used to be recognised and refused; it is extracted now, so a
    // header with no archive behind it is a corrupt file rather than an
    // unsupported format. Getting this wrong would tell a user to file a
    // registry bug when their download was simply cut short.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.7z");
    std::fs::write(&file, [b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]).unwrap();
    let err = extract(
      &file,
      &ArchiveFormat::SevenZ,
      dir.path(),
      ExtractLimits::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ArchiveError::Corrupt(_)), "{err}");
  }

  #[test]
  fn a_container_that_disagrees_with_the_manifest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.tar.gz");
    std::fs::write(&file, b"PK\x03\x04............").unwrap();
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    let err = extract(
      &file,
      &ArchiveFormat::TarGz,
      &dest,
      ExtractLimits::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ArchiveError::FormatMismatch { .. }), "{err}");
  }
}
