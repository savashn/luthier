//! A registry fetched as a snapshot over HTTPS.
//!
//! The registry is a git repository (§9), but the client does not need a git
//! implementation to read it: forges publish a tarball of any branch, and that
//! is enough for the MVP. The download goes through the same verified
//! downloader and the same hardened extractor as any plugin artifact — a
//! registry snapshot is untrusted input too.
//!
//! A `GitRegistry` using `gix` can be added later behind [`RegistryProvider`]
//! without touching anything that consumes an index.

use super::{
  RefreshOutcome, RegistryIndex, RegistryProvider, build_index, download_unverified, provenance,
};
use crate::archive::{self, ExtractLimits};
use crate::download::Downloader;
use crate::error::{Error, RegistryError, Result};
use crate::fsutil;
use luthier_manifest::{ArchiveFormat, ParseMode};
use std::path::{Path, PathBuf};
use url::Url;

pub struct HttpSnapshotRegistry {
  name: String,
  url: Url,
  /// Where the extracted snapshot lives between runs.
  snapshot_dir: PathBuf,
  cache_dir: PathBuf,
  offline: bool,
}

impl HttpSnapshotRegistry {
  pub fn new(
    name: impl Into<String>,
    url: Url,
    snapshot_dir: impl Into<PathBuf>,
    cache_dir: impl Into<PathBuf>,
  ) -> Self {
    Self {
      name: name.into(),
      url,
      snapshot_dir: snapshot_dir.into(),
      cache_dir: cache_dir.into(),
      offline: false,
    }
  }

  pub fn offline(mut self, offline: bool) -> Self {
    self.offline = offline;
    self
  }

  /// Where manifests are read from once a snapshot has been extracted.
  pub fn root(&self) -> &Path {
    &self.snapshot_dir
  }

  /// The directory holding every bench's snapshot, and the provenance records
  /// that sit beside them.
  fn registries_dir(&self) -> PathBuf {
    self
      .snapshot_dir
      .parent()
      .unwrap_or(&self.snapshot_dir)
      .to_path_buf()
  }

  /// A forge tarball wraps everything in one top-level directory named after
  /// the repository and commit. Unwrapping it keeps snapshot paths stable
  /// across refreshes.
  fn unwrap_single_root(staging: &Path) -> Result<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(staging)
      .map_err(|e| Error::io("list", staging, e))?
      .flatten()
      .collect();
    if entries.len() == 1 && entries[0].path().is_dir() {
      return Ok(entries[0].path());
    }
    Ok(staging.to_path_buf())
  }
}

#[async_trait::async_trait]
impl RegistryProvider for HttpSnapshotRegistry {
  fn name(&self) -> &str {
    &self.name
  }

  async fn refresh(&self) -> Result<RefreshOutcome> {
    // The snapshot's digest is not known ahead of time, so it cannot go
    // through the content-addressed artifact cache. It is downloaded to a
    // scratch file, checked for shape, and only then swapped into place.
    let scratch = self.cache_dir.join("registry-snapshots").join(&self.name);
    fsutil::remove_any(&scratch)?;
    fsutil::ensure_dir(&scratch)?;

    // Before a byte is fetched: a bench that has started answering from a
    // different host is refused rather than quietly believed.
    let registries_dir = self.registries_dir();
    provenance::check_origin(&registries_dir, &self.name, &self.url)?;

    let downloader = Downloader::new(&scratch).offline(self.offline);
    let archive_path = scratch.join("snapshot.tar.gz");
    let (bytes, digest) =
      download_unverified(&downloader, &self.url, &archive_path, self.offline).await?;
    tracing::debug!(registry = %self.name, bytes, "fetched registry snapshot");

    let staging = scratch.join("extract");
    fsutil::ensure_dir(&staging)?;
    archive::extract(
      &archive_path,
      &ArchiveFormat::TarGz,
      &staging,
      ExtractLimits::default(),
    )?;

    let extracted_root = Self::unwrap_single_root(&staging)?;
    // Parse before publishing, so a broken snapshot never replaces a good one.
    let index = build_index(&self.name, &extracted_root, ParseMode::Lenient)?;
    let packages = index.len();

    fsutil::ensure_dir(self.snapshot_dir.parent().unwrap_or(&self.snapshot_dir))?;
    fsutil::remove_any(&self.snapshot_dir)?;
    std::fs::rename(&extracted_root, &self.snapshot_dir)
      .map_err(|e| Error::io("install registry snapshot", &self.snapshot_dir, e))?;
    fsutil::remove_any(&scratch)?;
    provenance::record(&registries_dir, &self.name, &self.url, digest, bytes);

    Ok(RefreshOutcome {
      registry: self.name.clone(),
      packages,
      updated: true,
    })
  }

  fn load_index(&self) -> Result<RegistryIndex> {
    if !self.snapshot_dir.is_dir() {
      return Err(Error::Registry(RegistryError::NotFetched(
        self.name.clone(),
      )));
    }
    build_index(&self.name, &self.snapshot_dir, ParseMode::Lenient)
  }
}
