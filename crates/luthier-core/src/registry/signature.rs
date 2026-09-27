//! Detached signatures over a registry snapshot, in minisign's format.
//!
//! Every artifact is verified against a checksum in a manifest, and every
//! manifest arrives in a snapshot. Until this existed, that snapshot arrived
//! on the strength of HTTPS and a pinned origin, which answers "is this the
//! host I fetched from before" but not "did the people who maintain this
//! bench publish it" — the question a compromised forge account makes real.
//!
//! The format is [minisign](https://jedisct1.github.io/minisign/)'s, byte for
//! byte, so a signature this project publishes can be checked with
//! `minisign -V` by anyone who does not want to take the manager's word for
//! it, and a bench maintainer can sign with stock minisign instead of
//! `luthier-registry sign`. What is signed is the snapshot's BLAKE2b-512, which
//! minisign calls a prehashed (`ED`) signature; the older unhashed `Ed` form is
//! refused, since nothing here has ever written one. The trusted comment is
//! signed too, and verified, as minisign does.
//!
//! A minisign signature names its key only by an 8-byte ID, not by the key
//! itself, so a signature can never vouch for itself: the key comes from the
//! manager. Only the default bench signs, and its key is built in
//! ([`DEFAULT_BENCH_KEY`](crate::config::DEFAULT_BENCH_KEY)); a source with
//! no key — the Open Audio Stack registry, or a `--registry-path` checkout —
//! is read as unsigned.
//!
//! Keys written by 0.1 as 64 hex characters still parse, as provenance
//! records hold them, and are given the ID [`legacy_id`] derives. The default
//! bench's key was converted the same way, so the key 0.1 recorded and the
//! one this build carries are equal. [`legacy`] keeps the format 0.1 reads,
//! for publishing beside the minisign file until no 0.1 client is left; the
//! manager itself no longer reads it.
//!
//! Signing lives here too, beside verification, although the manager never
//! signs anything. One implementation of the file format means the tool that
//! writes it and the manager that reads it cannot drift, which is the same
//! reason `install::derive` is shared rather than reimplemented — and the
//! tests hold that implementation to minisign's own output.

use crate::error::{Error, RegistryError, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Blake2b512, Digest};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Read;
use std::path::Path;

/// What a public key and a secret key are for: Ed25519.
const KEY_ALGORITHM: &[u8; 2] = b"Ed";
/// A signature over the BLAKE2b-512 of the file rather than the file itself.
const PREHASHED: &[u8; 2] = b"ED";
/// A secret key stored unencrypted, as `minisign -G -W` writes one.
const KDF_NONE: [u8; 2] = [0, 0];
const CHECKSUM_ALGORITHM: &[u8; 2] = b"B2";

const UNTRUSTED: &str = "untrusted comment: ";
const TRUSTED: &str = "trusted comment: ";

/// minisign's 8-byte key ID.
pub type KeyId = [u8; 8];

/// A snapshot's BLAKE2b-512: what a prehashed signature covers.
pub type Prehash = [u8; 64];

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
  #[error("{0} is not valid base64")]
  NotBase64(&'static str),
  #[error("{field} decodes to {actual} bytes where minisign uses {expected}")]
  BadSize {
    field: &'static str,
    expected: usize,
    actual: usize,
  },
  #[error("{0} is not a point on the curve")]
  NotAKey(&'static str),
  #[error("{what} uses algorithm {found:?}; this build reads {expected:?}")]
  UnknownAlgorithm {
    what: &'static str,
    found: String,
    expected: &'static str,
  },
  #[error("the {0} line is missing")]
  Missing(&'static str),
  #[error("unexpected line {0:?}")]
  Unexpected(String),
  #[error(
    "the secret key is encrypted; this tool reads only unencrypted keys, as \
     `minisign -G -W` writes them"
  )]
  Encrypted,
  #[error("the secret key does not match the public key stored with it")]
  Inconsistent,
}

type Result2<T> = std::result::Result<T, SignatureError>;

/// Parses `N` bytes of lower or upper hex.
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

fn is_hex(raw: &str) -> bool {
  raw.chars().all(|c| c.is_ascii_hexdigit())
}

/// Decodes base64 that must come to exactly `N` bytes.
fn from_base64<const N: usize>(field: &'static str, raw: &str) -> Result2<[u8; N]> {
  let bytes = BASE64
    .decode(raw.trim())
    .map_err(|_| SignatureError::NotBase64(field))?;
  bytes
    .try_into()
    .map_err(|b: Vec<u8>| SignatureError::BadSize {
      field,
      expected: N,
      actual: b.len(),
    })
}

