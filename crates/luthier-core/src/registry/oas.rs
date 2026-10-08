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
//!
//! The download is conditional: the server is sent the `ETag` and
//! `Last-Modified` it gave last time, and an index that has not changed costs
//! a 304 and no bytes. That is what makes it cheap for a command to refresh
//! the index itself once a day ([`RegistryProvider::refresh_if_due`]).

pub mod license;
pub mod translate;

use super::{IndexEntry, RefreshOutcome, RegistryIndex, RegistryProvider, provenance};
use crate::download::{Conditional, Downloader, Validators};
use crate::error::{Error, RegistryError, Result};
use crate::fsutil;
use jiff::{SignedDuration, Timestamp};
use luthier_manifest::{PackageId, Sha256Hash};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use translate::OasPackage;
use url::Url;

/// Where the published index lives, relative to the configured base.
const PLUGINS_INDEX: &str = "plugins/index.json";

/// The most the index may be. It is under two megabytes (2026-10).
const INDEX_LIMIT: u64 = 64 * 1024 * 1024;

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

  /// Where the provenance record is kept, beside every source's snapshot.
  fn registries_dir(&self) -> &Path {
    self.snapshot_dir.parent().unwrap_or(&self.snapshot_dir)
  }

  /// What to ask the server whether it changed since: the validators it
  /// sent with the snapshot on disk — only if that snapshot, `on_disk`, is
  /// still the one they came with, from this URL, or a 304 would vouch for a
  /// file that is not there, or not what was fetched, or not from here.
  fn known_validators(
    record: Option<&provenance::Provenance>,
    on_disk: Option<Sha256Hash>,
    url: &Url,
  ) -> Option<Validators> {
    let record = record?;
    let matches = on_disk == Some(record.sha256) && record.url == url.as_str();
    (matches && !record.validators.is_empty()).then(|| record.validators.clone())
  }

  /// Asks for the index, sending what names the copy on disk, and replaces
  /// that copy only with one that parses. `impatient` for a refresh no one
  /// asked for.
  async fn fetch(&self, impatient: bool) -> Result<RefreshOutcome> {
    let url = self.index_url()?;
    // Where 0.4 and earlier downloaded it, twice; one interrupted left this.
    let _ = fsutil::remove_any(&self.cache_dir.join("registry-snapshots").join(&self.name));
    let record = provenance::load(self.registries_dir(), &self.name);
    let on_disk = fsutil::hash_file(&self.snapshot_path()).ok();
    let known = Self::known_validators(record.as_ref(), on_disk, &url);

    let mut downloader = Downloader::new(&self.cache_dir).offline(self.offline);
    if impatient {
      downloader = downloader.impatient();
    }
    let (body, validators) = match downloader
      .get_if_changed(&url, INDEX_LIMIT, known.as_ref())
      .await?
    {
      Conditional::Unchanged => {
        provenance::record_checked(self.registries_dir(), &self.name);
        return Ok(RefreshOutcome {
          registry: self.name.clone(),
          packages: self.load_index()?.len(),
          updated: false,
          failure: None,
        });
      }
      Conditional::Changed { body, validators } => (body, validators),
    };

    // Parsed before it replaces the snapshot, so a truncated or reshaped
    // response leaves the previous one in place rather than costing the
    // user every command until the next refresh.
    let index = index_from_json(&self.name, &body)?;
    let sha256 = {
      use sha2::{Digest, Sha256};
      Sha256Hash::from_bytes(Sha256::digest(&body).into())
    };
    fsutil::ensure_dir(&self.snapshot_dir)?;
    fsutil::write_atomic(&self.snapshot_path(), &body)?;
    provenance::record(
      self.registries_dir(),
      &self.name,
      &url,
      sha256,
      body.len() as u64,
      validators,
    );

    Ok(RefreshOutcome {
      registry: self.name.clone(),
      packages: index.len(),
      // Against the copy that was here: a server that does not answer
      // conditionally can send the same index again, and a copy that was
      // missing or damaged is replaced even when the index has not moved.
      updated: on_disk != Some(sha256),
      failure: None,
    })
  }

  /// Whether the snapshot is due a refresh: missing, or last checked more
  /// than `max_age` ago — or in the future, which a clock set back explains.
  fn is_due(&self, max_age: SignedDuration) -> bool {
    if !self.snapshot_path().is_file() {
      return true;
    }
    let Some(record) = provenance::load(self.registries_dir(), &self.name) else {
      return true;
    };
    let age = Timestamp::now().duration_since(record.last_checked());
    age >= max_age || age.is_negative()
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
    self.fetch(false).await
  }

  async fn refresh_if_due(&self, max_age: SignedDuration) -> Option<Result<RefreshOutcome>> {
    if self.offline || !self.is_due(max_age) {
      return None;
    }
    Some(self.fetch(true).await)
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

  /// An Open Audio Stack site serving `body` with `ETag: "v1"`, answering
  /// 304 to a request that names it. Returns the server and a registry
  /// reading it into `dir`.
  async fn served(dir: &Path, body: &'static str) -> (wiremock::MockServer, OasRegistry) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/plugins/index.json"))
      .respond_with(move |request: &Request| {
        let named = request
          .headers
          .get("if-none-match")
          .and_then(|v| v.to_str().ok());
        match named {
          Some("\"v1\"") => ResponseTemplate::new(304),
          _ => ResponseTemplate::new(200)
            .insert_header("etag", "\"v1\"")
            .set_body_string(body),
        }
      })
      .mount(&server)
      .await;
    let registry = OasRegistry::new(
      "oas",
      Url::parse(&format!("{}/", server.uri())).unwrap(),
      dir.join("registries/oas"),
      dir.join("cache"),
    );
    (server, registry)
  }

  /// The `If-None-Match` each request the server received carried.
  async fn named(server: &wiremock::MockServer) -> Vec<Option<String>> {
    server
      .received_requests()
      .await
      .unwrap()
      .iter()
      .map(|r| {
        r.headers
          .get("if-none-match")
          .and_then(|v| v.to_str().ok())
          .map(str::to_owned)
      })
      .collect()
  }

  #[tokio::test]
  async fn a_refresh_downloads_the_index_once_and_then_only_asks_whether_it_changed() {
    let dir = tempfile::tempdir().unwrap();
    let (server, registry) = served(dir.path(), TWO_ORGS).await;

    let first = registry.refresh().await.unwrap();
    assert!(first.updated);
    assert_eq!(first.packages, 2);
    // One request: the bytes are not fetched a second time to learn their
    // digest.
    assert_eq!(named(&server).await, [None]);

    let second = registry.refresh().await.unwrap();
    assert!(!second.updated, "a 304 changes nothing");
    assert_eq!(second.packages, 2);
    assert_eq!(named(&server).await, [None, Some("\"v1\"".into())]);
    let record = provenance::load(&dir.path().join("registries"), "oas").unwrap();
    assert!(record.checked_at.is_some());

    // A snapshot that is gone, or not the one the validators came with, is
    // fetched whole: a 304 would vouch for a file that is not there. And it
    // is an update, though the index itself has not moved.
    std::fs::write(registry.snapshot_path(), b"{}").unwrap();
    assert!(registry.refresh().await.unwrap().updated);
    assert_eq!(named(&server).await.last().unwrap(), &None);
    assert_eq!(registry.load_index().unwrap().len(), 2);
  }

  /// Rewrites the record's timestamps, as if they had been written at other
  /// times.
  fn backdate(dir: &Path, fetched: SignedDuration, checked: Option<SignedDuration>) {
    let path = dir.join("registries/oas.source.json");
    let mut record: serde_json::Value =
      serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let ago = |by: SignedDuration| serde_json::json!((Timestamp::now() - by).to_string());
    record["fetched_at"] = ago(fetched);
    match checked {
      Some(by) => record["checked_at"] = ago(by),
      None => {
        record.as_object_mut().unwrap().remove("checked_at");
      }
    }
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
  }

  #[tokio::test]
  async fn a_command_refreshes_the_index_only_when_it_is_due() {
    let dir = tempfile::tempdir().unwrap();
    let (server, registry) = served(dir.path(), TWO_ORGS).await;
    let day = SignedDuration::from_hours(24);

    // Never fetched: due.
    assert!(registry.refresh_if_due(day).await.unwrap().unwrap().updated);
    // Just fetched: not due, and the server is not asked.
    assert!(registry.refresh_if_due(day).await.is_none());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let hours = SignedDuration::from_hours;
    // Fetched two days ago: due, and answered with a 304 ...
    backdate(dir.path(), hours(48), None);
    let outcome = registry.refresh_if_due(day).await.unwrap().unwrap();
    assert!(!outcome.updated);
    assert_eq!(
      named(&server).await.last().unwrap().as_deref(),
      Some("\"v1\"")
    );
    // ... which counts as asking.
    assert!(registry.refresh_if_due(day).await.is_none());

    // Fetched a week ago and last asked two days ago: due again. This is
    // the path every day after the first takes.
    backdate(dir.path(), hours(24 * 7), Some(hours(48)));
    assert!(registry.refresh_if_due(day).await.is_some());
    assert!(registry.refresh_if_due(day).await.is_none());

    // Stamped by a clock that was a day ahead: due, once, and then not
    // again until a day has passed on this clock.
    backdate(dir.path(), hours(-24), None);
    assert!(registry.refresh_if_due(day).await.is_some());
    assert!(registry.refresh_if_due(day).await.is_none());

    // Never under --offline, due or not.
    let offline = OasRegistry::new(
      "oas",
      Url::parse(&format!("{}/", server.uri())).unwrap(),
      dir.path().join("elsewhere/oas"),
      dir.path().join("cache"),
    )
    .offline(true);
    assert!(offline.refresh_if_due(day).await.is_none());
  }

  #[tokio::test]
  async fn an_index_that_cannot_be_fetched_leaves_the_one_on_disk_and_is_tried_once() {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};

    let dir = tempfile::tempdir().unwrap();
    let (server, registry) = served(dir.path(), TWO_ORGS).await;
    registry.refresh().await.unwrap();
    server.reset().await;
    Mock::given(method("GET"))
      .respond_with(ResponseTemplate::new(503))
      .mount(&server)
      .await;

    let error = registry
      .refresh_if_due(SignedDuration::ZERO)
      .await
      .unwrap()
      .unwrap_err();
    assert!(error.to_string().contains("503"), "{error}");
    // No retries for a refresh nobody asked for.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(registry.load_index().unwrap().len(), 2);
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
