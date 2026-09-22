//! Package discovery.
//!
//! A registry is data, not a service (§9). The MVP reads a directory of YAML
//! manifests, whether that directory is a git checkout the user maintains or a
//! snapshot fetched over HTTPS. [`RegistryProvider`] is the seam: adding a
//! `GitRegistry` backed by `gix`, or an index served as a single JSON file,
//! means adding an implementation, not touching anything below.

mod http;
mod local;
pub mod oas;
pub mod provenance;
pub mod signature;

pub use http::HttpSnapshotRegistry;
pub use local::LocalRegistry;
pub use oas::OasRegistry;

use crate::download::{Downloader, NoProgress};
use crate::error::{Error, RegistryError, Result};
use luthier_manifest::{Content, Manifest, PackageId, ParseMode, Sha256Hash, Target};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use url::Url;

/// One package as read from a registry.
#[derive(Debug, Clone)]
pub struct IndexEntry {
  pub manifest: Manifest,
  /// Where the manifest was read from, for error messages.
  pub path: PathBuf,
  /// Digest of the manifest bytes, recorded on install so that registry
  /// drift — the same version republished with different contents — is
  /// detectable later.
  pub digest: Sha256Hash,
  /// Fields this build did not recognise. Non-fatal; see §7.
  pub unknown_fields: Vec<String>,
  /// Which registry this entry came from. Per-entry rather than per-index
  /// because an index can be a merge of several, and what gets recorded on
  /// install is where *this* package came from.
  pub registry: String,
  /// Caveats the provider recorded: data it had to interpret rather than
  /// read. A source whose vocabulary is coarser than this schema's — the
  /// Open Audio Stack registry's single `gpl-3.0` for two different grants —
  /// leaves a note here rather than presenting the guess as fact.
  pub notes: Vec<String>,
}

/// Every package a registry offers.
#[derive(Debug, Clone)]
pub struct RegistryIndex {
  /// The registry this index came from, or every one it was merged from.
  pub name: String,
  pub packages: BTreeMap<PackageId, IndexEntry>,
  /// Which packages play which content, from every registry's
  /// `engines.toml`. An entry may name a package from any registry, which is
  /// the point: the bench knows DrumCraker plays DrumGizmo kits, and the
  /// Open Audio Stack registry, which carries DrumCraker, has no field to say
  /// so.
  pub engines: Vec<luthier_manifest::EngineEntry>,
  /// Registries that could not be read. Kept rather than raised so that one
  /// bench a user has not fetched yet does not cost them every command, and
  /// reported rather than dropped so the absence is never silent.
  pub problems: Vec<String>,
}

/// A search hit and why it matched.
#[derive(Debug, Clone)]
pub struct SearchHit<'a> {
  pub entry: &'a IndexEntry,
  pub score: u32,
}

impl RegistryIndex {
  /// Merges configured registries into one view, earliest wins.
  ///
  /// Precedence is the configured order, and the curated bench is first.
  /// That is what lets a hand-written manifest correct a broader source:
  /// where a derived entry installs the wrong thing, or an upstream record
  /// points at the wrong artifact, the bench's version of that package is
  /// the one the resolver sees. Refusing the collision instead would kill
  /// that ability, and namespacing it would make every ID two-part.
  pub fn merge(
    indexes: impl IntoIterator<Item = std::result::Result<RegistryIndex, Error>>,
  ) -> Result<RegistryIndex> {
    let mut names: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut packages: BTreeMap<PackageId, IndexEntry> = BTreeMap::new();
    let mut engines: Vec<luthier_manifest::EngineEntry> = Vec::new();
    let mut first_error: Option<Error> = None;

    for result in indexes {
      let index = match result {
        Ok(index) => index,
        Err(error) => {
          problems.push(error.to_string());
          first_error.get_or_insert(error);
          continue;
        }
      };
      names.push(index.name.clone());
      problems.extend(index.problems);
      engines.extend(index.engines);
      for (id, entry) in index.packages {
        // `or_insert` and not `insert`: the first registry to claim an
        // ID keeps it.
        packages.entry(id).or_insert(entry);
      }
    }

    if names.is_empty() {
      return Err(
        first_error
          .unwrap_or_else(|| Error::Registry(RegistryError::NoSuchRegistry("default".into()))),
      );
    }

    Ok(RegistryIndex {
      name: names.join(", "),
      packages,
      problems,
      engines,
    })
  }

  pub fn get(&self, id: &PackageId) -> Result<&IndexEntry> {
    self.packages.get(id).ok_or_else(|| {
      Error::Registry(RegistryError::PackageNotFound {
        id: id.to_string(),
        registry: self.name.clone(),
      })
    })
  }

