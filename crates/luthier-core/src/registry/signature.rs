//! Ed25519 signatures over a registry snapshot.
//!
//! Every artifact is verified against a checksum in a manifest, and every
//! manifest arrives in a snapshot. Until this existed, that snapshot arrived
//! on the strength of HTTPS and a pinned origin, which answers "is this the
//! host I fetched from before" but not "did the people who maintain this
//! bench publish it" — the question a compromised forge account makes real.
//!
//! What is signed is the snapshot's SHA-256 digest, not its bytes. That is
//! the digest `provenance.rs` already records, so verification needs no
//! second pass over a tarball and the recorded digest becomes the thing a key
//! vouches for, which is what ROADMAP 2.1 left open. The digest is prefixed
//! with a context string before signing, so a signature over a snapshot can
//! never be replayed as a signature over something else this project might
//! sign later.
//!
//! Hex rather than base64, for keys and signatures alike: a checksum in a
//! manifest is already hex, and one encoding is one fewer thing to get wrong
//! when a maintainer reads a key out of an announcement.
//!
//! Signing lives here too, beside verification, although the manager never
//! signs anything. One implementation of the file format means the tool that
//! writes it and the manager that reads it cannot drift, which is the same
//! reason `install::derive` is shared rather than reimplemented.

use crate::error::{Error, RegistryError, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use luthier_manifest::Sha256Hash;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Prepended to the digest before signing, so a signature is good for this
/// purpose and no other.
const CONTEXT: &[u8] = b"luthier-registry-snapshot-v1\0";

/// The only algorithm this build understands. Written into the file so a
/// later one can be added without guessing what an old file meant.
const ALGORITHM: &str = "ed25519";

/// What a signature covers: the context, then the raw digest.
fn message(digest: &Sha256Hash) -> [u8; CONTEXT.len() + 32] {
  let mut out = [0u8; CONTEXT.len() + 32];
  out[..CONTEXT.len()].copy_from_slice(CONTEXT);
  out[CONTEXT.len()..].copy_from_slice(digest.as_bytes());
  out
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
  #[error("{field} must be {expected} hex characters, got {actual}")]
  BadLength {
    field: &'static str,
    expected: usize,
    actual: usize,
  },
  #[error("{field} contains a non-hex character {character:?}")]
  NotHex {
    field: &'static str,
    character: char,
  },
  #[error("{0} is not a point on the curve")]
  NotAKey(&'static str),
  #[error("the signature file names algorithm {0:?}; this build verifies ed25519")]
  UnknownAlgorithm(String),
  #[error("the signature file has no {0} line")]
  Missing(&'static str),
  #[error("unexpected line {0:?} in the signature file")]
  Unexpected(String),
}

/// Parses `len` bytes of lower or upper hex.
fn from_hex<const N: usize>(field: &'static str, raw: &str) -> Result2<[u8; N]> {
  let raw = raw.trim();
  if raw.len() != N * 2 {
    return Err(SignatureError::BadLength {
      field,
      expected: N * 2,
      actual: raw.len(),
    });
  }
  let bytes = raw.as_bytes();
  let mut out = [0u8; N];
  for (i, slot) in out.iter_mut().enumerate() {
    let hi = hex_val(field, bytes[i * 2] as char)?;
    let lo = hex_val(field, bytes[i * 2 + 1] as char)?;
    *slot = (hi << 4) | lo;
  }
  Ok(out)
}

fn hex_val(field: &'static str, c: char) -> Result2<u8> {
  c.to_digit(16)
    .map(|d| d as u8)
    .ok_or(SignatureError::NotHex {
      field,
      character: c,
    })
}

fn to_hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

type Result2<T> = std::result::Result<T, SignatureError>;

// ------------------------------------------------------------ public keys --

/// An Ed25519 public key, written as 64 hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
  pub fn parse(raw: &str) -> Result2<Self> {
    let bytes: [u8; 32] = from_hex("a public key", raw)?;
    // Refused here rather than at verification time: a key that is not a
    // key cannot be configured, so `bench trust` rejects a typo at the
    // moment someone can still fix it.
    VerifyingKey::from_bytes(&bytes).map_err(|_| SignatureError::NotAKey("a public key"))?;
    Ok(Self(bytes))
  }

  /// The first `n` hex characters, for output that has to fit a line.
  pub fn short(&self, n: usize) -> String {
    self.to_string().chars().take(n).collect()
  }

  fn verifying(&self) -> Result2<VerifyingKey> {
    VerifyingKey::from_bytes(&self.0).map_err(|_| SignatureError::NotAKey("a public key"))
  }
}

impl fmt::Display for PublicKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&to_hex(&self.0))
  }
}