/// The one line of a minisign key file that is not a comment.
///
/// Accepting the whole file as well as the bare line means a maintainer can
/// paste either out of an announcement.
fn key_line<'a>(field: &'static str, raw: &'a str) -> Result2<&'a str> {
  let mut lines = raw
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty() && !l.starts_with(UNTRUSTED.trim_end()));
  let line = lines.next().ok_or(SignatureError::Missing(field))?;
  if let Some(extra) = lines.next() {
    return Err(SignatureError::Unexpected(extra.to_owned()));
  }
  Ok(line)
}

/// minisign prints a key ID as the little-endian integer it stores, in hex.
fn id_hex(id: &KeyId) -> String {
  id.iter().rev().map(|b| format!("{b:02X}")).collect()
}

/// The ID given to a key that arrived as 0.1's bare hex, which carries none.
///
/// Derived rather than random so that the same key always gets the same ID:
/// that is what lets a key 0.1 pinned in a provenance record equal the
/// minisign form of it this build carries.
pub fn legacy_id(key: &[u8; 32]) -> KeyId {
  let digest = Blake2b512::digest(key);
  let mut id = [0u8; 8];
  id.copy_from_slice(&digest[..8]);
  id
}

/// The BLAKE2b-512 of `bytes`.
pub fn prehash(bytes: &[u8]) -> Prehash {
  Blake2b512::digest(bytes).into()
}

/// The BLAKE2b-512 of a file, read in pieces.
pub fn prehash_file(path: &Path) -> Result<Prehash> {
  let mut file = std::fs::File::open(path).map_err(|e| Error::io("open", path, e))?;
  let mut hasher = Blake2b512::new();
  let mut buffer = [0u8; 64 * 1024];
  loop {
    let n = file
      .read(&mut buffer)
      .map_err(|e| Error::io("read", path, e))?;
    if n == 0 {
      break;
    }
    hasher.update(&buffer[..n]);
  }
  Ok(hasher.finalize().into())
}

// ------------------------------------------------------------ public keys --

/// An Ed25519 public key and its minisign ID, written as minisign writes it:
/// `RW` and 54 more base64 characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey {
  id: KeyId,
  key: [u8; 32],
}

impl PublicKey {
  /// Reads a key in minisign's form — the bare line or the whole `.pub`
  /// file — or 0.1's 64 hex characters.
  pub fn parse(raw: &str) -> Result2<Self> {
    let line = key_line("public key", raw)?;
    let (id, key) = if line.len() == 64 && is_hex(line) {
      let key: [u8; 32] = from_hex("a public key", line)?;
      (legacy_id(&key), key)
    } else {
      let bytes: [u8; 42] = from_base64("a public key", line)?;
      if &bytes[..2] != KEY_ALGORITHM {
        return Err(SignatureError::UnknownAlgorithm {
          what: "the public key",
          found: String::from_utf8_lossy(&bytes[..2]).into_owned(),
          expected: "Ed",
        });
      }
      let mut id = [0u8; 8];
      id.copy_from_slice(&bytes[2..10]);
      let mut key = [0u8; 32];
      key.copy_from_slice(&bytes[10..]);
      (id, key)
    };
    // Refused here rather than at verification time, so a typo in a key is
    // caught where it is written rather than on some later refresh.
    VerifyingKey::from_bytes(&key).map_err(|_| SignatureError::NotAKey("a public key"))?;
    Ok(Self { id, key })
  }

  pub fn id(&self) -> KeyId {
    self.id
  }

  /// The ID as minisign prints it.
  pub fn id_hex(&self) -> String {
    id_hex(&self.id)
  }

  /// The first `n` characters, for output that has to fit a line.
  pub fn short(&self, n: usize) -> String {
    self.to_string().chars().take(n).collect()
  }

  /// The key alone, as 0.1 wrote it.
  pub fn to_hex(&self) -> String {
    to_hex(&self.key)
  }

  /// The contents of a minisign `.pub` file.
  pub fn to_file(&self) -> String {
    format!("{UNTRUSTED}minisign public key {}\n{self}\n", self.id_hex())
  }

  fn verifying(&self) -> Result2<VerifyingKey> {
    VerifyingKey::from_bytes(&self.key).map_err(|_| SignatureError::NotAKey("a public key"))
  }
}

