//! Package identifiers.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Why a candidate string is not a valid package ID.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
  #[error("package ID is empty")]
  Empty,
  #[error("package ID {0:?} is longer than {max} characters", max = PackageId::MAX_LEN)]
  TooLong(String),
  #[error("package ID {0:?} contains {1:?}; only lowercase letters, digits and '-' are allowed")]
  InvalidCharacter(String, char),
  #[error("package ID {0:?} must start and end with a letter or digit")]
  BadBoundary(String),
}

/// An immutable package identifier, e.g. `surge-xt`.
///
/// The character set is deliberately narrow. A `PackageId` is used as a
/// filesystem path component (state files, registry filenames, cache keys), so
/// the invariant enforced here — no separators, no `..`, no leading dot, no
/// whitespace, no uppercase — is a security boundary, not just a style rule.
/// It is validated on construction *and* on deserialisation so the type can
/// never hold an unchecked value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PackageId(String);

impl PackageId {
  pub const MAX_LEN: usize = 64;

  /// Validates and wraps a package ID.
  pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
    let raw = raw.into();
    if raw.is_empty() {
      return Err(IdError::Empty);
    }
    if raw.len() > Self::MAX_LEN {
      return Err(IdError::TooLong(raw));
    }
    if let Some(bad) = raw
      .chars()
      .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
      return Err(IdError::InvalidCharacter(raw.clone(), bad));
    }
    let boundary_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let first = raw.chars().next().expect("non-empty");
    let last = raw.chars().next_back().expect("non-empty");
    if !boundary_ok(first) || !boundary_ok(last) {
      return Err(IdError::BadBoundary(raw));
    }
    Ok(Self(raw))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Whether the ID looks like it has a version baked into it (§5).
  ///
  /// A lint rather than an error: `vsco2` and `odin2` are legitimate names
  /// where the digit is part of the product, so this only flags a trailing
  /// dash-separated dotted number such as `surge-xt-1.3.4`. It cannot be a
  /// hard rule without rejecting real package names.
  pub fn looks_versioned(&self) -> bool {
    let Some((_, tail)) = self.0.rsplit_once('-') else {
      return false;
    };
    !tail.is_empty()
      && tail.chars().next().is_some_and(|c| c.is_ascii_digit())
      && tail.chars().all(|c| c.is_ascii_digit())
      && tail.len() >= 2
  }
}

impl fmt::Display for PackageId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::str::FromStr for PackageId {
  type Err = IdError;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Self::new(s)
  }
}

impl AsRef<str> for PackageId {
  fn as_ref(&self) -> &str {
    &self.0
  }
}

impl<'de> Deserialize<'de> for PackageId {
  fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
    let raw = String::deserialize(de)?;
    Self::new(raw).map_err(serde::de::Error::custom)
  }
}

impl schemars::JsonSchema for PackageId {
  fn schema_name() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("PackageId")
  }
  fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "pattern": r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$",
        "maxLength": PackageId::MAX_LEN,
        "description": "Immutable package identifier; never contains a version.",
        "examples": ["surge-xt", "dexed", "dragonfly-reverb"],
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn accepts_realistic_ids() {
    for id in ["surge-xt", "dexed", "sfizz", "lsp-plugins", "vsco2", "a1"] {
      assert!(PackageId::new(id).is_ok(), "{id} should be valid");
    }
  }

  #[test]
  fn rejects_path_traversal_and_separators() {
    // These are the cases that matter: a PackageId becomes a path component.
    for id in [
      "..", "../etc", "a/b", "a\\b", ".hidden", "a b", "Surge-XT", "a.b", "",
    ] {
      assert!(PackageId::new(id).is_err(), "{id:?} must be rejected");
    }
  }

  #[test]
  fn rejects_dash_boundaries_and_overlong_ids() {
    assert!(matches!(
      PackageId::new("-lead"),
      Err(IdError::BadBoundary(_))
    ));
    assert!(matches!(
      PackageId::new("trail-"),
      Err(IdError::BadBoundary(_))
    ));
    assert!(matches!(
      PackageId::new("a".repeat(PackageId::MAX_LEN + 1)),
      Err(IdError::TooLong(_))
    ));
  }

  #[test]
  fn deserialisation_enforces_the_invariant() {
    assert!(serde_json::from_str::<PackageId>("\"../../evil\"").is_err());
    assert_eq!(
      serde_json::from_str::<PackageId>("\"surge-xt\"")
        .unwrap()
        .as_str(),
      "surge-xt"
    );
  }

  #[test]
  fn version_lint_spares_legitimate_trailing_digits() {
    assert!(!PackageId::new("vsco2").unwrap().looks_versioned());
    assert!(!PackageId::new("odin2").unwrap().looks_versioned());
    assert!(!PackageId::new("surge-xt").unwrap().looks_versioned());
    assert!(PackageId::new("surge-xt-134").unwrap().looks_versioned());
  }
}
