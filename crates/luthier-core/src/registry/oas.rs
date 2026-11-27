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

use super::{Completion, IndexEntry, RefreshOutcome, RegistryIndex, RegistryProvider, provenance};
use crate::download::{Conditional, Downloader, Validators};
use crate::error::{Error, RegistryError, Result};
use crate::fsutil;
use jiff::{SignedDuration, Timestamp};
#[cfg(test)]
use luthier_manifest::Target;
use luthier_manifest::{PackageId, Sha256Hash};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use translate::OasPackage;
use url::Url;

/// Where the published index lives, relative to the configured base.
const PLUGINS_INDEX: &str = "plugins/index.json";

/// The most the index may be. It is under two megabytes (2026-10).
const INDEX_LIMIT: u64 = 64 * 1024 * 1024;

/// The most one package's own document may be. LSP Plugins, with eleven
/// versions of four files each, is under 40 KiB (2026-11).
const DOCUMENT_LIMIT: u64 = 4 * 1024 * 1024;

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

  /// Where a package's own document is kept once fetched: beside the
  /// snapshot, not in the cache, so `cache clean` cannot take what an
  /// installed package needs to be reinstalled offline. Named by the
  /// qualified ID, which is a plain file name whatever the slug holds; two
  /// slugs that normalise alike share a file, and each then reads the other's
  /// document as not current ([`is_current`] compares slugs) and fetches its
  /// own.
  fn document_path(&self, slug: &str) -> Option<PathBuf> {
    let stem = translate::qualified_id(slug).ok()?;
    Some(
      self
        .snapshot_dir
        .join("packages")
        .join(format!("{stem}.json")),
    )
  }

  /// `plugins/<org>/<name>/index.json`, the package's own document. The slug
  /// comes from upstream's data, so it must be exactly two plain segments: a
  /// `..` or an empty segment would have the URL resolve somewhere else.
  fn document_url(&self, slug: &str) -> Result<Url> {
    let plain = |segment: &str| {
      !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
          .chars()
          .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let segments: Vec<&str> = slug.split('/').collect();
    if segments.len() != 2 || !segments.iter().all(|s| plain(s)) {
      return Err(Error::Registry(RegistryError::Malformed {
        registry: self.name.clone(),
        reason: format!("slug {slug:?} cannot name a document"),
      }));
    }
    let index = self.index_url()?;
    index.join(&format!("{slug}/index.json")).map_err(|e| {
      Error::Registry(RegistryError::Malformed {
        registry: self.name.clone(),
        reason: e.to_string(),
      })
    })
  }

  /// The package's own document as fetched last, if it is on disk and reads.
  fn read_document(&self, listed: &OasPackage) -> Option<OasPackage> {
    read_document_at(&self.document_path(&listed.slug)?)
  }

  /// The documents kept, by path: listed once, so loading the index opens a
  /// file only for a package one was fetched for, not for every summary.
  fn kept_documents(&self) -> BTreeSet<PathBuf> {
    std::fs::read_dir(self.snapshot_dir.join("packages"))
      .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
      .unwrap_or_default()
  }

  /// Deletes every kept document nothing will read again: its package left
  /// the listing, is no longer listed in summary, or has moved past it. One
  /// is read only while it is current, and a package is fetched again when
  /// it is needed, so what this deletes was dead weight. Best effort.
  fn forget_documents(&self, listing: &BTreeMap<String, OasPackage>) {
    let by_slug: BTreeMap<&str, &OasPackage> =
      listing.values().map(|l| (l.slug.as_str(), l)).collect();
    for path in self.kept_documents() {
      let wanted = read_document_at(&path).is_some_and(|document| {
        by_slug.get(document.slug.as_str()).is_some_and(|listed| {
          listed.is_summary()
            && is_current(listed, &document)
            && self.document_path(&listed.slug).as_ref() == Some(&path)
        })
      });
      if !wanted {
        let _ = std::fs::remove_file(&path);
      }
    }
  }

  /// The snapshot's packages, keyed by slug.
  fn listing(&self) -> Result<BTreeMap<String, OasPackage>> {
    let path = self.snapshot_path();
    if !path.exists() {
      return Err(Error::Registry(RegistryError::NotFetched(
        self.name.clone(),
      )));
    }
    parse_listing(&self.name, &read_snapshot(&path)?)
  }

  /// Fetches one package's own document and keeps it, if it describes what
  /// the listing names. One that does not is not kept: it would never be
  /// read, and a CDN still serving the old document is the likely reason.
  async fn fetch_document(&self, downloader: &Downloader, listed: &OasPackage) -> Result<()> {
    let url = self.document_url(&listed.slug)?;
    let body = match downloader
      .get_if_changed(&url, DOCUMENT_LIMIT, None)
      .await?
    {
      Conditional::Changed { body, .. } => body,
      // Nothing was named for the server to compare against.
      Conditional::Unchanged => unreachable!("a request without validators is answered in full"),
    };
    let malformed = |reason: String| {
      Error::Registry(RegistryError::Malformed {
        registry: self.name.clone(),
        reason,
      })
    };
    let document: OasPackage = serde_json::from_slice(&body)
      .map_err(|e| malformed(format!("{url} is not a package document: {e}")))?;
    if !is_current(listed, &document) {
      return Err(malformed(format!(
        "{url} does not describe {} {} as the package list does yet",
        listed.slug, listed.version
      )));
    }
    let path = self
      .document_path(&listed.slug)
      .ok_or_else(|| malformed(format!("slug {:?} cannot name a file", listed.slug)))?;
    if let Some(dir) = path.parent() {
      fsutil::ensure_dir(dir)?;
    }
    fsutil::write_atomic(&path, &body)
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
  /// that copy only with one that parses and holds a package. `impatient` for
  /// a refresh no one asked for.
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
    // Parsing is not enough. Every field of a file is optional to the
    // parser and an entry that cannot be used is skipped, so an index
    // reshaped this way reads as an index of nothing, and would cost the
    // user every package. Not recorded as asked, like any failed fetch: a
    // newer build must read it at once, and every command should say so.
    if index.packages.is_empty() {
      return Err(Error::Registry(RegistryError::Malformed {
        registry: self.name.clone(),
        reason: "it holds no package this build can install, so nothing here was replaced; \
                 a newer release of Luthier may read it"
          .to_owned(),
      }));
    }
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
  Ok(index_from_listing(
    name,
    &parse_listing(name, json)?,
    &|_| None,
  ))
}

/// A package's own document at `path`, if it is there and reads.
fn read_document_at(path: &Path) -> Option<OasPackage> {
  serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The packages of an index document, keyed by slug.
fn parse_listing(name: &str, json: &[u8]) -> Result<BTreeMap<String, OasPackage>> {
  serde_json::from_slice(json).map_err(|e| {
    Error::Registry(RegistryError::Malformed {
      registry: name.to_owned(),
      reason: e.to_string(),
    })
  })
}

/// Whether `document`, a package's own document, describes what `listed`,
/// its entry in a summary listing, names: the same package at the same
/// latest version, with the same files where the listing has them. One that
/// does not is older or newer than the listing and is not used — an install
/// from it would fetch something other than what `search` showed.
fn is_current(listed: &OasPackage, document: &OasPackage) -> bool {
  document.slug == listed.slug
    && document.version == listed.version
    && listed.versions.iter().all(|(key, version)| {
      document.versions.get(key).is_some_and(|full| {
        full.files.len() == version.files.len()
          && full
            .files
            .iter()
            .zip(&version.files)
            .all(|(a, b)| a.listed() == b.listed())
      })
    })
}

/// The ID each listed package goes by, by slug, and whether it had to be
/// qualified: its name, or `org-name` where two organisations publish the
/// same one, since dropping either would be a worse answer. One place, so
/// the index and [`OasRegistry::complete`] cannot disagree on what an ID
/// names. A slug that makes no usable ID has none and is not listed.
fn listed_ids(raw: &BTreeMap<String, OasPackage>) -> BTreeMap<&str, (PackageId, bool)> {
  let mut claimed: BTreeMap<PackageId, usize> = BTreeMap::new();
  for entry in raw.values() {
    if let Ok(id) = translate::id_from_slug(&entry.slug) {
      *claimed.entry(id).or_default() += 1;
    }
  }
  raw
    .values()
    .filter_map(|entry| {
      let id = translate::id_from_slug(&entry.slug).ok()?;
      let listed = if claimed[&id] > 1 {
        (translate::qualified_id(&entry.slug).ok()?, true)
      } else {
        (id, false)
      };
      Some((entry.slug.as_str(), listed))
    })
    .collect()
}

/// [`index_from_json`] over a parsed listing, reading a package the listing
/// has only in summary from its own document where `document` has one that
/// is current ([`is_current`]).
fn index_from_listing(
  name: &str,
  raw: &BTreeMap<String, OasPackage>,
  document: &dyn Fn(&OasPackage) -> Option<OasPackage>,
) -> RegistryIndex {
  let ids = listed_ids(raw);
  let mut packages: BTreeMap<PackageId, IndexEntry> = BTreeMap::new();
  for listed in raw.values() {
    let Some((id, contested)) = ids.get(listed.slug.as_str()).cloned() else {
      continue;
    };
    // A package listed in summary is described by its own document when
    // one has been fetched and still matches the listing. One that holds
    // nothing installable stays a summary that says so, rather than
    // vanishing from `search` — and is not fetched again until the
    // listing moves.
    let full = listed
      .is_summary()
      .then(|| document(listed))
      .flatten()
      .filter(|full| is_current(listed, full));
    let (entry, mut translated) = match &full {
      Some(full) => match translate::package(full) {
        Ok(translated) => (full, translated),
        Err(reason) => {
          let Ok(mut translated) = translate::package(listed) else {
            continue;
          };
          if let Some(summary) = &mut translated.summary {
            summary.unavailable = Some(format!("its own document has {reason}"));
          }
          (listed, translated)
        }
      },
      None => match translate::package(listed) {
        Ok(translated) => (listed, translated),
        Err(_) => continue,
      },
    };
    if contested {
      translated.notes.push(format!(
        "another organisation publishes {:?} too, so this one is {}",
        translated.manifest.id.as_str(),
        id.as_str()
      ));
    }
    translated.manifest.id = id;

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
        summary: translated.summary,
      },
    );
  }

  RegistryIndex {
    name: name.to_owned(),
    packages,
    problems: Vec::new(),
    engines: Vec::new(),
  }
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
    let kept = self.kept_documents();
    Ok(index_from_listing(
      &self.name,
      &self.listing()?,
      &|listed| {
        let path = self.document_path(&listed.slug)?;
        kept
          .contains(&path)
          .then(|| read_document_at(&path))
          .flatten()
      },
    ))
  }

  async fn complete(&self, wanted: &BTreeSet<PackageId>, impatient: bool) -> Completion {
    let mut completion = Completion::default();
    if wanted.is_empty() {
      return completion;
    }
    let Ok(listing) = self.listing() else {
      // Not fetched, or unreadable: every command already says so.
      return completion;
    };
    self.forget_documents(&listing);
    // Only what was asked for is looked at: a summary this build lists
    // (one with a file for one of its targets) and no current document of.
    // One whose current document had nothing installable was read in full
    // already, and asking again changes nothing until the listing moves.
    let ids = listed_ids(&listing);
    let due: Vec<(PackageId, &OasPackage)> = listing
      .values()
      .filter_map(|listed| {
        let (id, _) = ids.get(listed.slug.as_str())?;
        (wanted.contains(id) && listed.is_summary()).then(|| (id.clone(), listed))
      })
      .filter(|(_, listed)| translate::package(listed).is_ok())
      .filter(|(_, listed)| {
        !self
          .read_document(listed)
          .is_some_and(|document| is_current(listed, &document))
      })
      .collect();
    if due.is_empty() {
      return completion;
    }
    if self.offline {
      completion.failures = due
        .into_iter()
        .map(|(id, _)| (id, "not fetched under --offline".to_owned()))
        .collect();
      return completion;
    }

    let mut downloader = Downloader::new(&self.cache_dir);
    if impatient {
      downloader = downloader.impatient();
    }
    let mut due = due.into_iter();
    while let Some((id, listed)) = due.next() {
      match self.fetch_document(&downloader, listed).await {
        Ok(()) => completion.fetched.push(id),
        Err(error) => {
          let reason = error.to_string();
          // A fetch no one asked for gives up at the first failure that
          // says nothing about the package: with the network gone, every
          // other one would wait out its own timeout to say the same.
          let transport = matches!(
            error,
            Error::Download(
              crate::error::DownloadError::Transport { .. }
                | crate::error::DownloadError::NoHttpClient { .. }
            )
          );
          completion.failures.push((id, reason.clone()));
          if impatient && transport {
            completion
              .failures
              .extend(due.by_ref().map(|(id, _)| (id, reason.clone())));
          }
        }
      }
    }
    completion
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

  /// A listing reshaped under the parser: `files` renamed, which every field
  /// being optional reads as a version with no files — the right shape, and
  /// not one package this build can install. (A listing whose files only
  /// lost `url` and `sha256` is not this: that is a summary, and is read.)
  const NOTHING_TO_INSTALL: &str = r#"{
      "distrho/mverb": { "slug": "distrho/mverb", "version": "1.0.0", "versions": { "1.0.0": {
        "name": "MVerb", "author": "DISTRHO", "description": "Reverb.",
        "license": "gpl-3.0", "type": "effect", "tags": ["reverb"],
        "url": "https://example.invalid/a",
        "assets": [{ "systems": [{"type":"linux"}], "architectures": ["x64"],
          "contains": ["lv2"], "type": "archive", "size": 100,
          "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
          "url": "https://example.invalid/a.tar.xz" }] }}}
    }"#;

  #[tokio::test]
  async fn an_index_with_nothing_to_install_leaves_the_one_on_disk() {
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};

    for unusable in [NOTHING_TO_INSTALL, "{}"] {
      let dir = tempfile::tempdir().unwrap();
      let (server, registry) = served(dir.path(), TWO_ORGS).await;
      registry.refresh().await.unwrap();
      let before = std::fs::read(registry.snapshot_path()).unwrap();
      server.reset().await;
      Mock::given(method("GET"))
        .respond_with(
          ResponseTemplate::new(200)
            .insert_header("etag", "\"v2\"")
            .set_body_string(unusable),
        )
        .mount(&server)
        .await;

      let error = registry.refresh().await.unwrap_err();
      assert!(error.to_string().contains("no package"), "{error}");
      assert_eq!(std::fs::read(registry.snapshot_path()).unwrap(), before);
      assert_eq!(registry.load_index().unwrap().len(), 2);

      // Every command asks again, as after any failed fetch, or a build that
      // can read it would wait a day to find out ...
      let day = SignedDuration::from_hours(24);
      backdate(dir.path(), SignedDuration::from_hours(48), None);
      assert!(registry.refresh_if_due(day).await.unwrap().is_err());
      assert!(registry.refresh_if_due(day).await.unwrap().is_err());

      // ... and about the copy that is still here.
      assert_eq!(
        named(&server).await.last().unwrap().as_deref(),
        Some("\"v1\"")
      );
    }
  }

  #[tokio::test]
  async fn an_index_with_nothing_to_install_is_not_written_the_first_time() {
    let dir = tempfile::tempdir().unwrap();
    let (_server, registry) = served(dir.path(), NOTHING_TO_INSTALL).await;

    assert!(registry.refresh().await.is_err());
    assert!(!registry.snapshot_path().exists());
    assert!(matches!(
      registry.load_index(),
      Err(Error::Registry(RegistryError::NotFetched(_)))
    ));
  }

  /// One package's own document, version by version: each version has one
  /// Linux file, whose URL and checksum say which version it is.
  fn document(version: &str, versions: &[&str]) -> String {
    let versions: Vec<String> = versions
      .iter()
      .map(|v| {
        format!(
          r#""{v}": {{ "name": "WSTD EQ", "author": "Wasted Audio", "license": "gpl-3.0",
            "type": "effect", "url": "https://example.invalid/home",
            "files": [{{ "systems": [{{"type":"linux"}}], "architectures": ["x64"],
              "contains": ["clap"], "type": "archive", "size": 100,
              "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
              "url": "https://example.invalid/wstd-eq-{v}.tar.xz" }}] }}"#
        )
      })
      .collect();
    format!(
      r#"{{ "slug": "wasted-audio/wstd-eq", "version": "{version}", "downloads": 7,
        "versions": {{ {} }} }}"#,
      versions.join(",")
    )
  }

  /// The type listing as open-audio-stack-registry#933 publishes it: the
  /// latest version alone, its files without `url` or `sha256`.
  fn summary_listing(version: &str) -> String {
    let mut package: serde_json::Value =
      serde_json::from_str(&document(version, &[version])).unwrap();
    for file in package["versions"][version]["files"]
      .as_array_mut()
      .unwrap()
    {
      let file = file.as_object_mut().unwrap();
      file.remove("url");
      file.remove("sha256");
    }
    serde_json::json!({ "wasted-audio/wstd-eq": package }).to_string()
  }

  /// A server publishing `listing` and, at the package's own path,
  /// `document`, and a registry reading it into `dir`.
  async fn served_in_summary(
    dir: &Path,
    listing: String,
    document: String,
  ) -> (wiremock::MockServer, OasRegistry) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/plugins/index.json"))
      .respond_with(ResponseTemplate::new(200).set_body_string(listing))
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/plugins/wasted-audio/wstd-eq/index.json"))
      .respond_with(ResponseTemplate::new(200).set_body_string(document))
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

  fn wstd_eq() -> BTreeSet<PackageId> {
    BTreeSet::from([PackageId::new("wstd-eq").unwrap()])
  }

  async fn document_requests(server: &wiremock::MockServer) -> usize {
    server
      .received_requests()
      .await
      .unwrap()
      .iter()
      .filter(|r| r.url.path().ends_with("wstd-eq/index.json"))
      .count()
  }

  #[tokio::test]
  async fn a_package_listed_in_summary_is_fetched_once_when_it_is_needed() {
    let dir = tempfile::tempdir().unwrap();
    let (server, registry) = served_in_summary(
      dir.path(),
      summary_listing("1.1.0"),
      document("1.1.0", &["1.0.0", "1.1.0"]),
    )
    .await;

    // A listing of summaries is not an index of nothing: it is kept.
    let outcome = registry.refresh().await.unwrap();
    assert_eq!(outcome.packages, 1);

    // Searchable, and nothing to download until it is completed.
    let index = registry.load_index().unwrap();
    let entry = &index.packages[&PackageId::new("wstd-eq").unwrap()];
    assert!(entry.summary.is_some());
    assert!(
      entry
        .manifest
        .releases
        .iter()
        .all(|r| r.artifacts.is_empty())
    );

    // Nothing is fetched for what nobody asked for.
    let unrelated = BTreeSet::from([PackageId::new("surge-xt").unwrap()]);
    assert_eq!(
      registry.complete(&unrelated, true).await,
      Completion::default()
    );
    assert_eq!(document_requests(&server).await, 0);

    let completion = registry.complete(&wstd_eq(), true).await;
    assert_eq!(
      completion.fetched,
      wstd_eq().into_iter().collect::<Vec<_>>()
    );
    assert!(completion.failures.is_empty(), "{completion:?}");

    // Described in full now: every version, each with what to download.
    let index = registry.load_index().unwrap();
    let entry = &index.packages[&PackageId::new("wstd-eq").unwrap()];
    assert_eq!(entry.summary, None);
    let versions: Vec<String> = entry
      .manifest
      .releases
      .iter()
      .map(|r| r.version.to_string())
      .collect();
    assert_eq!(versions, vec!["1.0.0", "1.1.0"]);
    assert!(
      entry
        .manifest
        .releases
        .iter()
        .all(|r| !r.artifacts.is_empty())
    );

    // And kept: asking again costs nothing while the listing stands still.
    assert_eq!(
      registry.complete(&wstd_eq(), true).await,
      Completion::default()
    );
    assert_eq!(document_requests(&server).await, 1);
  }

  #[tokio::test]
  async fn a_document_the_listing_has_moved_past_is_not_used() {
    let dir = tempfile::tempdir().unwrap();
    let (_server, registry) = served_in_summary(
      dir.path(),
      summary_listing("1.0.0"),
      document("1.0.0", &["1.0.0"]),
    )
    .await;
    registry.refresh().await.unwrap();
    registry.complete(&wstd_eq(), true).await;
    assert!(
      registry.load_index().unwrap().packages[&PackageId::new("wstd-eq").unwrap()]
        .summary
        .is_none()
    );

    // 1.1.0 is listed; the document kept describes 1.0.0, and installing
    // from it would fetch something other than what `search` shows.
    fsutil::write_atomic(
      &registry.snapshot_path(),
      summary_listing("1.1.0").as_bytes(),
    )
    .unwrap();
    let index = registry.load_index().unwrap();
    let entry = &index.packages[&PackageId::new("wstd-eq").unwrap()];
    assert!(entry.summary.is_some());
    assert_eq!(entry.manifest.releases[0].version.to_string(), "1.1.0");
  }

  #[tokio::test]
  async fn documents_nothing_will_read_again_are_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (_server, registry) = served_in_summary(
      dir.path(),
      summary_listing("1.1.0"),
      document("1.1.0", &["1.1.0"]),
    )
    .await;
    registry.refresh().await.unwrap();

    // One for a package the listing no longer carries.
    let gone = dir.path().join("registries/oas/packages/gone-pkg.json");
    std::fs::create_dir_all(gone.parent().unwrap()).unwrap();
    std::fs::write(
      &gone,
      document("1.0.0", &["1.0.0"]).replace("wasted-audio/wstd-eq", "gone/pkg"),
    )
    .unwrap();
    // And one for this package, from before the listing moved to 1.1.0.
    let kept = registry.document_path("wasted-audio/wstd-eq").unwrap();
    std::fs::write(&kept, document("1.0.0", &["1.0.0"])).unwrap();

    let completion = registry.complete(&wstd_eq(), true).await;
    assert!(completion.failures.is_empty(), "{completion:?}");
    assert!(!gone.exists());
    // Replaced by the current one, not merely deleted.
    let current: OasPackage = serde_json::from_slice(&std::fs::read(&kept).unwrap()).unwrap();
    assert_eq!(current.version, "1.1.0");
  }

  #[tokio::test]
  async fn a_document_behind_the_listing_is_a_failure_and_is_not_kept() {
    // A CDN still serving the document from before the release.
    let dir = tempfile::tempdir().unwrap();
    let (_server, registry) = served_in_summary(
      dir.path(),
      summary_listing("1.1.0"),
      document("1.0.0", &["1.0.0"]),
    )
    .await;
    registry.refresh().await.unwrap();

    let completion = registry.complete(&wstd_eq(), true).await;
    assert!(completion.fetched.is_empty());
    assert_eq!(completion.failures.len(), 1);
    assert!(
      completion.failures[0].1.contains("does not describe"),
      "{completion:?}"
    );
    assert!(
      !registry
        .document_path("wasted-audio/wstd-eq")
        .unwrap()
        .exists()
    );
  }

  #[tokio::test]
  async fn a_document_with_nothing_installable_says_so_and_is_not_asked_for_again() {
    let dir = tempfile::tempdir().unwrap();
    let listing = summary_listing("1.1.0");
    // The listing cannot show it, but the file is a .pkg.
    let document = document("1.1.0", &["1.1.0"]).replace(".tar.xz", ".pkg");
    let (server, registry) = served_in_summary(dir.path(), listing, document).await;
    registry.refresh().await.unwrap();

    registry.complete(&wstd_eq(), true).await;
    let index = registry.load_index().unwrap();
    let summary = index.packages[&PackageId::new("wstd-eq").unwrap()]
      .summary
      .clone()
      .expect("still a summary");
    assert!(
      summary
        .unavailable
        .as_deref()
        .is_some_and(|why| why.contains("no file this build can install")),
      "{summary:?}"
    );

    assert_eq!(
      registry.complete(&wstd_eq(), true).await,
      Completion::default()
    );
    assert_eq!(document_requests(&server).await, 1);
  }

  #[tokio::test]
  async fn nothing_is_fetched_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (server, registry) = served_in_summary(
      dir.path(),
      summary_listing("1.1.0"),
      document("1.1.0", &["1.1.0"]),
    )
    .await;
    registry.refresh().await.unwrap();

    let offline = OasRegistry::new(
      "oas",
      Url::parse(&format!("{}/", server.uri())).unwrap(),
      dir.path().join("registries/oas"),
      dir.path().join("cache"),
    )
    .offline(true);
    let completion = offline.complete(&wstd_eq(), true).await;
    assert!(completion.fetched.is_empty());
    assert_eq!(completion.failures.len(), 1);
    assert_eq!(document_requests(&server).await, 0);
  }

  /// Against a copy of the published registry laid out as on the site:
  /// `$LUTHIER_OAS_MIRROR/plugins/index.json`, and the documents of the
  /// packages named in `$LUTHIER_OAS_MIRROR_PACKAGES` (comma-separated IDs)
  /// under `plugins/<org>/<name>/index.json`. Skipped unless both are set,
  /// like [`the_published_index_translates`], so the suite never needs the
  /// network (§55). Run it on a listing in summary to see that every
  /// package is listed and the named ones complete.
  #[tokio::test]
  async fn the_published_listing_completes() {
    let (Ok(mirror), Ok(named)) = (
      std::env::var("LUTHIER_OAS_MIRROR"),
      std::env::var("LUTHIER_OAS_MIRROR_PACKAGES"),
    ) else {
      return;
    };
    let dir = tempfile::tempdir().unwrap();
    let registry = OasRegistry::new(
      "oas",
      Url::from_directory_path(&mirror).unwrap(),
      dir.path().join("registries/oas"),
      dir.path().join("cache"),
    );
    registry.refresh().await.unwrap();
    let wanted: BTreeSet<PackageId> = named
      .split(',')
      .map(|id| PackageId::new(id.trim()).unwrap())
      .collect();

    let before = registry.load_index().unwrap();
    let linux = Target::host().unwrap();
    let listed = before
      .packages
      .values()
      .filter(|e| !e.formats_for(&linux).is_empty())
      .count();
    let summaries = before
      .packages
      .values()
      .filter(|e| e.summary.is_some())
      .count();
    eprintln!(
      "{} packages, {summaries} in summary, {listed} for {linux}",
      before.len()
    );

    let completion = registry.complete(&wanted, false).await;
    assert!(completion.failures.is_empty(), "{completion:?}");
    let after = registry.load_index().unwrap();
    for id in &wanted {
      let entry = &after.packages[id];
      assert_eq!(entry.summary, None, "{id}");
      let newest = crate::resolver::newest_installable(&entry.manifest, &linux)
        .unwrap_or_else(|| panic!("{id} has nothing for {linux}"));
      eprintln!(
        "{id}: {} releases, newest installable {}, {:?}",
        entry.manifest.releases.len(),
        newest.version,
        entry.formats_for(&linux)
      );
    }
  }

  #[test]
  fn a_slug_that_is_not_two_plain_segments_names_no_document() {
    let dir = tempfile::tempdir().unwrap();
    let registry = OasRegistry::new(
      "oas",
      Url::parse("https://example.invalid/registry/").unwrap(),
      dir.path().join("registries/oas"),
      dir.path().join("cache"),
    );
    assert_eq!(
      registry
        .document_url("wasted-audio/wstd-eq")
        .unwrap()
        .as_str(),
      "https://example.invalid/registry/plugins/wasted-audio/wstd-eq/index.json"
    );
    for slug in [
      "../x", "a/..", "a/b/c", "a", "a//b", "a/b?c", "a/./b", "/a/b",
    ] {
      assert!(registry.document_url(slug).is_err(), "{slug}");
    }
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
