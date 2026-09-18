//! Constrained path types.
//!
//! These are security types. A manifest is registry data, and the registry is
//! only as trustworthy as its review process, so a manifest must never be able
//! to name a location outside the tree it is allowed to touch. Both types
//! validate on construction *and* on deserialisation, which means the rest of
//! the codebase can join them onto a root without re-checking.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
  #[error("path is empty")]
  Empty,
  #[error("path {0:?} is absolute; only paths relative to the archive root are allowed")]
  Absolute(String),
  #[error("path {0:?} escapes the archive root via '..'")]
  Traversal(String),
  #[error("path {0:?} contains a '.' component")]
  CurrentDir(String),
  #[error("path {0:?} contains an empty component")]
  EmptyComponent(String),
  #[error("path {0:?} contains a backslash, which some tools treat as a separator")]
  Backslash(String),
  #[error("path {0:?} contains a NUL byte")]
  Nul(String),
  #[error("path {0:?} contains a Windows drive or UNC prefix")]
  Prefix(String),
  #[error("path {0:?} is deeper than {1} components")]
  TooDeep(String, usize),
  #[error("path {0:?} is longer than {1} bytes")]
  TooLong(String, usize),
  #[error("{0:?} is not a single filename")]
  NotASingleComponent(String),
}

const MAX_DEPTH: usize = 32;
const MAX_LEN: usize = 1024;

/// Shared checks for anything that will be joined onto a trusted root.
///
/// The validation is deliberately textual rather than going through
/// [`std::path::Path::components`]. `Path` normalises interior `.` components
/// away, so `a/./b` would slip past a component-based check; and on Linux it
/// does not recognise `C:` as a drive prefix. Splitting the raw string means
/// what we validate is exactly what a later `join` will use.
fn check(raw: &str) -> Result<(), PathError> {
  if raw.is_empty() {
    return Err(PathError::Empty);
  }
  if raw.len() > MAX_LEN {
    return Err(PathError::TooLong(raw.to_owned(), MAX_LEN));
  }
  if raw.contains('\0') {
    return Err(PathError::Nul(raw.to_owned()));
  }
  if raw.contains('\\') {
    return Err(PathError::Backslash(raw.to_owned()));
  }
  if raw.starts_with('/') {
    return Err(PathError::Absolute(raw.to_owned()));
  }

  let segments: Vec<&str> = raw.split('/').collect();
  for (i, segment) in segments.iter().enumerate() {
    match *segment {
      "" => return Err(PathError::EmptyComponent(raw.to_owned())),
      "." => return Err(PathError::CurrentDir(raw.to_owned())),
      ".." => return Err(PathError::Traversal(raw.to_owned())),
      _ => {}
    }
    // A leading `C:` would be a drive prefix once this reached a platform
    // that understands one. Only the first segment can be a prefix.
    if i == 0 {
      let bytes = segment.as_bytes();
      if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(PathError::Prefix(raw.to_owned()));
      }
    }
  }
  if segments.len() > MAX_DEPTH {
    return Err(PathError::TooDeep(raw.to_owned(), MAX_DEPTH));
  }
  Ok(())
}

/// A relative, traversal-free path inside an archive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ArchivePath(String);

impl ArchivePath {
  pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
    let raw = raw.into();
    check(&raw)?;
    Ok(Self(raw))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// The validated path as a `Path`, safe to join onto a root.
  pub fn as_path(&self) -> &Path {
    Path::new(&self.0)
  }

  /// The final component, e.g. `Surge XT.clap`.
  pub fn file_name(&self) -> &str {
    self.0.rsplit('/').next().unwrap_or(&self.0)
  }

  /// Joins onto `root`. Sound because the invariant forbids traversal.
  pub fn resolve_under(&self, root: &Path) -> PathBuf {
    root.join(self.as_path())
  }
}

impl fmt::Display for ArchivePath {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::str::FromStr for ArchivePath {
  type Err = PathError;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Self::new(s)
  }
}

