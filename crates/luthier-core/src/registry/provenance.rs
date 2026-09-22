//! Where each registry snapshot came from, and what it was.
//!
//! Package artifacts are verified against a checksum in a manifest. The
//! document carrying those checksums — a bench tarball, an OAS index — arrives
//! on the strength of HTTPS alone unless the bench signs it, and most do not.
//! [`signature`](super::signature) is the answer where there is one; this is
//! what every bench gets either way.
//!
//! What it does is trust on first use, applied to the thing that should never
//! change rather than to the thing that always does. A
//! snapshot's *contents* change on every refresh — that is what a refresh is
//! for — so pinning its digest would reject every genuine update. Its *origin*
//! should not change at all: a bench that silently starts answering from
//! somewhere else is the case worth refusing, and the one a user could not
//! otherwise notice.
//!
//! The digest is recorded rather than enforced. What makes it more than an
//! audit trail is [`signature`](super::signature): where a bench publishes a
//! detached signature, the recorded digest is the thing that signature
//! vouches for, and the key that signed it is pinned here exactly as the
//! origin is. From then on a snapshot signed by anyone else, or by nobody, is
//! refused rather than believed.

use super::signature::{PublicKey, Requirement};
use crate::error::{Error, RegistryError, Result};
use crate::fsutil;
use jiff::Timestamp;
use luthier_manifest::Sha256Hash;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use url::Url;

/// What was fetched for one bench, and from where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
  /// The URL as configured, recorded in full for the audit trail.
  pub url: String,
  /// Scheme, host and port: what must not change silently.
  pub origin: String,
  pub sha256: Sha256Hash,
  pub bytes: u64,
  pub fetched_at: Timestamp,
  /// The key whose signature covered the recorded digest, where one did.
  ///
  /// Defaulted rather than required, so a record written before signatures
  /// existed still parses as what it is: a bench nobody has signed yet.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub signed_by: Option<PublicKey>,
}

/// Where the record for `name` lives, beside the snapshot itself.
fn record_path(registries_dir: &Path, name: &str) -> PathBuf {
  registries_dir.join(format!("{name}.source.json"))
}