  /// Engines that play `content`, in configured order, each named once.
  pub fn engines_for(&self, content: &Content) -> Vec<&luthier_manifest::EngineEntry> {
    let mut seen = BTreeSet::new();
    self
      .engines
      .iter()
      .filter(|entry| entry.plays.contains(content))
      .filter(|entry| seen.insert(&entry.package))
      .collect()
  }

  pub fn len(&self) -> usize {
    self.packages.len()
  }

  pub fn is_empty(&self) -> bool {
    self.packages.is_empty()
  }

  /// Ranked substring search over ID, name, category, tags and description.
  ///
  /// Ties break on package ID so the same query always prints the same
  /// order, which matters for scripted use and for tests (§57).
  pub fn search(&self, query: &str) -> Vec<SearchHit<'_>> {
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<SearchHit<'_>> = self
      .packages
      .values()
      .filter_map(|entry| {
        let score = score(entry, &needle);
        (score > 0).then_some(SearchHit { entry, score })
      })
      .collect();
    hits.sort_by(|a, b| {
      b.score
        .cmp(&a.score)
        .then_with(|| a.entry.manifest.id.cmp(&b.entry.manifest.id))
    });
    hits
  }

  /// Packages with at least one release providing an artifact for `target`.
  pub fn installable_for(&self, target: &Target) -> Vec<&IndexEntry> {
    self
      .packages
      .values()
      .filter(|entry| {
        entry.manifest.is_external()
          || entry
            .manifest
            .releases
            .iter()
            .any(|r| r.artifacts_for(target).next().is_some())
      })
      .collect()
  }
}

fn score(entry: &IndexEntry, needle: &str) -> u32 {
  if needle.is_empty() {
    return 1;
  }
  let manifest = &entry.manifest;
  let id = manifest.id.as_str().to_lowercase();
  let name = manifest.name.to_lowercase();

  let mut score = 0;
  if id == needle || name == needle {
    score += 100;
  } else if id.contains(needle) {
    score += 50;
  }
  if name.contains(needle) && name != needle {
    score += 40;
  }
  // The primary category is a single controlled value, so it either matches
  // or it does not; tags are free-form, so a substring hit still counts but
  // scores lower.
  if manifest.category.as_str() == needle {
    score += 30;
  }
  if manifest.tags.iter().any(|t| t.to_lowercase() == needle) {
    score += 25;
  } else if manifest
    .tags
    .iter()
    .any(|t| t.to_lowercase().contains(needle))
  {
    score += 15;
  }
  if manifest
    .description
    .as_deref()
    .is_some_and(|d| d.to_lowercase().contains(needle))
  {
    score += 10;
  }
  score
}

/// What a refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
  pub registry: String,
  pub packages: usize,
  /// False when the snapshot was already current.
  pub updated: bool,
}

/// A source of package manifests.
#[async_trait::async_trait]
pub trait RegistryProvider: Send + Sync {
  /// The configured name, used in messages and to key the snapshot directory.
  fn name(&self) -> &str;

  /// Brings the local snapshot up to date.
  async fn refresh(&self) -> Result<RefreshOutcome>;

  /// Parses the local snapshot. Does not touch the network.
  fn load_index(&self) -> Result<RegistryIndex>;
}

