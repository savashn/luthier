//! Where each registry snapshot came from, and what it was.
//!
//! Package artifacts are verified against a checksum in a manifest. The
//! document carrying those checksums — a bench tarball, an OAS index — arrives
//! on the strength of HTTPS alone, which makes it the weakest link in the chain
//! (ROADMAP 2.1, 2.2). Signatures are the eventual answer and the manifest
//! format reserves room for them.
//!
//! What this does in the meantime is trust on first use, applied to the thing
//! that should never change rather than to the thing that always does. A
//! snapshot's *contents* change on every refresh — that is what a refresh is
//! for — so pinning its digest would reject every genuine update. Its *origin*
//! should not change at all: a bench that silently starts answering from
//! somewhere else is the case worth refusing, and the one a user could not
//! otherwise notice.
//!
//! The digest is recorded rather than enforced, so that a change is auditable
//! and so there is something for a signature to be checked against later.

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

/// Records what was just fetched.
///
/// Best-effort: a snapshot that fetched, parsed and installed correctly is not
/// worth failing over an unwritable audit record, and the next refresh will
/// write one.
pub fn record(registries_dir: &Path, name: &str, url: &Url, sha256: Sha256Hash, bytes: u64) {
  let provenance = Provenance {
    url: url.to_string(),
    origin: origin_of(url),
    sha256,
    bytes,
    fetched_at: Timestamp::now(),
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
  fn a_corrupted_record_is_ignored_rather_than_fatal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(record_path(dir.path(), "bench"), b"{ not json").unwrap();
    assert!(load(dir.path(), "bench").is_none());
    check_origin(dir.path(), "bench", &url("https://example.com/a.tar.gz")).unwrap();
  }
}
