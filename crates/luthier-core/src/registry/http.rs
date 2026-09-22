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
//!
//! This is also the one provider that can verify who published what it read.
//! A snapshot is a file at a URL, so a detached signature can sit beside it;
//! a local checkout is the user's own directory, and an Open Audio Stack site
//! publishes no signatures. What that verification means is
//! [`signature`](super::signature)'s business — this module fetches the file
//! and refuses to extract anything the policy turned down.

use super::signature::{self, PublicKey};
use super::{
  RefreshOutcome, RegistryIndex, RegistryProvider, build_index, download_unverified, provenance,
};
use crate::archive::{self, ExtractLimits};
use crate::download::Downloader;
use crate::error::{DownloadError, Error, RegistryError, Result};
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
  /// Keys this bench is trusted to be signed with, from the configuration.
  keys: Vec<PublicKey>,
  /// Accept an unsigned snapshot from a bench that was signed before. One
  /// run only: it never discards the pinned key.
  allow_unsigned: bool,
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
      keys: Vec::new(),
      allow_unsigned: false,
    }
  }

  pub fn offline(mut self, offline: bool) -> Self {
    self.offline = offline;
    self
  }

  pub fn keys(mut self, keys: Vec<PublicKey>) -> Self {
    self.keys = keys;
    self
  }

  pub fn allow_unsigned(mut self, allow: bool) -> Self {
    self.allow_unsigned = allow;
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

  /// Where a bench's detached signature lives: beside the snapshot, with
  /// `.sig` on the end.
  ///
  /// Convention rather than configuration, and the same convention the rest
  /// of the world uses. A URL for it in `config.json` would be one more thing
  /// to point somewhere else, and pointing the signature somewhere else is
  /// precisely what an attacker who could edit that file would do.
  ///
  /// It also says something about what a signed bench looks like: a generated
  /// branch tarball has no signature beside it and cannot have one, so a
  /// bench that signs publishes a snapshot it uploaded itself.
  fn signature_url(&self) -> Url {
    let mut url = self.url.clone();
    url.set_path(&format!("{}.sig", self.url.path()));
    url
  }

  /// The signature published for this snapshot, if there is one.
  ///
  /// A bench that publishes none answers 404, which is an answer rather than
  /// a failure. Whether the absence is acceptable is decided in
  /// [`signature::check`], with the pin and the configuration in hand.
  async fn fetch_signature(
    &self,
    downloader: &Downloader,
    scratch: &Path,
  ) -> Result<Option<String>> {
    let url = self.signature_url();
    let path = scratch.join("snapshot.sig");
    match download_unverified(downloader, &url, &path, self.offline).await {
      Ok(_) => {
        let text = std::fs::read_to_string(&path).map_err(|e| Error::io("read", &path, e))?;
        Ok(Some(text))
      }
      Err(Error::Download(DownloadError::HttpStatus { status: 404, .. })) => Ok(None),
      // The same answer over `file://`, which is how the tests serve a
      // bench that publishes nothing.
      Err(Error::Download(DownloadError::BadFileUrl { .. })) => Ok(None),
      Err(other) => Err(other),
    }
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

    // Before the archive is opened: a snapshot that fails this is untrusted
    // input that nothing has vouched for, and the extractor is the last
    // place to find that out. What is verified is the digest just computed,
    // which is also what gets recorded — so the audit trail and the
    // signature are statements about the same bytes.
    let requirement = provenance::requirement(&registries_dir, &self.name, &self.keys);
    let served = self.fetch_signature(&downloader, &scratch).await?;
    let verdict = signature::check(
      &self.name,
      &requirement,
      served.as_deref(),
      &digest,
      self.allow_unsigned,
    )?;
    if let Some(key) = verdict.key() {
      tracing::debug!(registry = %self.name, key = %key.short(16), "snapshot signature verified");
    }

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
    provenance::record(
      &registries_dir,
      &self.name,
      &self.url,
      digest,
      bytes,
      verdict.key(),
    );

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
