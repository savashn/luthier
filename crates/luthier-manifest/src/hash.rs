//! Content hashes.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A SHA-256 digest, stored as 32 raw bytes.
///
/// Parsing is strict and case-insensitive on input but always renders as lower
/// hex, so a digest read from a manifest and one computed from a download
/// compare byte-for-byte with no normalisation left to call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Hash([u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashError {
  #[error("sha256 must be 64 hex characters, got {0}")]
  BadLength(usize),
  #[error("sha256 contains a non-hex character {0:?}")]
  NotHex(char),
}

impl Sha256Hash {
  pub fn from_bytes(bytes: [u8; 32]) -> Self {
    Self(bytes)
  }

  pub fn as_bytes(&self) -> &[u8; 32] {
    &self.0
  }

  pub fn parse(raw: &str) -> Result<Self, HashError> {
    let raw = raw.trim();
    if raw.len() != 64 {
      return Err(HashError::BadLength(raw.len()));
    }
    let mut out = [0u8; 32];
    let bytes = raw.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
      let hi = hex_val(bytes[i * 2] as char)?;
      let lo = hex_val(bytes[i * 2 + 1] as char)?;
      *slot = (hi << 4) | lo;
    }
    Ok(Self(out))
  }

  /// The first `n` hex characters, for compact CLI output.
  pub fn short(&self, n: usize) -> String {
    self.to_string().chars().take(n).collect()
  }
}

fn hex_val(c: char) -> Result<u8, HashError> {
  c.to_digit(16).map(|d| d as u8).ok_or(HashError::NotHex(c))
}

impl fmt::Display for Sha256Hash {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in &self.0 {
      write!(f, "{byte:02x}")?;
    }
    Ok(())
  }
}

impl std::str::FromStr for Sha256Hash {
  type Err = HashError;
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Self::parse(s)
  }
}

impl Serialize for Sha256Hash {
  fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&self.to_string())
  }
}

impl<'de> Deserialize<'de> for Sha256Hash {
  fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
    let raw = String::deserialize(de)?;
    Self::parse(&raw).map_err(serde::de::Error::custom)
  }
}

impl schemars::JsonSchema for Sha256Hash {
  fn schema_name() -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Borrowed("Sha256Hash")
  }
  fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "pattern": "^[0-9a-fA-F]{64}$",
        "description": "Lowercase hex SHA-256 digest of the artifact.",
    })
  }
}

/// The integrity information attached to an artifact.
///
/// A struct rather than a bare string so that additional algorithms (or an
/// Ed25519 signature, §14) can be added without a schema break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Checksum {
  pub sha256: Sha256Hash,
}

#[cfg(test)]
mod tests {
  use super::*;

  const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";

  #[test]
  fn round_trips_lowercase_hex() {
    let text = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let h = Sha256Hash::parse(text).unwrap();
    assert_eq!(h.to_string(), text);
    assert_eq!(h.short(8), "e3b0c442");
  }

  #[test]
  fn uppercase_input_normalises() {
    let lower = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(
      Sha256Hash::parse(&lower.to_uppercase()).unwrap(),
      Sha256Hash::parse(lower).unwrap()
    );
  }

  #[test]
  fn rejects_malformed_digests() {
    assert_eq!(
      Sha256Hash::parse("abc").unwrap_err(),
      HashError::BadLength(3)
    );
    let bad = format!("{}zz", &ZERO[..62]);
    assert_eq!(Sha256Hash::parse(&bad).unwrap_err(), HashError::NotHex('z'));
    assert!(serde_json::from_str::<Sha256Hash>("\"not-a-hash\"").is_err());
  }
}