impl fmt::Debug for PublicKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "PublicKey({self})")
  }
}

impl std::str::FromStr for PublicKey {
  type Err = SignatureError;
  fn from_str(s: &str) -> Result2<Self> {
    Self::parse(s)
  }
}

impl Serialize for PublicKey {
  fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
    ser.serialize_str(&self.to_string())
  }
}

impl<'de> Deserialize<'de> for PublicKey {
  fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
    let raw = String::deserialize(de)?;
    Self::parse(&raw).map_err(serde::de::Error::custom)
  }
}

// ------------------------------------------------------------ secret keys --

/// A signing key: 32 bytes of seed, held only by whoever publishes a bench.
///
/// The manager never has one. It is here so that the format has a single
/// implementation, and so the test suite can produce a genuinely signed
/// snapshot rather than a fixture that asserts its own expectations.
pub struct SecretKey(SigningKey);

impl SecretKey {
  /// A new key from the platform's CSPRNG.
  pub fn generate() -> Result<Self> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| {
      Error::InvalidArgument(format!("cannot read random bytes for a new key: {e}"))
    })?;
    Ok(Self(SigningKey::from_bytes(&seed)))
  }

  pub fn parse(raw: &str) -> Result2<Self> {
    let seed: [u8; 32] = from_hex("a secret key", raw)?;
    Ok(Self(SigningKey::from_bytes(&seed)))
  }

  /// The seed, as 64 hex characters. What a key file holds.
  pub fn to_hex(&self) -> String {
    to_hex(&self.0.to_bytes())
  }

  pub fn public(&self) -> PublicKey {
    PublicKey(self.0.verifying_key().to_bytes())
  }

  /// Signs a snapshot, identified by its digest.
  pub fn sign(&self, digest: &Sha256Hash) -> SignatureFile {
    SignatureFile {
      key: self.public(),
      signature: self.0.sign(&message(digest)).to_bytes(),
    }
  }
}

// -------------------------------------------------------- signature files --

/// A detached signature, as published beside a snapshot.
///
/// The file is plain text so that a maintainer can read one:
///
/// ```text
/// # luthier registry signature
/// algorithm ed25519
/// key 7c4d24d1f4a2c3586a3e0572bc2435cc76fa22e98a9680203c46fabd6aef6e49
/// signature 4b615019...d55ab8be32cd5f6f2ece4bc81f63679516e5290e846090d849c2289b8649a3a35404
/// ```
///
/// It carries the public key as well as the signature. That is what makes the
/// first fetch of a bench worth anything: there is nothing yet to check the
/// signature against, so the key it names is pinned, exactly as the origin
/// is, and every later refresh has to match it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureFile {
  pub key: PublicKey,
  signature: [u8; 64],
}

impl SignatureFile {
  pub fn parse(text: &str) -> Result2<Self> {
    let mut algorithm: Option<String> = None;
    let mut key: Option<PublicKey> = None;
    let mut signature: Option<[u8; 64]> = None;

    for line in text.lines() {
      let line = line.trim();
      if line.is_empty() || line.starts_with('#') {
        continue;
      }
      let (field, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
      match field {
        "algorithm" => algorithm = Some(value.trim().to_owned()),
        "key" => key = Some(PublicKey::parse(value)?),
        "signature" => signature = Some(from_hex("a signature", value)?),
        // Strict, like the manifest parser: an unknown line is a typo
        // until proven otherwise, and a signature file is not the place
        // to be relaxed about what a document says.
        _ => return Err(SignatureError::Unexpected(field.to_owned())),
      }
    }

    match algorithm.as_deref() {
      Some(ALGORITHM) => {}
      Some(other) => return Err(SignatureError::UnknownAlgorithm(other.to_owned())),
      None => return Err(SignatureError::Missing("algorithm")),
    }

    Ok(Self {
      key: key.ok_or(SignatureError::Missing("key"))?,
      signature: signature.ok_or(SignatureError::Missing("signature"))?,
    })
  }

  pub fn render(&self) -> String {
    format!(
      "# luthier registry signature\nalgorithm {ALGORITHM}\nkey {}\nsignature {}\n",
      self.key,
      to_hex(&self.signature)
    )
  }

  /// Whether this signature is good for `digest` under the key it names.
  ///
  /// Says nothing about whether that key is one to trust — that is
  /// [`check`]'s job, and keeping the two apart is what stops a file from
  /// vouching for itself.
  pub fn verifies(&self, digest: &Sha256Hash) -> bool {
    let Ok(key) = self.key.verifying() else {
      return false;
    };
    let signature = ed25519_dalek::Signature::from_bytes(&self.signature);
    // `verify_strict` rather than `verify`: it rejects small-order keys and
    // non-canonical encodings, and signature malleability is not a property
    // worth having in something that decides whether to trust a registry.
    key.verify_strict(&message(digest), &signature).is_ok()
  }
}

// ------------------------------------------------------------------ policy --

/// What a refresh must prove about the snapshot it just downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
  /// One of these keys must have signed it. They come from the bench's
  /// configuration, or from the key pinned on a previous refresh.
  Signed(Vec<PublicKey>),
  /// Nothing is pinned and nothing is configured. A signature, if one is
  /// served, pins the key it names for every refresh after this one.
  FirstUse,
}