/// Reads every manifest under `root` into an index.
///
/// Shared by [`LocalRegistry`] and by [`HttpSnapshotRegistry`], which differ
/// only in how the directory got there.
pub(crate) fn build_index(name: &str, root: &Path, mode: ParseMode) -> Result<RegistryIndex> {
  if !root.is_dir() {
    return Err(Error::Registry(RegistryError::NotADirectory(
      root.to_path_buf(),
    )));
  }

  let mut packages: BTreeMap<PackageId, IndexEntry> = BTreeMap::new();
  let mut sources: BTreeMap<PackageId, PathBuf> = BTreeMap::new();

  for path in manifest_files(root)? {
    let bytes = std::fs::read(&path).map_err(|e| Error::io("read", &path, e))?;
    let text = String::from_utf8_lossy(&bytes);
    let display = path
      .strip_prefix(root)
      .unwrap_or(&path)
      .display()
      .to_string();
    let parsed = luthier_manifest::from_toml(&text, &display, mode)?;

    // Filing a manifest as <id>.toml is what makes a duplicate ID visible
    // in a pull request diff rather than only at load time.
    let stem = path
      .file_stem()
      .unwrap_or_default()
      .to_string_lossy()
      .into_owned();
    if parsed.manifest.id.as_str() != stem {
      return Err(Error::Registry(RegistryError::IdFilenameMismatch {
        path: path.clone(),
        declared: parsed.manifest.id.clone(),
        expected: stem,
      }));
    }

    if let Some(first) = sources.get(&parsed.manifest.id) {
      return Err(Error::Registry(RegistryError::DuplicatePackage {
        registry: name.to_owned(),
        id: parsed.manifest.id.clone(),
        first: first.clone(),
        second: path.clone(),
      }));
    }
    sources.insert(parsed.manifest.id.clone(), path.clone());

    let digest = {
      use sha2::{Digest, Sha256};
      let mut hasher = Sha256::new();
      hasher.update(&bytes);
      Sha256Hash::from_bytes(hasher.finalize().into())
    };

    packages.insert(
      parsed.manifest.id.clone(),
      IndexEntry {
        manifest: parsed.manifest,
        path,
        digest,
        unknown_fields: parsed.unknown_fields,
        registry: name.to_owned(),
        notes: Vec::new(),
      },
    );
  }

  let engines_path = root.join(luthier_manifest::ENGINES_FILE);
  let engines = if engines_path.is_file() {
    let text =
      std::fs::read_to_string(&engines_path).map_err(|e| Error::io("read", &engines_path, e))?;
    luthier_manifest::EnginesFile::parse(&text)
      .map_err(|e| {
        Error::Registry(RegistryError::Malformed {
          registry: name.to_owned(),
          reason: format!("{}: {e}", luthier_manifest::ENGINES_FILE),
        })
      })?
      .entries
  } else {
    Vec::new()
  };

  Ok(RegistryIndex {
    name: name.to_owned(),
    packages,
    problems: Vec::new(),
    engines,
  })
}

/// Every `.toml` file under `root`, sorted, skipping hidden and
/// non-manifest directories.
///
/// Re-exported from `luthier-manifest` so the registry's validator can walk a
/// tree without an async runtime, while both crates keep one implementation of
/// the rules — notably that a symlink in an untrusted snapshot is skipped.
pub use luthier_manifest::manifest_files;

/// the manifest's checksum.
pub(crate) async fn download_unverified(
  downloader: &Downloader,
  url: &Url,
  destination: &Path,
  offline: bool,
) -> Result<(u64, Sha256Hash)> {
  if offline {
    return Err(Error::Download(crate::error::DownloadError::Offline {
      url: url.to_string(),
    }));
  }
  // Fetch with a digest of "whatever it turns out to be": ask for the bytes,
  // then move them where we want them.
  let placeholder = Sha256Hash::from_bytes([0; 32]);
  match downloader
    .fetch(url, &placeholder, None, &mut NoProgress)
    .await
  {
    Ok(fetched) => {
      std::fs::rename(&fetched.path, destination)
        .map_err(|e| Error::io("move snapshot", destination, e))?;
      Ok((fetched.bytes, fetched.sha256))
    }
    Err(Error::Download(crate::error::DownloadError::ChecksumMismatch { actual, .. })) => {
      // Expected: the digest is unknown up front. The bytes landed in the
      // cache under their real digest before being rejected, so re-fetch
      // now that the digest is known rather than downloading twice.
      let fetched = downloader
        .fetch(url, &actual, None, &mut NoProgress)
        .await?;
      std::fs::rename(&fetched.path, destination)
        .map_err(|e| Error::io("move snapshot", destination, e))?;
      Ok((fetched.bytes, fetched.sha256))
    }
    Err(other) => Err(other),
  }
}

#[cfg(test)]
mod tests {

