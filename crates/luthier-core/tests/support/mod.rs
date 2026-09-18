//! Builders for the malicious-archive corpus.
//!
//! The hostile archives are generated rather than committed as binary blobs so
//! that a reviewer can see exactly what each test feeds the extractor, and so
//! the corpus can be extended without adding opaque files to the repository.

#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use tar::{EntryType, Header};

/// Writes a tar header whose name bypasses any normalisation the tar crate
/// would otherwise apply.
///
/// `Header::set_path` rejects or rewrites some of the very paths we need to
/// test with, so the raw 100-byte name field is written directly and the
/// checksum recomputed.
fn set_raw_name(header: &mut Header, name: &str) {
  let bytes = name.as_bytes();
  assert!(bytes.len() < 100, "test name too long for a v7 header");
  let old = header.as_old_mut();
  old.name.fill(0);
  old.name[..bytes.len()].copy_from_slice(bytes);
}

/// One entry to place in a generated tar.
pub struct TarEntry {
  pub name: String,
  pub kind: EntryType,
  pub mode: u32,
  pub data: Vec<u8>,
  pub link_target: Option<String>,
}

impl TarEntry {
  pub fn file(name: &str, data: &[u8]) -> Self {
    Self {
      name: name.into(),
      kind: EntryType::Regular,
      mode: 0o644,
      data: data.to_vec(),
      link_target: None,
    }
  }

  pub fn dir(name: &str) -> Self {
    Self {
      name: name.into(),
      kind: EntryType::Directory,
      mode: 0o755,
      data: Vec::new(),
      link_target: None,
    }
  }

  pub fn symlink(name: &str, target: &str) -> Self {
    Self {
      name: name.into(),
      kind: EntryType::Symlink,
      mode: 0o777,
      data: Vec::new(),
      link_target: Some(target.into()),
    }
  }

  pub fn hardlink(name: &str, target: &str) -> Self {
    Self {
      name: name.into(),
      kind: EntryType::Link,
      mode: 0o644,
      data: Vec::new(),
      link_target: Some(target.into()),
    }
  }

  pub fn special(name: &str, kind: EntryType) -> Self {
    Self {
      name: name.into(),
      kind,
      mode: 0o644,
      data: Vec::new(),
      link_target: None,
    }
  }

  pub fn with_mode(mut self, mode: u32) -> Self {
    self.mode = mode;
    self
  }
}

/// Builds an uncompressed tar containing exactly `entries`.
pub fn build_tar(entries: &[TarEntry]) -> Vec<u8> {
  let mut builder = tar::Builder::new(Vec::new());
  for entry in entries {
    let mut header = Header::new_gnu();
    header.set_size(entry.data.len() as u64);
    header.set_mode(entry.mode);
    header.set_entry_type(entry.kind);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    if let Some(target) = &entry.link_target {
      header
        .set_link_name(target)
        .expect("link target fits in the header");
    }
    set_raw_name(&mut header, &entry.name);
    header.set_cksum();
    builder
      .append(&header, entry.data.as_slice())
      .expect("append to in-memory tar");
  }
  builder.into_inner().expect("finish tar")
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
  let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
  encoder.write_all(bytes).expect("gzip");
  encoder.finish().expect("finish gzip")
}

pub fn xz(bytes: &[u8]) -> Vec<u8> {
  let mut encoder = liblzma::write::XzEncoder::new(Vec::new(), 1);
  encoder.write_all(bytes).expect("xz");
  encoder.finish().expect("finish xz")
}

/// Writes `bytes` to `dir/name` and returns the path.
pub fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
  let path = dir.join(name);
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).expect("create parent");
  }
  std::fs::write(&path, bytes).expect("write fixture");
  path
}

/// Convenience: a `.tar.gz` on disk containing `entries`.
pub fn tar_gz_file(dir: &Path, name: &str, entries: &[TarEntry]) -> std::path::PathBuf {
  write_file(dir, name, &gzip(&build_tar(entries)))
}

/// Convenience: a `.tar.xz` on disk containing `entries`.
pub fn tar_xz_file(dir: &Path, name: &str, entries: &[TarEntry]) -> std::path::PathBuf {
  write_file(dir, name, &xz(&build_tar(entries)))
}

/// One entry to place in a generated zip.
pub struct ZipEntry {
  pub name: String,
  pub data: Vec<u8>,
  pub mode: Option<u32>,
  pub symlink_target: Option<String>,
  pub directory: bool,
}

impl ZipEntry {
  pub fn file(name: &str, data: &[u8]) -> Self {
    Self {
      name: name.into(),
      data: data.to_vec(),
      mode: Some(0o644),
      symlink_target: None,
      directory: false,
    }
  }