impl fmt::Display for PublicKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut bytes = [0u8; 42];
    bytes[..2].copy_from_slice(KEY_ALGORITHM);
    bytes[2..10].copy_from_slice(&self.id);
    bytes[10..].copy_from_slice(&self.key);
    f.write_str(&BASE64.encode(bytes))
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

/// A signing key, held only by whoever publishes a bench.
///
/// The manager never has one. It is here so that the format has a single
/// implementation, and so the test suite can produce a genuinely signed
/// snapshot rather than a fixture that asserts its own expectations.
pub struct SecretKey {
  id: KeyId,
  signing: SigningKey,
}

impl SecretKey {
  /// A new key and ID from the platform's CSPRNG.
  pub fn generate() -> Result<Self> {
    let mut bytes = [0u8; 40];
    getrandom::fill(&mut bytes).map_err(|e| {
      Error::InvalidArgument(format!("cannot read random bytes for a new key: {e}"))
    })?;
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes[..32]);
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[32..]);
    Ok(Self {
      id,
      signing: SigningKey::from_bytes(&seed),
    })
  }

  /// Reads an unencrypted minisign secret key file, or 0.1's 64 hex
  /// characters of seed.
  pub fn parse(raw: &str) -> Result2<Self> {
    let line = key_line("secret key", raw)?;
    if line.len() == 64 && is_hex(line) {
      let signing = SigningKey::from_bytes(&from_hex("a secret key", line)?);
      let id = legacy_id(&signing.verifying_key().to_bytes());
      return Ok(Self { id, signing });
    }

    // sig_alg, kdf_alg, chk_alg, salt, opslimit, memlimit, then the key ID,
    // the 64-byte secret key (seed, then public key) and a checksum.
    let bytes: [u8; 158] = from_base64("a secret key", line)?;
    if &bytes[..2] != KEY_ALGORITHM {
      return Err(SignatureError::UnknownAlgorithm {
        what: "the secret key",
        found: String::from_utf8_lossy(&bytes[..2]).into_owned(),
        expected: "Ed",
      });
    }
    if bytes[2..4] != KDF_NONE {
      return Err(SignatureError::Encrypted);
    }
    if &bytes[4..6] != CHECKSUM_ALGORITHM {
      return Err(SignatureError::UnknownAlgorithm {
        what: "the secret key's checksum",
        found: String::from_utf8_lossy(&bytes[4..6]).into_owned(),
        expected: "B2",
      });
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[54..62]);
    let secret = &bytes[62..126];
    // minisign leaves the checksum zeroed in an unencrypted key; where one is
    // present it has to agree.
    let checksum = &bytes[126..158];
    if checksum.iter().any(|&b| b != 0) && checksum != Self::checksum(&id, secret).as_slice() {
      return Err(SignatureError::Inconsistent);
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&secret[..32]);
    let signing = SigningKey::from_bytes(&seed);
    if signing.verifying_key().to_bytes() != secret[32..] {
      return Err(SignatureError::Inconsistent);
    }
    Ok(Self { id, signing })
  }

  fn checksum(id: &KeyId, secret: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(KEY_ALGORITHM);
    hasher.update(id);
    hasher.update(secret);
    hasher.finalize().into()
  }

  /// The contents of an unencrypted minisign secret key file, the same bytes
  /// `minisign -G -W` writes.
  pub fn to_file(&self) -> String {
    let mut bytes = [0u8; 158];
    bytes[..2].copy_from_slice(KEY_ALGORITHM);
    bytes[2..4].copy_from_slice(&KDF_NONE);
    bytes[4..6].copy_from_slice(CHECKSUM_ALGORITHM);
    bytes[54..62].copy_from_slice(&self.id);
    bytes[62..126].copy_from_slice(&self.signing.to_keypair_bytes());
    format!(
      "{UNTRUSTED}minisign secret key {}\n{}\n",
      id_hex(&self.id),
      BASE64.encode(bytes)
    )
  }

  pub fn public(&self) -> PublicKey {
    PublicKey {
      id: self.id,
      key: self.signing.verifying_key().to_bytes(),
    }
  }

  /// Signs a file, identified by its BLAKE2b-512, with minisign's trusted
  /// comment for it: when, and under what name.
  pub fn sign(&self, prehash: &Prehash, file_name: &str, timestamp: i64) -> Minisig {
    self.sign_with_comment(
      prehash,
      &format!("timestamp:{timestamp}\tfile:{file_name}\thashed"),
    )
  }

  pub fn sign_with_comment(&self, prehash: &Prehash, trusted_comment: &str) -> Minisig {
    let signature = self.signing.sign(prehash).to_bytes();
    let global = self
      .signing
      .sign(&Minisig::global_message(&signature, trusted_comment))
      .to_bytes();
    Minisig {
      untrusted_comment: "signature from minisign secret key".to_owned(),
      key_id: self.id,
      signature,
      trusted_comment: trusted_comment.to_owned(),
      global,
    }
  }

  /// Signs in the format 0.1 reads, for publishing beside the minisign file.
  pub fn sign_legacy(&self, digest: &luthier_manifest::Sha256Hash) -> legacy::SignatureFile {
    legacy::SignatureFile::sign(&self.signing, self.public(), digest)
  }
}