impl<'de> Deserialize<'de> for ArchivePath {
  fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
    let raw = String::deserialize(de)?;
    Self::new(raw).map_err(serde::de::Error::custom)
  }
}

impl schemars::JsonSchema for ArchivePath {
  fn schema_name() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("ArchivePath")
  }
  fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "description": "Path relative to the archive root. Must not be absolute or contain '..'.",
        "examples": ["Surge XT.clap", "lib/vst3/Dexed.vst3"],
    })
  }
}

/// Exactly one path component — a filename with no separators.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct FileName(String);

impl FileName {
  pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
    let raw = raw.into();
    check(&raw)?;
    if raw.contains('/') {
      return Err(PathError::NotASingleComponent(raw));
    }
    Ok(Self(raw))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl fmt::Display for FileName {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::str::FromStr for FileName {
  type Err = PathError;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Self::new(s)
  }
}

impl<'de> Deserialize<'de> for FileName {
  fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
    let raw = String::deserialize(de)?;
    Self::new(raw).map_err(serde::de::Error::custom)
  }
}

impl schemars::JsonSchema for FileName {
  fn schema_name() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("FileName")
  }
  fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "minLength": 1,
        "description": "A single filename with no path separators.",
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The corpus that matters. Every one of these must be impossible to
  /// construct, including through deserialisation.
  const HOSTILE: &[&str] = &[
    "../../evil",
    "..",
    "../etc/passwd",
    "/etc/passwd",
    "/",
    "a/../../b",
    "a/./b",
    "a//b",
    "",
    "..\\..\\evil",
    "C:\\Windows\\system32",
    "\\\\server\\share",
    "a\0b",
    "./x",
  ];

  #[test]
  fn hostile_paths_are_rejected() {
    for raw in HOSTILE {
      assert!(
        ArchivePath::new(*raw).is_err(),
        "ArchivePath must reject {raw:?}"
      );
      let json = serde_json::to_string(raw).unwrap();
      assert!(
        serde_json::from_str::<ArchivePath>(&json).is_err(),
        "deserialising {raw:?} must fail"
      );
    }
  }

  #[test]
  fn realistic_archive_paths_are_accepted() {
    for raw in [
      "Surge XT.clap",
      "lib/vst3/Dexed.vst3",
      "DragonflyHallReverb-vst3/Contents/x86_64-linux/x.so",
    ] {
      ArchivePath::new(raw).unwrap_or_else(|e| panic!("{raw:?} should be valid: {e}"));
    }
  }

  #[test]
  fn resolving_stays_under_the_root() {
    let p = ArchivePath::new("lib/vst3/Dexed.vst3").unwrap();
    let root = Path::new("/tmp/extract");
    let resolved = p.resolve_under(root);
    assert!(resolved.starts_with(root));
    assert_eq!(resolved, Path::new("/tmp/extract/lib/vst3/Dexed.vst3"));
  }

  #[test]
  fn depth_and_length_are_bounded() {
    let deep = vec!["a"; MAX_DEPTH + 1].join("/");
    assert!(matches!(
      ArchivePath::new(deep),
      Err(PathError::TooDeep(..))
    ));
    let long = "a".repeat(MAX_LEN + 1);
    assert!(matches!(
      ArchivePath::new(long),
      Err(PathError::TooLong(..))
    ));
  }

  #[test]
  fn filename_rejects_separators() {
    assert!(FileName::new("Surge XT.clap").is_ok());
    assert!(matches!(
      FileName::new("a/b"),
      Err(PathError::NotASingleComponent(_))
    ));
    for raw in HOSTILE {
      assert!(FileName::new(*raw).is_err(), "FileName must reject {raw:?}");
    }
  }

  #[test]
  fn file_name_extracts_the_last_component() {
    assert_eq!(
      ArchivePath::new("lib/vst3/Dexed.vst3").unwrap().file_name(),
      "Dexed.vst3"
    );
    assert_eq!(
      ArchivePath::new("Surge XT.clap").unwrap().file_name(),
      "Surge XT.clap"
    );
  }
}
