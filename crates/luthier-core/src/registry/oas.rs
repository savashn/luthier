//! The Open Audio Stack registry as a source.
//!
//! OAS publishes its registry as static JSON on GitHub Pages, under a CC0
//! dedication. That gives breadth this project cannot hand-curate — hundreds
//! of packages against a handful — at the cost of a schema that stops short of
//! saying how to install anything. [`translate`] decides every difference
//! between the two vocabularies; what OAS does not carry is marked for
//! derivation rather than invented.
//!
//! The snapshot is one JSON document cached on disk, so `refresh` is a
//! download and `load_index` never touches the network.

pub mod license;
pub mod translate;

use super::{
  IndexEntry, RefreshOutcome, RegistryIndex, RegistryProvider, download_unverified, provenance,
};
use crate::download::Downloader;
use crate::error::{Error, RegistryError, Result};
use crate::fsutil;
use luthier_manifest::{PackageId, Sha256Hash};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use translate::OasPackage;
use url::Url;

/// Where the published index lives, relative to the configured base.
const PLUGINS_INDEX: &str = "plugins/index.json";

pub struct OasRegistry {
  name: String,
  base: Url,
  snapshot_dir: PathBuf,
  cache_dir: PathBuf,
  offline: bool,
}

impl OasRegistry {
  pub fn new(
    name: impl Into<String>,
    base: Url,
    snapshot_dir: impl Into<PathBuf>,
    cache_dir: impl Into<PathBuf>,
  ) -> Self {
    Self {
      name: name.into(),
      base,
      snapshot_dir: snapshot_dir.into(),
      cache_dir: cache_dir.into(),
      offline: false,
    }
  }

  pub fn offline(mut self, offline: bool) -> Self {
    self.offline = offline;
    self
  }

  fn snapshot_path(&self) -> PathBuf {
    self.snapshot_dir.join("plugins.json")
  }

  fn index_url(&self) -> Result<Url> {
    let mut base = self.base.clone();
    if !base.path().ends_with('/') {
      base.set_path(&format!("{}/", base.path()));
    }
    base.join(PLUGINS_INDEX).map_err(|e| {
      Error::Registry(RegistryError::Malformed {
        registry: self.name.clone(),
        reason: e.to_string(),
      })
    })
  }
}

/// Reads a cached index document into packages.
///
/// A package that cannot be represented is skipped rather than failing the
/// whole load: one malformed upstream entry must not cost the user the other
/// five hundred.
pub fn index_from_json(name: &str, json: &[u8]) -> Result<RegistryIndex> {
  let raw: BTreeMap<String, OasPackage> = serde_json::from_slice(json).map_err(|e| {
    Error::Registry(RegistryError::Malformed {
      registry: name.to_owned(),
      reason: e.to_string(),
    })
  })?;

  // Two organisations publishing the same name both keep it, qualified.
  let mut claimed: BTreeMap<PackageId, usize> = BTreeMap::new();
  for entry in raw.values() {
    if let Ok(id) = translate::id_from_slug(&entry.slug) {
      *claimed.entry(id).or_default() += 1;
    }
  }

  let mut packages: BTreeMap<PackageId, IndexEntry> = BTreeMap::new();
  for entry in raw.values() {
    let Ok(mut translated) = translate::package(entry) else {
      continue;
    };
    let contested = translate::id_from_slug(&entry.slug)
      .ok()
      .and_then(|id| claimed.get(&id).copied())
      .is_some_and(|count| count > 1);
    if contested {
      let Ok(qualified) = translate::qualified_id(&entry.slug) else {
        continue;
      };
      translated.notes.push(format!(
        "another organisation publishes {:?} too, so this one is {}",
        translated.manifest.id.as_str(),
        qualified.as_str()
      ));
      translated.manifest.id = qualified;
    }

    // Keyed on what the entry actually says, so republishing the same
    // version with different bytes is detectable exactly as it is for a
    // manifest read from a file. Every version key is folded in, not just
    // the current one: a version disappearing upstream is the change that
    // breaks an `env import`, and it must not leave the digest unmoved.
    let digest = {
      use sha2::{Digest, Sha256};
      let mut hasher = Sha256::new();
      hasher.update(entry.slug.as_bytes());
      hasher.update(b"@");
      hasher.update(entry.version.as_bytes());
      for key in entry.versions.keys() {
        hasher.update(b"\0");
        hasher.update(key.as_bytes());
      }
      Sha256Hash::from_bytes(hasher.finalize().into())
    };
    packages.insert(
      translated.manifest.id.clone(),
      IndexEntry {
        manifest: translated.manifest,
        path: PathBuf::from(&entry.slug),
        digest,
        unknown_fields: Vec::new(),
        registry: name.to_owned(),
        notes: translated.notes,
      },
    );
  }

  Ok(RegistryIndex {
    name: name.to_owned(),
    packages,
    problems: Vec::new(),
    engines: Vec::new(),
  })
}

#[async_trait::async_trait]
impl RegistryProvider for OasRegistry {
  fn name(&self) -> &str {
    &self.name
  }