// -------------------------------------------------------- signature files --

/// A detached signature in minisign's format, as published beside a
/// snapshot with `.minisig` on the end (the trusted comment's fields are
/// separated by tabs):
///
/// ```text
/// untrusted comment: signature from minisign secret key
/// RUTIP+H7i3W+zG2l...
/// trusted comment: timestamp:1790527220 file:bench.tar.gz hashed
/// gjO5khgoqZ7+AHzb...
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minisig {
  untrusted_comment: String,
  key_id: KeyId,
  signature: [u8; 64],
  trusted_comment: String,
  global: [u8; 64],
}

impl Minisig {
  pub fn parse(text: &str) -> Result2<Self> {
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
    let untrusted_comment = lines
      .next()
      .and_then(|l| l.strip_prefix(UNTRUSTED))
      .ok_or(SignatureError::Missing("untrusted comment"))?
      .to_owned();

    let bytes: [u8; 74] = from_base64(
      "the signature",
      lines.next().ok_or(SignatureError::Missing("signature"))?,
    )?;
    if &bytes[..2] != PREHASHED {
      return Err(SignatureError::UnknownAlgorithm {
        what: "the signature",
        found: String::from_utf8_lossy(&bytes[..2]).into_owned(),
        expected: "ED",
      });
    }
    let mut key_id = [0u8; 8];
    key_id.copy_from_slice(&bytes[2..10]);
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&bytes[10..]);

    let trusted_comment = lines
      .next()
      .and_then(|l| l.strip_prefix(TRUSTED))
      .ok_or(SignatureError::Missing("trusted comment"))?
      .to_owned();
    let global: [u8; 64] = from_base64(
      "the comment signature",
      lines
        .next()
        .ok_or(SignatureError::Missing("comment signature"))?,
    )?;

    // Strict, like the manifest parser: anything after the four lines is a
    // typo or an addition until proven otherwise, and a signature file is
    // not the place to be relaxed about what a document says.
    if let Some(extra) = lines.find(|l| !l.trim().is_empty()) {
      return Err(SignatureError::Unexpected(extra.to_owned()));
    }

    Ok(Self {
      untrusted_comment,
      key_id,
      signature,
      trusted_comment,
      global,
    })
  }

  pub fn render(&self) -> String {
    let mut bytes = [0u8; 74];
    bytes[..2].copy_from_slice(PREHASHED);
    bytes[2..10].copy_from_slice(&self.key_id);
    bytes[10..].copy_from_slice(&self.signature);
    format!(
      "{UNTRUSTED}{}\n{}\n{TRUSTED}{}\n{}\n",
      self.untrusted_comment,
      BASE64.encode(bytes),
      self.trusted_comment,
      BASE64.encode(self.global)
    )
  }

  pub fn key_id(&self) -> KeyId {
    self.key_id
  }

  pub fn key_id_hex(&self) -> String {
    id_hex(&self.key_id)
  }

  pub fn trusted_comment(&self) -> &str {
    &self.trusted_comment
  }

  /// The global signature covers the file's signature and the trusted
  /// comment together, so the comment cannot be swapped between files.
  fn global_message(signature: &[u8; 64], trusted_comment: &str) -> Vec<u8> {
    let mut message = signature.to_vec();
    message.extend_from_slice(trusted_comment.as_bytes());
    message
  }

  /// Whether `key` made this signature over a file with this BLAKE2b-512,
  /// trusted comment included.
  ///
  /// Says nothing about whether that key is one to trust — that is
  /// [`check`]'s job.
  pub fn verifies(&self, key: &PublicKey, prehash: &Prehash) -> bool {
    if key.id != self.key_id {
      return false;
    }
    let Ok(verifying) = key.verifying() else {
      return false;
    };
    // `verify_strict` rather than `verify`: it rejects small-order keys and
    // non-canonical encodings, and signature malleability is not a property
    // worth having in something that decides whether to trust a registry.
    let signature = ed25519_dalek::Signature::from_bytes(&self.signature);
    let global = ed25519_dalek::Signature::from_bytes(&self.global);
    verifying.verify_strict(prehash, &signature).is_ok()
      && verifying
        .verify_strict(
          &Self::global_message(&self.signature, &self.trusted_comment),
          &global,
        )
        .is_ok()
  }
}