/// Scheme, host and port — the part of a URL that identifies who answered.
///
/// A path that moves within one origin is upstream reorganising itself, which
/// is theirs to do. A host that changes is a different party.
fn origin_of(url: &Url) -> String {
  match (url.host_str(), url.port_or_known_default()) {
    (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
    (Some(host), None) => format!("{}://{host}", url.scheme()),
    (None, _) => url.scheme().to_owned(),
  }
}

/// Reads what was recorded for `name`, if anything was.
///
/// An unreadable or unparsable record is treated as absent: it is a cache of a
/// previous observation, and refusing to work because it was corrupted would
/// turn a hint into an outage.
pub fn load(registries_dir: &Path, name: &str) -> Option<Provenance> {
  let bytes = std::fs::read(record_path(registries_dir, name)).ok()?;
  serde_json::from_slice(&bytes).ok()
}

/// Refuses a bench whose origin has changed since it was first fetched.
///
/// Called before the fetch, so nothing is downloaded from the new host.
pub fn check_origin(registries_dir: &Path, name: &str, url: &Url) -> Result<()> {
  let Some(previous) = load(registries_dir, name) else {
    return Ok(());
  };
  let now = origin_of(url);
  if previous.origin == now {
    return Ok(());
  }
  Err(Error::Registry(RegistryError::OriginChanged {
    registry: name.to_owned(),
    previous: previous.origin,
    current: now,
  }))
}

/// What this refresh has to prove about the snapshot it downloads.
///
/// Configured keys win: someone who wrote a key into the configuration has
/// said which key they mean, and a pin is only ever a guess at that. With
/// neither, the first signature to arrive pins the key it names — the same
/// trust-on-first-use this module applies to an origin, for the same reason.
pub fn requirement(registries_dir: &Path, name: &str, configured: &[PublicKey]) -> Requirement {
  if !configured.is_empty() {
    return Requirement::Signed(configured.to_vec());
  }
  match load(registries_dir, name).and_then(|p| p.signed_by) {
    Some(pinned) => Requirement::Signed(vec![pinned]),
    None => Requirement::FirstUse,
  }
}

/// Records what was just fetched.
///
/// Best-effort: a snapshot that fetched, parsed and installed correctly is not
/// worth failing over an unwritable audit record, and the next refresh will
/// write one.
///
/// A refresh accepted as unsigned never erases a key that was pinned before.
/// `--allow-unsigned` is for one run, not a way to turn verification off by
/// using it once.
pub fn record(
  registries_dir: &Path,
  name: &str,
  url: &Url,
  sha256: Sha256Hash,
  bytes: u64,
  signed_by: Option<PublicKey>,
) {
  let signed_by = signed_by.or_else(|| load(registries_dir, name).and_then(|p| p.signed_by));
  let provenance = Provenance {
    url: url.to_string(),
    origin: origin_of(url),
    sha256,
    bytes,
    fetched_at: Timestamp::now(),
    signed_by,
  };
  let Ok(mut encoded) = serde_json::to_vec_pretty(&provenance) else {
    return;
  };
  encoded.push(b'\n');
  if fsutil::ensure_dir(registries_dir).is_ok() {
    let _ = fsutil::write_atomic(&record_path(registries_dir, name), &encoded);
  }
}

/// Forgets what was recorded, so the next fetch pins afresh.
///
/// Removing a bench has to do this, or re-adding the name under a different
/// URL would be refused for a pin the user already discarded.
pub fn forget(registries_dir: &Path, name: &str) {
  let _ = fsutil::remove_any(&record_path(registries_dir, name));
}

/// Forgets the pinned key while keeping everything else.
///
/// What `bench untrust` needs: the user has decided this bench is one they
/// read unsigned, and the origin pin is a separate decision they have not
/// made. Nothing is written when there was no record to edit.
pub fn forget_key(registries_dir: &Path, name: &str) {
  let Some(previous) = load(registries_dir, name) else {
    return;
  };
  let updated = Provenance {
    signed_by: None,
    ..previous
  };
  let Ok(mut encoded) = serde_json::to_vec_pretty(&updated) else {
    return;
  };
  encoded.push(b'\n');
  let _ = fsutil::write_atomic(&record_path(registries_dir, name), &encoded);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn url(raw: &str) -> Url {
    Url::parse(raw).unwrap()
  }

  #[test]
  fn an_origin_is_scheme_host_and_port() {
    assert_eq!(
      origin_of(&url("https://example.com/a/b.tar.gz")),
      "https://example.com:443"
    );
    // A path moving within one host is upstream's business.
    assert_eq!(
      origin_of(&url("https://example.com/moved/elsewhere.tar.gz")),
      origin_of(&url("https://example.com/a/b.tar.gz"))
    );
    assert_ne!(
      origin_of(&url("https://example.com/a")),
      origin_of(&url("http://example.com/a"))
    );
  }

  #[test]
  fn the_first_fetch_pins_and_a_later_host_change_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = url("https://example.com/pkgs.tar.gz");

    // Nothing recorded yet: anything is allowed.
    check_origin(dir.path(), "bench", &first).unwrap();
    record(
      dir.path(),
      "bench",
      &first,
      Sha256Hash::from_bytes([1; 32]),
      10,
      None,
    );

    // The same origin, a different path: fine.
    check_origin(
      dir.path(),
      "bench",
      &url("https://example.com/other.tar.gz"),
    )
    .unwrap();

    let err = check_origin(
      dir.path(),
      "bench",
      &url("https://elsewhere.invalid/pkgs.tar.gz"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("elsewhere.invalid"), "{err}");

    // And discarding the pin makes the new host allowed again, which is what
    // `bench remove` then `bench add` has to mean.
    forget(dir.path(), "bench");
    check_origin(
      dir.path(),
      "bench",
      &url("https://elsewhere.invalid/pkgs.tar.gz"),
    )
    .unwrap();
  }

  #[test]
  fn a_configured_key_outranks_a_pinned_one() {
    use super::super::signature::SecretKey;

    let dir = tempfile::tempdir().unwrap();
    let pinned = SecretKey::generate().unwrap().public();
    let configured = SecretKey::generate().unwrap().public();

    // Nothing recorded, nothing configured: the first signature decides.
    assert_eq!(requirement(dir.path(), "bench", &[]), Requirement::FirstUse);

    record(
      dir.path(),
      "bench",
      &url("https://example.com/pkgs.tar.gz"),
      Sha256Hash::from_bytes([1; 32]),
      10,
      Some(pinned),
    );
    assert_eq!(
      requirement(dir.path(), "bench", &[]),
      Requirement::Signed(vec![pinned])
    );

    // Someone who wrote a key into the configuration has said which key
    // they mean; a pin is only ever a guess at that.
    assert_eq!(
      requirement(dir.path(), "bench", &[configured]),
      Requirement::Signed(vec![configured])
    );
  }

  #[test]
  fn an_unsigned_refresh_does_not_discard_a_pinned_key() {
    // Otherwise one `--allow-unsigned` would turn verification off for
    // good, which is the opposite of what a one-run override means.
    use super::super::signature::SecretKey;

    let dir = tempfile::tempdir().unwrap();
    let key = SecretKey::generate().unwrap().public();
    let source = url("https://example.com/pkgs.tar.gz");

    record(
      dir.path(),
      "bench",
      &source,
      Sha256Hash::from_bytes([1; 32]),
      10,
      Some(key),
    );
    record(
      dir.path(),
      "bench",
      &source,
      Sha256Hash::from_bytes([2; 32]),
      20,
      None,
    );

    let recorded = load(dir.path(), "bench").unwrap();
    assert_eq!(recorded.signed_by, Some(key));
    // The rest of the record is still this refresh's.
    assert_eq!(recorded.bytes, 20);

    // Giving up on signatures for a bench is its own decision, and this is
    // what makes it.
    forget_key(dir.path(), "bench");
    assert_eq!(load(dir.path(), "bench").unwrap().signed_by, None);
    assert_eq!(requirement(dir.path(), "bench", &[]), Requirement::FirstUse);
  }

  #[test]
  fn a_record_written_before_signatures_existed_still_parses() {
    // The shape on disk before `signed_by` was added: a bench nobody has
    // signed, which is exactly what it should read as.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
      record_path(dir.path(), "bench"),
      br#"{"url":"https://example.com/p.tar.gz","origin":"https://example.com:443",
          "sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
          "bytes":10,"fetched_at":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    assert_eq!(load(dir.path(), "bench").unwrap().signed_by, None);
    assert_eq!(requirement(dir.path(), "bench", &[]), Requirement::FirstUse);
  }

  #[test]
  fn a_corrupted_record_is_ignored_rather_than_fatal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(record_path(dir.path(), "bench"), b"{ not json").unwrap();
    assert!(load(dir.path(), "bench").is_none());
    check_origin(dir.path(), "bench", &url("https://example.com/a.tar.gz")).unwrap();
  }
}