  async fn refresh(&self) -> Result<RefreshOutcome> {
    let url = self.index_url()?;
    let scratch = self.cache_dir.join("registry-snapshots").join(&self.name);
    fsutil::remove_any(&scratch)?;
    fsutil::ensure_dir(&scratch)?;

    // Before a byte is fetched: an index that has started answering from a
    // different host is refused rather than quietly believed.
    let registries_dir = self
      .snapshot_dir
      .parent()
      .unwrap_or(&self.snapshot_dir)
      .to_path_buf();
    provenance::check_origin(&registries_dir, &self.name, &url)?;

    let downloader = Downloader::new(&scratch).offline(self.offline);
    let staged = scratch.join("plugins.json");
    let (bytes, digest) = download_unverified(&downloader, &url, &staged, self.offline).await?;

    // Parsed before it replaces the cache, so a truncated or reshaped
    // response leaves the previous snapshot in place rather than costing
    // the user every command until the next refresh.
    let body = read_snapshot(&staged)?;
    let index = index_from_json(&self.name, &body)?;
    let packages = index.len();

    fsutil::ensure_dir(&self.snapshot_dir)?;
    fsutil::write_atomic(&self.snapshot_path(), &body)?;
    fsutil::remove_any(&scratch)?;
    // Unsigned, and not for want of asking: an Open Audio Stack site
    // publishes a static JSON index and no signature beside it. The origin
    // pin and the recorded digest are what this bench gets.
    provenance::record(&registries_dir, &self.name, &url, digest, bytes, None);

    Ok(RefreshOutcome {
      registry: self.name.clone(),
      packages,
      updated: true,
    })
  }

  fn load_index(&self) -> Result<RegistryIndex> {
    let path = self.snapshot_path();
    if !path.exists() {
      return Err(Error::Registry(RegistryError::NotFetched(
        self.name.clone(),
      )));
    }
    let body = read_snapshot(&path)?;
    index_from_json(&self.name, &body)
  }
}

fn read_snapshot(path: &Path) -> Result<Vec<u8>> {
  std::fs::read(path).map_err(|e| Error::io("read", path, e))
}

#[cfg(test)]
mod tests {
  use super::*;

  const TWO_ORGS: &str = r#"{
      "distrho/mverb": { "slug": "distrho/mverb", "version": "1.0.0", "versions": { "1.0.0": {
        "name": "MVerb", "author": "DISTRHO", "description": "Reverb.",
        "license": "gpl-3.0", "type": "effect", "tags": ["reverb"],
        "url": "https://example.invalid/a",
        "files": [{ "systems": [{"type":"linux"}], "architectures": ["x64"],
          "contains": ["lv2"], "type": "archive", "size": 100,
          "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
          "url": "https://example.invalid/a.tar.xz" }] }}},
      "figbug/mverb": { "slug": "figbug/mverb", "version": "1.0.0", "versions": { "1.0.0": {
        "name": "MVerb", "author": "figbug", "description": "Reverb.",
        "license": "mit", "type": "effect", "tags": ["reverb"],
        "url": "https://example.invalid/b",
        "files": [{ "systems": [{"type":"linux"}], "architectures": ["x64"],
          "contains": ["lv2"], "type": "archive", "size": 100,
          "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
          "url": "https://example.invalid/b.tar.xz" }] }}}
    }"#;

  #[test]
  fn two_organisations_publishing_one_name_both_keep_it() {
    // `mverb` is the only clash across the published index, and dropping
    // either package would be a worse answer than qualifying both.
    let index = index_from_json("oas", TWO_ORGS.as_bytes()).unwrap();

    let ids: Vec<&str> = index.packages.keys().map(|id| id.as_str()).collect();
    assert_eq!(ids, vec!["distrho-mverb", "figbug-mverb"]);
    assert!(
      index
        .packages
        .values()
        .all(|e| e.notes.iter().any(|n| n.contains("another organisation"))),
      "the rename should be explained"
    );
  }

  #[test]
  fn one_unusable_entry_does_not_cost_the_others() {
    // An upstream entry this build cannot represent is skipped. Failing
    // the whole load would mean one bad record hides every good one.
    let broken = TWO_ORGS.replace(r#""type": "archive""#, r#""type": "installer""#);
    let index = index_from_json("oas", broken.as_bytes()).unwrap();
    assert!(index.packages.is_empty());

    let half = TWO_ORGS.replacen(r#""type": "archive""#, r#""type": "installer""#, 1);
    let index = index_from_json("oas", half.as_bytes()).unwrap();
    assert_eq!(index.packages.len(), 1);
  }

  #[test]
  fn a_response_that_is_not_an_index_is_refused() {
    let err = index_from_json("oas", b"not json").unwrap_err();
    assert!(err.to_string().contains("cannot read"), "{err}");
  }

  /// Runs against a copy of the published index when one is pointed at, so
  /// the shape can be checked against reality without the suite ever
  /// touching the network (§55).
  #[test]
  fn the_published_index_translates() {
    let Some(json) = std::env::var("LUTHIER_OAS_FIXTURE")
      .ok()
      .and_then(|p| std::fs::read(p).ok())
    else {
      return;
    };
    let index = index_from_json("oas", &json).unwrap();

    let linux = index
      .packages
      .values()
      .filter(|e| {
        e.manifest.releases.iter().any(|r| {
          r.artifacts.iter().any(|a| {
            a.target
              == luthier_manifest::Target {
                os: luthier_manifest::Os::Linux,
                arch: luthier_manifest::Arch::X86_64,
              }
          })
        })
      })
      .count();

    assert!(linux > 200, "only {linux} linux-x86_64 packages");
    assert!(
      index.packages.values().all(|e| e.manifest.releases[0]
        .artifacts
        .iter()
        .all(|a| a.derive_install)),
      "every OAS artifact derives its rules"
    );
  }
}