// ------------------------------------------------------------------ policy --

/// What a refresh must prove about the snapshot it just downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
  /// One of these keys must have signed it.
  Signed(Vec<PublicKey>),
  /// No key is known for this bench, so nothing it serves can be checked.
  Unknown,
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
/// `allow_unsigned` covers exactly one case — a bench that is expected to be
/// signed and served nothing this time. It does not cover a signature that
/// fails to verify or one made with a key nobody trusts: those are not the
/// absence of a claim, they are a claim that did not hold up, and no flag
/// should be able to wave one through.
pub fn check(
  registry: &str,
  requirement: &Requirement,
  served: Option<&str>,
  prehash: &Prehash,
  allow_unsigned: bool,
) -> Result<Verdict> {
  let Some(text) = served else {
    return match requirement {
      Requirement::Unknown => Ok(Verdict::Unsigned),
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

  // Parsed whatever the requirement: an unreadable signature is refused even
  // where there is no key to check a readable one against, so publishing
  // garbage is never a way to be read as unsigned.
  let signature = Minisig::parse(text).map_err(|e| {
    Error::Registry(RegistryError::SignatureMalformed {
      registry: registry.to_owned(),
      reason: e.to_string(),
    })
  })?;

  match requirement {
    Requirement::Unknown => {
      tracing::warn!(
        registry,
        key_id = %signature.key_id_hex(),
        "this bench is signed, but this build has no key to check it against; \
         it is read as unsigned"
      );
      Ok(Verdict::Unsigned)
    }
    Requirement::Signed(trusted) => {
      let Some(key) = trusted.iter().find(|k| k.id == signature.key_id) else {
        return Err(Error::Registry(RegistryError::SignatureUntrusted {
          registry: registry.to_owned(),
          key: format!("key ID {}", signature.key_id_hex()),
          trusted: trusted.iter().map(ToString::to_string).collect(),
        }));
      };
      if !signature.verifies(key, prehash) {
        return Err(Error::Registry(RegistryError::SignatureInvalid {
          registry: registry.to_owned(),
          key: key.to_string(),
        }));
      }
      Ok(Verdict::Signed(*key))
    }
  }
}

// ------------------------------------------------------------------ legacy --

/// The signature format 0.1 reads, at `<snapshot>.sig`.
///
/// Nothing in the manager reads it any more. `luthier-registry sign` still
/// writes it beside the minisign file, because a 0.1 client refreshing the
/// default bench requires it and would otherwise refuse every refresh from
/// the first release that stopped publishing it. Delete this module, and the
/// `.sig` upload, once 0.1 is no longer worth keeping working.
pub mod legacy {
  use super::{PublicKey, SignatureError, from_hex, to_hex};
  use ed25519_dalek::{Signer, SigningKey};
  use luthier_manifest::Sha256Hash;

  /// Prepended to the digest before signing, so a signature is good for this
  /// purpose and no other.
  const CONTEXT: &[u8] = b"luthier-registry-snapshot-v1\0";
  const ALGORITHM: &str = "ed25519";

  fn message(digest: &Sha256Hash) -> [u8; CONTEXT.len() + 32] {
    let mut out = [0u8; CONTEXT.len() + 32];
    out[..CONTEXT.len()].copy_from_slice(CONTEXT);
    out[CONTEXT.len()..].copy_from_slice(digest.as_bytes());
    out
  }

  /// `algorithm`, `key` and `signature` lines, all hex, over the snapshot's
  /// SHA-256 with a context string in front.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct SignatureFile {
    pub key: PublicKey,
    signature: [u8; 64],
  }

  impl SignatureFile {
    pub(super) fn sign(signing: &SigningKey, key: PublicKey, digest: &Sha256Hash) -> Self {
      Self {
        key,
        signature: signing.sign(&message(digest)).to_bytes(),
      }
    }

    pub fn render(&self) -> String {
      format!(
        "# luthier registry signature\nalgorithm {ALGORITHM}\nkey {}\nsignature {}\n",
        self.key.to_hex(),
        to_hex(&self.signature)
      )
    }

    /// Reads a file the way 0.1 does, so the tests can hold what is published
    /// to what 0.1 accepts.
    pub fn parse(text: &str) -> Result<Self, SignatureError> {
      let mut algorithm = None;
      let mut key = None;
      let mut signature = None;
      for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
          continue;
        }
        let (field, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match field {
          "algorithm" => algorithm = Some(value.trim().to_owned()),
          "key" => key = Some(PublicKey::parse(value)?),
          "signature" => signature = Some(from_hex("a signature", value)?),
          _ => return Err(SignatureError::Unexpected(field.to_owned())),
        }
      }
      if algorithm.as_deref() != Some(ALGORITHM) {
        return Err(SignatureError::Missing("algorithm"));
      }
      Ok(Self {
        key: key.ok_or(SignatureError::Missing("key"))?,
        signature: signature.ok_or(SignatureError::Missing("signature"))?,
      })
    }

    pub fn verifies(&self, digest: &Sha256Hash) -> bool {
      let Ok(key) = self.key.verifying() else {
        return false;
      };
      let signature = ed25519_dalek::Signature::from_bytes(&self.signature);
      key.verify_strict(&message(digest), &signature).is_ok()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Written by stock minisign 0.12 (`minisign -G -W`, then `minisign -S` over
  // a file holding "hello\n"). A throwaway key, generated for this test and
  // for nothing else; what it pins is that this module reads and writes the
  // same bytes minisign does, not just bytes it agrees with itself about.
  const MINISIGN_PUB: &str = "untrusted comment: minisign public key B2C0818A5F62F981\n\
    RWSB+WJfioHAsvY8Ymo9Hnaml7TOzvB2j9xxtIQFlEKKP/vvDAv2vX+6\n";
  const MINISIGN_KEY: &str = "untrusted comment: minisign encrypted secret key\n\
    RWQAAEIyAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgfliX4qBwLLAJqGdjXT/gGTG7vKN1hVndllN9GizPBy4nZeAYNAZYPY8Ymo9Hnaml7TOzvB2j9xxtIQFlEKKP/vvDAv2vX+6AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n";
  const MINISIGN_SIG: &str = "untrusted comment: signature from minisign secret key\n\
    RUSB+WJfioHAst2X3lCgyJJW6/++jHZ71xYzJkSrFKFuxDGTVaYwDjruaqhzkIIuhLwYqgHXS+Hqet5H0SYHobUa/cqqek1ImQs=\n\
    trusted comment: timestamp:1790527220\tfile:t.txt\thashed\n\
    gjO5khgoqZ7+AHzbwT2XF9mxPbUTFDY1a5zaQiqjaH1SdvluLJcdbBiO0EziLya99mnItMtIX53PCeYJNYs6Bg==\n";
  const SIGNED: &[u8] = b"hello\n";

  fn snapshot(byte: u8) -> Prehash {
    prehash(&[byte; 100])
  }

  #[test]
  fn a_signature_minisign_wrote_verifies() {
    let key = PublicKey::parse(MINISIGN_PUB).unwrap();
    let signature = Minisig::parse(MINISIGN_SIG).unwrap();
    assert_eq!(key.id_hex(), "B2C0818A5F62F981");
    assert!(signature.verifies(&key, &prehash(SIGNED)));
    assert!(!signature.verifies(&key, &prehash(b"hello")));
  }

  #[test]
  fn signing_with_minisigns_key_writes_minisigns_bytes() {
    // Ed25519 is deterministic, so the same key over the same file with the
    // same trusted comment must reproduce the file minisign wrote exactly.
    let secret = SecretKey::parse(MINISIGN_KEY).unwrap();
    assert_eq!(secret.public(), PublicKey::parse(MINISIGN_PUB).unwrap());
    let ours = secret.sign(&prehash(SIGNED), "t.txt", 1790527220);
    assert_eq!(ours.render(), MINISIGN_SIG);
  }

  #[test]
  fn a_key_file_this_writes_is_the_one_minisign_writes() {
    let secret = SecretKey::parse(MINISIGN_KEY).unwrap();
    // The comment line is free text; the key line is what minisign reads.
    assert_eq!(secret.to_file().lines().nth(1), MINISIGN_KEY.lines().nth(1));
    assert_eq!(secret.public().to_file(), MINISIGN_PUB);
  }

  #[test]
  fn minisigns_own_verifier_accepts_what_this_signs() {
    let secret = SecretKey::generate().unwrap();
    let file = b"a bench snapshot";
    let signed = secret.sign(&prehash(file), "bench.tar.gz", 1);

    let key = minisign_verify::PublicKey::from_base64(&secret.public().to_string()).unwrap();
    let signature = minisign_verify::Signature::decode(&signed.render()).unwrap();
    key.verify(file, &signature, false).unwrap();
    assert!(key.verify(b"other bytes", &signature, false).is_err());
  }

  #[test]
  fn a_signature_is_good_for_one_snapshot_only() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&snapshot(1), "bench.tar.gz", 1);
    let parsed = Minisig::parse(&signed.render()).unwrap();
    assert_eq!(parsed, signed);
    assert!(parsed.verifies(&secret.public(), &snapshot(1)));
    // The case this exists for: the same bench, a different snapshot.
    assert!(!parsed.verifies(&secret.public(), &snapshot(2)));
  }

  #[test]
  fn another_key_does_not_verify_even_under_the_same_id() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&snapshot(1), "bench.tar.gz", 1);

    let mut other = SecretKey::generate().unwrap().public();
    assert!(!signed.verifies(&other, &snapshot(1)));
    // An ID is a label, not a credential: claiming the right one buys nothing.
    other.id = secret.id;
    assert!(!signed.verifies(&other, &snapshot(1)));
  }

  #[test]
  fn the_trusted_comment_is_signed_too() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&snapshot(1), "bench.tar.gz", 1).render();
    let edited = signed.replace("timestamp:1", "timestamp:2");
    let parsed = Minisig::parse(&edited).unwrap();
    assert!(!parsed.verifies(&secret.public(), &snapshot(1)));
  }

  #[test]
  fn a_malformed_file_is_refused_rather_than_half_read() {
    let good = SecretKey::generate()
      .unwrap()
      .sign(&snapshot(1), "bench.tar.gz", 1)
      .render();
    let lines: Vec<&str> = good.lines().collect();

    assert!(matches!(
      Minisig::parse(&lines[1..].join("\n")),
      Err(SignatureError::Missing("untrusted comment"))
    ));
    assert!(matches!(
      Minisig::parse(&lines[..3].join("\n")),
      Err(SignatureError::Missing("comment signature"))
    ));
    assert!(matches!(
      Minisig::parse(&format!("{good}destination /etc\n")),
      Err(SignatureError::Unexpected(_))
    ));
    assert!(matches!(
      Minisig::parse(&good.replace(lines[1], "not base64!")),
      Err(SignatureError::NotBase64(_))
    ));
    // Blank lines after the four are what editors leave behind.
    assert!(Minisig::parse(&format!("{good}\n\n")).is_ok());
  }

  #[test]
  fn an_unhashed_legacy_minisign_signature_is_refused() {
    // Same bytes with `Ed` in front: minisign's pre-0.10 form, which signs the
    // whole file rather than its hash. Nothing here writes one.
    let good = Minisig::parse(MINISIGN_SIG).unwrap().render();
    let mut bytes = BASE64.decode(good.lines().nth(1).unwrap()).unwrap();
    bytes[1] = b'd';
    let legacy = good.replace(good.lines().nth(1).unwrap(), &BASE64.encode(bytes));
    assert!(matches!(
      Minisig::parse(&legacy),
      Err(SignatureError::UnknownAlgorithm { .. })
    ));
  }

  #[test]
  fn a_key_parses_bare_whole_or_as_hex_and_refuses_anything_else() {
    let key = PublicKey::parse(MINISIGN_PUB).unwrap();
    assert_eq!(PublicKey::parse(&key.to_string()).unwrap(), key);

    // 0.1's hex, which is how every key configured or pinned before this was
    // written down. It gets the derived ID, so it equals its minisign form
    // only where that form was made with the same derivation.
    let hex = PublicKey::parse(&key.to_hex()).unwrap();
    assert_eq!(hex.key, key.key);
    assert_eq!(hex.id, legacy_id(&key.key));
    assert_eq!(PublicKey::parse(&hex.to_string()).unwrap(), hex);
    assert_eq!(PublicKey::parse(&key.to_hex().to_uppercase()).unwrap(), hex);

    assert!(matches!(
      PublicKey::parse("nothex"),
      Err(SignatureError::NotBase64(_))
    ));
    // Valid base64, but not 42 bytes of it.
    assert!(matches!(
      PublicKey::parse(&"zz".repeat(32)),
      Err(SignatureError::BadSize { .. })
    ));
    assert!(matches!(
      PublicKey::parse(""),
      Err(SignatureError::Missing(_))
    ));
  }

  #[test]
  fn a_secret_key_round_trips_and_a_legacy_seed_still_reads() {
    let secret = SecretKey::generate().unwrap();
    let again = SecretKey::parse(&secret.to_file()).unwrap();
    assert_eq!(again.public(), secret.public());

    let seed = to_hex(&secret.signing.to_bytes());
    let legacy = SecretKey::parse(&seed).unwrap();
    assert_eq!(legacy.public().key, secret.public().key);
    assert_eq!(legacy.public().id, legacy_id(&secret.public().key));
  }

  #[test]
  fn an_encrypted_or_inconsistent_secret_key_is_refused() {
    let line = MINISIGN_KEY.lines().nth(1).unwrap();
    let mut bytes = BASE64.decode(line).unwrap();
    bytes[2..4].copy_from_slice(b"Sc");
    assert!(matches!(
      SecretKey::parse(&BASE64.encode(&bytes)),
      Err(SignatureError::Encrypted)
    ));

    let mut bytes = BASE64.decode(line).unwrap();
    bytes[100] ^= 1;
    assert!(matches!(
      SecretKey::parse(&BASE64.encode(&bytes)),
      Err(SignatureError::Inconsistent)
    ));
  }

  #[test]
  fn what_is_published_for_0_1_is_what_0_1_accepts() {
    let secret = SecretKey::generate().unwrap();
    let digest = luthier_manifest::Sha256Hash::from_bytes([9; 32]);
    let file = legacy::SignatureFile::parse(&secret.sign_legacy(&digest).render()).unwrap();
    assert!(file.verifies(&digest));
    assert!(!file.verifies(&luthier_manifest::Sha256Hash::from_bytes([8; 32])));
    // 0.1 pins the key it names, as hex; that pin must equal the minisign
    // key this build is configured with, or an upgrade breaks the bench.
    assert_eq!(file.key.key, secret.public().key);
  }

  #[test]
  fn with_no_key_known_a_signature_is_read_but_not_believed() {
    let secret = SecretKey::generate().unwrap();
    let signed = secret.sign(&snapshot(1), "bench.tar.gz", 1).render();
    assert_eq!(
      check(
        "bench",
        &Requirement::Unknown,
        Some(&signed),
        &snapshot(1),
        false
      )
      .unwrap(),
      Verdict::Unsigned
    );
    // Garbage is still refused: it is never a way to be read as unsigned.
    assert!(
      check(
        "bench",
        &Requirement::Unknown,
        Some("junk"),
        &snapshot(1),
        false
      )
      .is_err()
    );
  }

  #[test]
  fn an_unsigned_snapshot_is_refused_where_a_key_is_expected() {
    assert_eq!(
      check("bench", &Requirement::Unknown, None, &snapshot(1), false).unwrap(),
      Verdict::Unsigned
    );
    let err = check(
      "bench",
      &Requirement::Signed(vec![SecretKey::generate().unwrap().public()]),
      None,
      &snapshot(1),
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
      check("bench", &requirement, None, &snapshot(1), true).unwrap(),
      Verdict::Unsigned
    );

    // A signature that does not hold up is a claim that failed, not an
    // absent one, and the flag does not touch it.
    let wrong = trusted.sign(&snapshot(2), "bench.tar.gz", 1).render();
    let err = check("bench", &requirement, Some(&wrong), &snapshot(1), true).unwrap_err();
    assert!(err.to_string().contains("does not verify"), "{err}");

    // Nor does it accept a stranger's key.
    let stranger = SecretKey::generate().unwrap();
    let signed = stranger.sign(&snapshot(1), "bench.tar.gz", 1).render();
    let err = check("bench", &requirement, Some(&signed), &snapshot(1), true).unwrap_err();
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
      let signed = key.sign(&snapshot(7), "bench.tar.gz", 1).render();
      assert_eq!(
        check("bench", &requirement, Some(&signed), &snapshot(7), false).unwrap(),
        Verdict::Signed(key.public())
      );
    }
  }
}