  pub fn dir(name: &str) -> Self {
    Self {
      name: name.into(),
      data: Vec::new(),
      mode: Some(0o755),
      symlink_target: None,
      directory: true,
    }
  }

  pub fn symlink(name: &str, target: &str) -> Self {
    Self {
      name: name.into(),
      data: Vec::new(),
      mode: Some(0o120_777),
      symlink_target: Some(target.into()),
      directory: false,
    }
  }

  pub fn with_mode(mut self, mode: u32) -> Self {
    self.mode = Some(mode);
    self
  }
}

/// Builds a zip containing exactly `entries`, allowing duplicate names.
pub fn build_zip(entries: &[ZipEntry]) -> Vec<u8> {
  use zip::write::SimpleFileOptions;

  let cursor = std::io::Cursor::new(Vec::new());
  let mut writer = zip::ZipWriter::new(cursor);
  for entry in entries {
    let mut options =
      SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    if let Some(mode) = entry.mode {
      options = options.unix_permissions(mode);
    }
    if let Some(target) = &entry.symlink_target {
      writer
        .add_symlink(entry.name.clone(), target.clone(), options)
        .expect("add symlink");
    } else if entry.directory {
      writer
        .add_directory(entry.name.clone(), options)
        .expect("add directory");
    } else {
      writer
        .start_file(entry.name.clone(), options)
        .expect("start file");
      writer.write_all(&entry.data).expect("write zip entry");
    }
  }
  writer.finish().expect("finish zip").into_inner()
}

pub fn zip_file(dir: &Path, name: &str, entries: &[ZipEntry]) -> std::path::PathBuf {
  write_file(dir, name, &build_zip(entries))
}

// ---------------------------------------------------------------------- 7z --

pub struct SevenZEntry {
  pub name: String,
  pub data: Vec<u8>,
  /// Windows attributes, where bit 15 flags a packed Unix mode in the high
  /// half — the same convention zip uses, and how a symlink is expressed.
  pub attributes: Option<u32>,
  pub directory: bool,
}

impl SevenZEntry {
  pub fn file(name: &str, data: &[u8]) -> Self {
    Self {
      name: name.into(),
      data: data.to_vec(),
      attributes: None,
      directory: false,
    }
  }

  pub fn dir(name: &str) -> Self {
    Self {
      name: name.into(),
      data: Vec::new(),
      attributes: None,
      directory: true,
    }
  }

  /// A symlink, encoded the way a unix-built 7z encodes one.
  pub fn symlink(name: &str, target: &str) -> Self {
    Self {
      name: name.into(),
      data: target.as_bytes().to_vec(),
      attributes: Some(0x8000 | (0o120_777 << 16)),
      directory: false,
    }
  }

  pub fn with_unix_mode(mut self, mode: u32) -> Self {
    self.attributes = Some(0x8000 | (mode << 16));
    self
  }
}

/// Builds a 7z containing exactly `entries`.
pub fn build_7z(entries: &[SevenZEntry]) -> Vec<u8> {
  use sevenz_rust2::{ArchiveEntry, ArchiveWriter};

  let cursor = std::io::Cursor::new(Vec::new());
  let mut writer = ArchiveWriter::new(cursor).expect("7z writer");
  for entry in entries {
    let mut archive_entry = if entry.directory {
      ArchiveEntry::new_directory(&entry.name)
    } else {
      ArchiveEntry::new_file(&entry.name)
    };
    if let Some(attributes) = entry.attributes {
      archive_entry.windows_attributes = attributes;
      archive_entry.has_windows_attributes = true;
    }
    writer
      .push_archive_entry(
        archive_entry,
        Some(std::io::Cursor::new(entry.data.clone())),
      )
      .expect("push 7z entry");
  }
  writer.finish().expect("finish 7z").into_inner()
}

pub fn sevenz_file(dir: &Path, name: &str, entries: &[SevenZEntry]) -> std::path::PathBuf {
  write_file(dir, name, &build_7z(entries))
}

/// Recursively lists paths under `root`, relative and sorted, for assertions.
pub fn list_tree(root: &Path) -> Vec<String> {
  fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
      return;
    };
    for entry in entries.flatten() {
      let path = entry.path();
      let relative = path
        .strip_prefix(base)
        .unwrap()
        .to_string_lossy()
        .into_owned();
      let meta = std::fs::symlink_metadata(&path).expect("stat");
      if meta.is_dir() {
        out.push(format!("{relative}/"));
        walk(base, &path, out);
      } else if meta.file_type().is_symlink() {
        out.push(format!("{relative} -> symlink"));
      } else {
        out.push(relative);
      }
    }
  }
  let mut out = Vec::new();
  walk(root, root, &mut out);
  out.sort();
  out
}