  fn index_of(name: &str, ids: &[&str]) -> RegistryIndex {
    let mut packages = BTreeMap::new();
    for id in ids {
      let text = format!(
        "schema = 1\nid = \"{id}\"\nname = \"{name} {id}\"\nkind = \"external\"\n\
                 category = \"effect\"\ndescription = \"x\"\n\
                 license = {{ kind = \"open-source\", spdx = \"MIT\" }}\n"
      );
      let manifest = luthier_manifest::from_toml(&text, id, ParseMode::Strict)
        .unwrap()
        .manifest;
      packages.insert(
        manifest.id.clone(),
        IndexEntry {
          manifest,
          path: PathBuf::from(format!("{id}.toml")),
          digest: Sha256Hash::from_bytes([0; 32]),
          unknown_fields: Vec::new(),
          registry: name.to_owned(),
          notes: Vec::new(),
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

  fn engine(package: &str, plays: &[&str]) -> luthier_manifest::EngineEntry {
    luthier_manifest::EngineEntry {
      package: PackageId::new(package).unwrap(),
      plays: plays.iter().map(|p| p.parse().unwrap()).collect(),
      detect: Vec::new(),
      extra: Default::default(),
    }
  }

  #[test]
  fn a_bench_names_engines_from_any_registry() {
    // DrumCraker comes from the Open Audio Stack registry, which cannot say
    // it plays DrumGizmo kits. The bench says so, and the merged index
    // keeps the answer whichever registry carries the package.
    let mut bench = index_of("bench", &["drumgizmo"]);
    bench.engines = vec![
      engine("drumgizmo", &["drumgizmo"]),
      engine("drumcraker", &["drumgizmo"]),
      engine("sfizz", &["sfz"]),
    ];
    let merged = RegistryIndex::merge([Ok(bench), Ok(index_of("oas", &["drumcraker"]))]).unwrap();

    let ids: Vec<&str> = merged
      .engines_for(&Content::Drumgizmo)
      .iter()
      .map(|e| e.package.as_str())
      .collect();
    assert_eq!(ids, vec!["drumgizmo", "drumcraker"]);
    assert!(merged.engines_for(&Content::Sf2).is_empty());
  }

  #[test]
  fn an_engine_two_registries_both_name_is_listed_once() {
    let mut first = index_of("bench", &[]);
    first.engines = vec![engine("sfizz", &["sfz"])];
    let mut second = index_of("other", &[]);
    second.engines = vec![engine("sfizz", &["sfz", "sf2"])];

    let merged = RegistryIndex::merge([Ok(first), Ok(second)]).unwrap();
    assert_eq!(merged.engines_for(&Content::Sfz).len(), 1);
    // A later registry can still add content an earlier one did not list.
    assert_eq!(merged.engines_for(&Content::Sf2).len(), 1);
  }

  #[test]
  fn an_engines_file_at_the_root_is_read_and_is_not_a_manifest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
      dir.path().join("engines.toml"),
      "schema = 1\n\n[[engine]]\npackage = \"sfizz\"\nplays = [\"sfz\"]\n",
    )
    .unwrap();

    let index = build_index("bench", dir.path(), ParseMode::Strict).unwrap();
    assert!(index.is_empty());
    assert_eq!(index.engines.len(), 1);
    assert_eq!(index.engines[0].package.as_str(), "sfizz");
  }

  #[test]
  fn the_first_registry_to_claim_an_id_keeps_it() {
    // This is what lets a curated bench correct a broader source: where
    // the two carry the same package, the bench's manifest is the one the
    // resolver sees.
    let merged = RegistryIndex::merge([
      Ok(index_of("bench", &["surge-xt", "sfizz"])),
      Ok(index_of("oas", &["surge-xt", "dexed"])),
    ])
    .unwrap();

    assert_eq!(merged.packages.len(), 3);
    let contested = &merged.packages[&PackageId::new("surge-xt").unwrap()];
    assert_eq!(contested.registry, "bench");
    assert_eq!(contested.manifest.name, "bench surge-xt");
    assert_eq!(
      merged.packages[&PackageId::new("dexed").unwrap()].registry,
      "oas"
    );
  }

  #[test]
  fn a_registry_that_cannot_be_read_is_reported_not_fatal() {
    // A second bench the user has not fetched yet must not cost them
    // every command against the first.
    let merged = RegistryIndex::merge([
      Ok(index_of("bench", &["sfizz"])),
      Err(Error::Registry(RegistryError::NotFetched("oas".into()))),
    ])
    .unwrap();

    assert_eq!(merged.packages.len(), 1);
    assert_eq!(merged.problems.len(), 1);
    assert!(merged.problems[0].contains("oas"), "{:?}", merged.problems);
  }

  #[test]
  fn nothing_readable_at_all_is_still_an_error() {
    let err = RegistryIndex::merge([Err(Error::Registry(RegistryError::NotFetched(
      "oas".into(),
    )))])
    .unwrap_err();
    assert!(err.to_string().contains("has not been fetched"), "{err}");
  }
  use super::*;

  fn fixture_registry() -> (tempfile::TempDir, RegistryIndex) {
    let dir = tempfile::tempdir().unwrap();
    let plugins = dir.path().join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();

    let write = |id: &str, name: &str, tags: &str, description: &str| {
      std::fs::write(
        plugins.join(format!("{id}.toml")),
        format!(
          r#"schema = 1
id = "{id}"
name = "{name}"
kind = "plugin"
category = "instrument"
tags = ["{tags}"]
description = "{description}"
license = {{ kind = "open-source", spdx = "GPL-3.0-or-later" }}

[[releases]]
version = "1.0.0"

[[releases.artifacts]]
target = {{ os = "linux", arch = "x86_64" }}
source = {{ type = "http", url = "https://example.invalid/{id}.tar.gz" }}
archive = "tar.gz"
checksum = {{ sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" }}
provides = ["clap"]
install = [{{ format = "clap", source = "{name}.clap", kind = "file" }}]
"#
        ),
      )
      .unwrap();
    };
    write("surge-xt", "Surge", "synthesizer", "A hybrid synthesizer.");
    write(
      "dexed",
      "Dexed",
      "synthesizer",
      "FM synth modelled on the DX7.",
    );
    write(
      "dragonfly-reverb",
      "Dragonfly",
      "reverb",
      "A hall reverb effect.",
    );

    let index = build_index("default", dir.path(), ParseMode::Strict).unwrap();
    (dir, index)
  }

  #[test]
  fn index_reads_every_manifest() {
    let (_dir, index) = fixture_registry();
    assert_eq!(index.len(), 3);
    assert!(index.get(&PackageId::new("dexed").unwrap()).is_ok());
  }

  #[test]
  fn search_ranks_by_how_well_a_package_matches() {
    let (_dir, index) = fixture_registry();
    let hits = index.search("synthesizer");
    let ids: Vec<&str> = hits.iter().map(|h| h.entry.manifest.id.as_str()).collect();
    // Both are in the `synthesizer` category, but Surge's description
    // mentions the word too, so it ranks higher.
    assert_eq!(ids, vec!["surge-xt", "dexed"]);
    assert!(hits[0].score > hits[1].score);

    assert_eq!(index.search("dexed")[0].entry.manifest.id.as_str(), "dexed");
  }

  #[test]
  fn equally_good_matches_are_ordered_by_id() {
    // Determinism (§57): when scores tie there must still be exactly one
    // possible order, or scripted use and tests become flaky.
    let (_dir, index) = fixture_registry();
    let hits = index.search("");
    let ids: Vec<&str> = hits.iter().map(|h| h.entry.manifest.id.as_str()).collect();
    assert_eq!(ids, vec!["dexed", "dragonfly-reverb", "surge-xt"]);
    assert!(
      hits.iter().all(|h| h.score == hits[0].score),
      "this query should tie"
    );

    for _ in 0..5 {
      let repeat: Vec<&str> = index
        .search("")
        .iter()
        .map(|h| h.entry.manifest.id.as_str())
        .collect();
      assert_eq!(repeat, ids);
    }
  }

  #[test]
  fn search_matches_descriptions_too() {
    let (_dir, index) = fixture_registry();
    let hits = index.search("hall");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.manifest.id.as_str(), "dragonfly-reverb");
  }

  #[test]
  fn an_unknown_package_names_the_registry_and_suggests_search() {
    let (_dir, index) = fixture_registry();
    let err = index
      .get(&PackageId::new("nonexistent").unwrap())
      .unwrap_err();
    assert!(err.to_string().contains("default"), "{err}");
    assert!(err.hint().unwrap().contains("luthier search"));
  }

  #[test]
  fn a_manifest_filed_under_the_wrong_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("wrong-name.toml"),
            "schema = 1\nid = \"dexed\"\nname = \"Dexed\"\nkind = \"plugin\"\ncategory = \"instrument\"\nlicense = { kind = \"open-source\", spdx = \"MIT\" }\nreleases = []\n",
        )
        .unwrap();
    let err = build_index("default", dir.path(), ParseMode::Lenient).unwrap_err();
    assert!(err.to_string().contains("filed as"), "{err}");
  }

  #[test]
  fn a_duplicate_package_id_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["plugins", "libraries"] {
      let d = dir.path().join(sub);
      std::fs::create_dir_all(&d).unwrap();
      std::fs::write(
                d.join("dexed.toml"),
                "schema = 1\nid = \"dexed\"\nname = \"Dexed\"\nkind = \"plugin\"\ncategory = \"instrument\"\nlicense = { kind = \"open-source\", spdx = \"MIT\" }\nreleases = []\n",
            )
            .unwrap();
    }
    let err = build_index("default", dir.path(), ParseMode::Lenient).unwrap_err();
    assert!(err.to_string().contains("twice"), "{err}");
  }

  #[test]
  fn schema_directory_and_hidden_files_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("schemas")).unwrap();
    std::fs::write(
      dir.path().join("schemas/package-v1.toml"),
      "not = \"a manifest\"",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/config.toml"), "not = \"a manifest\"").unwrap();
    let index = build_index("default", dir.path(), ParseMode::Lenient).unwrap();
    assert!(index.is_empty());
  }
}
