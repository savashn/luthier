//! Where each registry snapshot came from, and what it was.
//!
//! An audit trail: for each source, the URL it was fetched from, and the
//! SHA-256 and size of what arrived, when. Nothing here is enforced; which
//! URLs are read is built into the manager.
//!
//! 0.1 also pinned each source's origin and signing key here on first use,
//! because users could add sources of their own. They no longer can, so a
//! pin would only protect a URL the binary already fixes — and would lock
//! every user out the day a release moved one. A `signed_by` field 0.1 wrote
//! is ignored on reading.

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
  /// Scheme, host and port.
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
  fn a_record_0_1_wrote_still_parses() {
    // 0.1 recorded the key that signed the bench; that field is ignored now.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
      record_path(dir.path(), "luthier-extras"),
      br#"{"url":"https://example.com/p.tar.gz","origin":"https://example.com:443",
          "sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
          "bytes":10,"fetched_at":"2026-01-01T00:00:00Z",
          "signed_by":"e3796d9892f200f145a5befbb421a66fb9d6ba5a68afe6246044dfba716a99aa"}"#,
    )
    .unwrap();
    assert_eq!(load(dir.path(), "luthier-extras").unwrap().bytes, 10);
  }

  #[test]
  fn a_corrupted_record_is_ignored_rather_than_fatal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(record_path(dir.path(), "bench"), b"{ not json").unwrap();
    assert!(load(dir.path(), "bench").is_none());
  }
}