/// What the check concluded, and what should be recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
  Signed(PublicKey),
  Unsigned,
}

impl Verdict {
  pub fn key(&self) -> Option<PublicKey> {
    match self {
      Verdict::Signed(key) => Some(*key),
      Verdict::Unsigned => None,
    }
  }
}

/// Decides whether a downloaded snapshot may be used.
///
/// Called after the download and before extraction: a snapshot that fails
/// this is never opened, let alone allowed to replace the one already there.
///
/// `allow_unsigned` covers exactly one case — a bench that was signed before
/// and served nothing this time. It does not cover a signature that fails to
/// verify or one made with a key nobody trusts: those are not the absence of
/// a claim, they are a claim that did not hold up, and no flag should be able
/// to wave one through.
pub fn check(
  registry: &str,
  requirement: &Requirement,
  served: Option<&str>,
  digest: &Sha256Hash,
  allow_unsigned: bool,
) -> Result<Verdict> {
  let Some(text) = served else {
    return match requirement {
      Requirement::FirstUse => Ok(Verdict::Unsigned),
      Requirement::Signed(_) if allow_unsigned => {
        tracing::warn!(
          registry,
          "accepting an unsigned snapshot because --allow-unsigned was given"
        );
        Ok(Verdict::Unsigned)
      }
      Requirement::Signed(_) => Err(Error::Registry(RegistryError::SignatureMissing {
        registry: registry.to_owned(),
      })),
    };
  };

  let file = SignatureFile::parse(text).map_err(|e| {
    Error::Registry(RegistryError::SignatureMalformed {
      registry: registry.to_owned(),
      reason: e.to_string(),
    })
  })?;

  if !file.verifies(digest) {
    return Err(Error::Registry(RegistryError::SignatureInvalid {
      registry: registry.to_owned(),
      key: file.key.to_string(),
    }));
  }

  match requirement {
    // Trust on first use, applied to the key: there is nothing yet to check
    // it against, so what this buys is that every refresh after it has
    // something to check against.
    Requirement::FirstUse => Ok(Verdict::Signed(file.key)),
    Requirement::Signed(trusted) if trusted.contains(&file.key) => Ok(Verdict::Signed(file.key)),
    Requirement::Signed(trusted) => Err(Error::Registry(RegistryError::SignatureUntrusted {
      registry: registry.to_owned(),
      key: file.key.to_string(),
      trusted: trusted.iter().map(ToString::to_string).collect(),
    })),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn digest(byte: u8) -> Sha256Hash {
    Sha256Hash::from_bytes([byte; 32])
  }

  #[test]
  fn a_signature_round_trips_through_the_file_format() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&digest(1));
    let parsed = SignatureFile::parse(&signed.render()).unwrap();

    assert_eq!(parsed, signed);
    assert_eq!(parsed.key, secret.public());
    assert!(parsed.verifies(&digest(1)));
  }

  #[test]
  fn a_signature_is_good_for_one_snapshot_only() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&digest(1));
    // The case this exists for: the same bench, a different snapshot.
    assert!(!signed.verifies(&digest(2)));
  }

  #[test]
  fn another_key_does_not_verify() {
    let secret = SecretKey::generate().unwrap();
    let other = SecretKey::generate().unwrap();
    let mut forged = secret.sign(&digest(1));
    forged.key = other.public();
    assert!(!forged.verifies(&digest(1)));
  }

  #[test]
  fn a_malformed_file_is_refused_rather_than_half_read() {
    let secret = SecretKey::generate().unwrap();
    let good = secret.sign(&digest(1)).render();

    assert!(matches!(
      SignatureFile::parse(&good.replace("ed25519", "rsa")),
      Err(SignatureError::UnknownAlgorithm(_))
    ));
    assert!(matches!(
      SignatureFile::parse("algorithm ed25519\nkey aa\n"),
      Err(SignatureError::BadLength { .. })
    ));
    assert!(matches!(
      SignatureFile::parse(
        &good
          .lines()
          .filter(|l| !l.starts_with("signature"))
          .collect::<Vec<_>>()
          .join("\n")
      ),
      Err(SignatureError::Missing("signature"))
    ));
    assert!(matches!(
      SignatureFile::parse(&format!("{good}destination /etc\n")),
      Err(SignatureError::Unexpected(_))
    ));
  }

  #[test]
  fn comments_and_blank_lines_are_ignored() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&digest(3));
    let decorated = format!("# published 2026-09-22\n\n{}\n\n", signed.render());
    assert_eq!(SignatureFile::parse(&decorated).unwrap(), signed);
  }

  #[test]
  fn a_key_that_is_not_a_key_is_refused_where_it_is_written() {
    assert!(matches!(
      PublicKey::parse("nothex"),
      Err(SignatureError::BadLength { .. })
    ));
    assert!(matches!(
      PublicKey::parse(&"zz".repeat(32)),
      Err(SignatureError::NotHex { .. })
    ));
    let key = SecretKey::generate().unwrap().public();
    assert_eq!(PublicKey::parse(&key.to_string()).unwrap(), key);
    // Uppercase is a normal way to paste a key out of an announcement.
    assert_eq!(
      PublicKey::parse(&key.to_string().to_uppercase()).unwrap(),
      key
    );
  }

  #[test]
  fn the_first_fetch_pins_whatever_key_signed_it() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&digest(1)).render();
    let verdict = check(
      "bench",
      &Requirement::FirstUse,
      Some(&signed),
      &digest(1),
      false,
    )
    .unwrap();
    assert_eq!(verdict, Verdict::Signed(secret.public()));
  }

  #[test]
  fn an_unsigned_snapshot_is_fine_until_a_signed_one_arrives() {
    assert_eq!(
      check("bench", &Requirement::FirstUse, None, &digest(1), false).unwrap(),
      Verdict::Unsigned
    );

    let err = check(
      "bench",
      &Requirement::Signed(vec![SecretKey::generate().unwrap().public()]),
      None,
      &digest(1),
      false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unsigned"), "{err}");
  }

  #[test]
  fn allow_unsigned_covers_absence_and_nothing_else() {
    let trusted = SecretKey::generate().unwrap();
    let requirement = Requirement::Signed(vec![trusted.public()]);

    // Absence, deliberately accepted.
    assert_eq!(
      check("bench", &requirement, None, &digest(1), true).unwrap(),
      Verdict::Unsigned
    );

    // A signature that does not hold up is a claim that failed, not an
    // absent one, and the flag does not touch it.
    let wrong = trusted.sign(&digest(2)).render();
    let err = check("bench", &requirement, Some(&wrong), &digest(1), true).unwrap_err();
    assert!(err.to_string().contains("does not verify"), "{err}");

    // Nor does it accept a stranger's key.
    let stranger = SecretKey::generate().unwrap();
    let signed = stranger.sign(&digest(1)).render();
    let err = check("bench", &requirement, Some(&signed), &digest(1), true).unwrap_err();
    assert!(err.to_string().contains("not one of the keys"), "{err}");
  }

  #[test]
  fn a_second_trusted_key_is_what_makes_a_rotation_possible() {
    // The overlap a rotation needs: the new key is trusted before the old
    // one stops being used, so no refresh falls between the two.
    let old = SecretKey::generate().unwrap();
    let new = SecretKey::generate().unwrap();
    let requirement = Requirement::Signed(vec![old.public(), new.public()]);

    for key in [&old, &new] {
      let signed = key.sign(&digest(7)).render();
      assert_eq!(
        check("bench", &requirement, Some(&signed), &digest(7), false).unwrap(),
        Verdict::Signed(key.public())
      );
    }
  }
}
