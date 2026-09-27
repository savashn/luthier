//! The operations a front end performs.
//!
//! This is the whole public surface of the manager (§38). The CLI parses
//! arguments and renders results; every decision about what to do lives here,
//! so a GUI added later reuses it rather than reimplementing it. Results are
//! plain owned, serialisable data — never borrowed from an index — so a caller
//! can render them however it likes, including as JSON (§33).

use crate::archive::{self, ExtractLimits};
use crate::config::{Config, RegistrySource};
use crate::download::{Downloader, Progress};
use crate::engine;
use crate::env::{self, Activation, EnvName, EnvSummary};
use crate::envfile::EnvFile;
use crate::error::{Error, InstallError, ResolveError, Result, StateError};
use crate::fsutil;
use crate::install::{self, EntryStatus, InstallTransaction};
use crate::layout::{Layout, LocationKind};
use crate::registry::{RefreshOutcome, RegistryIndex};
use crate::resolver::{self, Disposition, Resolution, ResolveRequest};
use crate::scan::{self, DetectedPlugin};
use crate::state::{
  ArtifactRecord, InstallReason, InstalledEntry, InstalledPackage, State, StateGuard,
};
use jiff::Timestamp;
use luthier_manifest::{Format, PackageId, Target};
use semver::Version;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A configured manager.
pub struct Session {
  layout: Layout,
  config: Config,
  target: Target,
  offline: bool,
  /// The merged index, built at most once per session.
  ///
  /// `install` alone used to ask for it three times — once to report
  /// registry problems, once to plan, once to act — and each time meant
  /// parsing 1.6 MB of JSON and translating 560 packages again.
  index: std::sync::OnceLock<RegistryIndex>,
  /// What is on the machine, looked for at most once per session.
  ///
  /// Both scans walk every package's detect rules across the managed and
  /// system plugin roots, and `install` ran each of them twice — once to
  /// plan and once to act. A session is one command, so the second answer
  /// was the first answer with more stat calls.
  detected_externals: std::sync::OnceLock<BTreeSet<PackageId>>,
  detected_engines: std::sync::OnceLock<BTreeSet<PackageId>>,
}

impl Session {
  pub fn new(layout: Layout, config: Config) -> Result<Self> {
    let target = Target::host().ok_or_else(|| {
      Error::InvalidArgument(format!(
        "unsupported platform {}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
      ))
    })?;
    Ok(Self {
      layout,
      config,
      target,
      offline: false,
      index: std::sync::OnceLock::new(),
      detected_externals: std::sync::OnceLock::new(),
      detected_engines: std::sync::OnceLock::new(),
    })
  }

  /// External packages the machine already provides.
  fn externals_present(&self, index: &RegistryIndex) -> &BTreeSet<PackageId> {
    self
      .detected_externals
      .get_or_init(|| scan::detect_externals(&self.layout, index))
  }

  /// Engines the machine already provides.
  fn engines_present(&self, index: &RegistryIndex) -> &BTreeSet<PackageId> {
    self
      .detected_engines
      .get_or_init(|| scan::detect_engines(&self.layout, index))
  }

  pub fn offline(mut self, offline: bool) -> Self {
    self.offline = offline;
    self
  }

  /// Overrides the target. Used by tests and cross-platform inspection.
  pub fn with_target(mut self, target: Target) -> Self {
    self.target = target;
    self
  }

  pub fn layout(&self) -> &Layout {
    &self.layout
  }

  /// Refuses when a location the user chose for one of `kinds` is missing.
  ///
  /// Each operation names only what it touches, so a samples disk left
  /// unplugged does not stop `refresh`, and a missing cache disk does not
  /// stop `remove`. Removal needs its roots present above all: deleting from
  /// a disk that is not there finds every file already gone, and the
  /// package would leave state with its files still on the disk.
  fn require_locations(&self, kinds: &[LocationKind]) -> Result<()> {
    match self
      .layout
      .unavailable_locations()
      .into_iter()
      .find(|(kind, _)| kinds.contains(kind))
    {
      Some((kind, path)) => Err(Error::LocationUnavailable { kind, path }),
      None => Ok(()),
    }
  }

  pub fn target(&self) -> &Target {
    &self.target
  }

  /// Registries that could not be read, if any.
  ///
  /// A second bench the user has not fetched yet does not stop the first
  /// from working, so the absence has to be reported somewhere or a command
  /// silently answers from half the registries it was configured with.
  pub fn registry_problems(&self) -> Vec<String> {
    match self.index() {
      Ok(index) => index.problems.clone(),
      // A total failure is raised by whatever the command does next,
      // with its own hint. Reporting it twice would be noise.
      Err(_) => Vec::new(),
    }
  }

  /// Every configured registry, merged, without touching the network. The
  /// first to claim an ID keeps it, so a curated bench corrects a broader
  /// source rather than colliding with it.
  ///
  /// Built once and reused for the rest of the session. Snapshots only change
  /// during `refresh`, which does not read the index, so there is nothing a
  /// later call could see that the first did not.
  pub fn index(&self) -> Result<&RegistryIndex> {
    if let Some(index) = self.index.get() {
      return Ok(index);
    }
    let providers = self.config.providers(&self.layout, self.offline);
    let built = RegistryIndex::merge(providers.iter().map(|provider| provider.load_index()))?;
    Ok(self.index.get_or_init(|| built))
  }

  // -------------------------------------------------------------- refresh --

  /// Updates every registry (§31 `refresh`).
  pub async fn refresh(&self) -> Result<Vec<RefreshOutcome>> {
    self.require_locations(&[LocationKind::Cache])?;
    let mut outcomes = Vec::new();
    let mut first_error = None;
    for provider in self.config.providers(&self.layout, self.offline) {
      match provider.refresh().await {
        Ok(outcome) => outcomes.push(outcome),
        // One bench that cannot be reached does not cost a user the others.
        // A failed refresh leaves that bench's previous snapshot alone, so
        // the command answers with what it could update and says what it
        // could not; only nothing at all is an error.
        Err(error) => {
          outcomes.push(RefreshOutcome::failed(provider.name(), error.to_string()));
          first_error.get_or_insert(error);
        }
      }
    }

    match first_error {
      Some(error) if outcomes.iter().all(|o| o.failure.is_some()) => Err(error),
      _ => Ok(outcomes),
    }
  }

  // --------------------------------------------------------------- search --

  pub fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
    let index = self.index()?;
    Ok(
      index
        .search(query)
        .into_iter()
        .map(|hit| {
          let manifest = &hit.entry.manifest;
          SearchResult {
            id: manifest.id.to_string(),
            name: manifest.name.clone(),
            kind: manifest.kind.to_string(),
            category: manifest.category.to_string(),
            tags: manifest.tags.clone(),
            description: manifest.description.clone(),
            version: manifest.latest_release().map(|r| r.version.to_string()),
            formats: manifest
              .formats_for(&self.target)
              .iter()
              .map(Format::to_string)
              .collect(),
          }
        })
        .collect(),
    )
  }

  // ----------------------------------------------------------------- info --

  pub fn info(&self, id: &PackageId) -> Result<PackageInfo> {
    let index = self.index()?;
    let entry = index.get(id)?;
    let manifest = &entry.manifest;
    let state = crate::state::load(&self.layout)?;
    let installed = state.get(id);

    Ok(PackageInfo {
      id: manifest.id.to_string(),
      name: manifest.name.clone(),
      kind: manifest.kind.to_string(),
      description: manifest.description.clone(),
      category: manifest.category.to_string(),
      tags: manifest.tags.clone(),
      license: manifest.license.display(),
      license_kind: manifest.license.kind.to_string(),
      homepage: manifest.homepage.as_ref().map(ToString::to_string),
      repository: manifest.repository.as_ref().map(ToString::to_string),
      documentation: manifest.documentation.as_ref().map(ToString::to_string),
      authors: manifest.authors.clone(),
      latest_version: manifest.latest_release().map(|r| r.version.to_string()),
      available_versions: manifest
        .releases_newest_first()
        .iter()
        .map(|r| r.version.to_string())
        .collect(),
      formats: manifest
        .formats_for(&self.target)
        .iter()
        .map(Format::to_string)
        .collect(),
      target: self.target.to_string(),
      registry: entry.registry.clone(),
      installed_version: installed.map(|p| p.version.to_string()),
      // An `external` package is never installed *by us*, so
      // `installed_version` is permanently None for it and reporting only
      // that would say "no" about software sitting in /usr/lib.
      detected_at: scan::locate_external(&self.layout, manifest).map(|p| p.display().to_string()),
      pinned: installed.and_then(|p| p.pin.as_ref().map(ToString::to_string)),
      dependencies: manifest
        .latest_release()
        .map(|r| r.dependencies.iter().map(|d| d.id.to_string()).collect())
        .unwrap_or_default(),
      content: manifest
        .content
        .iter()
        .map(|c| c.label().to_owned())
        .collect(),
      played_by: {
        let mut engines: Vec<String> = Vec::new();
        for content in &manifest.content {
          for engine in index.engines_for(content) {
            let id = engine.package.to_string();
            if !engines.contains(&id) {
              engines.push(id);
            }
          }
        }
        engines
      },
      // Read from the release a user would actually get, and left unanswered
      // for a package with no artifact for this target at all — `external`
      // and `pack` install nothing, so there is nothing to have reviewed.
      rules_reviewed: manifest
        .latest_release()
        .and_then(|release| release.artifacts_for(&self.target).next())
        .map(|artifact| !artifact.derive_install),
      provisioning_hint: manifest.provisioning_hint.clone(),
      unknown_fields: entry.unknown_fields.clone(),
    })
  }

  // ----------------------------------------------------------------- list --

  pub fn list(&self) -> Result<Vec<InstalledSummary>> {
    let state = crate::state::load(&self.layout)?;
    Ok(
      state
        .packages
        .values()
        .map(InstalledSummary::from)
        .collect(),
    )
  }

  /// Plugins on disk, whether or not Luthier installed them (§29).
  pub fn scan(&self) -> Result<Vec<DetectedPlugin>> {
    let state = crate::state::load(&self.layout)?;
    scan::scan(&self.layout, &state)
  }

  // -------------------------------------------------------------- install --

  /// Works out what installing `ids` would do, without changing anything.
  pub fn plan_install(&self, ids: &[PackageId], force: bool) -> Result<InstallPlan> {
    self.plan_install_at(ids, force, &BTreeMap::new())
  }

  /// [`Session::plan_install`], with versions the resolution must land on.
  ///
  /// Used by `env import`, where the whole point is to reproduce an exact
  /// set rather than take whatever is newest (§51).
  pub fn plan_install_at(
    &self,
    ids: &[PackageId],
    force: bool,
    required_versions: &BTreeMap<PackageId, Version>,
  ) -> Result<InstallPlan> {
    self.require_locations(&LocationKind::ALL)?;
    let index = self.index()?;
    let state = crate::state::load(&self.layout)?;
    let detected = self.externals_present(index);
    let resolution = resolver::resolve(
      index,
      &ResolveRequest {
        roots: ids,
        target: &self.target,
        state: &state,
        detected_externals: detected,
        force,
        required_versions,
      },
    )?;

    let unplayable = self.unplayable(&resolution, index, &state);
    let sizes = ArtifactSizes::of(&resolution, &self.layout);
    let space = self.space_needed(&sizes);

    // The typed refusal is decided here, from the resolution this plan was
    // built out of, so a front end refuses the plan it showed the user rather
    // than relying on the installer to derive the same verdict a second time
    // from freshly-read state. Two derivations can disagree; the one the user
    // was shown is the one that must decide.
    let local_changes = self.local_changes(&resolution, &state, force)?;
    let blocked = resolution
      .missing_externals()
      .next()
      .map(|external| BlockedReason::MissingExternal {
        id: external.id.clone(),
        provisioning_hint: external.provisioning_hint.clone(),
      })
      .or_else(|| local_changes.map(|(id, changes)| BlockedReason::LocalChanges { id, changes }))
      .or_else(|| {
        space
          .iter()
          .find(|s| s.is_short())
          .map(|short| BlockedReason::NotEnoughSpace {
            path: short.path.clone(),
            required: short.required_bytes,
            available: short.available_bytes,
          })
      });

    Ok(InstallPlan {
      blocked,
      steps: resolution
        .order
        .iter()
        .map(|package| InstallStep {
          id: package.id().to_string(),
          name: package.entry.manifest.name.clone(),
          version: package.version.to_string(),
          action: match &package.disposition {
            Disposition::Install => "install".into(),
            Disposition::Upgrade { from } => format!("upgrade from {from}"),
            Disposition::Satisfied => "already installed".into(),
          },
          reason: match package.reason {
            InstallReason::Explicit => "requested",
            InstallReason::Dependency => "dependency",
          }
          .into(),
          download_bytes: package.artifact.and_then(|a| a.size),
          formats: package
            .artifact
            .map(|a| a.provides.iter().map(Format::to_string).collect())
            .unwrap_or_default(),
        })
        .collect(),
      missing_externals: resolution
        .missing_externals()
        .map(|external| MissingExternal {
          id: external.id.to_string(),
          name: external.name.clone(),
          provisioning_hint: external.provisioning_hint.clone(),
        })
        .collect(),
      unplayable: unplayable
        .into_iter()
        .map(|found| self.describe_unplayable(found, index))
        .collect(),
      space,
    })
  }

  /// The first package this resolution would replace whose files someone
  /// changed or added to since it was installed, and what changed.
  ///
  /// Replacing a package moves its old files aside and deletes them when the
  /// new ones are in place, so an upgrade would throw away exactly what
  /// `remove` is careful to keep: a preset edited inside a bundle, or an
  /// `.sfz` a user wrote into a library's directory. `force` is the user
  /// saying they know, and is what `verify` already tells them to use to
  /// repair a package, which is the same act.
  fn local_changes(
    &self,
    resolution: &Resolution<'_>,
    state: &State,
    force: bool,
  ) -> Result<Option<(PackageId, Vec<String>)>> {
    if force {
      return Ok(None);
    }
    for package in &resolution.order {
      if !matches!(package.disposition, Disposition::Upgrade { .. }) {
        continue;
      }
      let Some(installed) = state.get(package.id()) else {
        continue;
      };
      let mut changes = Vec::new();
      for entry in &installed.files {
        if let EntryStatus::Modified { detail } = install::verify_entry(entry)? {
          changes.push(format!("{}: {detail}", entry.path().display()));
        }
      }
      if !changes.is_empty() {
        return Ok(Some((package.id().clone(), changes)));
      }
    }
    Ok(None)
  }

  /// What carrying out a plan would ask of each filesystem involved.
  ///
  /// Three copies of an artifact exist at once at the worst moment: the
  /// archive in the cache, the extracted tree in the transaction workspace,
  /// and the files being copied into place. Sample content is the case that
  /// makes this matter and also the least compressible, so the extracted size
  /// is estimated as the archive's rather than as some fraction of it.
  ///
  /// Only the workspace copy is transient: `install_at` runs one transaction
  /// per package and `commit` deletes the workspace before the next begins,
  /// so what the workspace has to hold is the *largest* package, not the sum
  /// of all of them. Charging the whole plan there three times over refuses
  /// installs that would fit with room to spare — the same over-conservative
  /// arithmetic that once made a 5 GiB kit uninstallable everywhere.
  ///
  /// The cache and the install roots are often the same filesystem and
  /// sometimes not — a user can put either on another disk — so the
  /// requirement is summed per device rather than checked twice against the
  /// same free space, and each package is charged to the root it lands in.
  fn space_needed(&self, sizes: &ArtifactSizes) -> Vec<SpaceRequirement> {
    if sizes.total == 0 {
      return Vec::new();
    }
    let cache = self.layout.artifact_cache_dir();
    let workspace = self.layout.transactions_dir();

    let mut by_device: BTreeMap<Option<u64>, (PathBuf, u64)> = BTreeMap::new();
    // Directories sharing a device share one requirement, and only one path
    // can name it. They are visited in the order a user would act on them —
    // the install root, then the cache, then the workspace — so the name a
    // shortage carries is the one worth freeing space in, rather than
    // whichever directory happened to be listed first.
    let installs = sizes
      .by_root
      .iter()
      .map(|(root, bytes)| (root.clone(), *bytes));
    for (path, needed) in installs.chain([(cache, sizes.total), (workspace, sizes.largest)]) {
      let device = fsutil::filesystem_id(&path);
      let entry = by_device.entry(device).or_insert((path, 0));
      entry.1 += needed;
    }

    by_device
      .into_values()
      .filter_map(|(path, required_bytes)| {
        // Not knowing how much room there is is not the same as knowing
        // there is none: an unanswerable filesystem is left alone.
        let available_bytes = fsutil::available_bytes(&path)?;
        Some(SpaceRequirement {
          path,
          required_bytes,
          available_bytes,
        })
      })
      .collect()
  }

  /// Content in `resolution` that nothing on this machine could play.
  fn unplayable(
    &self,
    resolution: &Resolution<'_>,
    index: &RegistryIndex,
    state: &State,
  ) -> Vec<engine::Unplayable> {
    let present = self.engines_present(index);
    engine::unplayable(resolution, index, state, present)
  }

  fn describe_unplayable(
    &self,
    found: engine::Unplayable,
    index: &RegistryIndex,
  ) -> UnplayableContent {
    let package = index.packages.get(&found.package);
    UnplayableContent {
      id: found.package.to_string(),
      name: package
        .map(|entry| entry.manifest.name.clone())
        .unwrap_or_else(|| found.package.to_string()),
      content: found.content.label().to_owned(),
      played_by: found.content.played_by(),
      engines: found
        .engines
        .iter()
        .map(|id| match index.packages.get(id) {
          Some(entry) => EngineChoice {
            id: id.to_string(),
            name: entry.manifest.name.clone(),
            installable: !entry.manifest.is_external()
              && entry.manifest.releases.iter().any(|release| {
                release
                  .artifacts_for(&self.target)
                  .any(install::installable)
              }),
            provisioning_hint: entry.manifest.provisioning_hint.clone(),
          },
          // Named by a bench but carried by no configured registry: it
          // can still be detected, just not offered.
          None => EngineChoice {
            id: id.to_string(),
            name: id.to_string(),
            installable: false,
            provisioning_hint: None,
          },
        })
        .collect(),
    }
  }

  /// Downloads, verifies, extracts and installs `ids` and their dependencies.
  pub async fn install(
    &self,
    ids: &[PackageId],
    force: bool,
    progress: &mut dyn Progress,
  ) -> Result<InstallOutcome> {
    self
      .install_at(ids, force, &BTreeMap::new(), progress)
      .await
  }

  /// [`Session::install`], with versions the resolution must land on.
  pub async fn install_at(
    &self,
    ids: &[PackageId],
    force: bool,
    required_versions: &BTreeMap<PackageId, Version>,
    progress: &mut dyn Progress,
  ) -> Result<InstallOutcome> {
    self.require_locations(&LocationKind::ALL)?;
    let mut guard = StateGuard::acquire(&self.layout)?;

    // Replay any journal left by an interrupted run before touching disk.
    let recovered = install::recover_interrupted(&self.layout)?;
    if !recovered.is_empty() {
      tracing::info!(
        count = recovered.len(),
        "rolled back interrupted installations"
      );
    }

    let index = self.index()?;
    let detected = self.externals_present(index);
    let resolution = resolver::resolve(
      index,
      &ResolveRequest {
        roots: ids,
        target: &self.target,
        state: guard.state(),
        detected_externals: detected,
        force,
        required_versions,
      },
    )?;

    // An external dependency cannot be installed for the user, so stop
    // before downloading anything rather than half-installing.
    if let Some(missing) = resolution.missing_externals().next() {
      return Err(Error::Resolve(ResolveError::ExternalMissing {
        id: missing.id.clone(),
        provisioning_hint: missing.provisioning_hint.clone(),
      }));
    }

    // Checked before the download for the same reason, and by the same
    // function as the plan, so the two cannot disagree about it.
    if let Some((id, changes)) = self.local_changes(&resolution, guard.state(), force)? {
      return Err(Error::Install(InstallError::LocalChanges { id, changes }));
    }

    // Running out of disk is refused here too, and for the same reason:
    // discovering it mid-extraction leaves a rolled-back transaction and a
    // cache full of bytes the user waited an hour for.
    if let Some(short) = self
      .space_needed(&ArtifactSizes::of(&resolution, &self.layout))
      .into_iter()
      .find(SpaceRequirement::is_short)
    {
      return Err(Error::Install(InstallError::NotEnoughSpace {
        path: short.path,
        required: short.required_bytes,
        available: short.available_bytes,
      }));
    }

    let downloader = Downloader::new(self.layout.artifact_cache_dir()).offline(self.offline);
    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    let mut warnings = Vec::new();

    for package in &resolution.order {
      if package.disposition == Disposition::Satisfied {
        skipped.push(InstalledSummary::from(
          guard
            .state()
            .get(package.id())
            .expect("satisfied means installed"),
        ));
        continue;
      }

      // A pack installs nothing of its own; it is recorded so that it can
      // be removed and so its members become orphans when it goes (§49).
      let Some(artifact) = package.artifact else {
        let record = InstalledPackage {
          id: package.id().clone(),
          name: package.entry.manifest.name.clone(),
          version: package.version.clone(),
          registry: package.entry.registry.clone(),
          manifest_digest: package.entry.digest,
          reason: package.reason,
          pin: guard.state().get(package.id()).and_then(|p| p.pin.clone()),
          installed_at: Timestamp::now(),
          formats: Vec::new(),
          artifacts: Vec::new(),
          dependencies: package.dependencies.clone(),
          files: Vec::new(),
        };
        installed.push(InstalledSummary::from(&record));
        guard
          .state_mut()
          .packages
          .insert(package.id().clone(), record);
        guard.commit()?;
        continue;
      };

      let fetched = downloader
        .fetch(
          &artifact.source.url,
          &artifact.checksum.sha256,
          artifact.size,
          progress,
        )
        .await?;

      // One transaction per package: a failure part-way through a batch
      // leaves earlier packages properly installed and recorded, and this
      // package as it was.
      let mut transaction = InstallTransaction::begin(&self.layout, package.id())?;
      let extract_root = transaction.workspace().join("extract");
      fsutil::ensure_dir(&extract_root)?;
      archive::unpack(
        &fetched.path,
        &artifact.archive,
        &published_name(&artifact.source.url),
        &extract_root,
        ExtractLimits::for_download(fetched.bytes),
      )?;

      // A source that carries no install rules hands over bytes and a
      // checksum and nothing else. The rules are then read from the
      // archive that checksum just verified, by the same code
      // `luthier-registry inspect` shows a contributor — one
      // implementation, so what the tool promises is what happens here.
      let derived;
      // Content is installed only where the artifact says it holds some.
      // Reading it out of the shape of any archive that happened to carry
      // no plugin would install a broken plugin release as a folder of
      // samples, which is a guess, and the wrong one.
      let mut content = None;
      let rules: &[luthier_manifest::InstallRule] = if artifact.derive_install {
        derived = install::derive::from_tree(&extract_root)?;
        if artifact.provides.contains(&Format::Library) {
          content = derived.content.clone();
        }
        if derived.rules.is_empty() && content.is_none() {
          return Err(Error::Install(InstallError::NothingToInstall {
            id: package.id().clone(),
            version: package.version.clone(),
          }));
        }
        // The source's `provides` was its own claim about the archive,
        // and a source that carries no install rules is a source whose
        // claim nothing has ever checked. Now that the real contents
        // are known, disagreement is worth saying out loud: it means
        // upstream metadata is wrong, which is a thing to fix there
        // rather than absorb here.
        for missing in missing_formats(&artifact.provides, &derived.rules) {
          if missing == Format::Library && content.is_some() {
            continue;
          }
          warnings.push(format!(
            "{} {} claims to provide {missing}, and the archive holds none",
            package.id(),
            package.version
          ));
        }
        &derived.rules
      } else {
        &artifact.install
      };

      let mut items = install::plan(
        &self.layout,
        &extract_root,
        rules,
        package.id(),
        guard.state(),
      )?;
      if let Some(content) = &content {
        items.push(install::plan_content(
          &self.layout,
          &extract_root,
          content,
          package.id(),
          guard.state(),
        )?);
      }

      let mut files: Vec<InstalledEntry> = Vec::new();
      for item in &items {
        files.push(transaction.place(item)?);
      }
      transaction.commit()?;

      let record = InstalledPackage {
        id: package.id().clone(),
        name: package.entry.manifest.name.clone(),
        version: package.version.clone(),
        registry: package.entry.registry.clone(),
        manifest_digest: package.entry.digest,
        reason: package.reason,
        pin: guard.state().get(package.id()).and_then(|p| p.pin.clone()),
        installed_at: Timestamp::now(),
        formats: {
          // One entry per distinct format: Dragonfly ships four CLAPs
          // and four VST3s, which is still just "clap, vst3".
          let mut formats: Vec<Format> = items.iter().map(|i| i.format.clone()).collect();
          formats.sort();
          formats.dedup();
          formats
        },
        artifacts: vec![ArtifactRecord {
          url: artifact.source.url.to_string(),
          sha256: artifact.checksum.sha256,
        }],
        dependencies: package.dependencies.clone(),
        files,
      };

      installed.push(InstalledSummary::from(&record));
      guard
        .state_mut()
        .packages
        .insert(package.id().clone(), record);
      // Commit after each package so an interruption cannot lose the
      // record of something that is genuinely on disk.
      guard.commit()?;
    }

    Ok(InstallOutcome {
      installed,
      skipped,
      warnings,
    })
  }

  // --------------------------------------------------------------- remove --

  /// Describes what removing `ids` would do, including why a dependency is kept.
  pub fn plan_remove(&self, ids: &[PackageId]) -> Result<RemovalPlan> {
    self.require_locations(&[LocationKind::Libraries, LocationKind::Plugins])?;
    let state = crate::state::load(&self.layout)?;
    let mut packages = Vec::new();
    let stranded = self.stranded_by_removing(ids, &state);

    for id in ids {
      let installed = state
        .get(id)
        .ok_or_else(|| Error::State(StateError::NotInstalled { id: id.clone() }))?;
      let dependents: Vec<PackageId> = state
        .dependents_of(id)
        .into_iter()
        .filter(|d| !ids.contains(d))
        .collect();

      packages.push(RemovalTarget {
        id: id.to_string(),
        name: installed.name.clone(),
        version: installed.version.to_string(),
        files: installed
          .files
          .iter()
          .map(|f| f.path().display().to_string())
          .collect(),
        blocked_by: dependents.iter().map(ToString::to_string).collect(),
      });
    }

    Ok(RemovalPlan { packages, stranded })
  }

  /// Installed content that removing `ids` would leave with no engine.
  ///
  /// A warning, never a refusal: the user may be about to install an engine
  /// from their distribution, or may be removing the kit next. Silence with
  /// no explanation is the only outcome worth preventing.
  fn stranded_by_removing(&self, ids: &[PackageId], state: &State) -> Vec<StrandedContent> {
    let Ok(index) = self.index() else {
      return Vec::new();
    };
    let detected = self.externals_present(index);
    let removing: BTreeSet<PackageId> = ids.iter().cloned().collect();
    engine::stranded_by_removal(index, state, &removing, detected)
      .into_iter()
      .map(|found| StrandedContent {
        id: found.package.to_string(),
        content: found.content.label().to_owned(),
        engines: found.engines.iter().map(ToString::to_string).collect(),
      })
      .collect()
  }

  /// Removes packages, deleting only files it recorded installing (§20, §21).
  ///
  /// Every refusal is decided before the first file is deleted, and the state
  /// file is committed after each package. Deleting first and checking later
  /// would mean a refusal halfway down the list leaving earlier packages gone
  /// from disk and still recorded as installed — the state file claiming files
  /// that are not there, which `verify` reports and `install` declines to fix
  /// because it reads the package as satisfied.
  pub fn remove(&self, ids: &[PackageId], force: bool) -> Result<RemoveOutcome> {
    self.require_locations(&[LocationKind::Libraries, LocationKind::Plugins])?;
    let mut guard = StateGuard::acquire(&self.layout)?;
    let mut removed = Vec::new();
    let mut kept_files = Vec::new();

    // Named twice on one command line is one removal, not two: the second
    // pass would find the package already gone from state and fail, after
    // the first had deleted its files.
    let mut targets: Vec<&PackageId> = Vec::new();
    for id in ids {
      if !targets.contains(&id) {
        targets.push(id);
      }
    }

    // Computed against the state as it stands, because the question is what
    // this removal takes away. Once the loop below has run, the engine is
    // already gone from state and there is nothing left to notice.
    let stranded = self.stranded_by_removing(ids, guard.state());

    // Checked over every target first. Each of these is a refusal, and a
    // refusal has to happen while nothing has been deleted yet.
    let mut planned = Vec::new();
    for id in &targets {
      let installed = guard
        .state()
        .get(id)
        .cloned()
        .ok_or_else(|| Error::State(StateError::NotInstalled { id: (*id).clone() }))?;

      // §23: never take a package that something else still needs.
      let dependents: Vec<PackageId> = guard
        .state()
        .dependents_of(id)
        .into_iter()
        .filter(|d| !targets.contains(&d))
        .collect();
      if !dependents.is_empty() && !force {
        return Err(Error::State(StateError::StillRequired {
          id: (*id).clone(),
          dependents,
        }));
      }

      // A guard against a corrupted or hand-edited state file turning
      // removal into arbitrary deletion. Checked again below, immediately
      // before each delete, because this one is about refusing the whole
      // operation and that one is about never writing outside the roots.
      for entry in &installed.files {
        let path = entry.path();
        if !self.layout.is_managed_location(path) {
          return Err(Error::Install(InstallError::OutsideManagedRoot {
            path: path.to_path_buf(),
          }));
        }
        // A bundle's recorded contents are joined onto its root and
        // deleted one by one, so they are checked here too.
        if let InstalledEntry::Bundle { contents, .. } = entry
          && let Some(bad) = contents
            .iter()
            .find(|f| !install::is_plain_relative(&f.path))
        {
          return Err(Error::Install(InstallError::OutsideManagedRoot {
            path: path.join(&bad.path),
          }));
        }
      }

      planned.push(installed);
    }

    for installed in planned {
      for entry in installed.files.iter().rev() {
        let path = entry.path();

        if !self.layout.is_managed_location(path) {
          return Err(Error::Install(InstallError::OutsideManagedRoot {
            path: path.to_path_buf(),
          }));
        }

        // Whatever someone changed or added after we installed it is
        // their work, not ours to throw away.
        for kept in install::remove_entry(entry)? {
          kept_files.push(KeptFile {
            path: kept.path.display().to_string(),
            reason: kept.reason,
          });
        }
      }

      removed.push(InstalledSummary::from(&installed));
      guard.state_mut().packages.remove(&installed.id);
      // Committed per package, as `install_at` does and for the same
      // reason: an interruption must not lose the record of what is
      // genuinely on disk, in either direction.
      guard.commit()?;
    }

    Ok(RemoveOutcome {
      removed,
      kept_files,
      stranded,
    })
  }

  // --------------------------------------------------------------- verify --

  /// Checks installed files still match what was recorded (§31).
  pub fn verify(&self, ids: &[PackageId]) -> Result<Vec<VerifyResult>> {
    // Otherwise an unmounted disk reads as every file on it gone.
    self.require_locations(&[LocationKind::Libraries, LocationKind::Plugins])?;
    let state = crate::state::load(&self.layout)?;
    let targets: Vec<&InstalledPackage> = if ids.is_empty() {
      state.packages.values().collect()
    } else {
      ids
        .iter()
        .map(|id| {
          state
            .get(id)
            .ok_or_else(|| Error::State(StateError::NotInstalled { id: id.clone() }))
        })
        .collect::<Result<_>>()?
    };

    let mut results = Vec::new();
    for package in targets {
      let mut problems = Vec::new();
      for entry in &package.files {
        match install::verify_entry(entry)? {
          EntryStatus::Intact => {}
          EntryStatus::Missing => problems.push(FileProblem {
            path: entry.path().display().to_string(),
            problem: "missing".into(),
          }),
          EntryStatus::Modified { detail } => problems.push(FileProblem {
            path: entry.path().display().to_string(),
            problem: detail,
          }),
        }
      }
      results.push(VerifyResult {
        id: package.id.to_string(),
        name: package.name.clone(),
        version: package.version.to_string(),
        ok: problems.is_empty(),
        problems,
      });
    }
    Ok(results)
  }

  // --------------------------------------------------------------- update --

  /// Lists packages with a newer release available (§26). Changes nothing.
  pub fn available_updates(&self) -> Result<Vec<AvailableUpdate>> {
    let index = self.index()?;
    let state = crate::state::load(&self.layout)?;
    let mut updates = Vec::new();

    for package in state.packages.values() {
      let Some(entry) = index.packages.get(&package.id) else {
        continue;
      };
      let Some(latest) = entry
        .manifest
        .releases_newest_first()
        .into_iter()
        .find(|release| release.artifacts_for(&self.target).next().is_some())
      else {
        continue;
      };
      if latest.version > package.version {
        updates.push(AvailableUpdate {
          id: package.id.to_string(),
          name: package.name.clone(),
          installed: package.version.to_string(),
          available: latest.version.to_string(),
          // §27: a pin is a decision to stay put, and is reported
          // rather than silently overridden.
          pinned: package.pin.as_ref().map(ToString::to_string),
        });
      }
    }
    updates.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(updates)
  }

  /// Updates the named packages have available but are pinned away from.
  ///
  /// `update <id>` resolves through the pin and reports the package as
  /// already installed, which is true and, by itself, silent about why. §27
  /// says a pin is reported rather than silently obeyed, and `update` with no
  /// arguments does report it; this is what lets the targeted form say the
  /// same thing.
  pub fn held_back(&self, ids: &[PackageId]) -> Result<Vec<AvailableUpdate>> {
    Ok(
      self
        .available_updates()?
        .into_iter()
        .filter(|update| update.pinned.is_some() && ids.iter().any(|id| id.as_str() == update.id))
        .collect(),
    )
  }

  // -------------------------------------------------------------- cleanup --

  /// Lists packages nothing needs any more. Deletes nothing (§24).
  pub fn cleanup(&self) -> Result<Vec<InstalledSummary>> {
    let state = crate::state::load(&self.layout)?;
    Ok(
      state
        .orphans()
        .iter()
        .filter_map(|id| state.get(id))
        .map(InstalledSummary::from)
        .collect(),
    )
  }

  // ---------------------------------------------------------------- bench --

  /// The benches this build reads, highest priority first.
  ///
  /// A fixed list: the curated bench, then the Open Audio Stack registry it
  /// corrects. Order is the whole mechanism behind that correction, so it is
  /// what this reports: the first bench to claim an ID keeps it.
  ///
  /// The built-in list rather than this session's, so that a session started
  /// with `--registry-path` still says where manifests normally come from.
  pub fn benches(&self) -> Vec<BenchSummary> {
    Self::summarise(&Config::default())
  }

  fn summarise(config: &Config) -> Vec<BenchSummary> {
    config
      .registries
      .iter()
      .enumerate()
      .map(|(i, registry)| BenchSummary {
        priority: i + 1,
        name: registry.name.clone(),
        kind: match registry.source {
          RegistrySource::Path { .. } => "path",
          RegistrySource::Snapshot { .. } => "snapshot",
          RegistrySource::Oas { .. } => "oas",
        }
        .into(),
        location: match &registry.source {
          RegistrySource::Path { path } => path.display().to_string(),
          RegistrySource::Snapshot { url } => url.to_string(),
          RegistrySource::Oas { url } => url.to_string(),
        },
      })
      .collect()
  }

  // ---------------------------------------------------------------- cache --

  /// Every artifact in the download cache, and whether anything still needs it.
  ///
  /// The cache is content-addressed, so an entry's filename is its digest and
  /// deciding whether it is still wanted is an exact question rather than a
  /// guess at a name: it is wanted if some installed package recorded that
  /// digest among its artifacts.
  pub fn cache_entries(&self) -> Result<Vec<CacheEntry>> {
    let state = crate::state::load(&self.layout)?;
    self.cache_entries_of(&state)
  }

  /// The listing, against a state the caller already holds.
  ///
  /// `clean_cache` needs it under its lock: deciding what to delete from a
  /// state read before the lock was taken would be deciding from a state
  /// another process may already have moved on from.
  fn cache_entries_of(&self, state: &State) -> Result<Vec<CacheEntry>> {
    let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in state.packages.values() {
      for artifact in &package.artifacts {
        wanted
          .entry(artifact.sha256.to_string())
          .or_default()
          .push(package.id.to_string());
      }
    }

    let dir = self.layout.artifact_cache_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
      return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
      let path = entry.path();
      let Ok(metadata) = entry.metadata() else {
        continue;
      };
      if !metadata.is_file() {
        continue;
      }
      let name = entry.file_name().to_string_lossy().into_owned();
      // A `.part` is an interrupted download, not a cached artifact. It is
      // listed so the space shows up, and it is never held by a package: its
      // digest is what the *finished* file will hash to, not what these
      // bytes are, and an installed package recording that digest is holding
      // the complete artifact rather than this fragment. Matching the two up
      // would report a truncated file as in use and keep `cache clean` from
      // ever collecting it.
      let partial = name.ends_with(".part");
      let digest = name.strip_suffix(".part").unwrap_or(&name).to_owned();
      out.push(CacheEntry {
        sha256: digest.clone(),
        path,
        bytes: metadata.len(),
        partial,
        used_by: if partial {
          Vec::new()
        } else {
          wanted.get(&digest).cloned().unwrap_or_default()
        },
      });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.sha256.cmp(&b.sha256)));
    Ok(out)
  }

  /// Deletes cached artifacts no installed package recorded.
  ///
  /// Safe by construction: an entry kept is one some package names by digest,
  /// and the worst an over-eager deletion could cost is a re-download — but
  /// `--offline` reinstall depends on the cache (§28), so what is in use stays.
  /// Interrupted `.part` files are removed too, since a download resumes only
  /// while the command that started it is still running or the user runs it
  /// again, and an abandoned one is pure occupancy.
  pub fn clean_cache(&self, dry_run: bool) -> Result<CacheCleaned> {
    self.require_locations(&[LocationKind::Cache])?;
    // Under the state lock, like every other operation that deletes
    // something. Without it this races an install running in another
    // terminal and unlinks the `.part` it is writing, or the verified
    // artifact it is about to extract, neither of which belongs to an
    // installed package yet.
    let guard = StateGuard::acquire(&self.layout)?;
    let entries = self.cache_entries_of(guard.state())?;
    let mut removed = Vec::new();
    let mut bytes = 0;
    for entry in entries {
      if !entry.used_by.is_empty() {
        continue;
      }
      if !dry_run {
        fsutil::remove_any(&entry.path)?;
      }
      bytes += entry.bytes;
      removed.push(entry);
    }
    Ok(CacheCleaned {
      dry_run,
      bytes,
      removed,
    })
  }

  // ------------------------------------------------------------------ pin --

  /// Reapplies the pins an environment file carried, reporting what it could
  /// not apply.
  ///
  /// One lock and one commit for the whole set, rather than a lock cycle and
  /// two atomic writes per pin with the file unlocked in between. A pin that
  /// names a package this import did not install is reported rather than
  /// dropped: the file exists to reproduce an installation, so losing the
  /// pins that make it reproducible is exactly the thing worth saying out
  /// loud.
  fn reapply_pins(&self, pins: Vec<(PackageId, Version)>) -> Result<(Vec<String>, Vec<String>)> {
    if pins.is_empty() {
      return Ok((Vec::new(), Vec::new()));
    }
    let mut guard = StateGuard::acquire(&self.layout)?;
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    for (id, version) in pins {
      match guard.state_mut().packages.get_mut(&id) {
        Some(installed) => {
          installed.pin = Some(version.clone());
          applied.push(format!("{id} {version}"));
        }
        None => skipped.push(format!("{id} {version} (not installed)")),
      }
    }
    guard.commit()?;
    Ok((applied, skipped))
  }

  /// Holds a package at its installed version, or at a named one (§27).
  ///
  /// A named version has to be one the registry actually carries, or the
  /// installed one. A pin is read back by every later resolution, so an
  /// invented version is not a harmless note: it makes `update` report a
  /// package held above the newest release that exists, and hands
  /// `select_version` a requirement nothing can satisfy.
  pub fn pin(&self, id: &PackageId, version: Option<Version>) -> Result<InstalledSummary> {
    let mut guard = StateGuard::acquire(&self.layout)?;
    let installed = guard
      .state()
      .get(id)
      .cloned()
      .ok_or_else(|| Error::State(StateError::NotInstalled { id: id.clone() }))?;

    let pin = match version {
      None => installed.version.clone(),
      Some(wanted) if wanted == installed.version => wanted,
      Some(wanted) => {
        let entry = self.index()?.get(id)?;
        let known: Vec<String> = entry
          .manifest
          .releases
          .iter()
          .map(|release| release.version.to_string())
          .collect();
        if !known.contains(&wanted.to_string()) {
          return Err(Error::InvalidArgument(format!(
            "{id} has no version {wanted}; the registry carries {}",
            known.join(", ")
          )));
        }
        wanted
      }
    };

    let installed = guard
      .state_mut()
      .packages
      .get_mut(id)
      .ok_or_else(|| Error::State(StateError::NotInstalled { id: id.clone() }))?;
    installed.pin = Some(pin);
    let summary = InstalledSummary::from(&*installed);
    guard.commit()?;
    Ok(summary)
  }

  pub fn unpin(&self, id: &PackageId) -> Result<InstalledSummary> {
    let mut guard = StateGuard::acquire(&self.layout)?;
    let installed = guard
      .state_mut()
      .packages
      .get_mut(id)
      .ok_or_else(|| Error::State(StateError::NotInstalled { id: id.clone() }))?;
    installed.pin = None;
    let summary = InstalledSummary::from(&*installed);
    guard.commit()?;
    Ok(summary)
  }

  // -------------------------------------------------- export and import --

  /// Captures what is installed as a portable environment file (§51).
  ///
  /// `pinned` records the exact version of every package, including ones
  /// that arrived as dependencies, so the file reproduces an installation
  /// rather than merely listing it. `false` produces the `--loose` shape.
  pub fn export_env(&self, pinned: bool) -> Result<EnvFile> {
    let state = crate::state::load(&self.layout)?;
    let environment = self.layout.environment().map(|e| e.as_str().to_owned());
    Ok(EnvFile::from_state(&state, environment.as_deref(), pinned))
  }

  /// Installs everything an environment file describes, then reapplies its
  /// pins.
  ///
  /// Only `explicit` entries become roots; dependency versions are supplied
  /// to the resolver as requirements so the same graph comes back, but they
  /// stay recorded as dependencies and so remain eligible for `cleanup`.
  ///
  /// With `prune`, the import converges rather than adds: once what the file
  /// names is installed, every package it neither names nor needs is
  /// removed ([`Session::prune_candidates`]). That is what makes the file a
  /// description of the environment rather than a lower bound on it, and
  /// what a declarative front end — the Home Manager module — relies on.
  /// A file naming nothing is refused without `prune`, since it would do
  /// nothing; with it, it empties the environment.
  pub async fn import_env(
    &self,
    file: &EnvFile,
    prune: bool,
    progress: &mut dyn Progress,
  ) -> Result<ImportOutcome> {
    let roots = file.roots();
    if roots.is_empty() && !prune {
      return Err(Error::InvalidArgument(
        "the environment file lists no explicitly installed packages".into(),
      ));
    }
    let required = file.required_versions();
    let outcome = if roots.is_empty() {
      InstallOutcome::default()
    } else {
      self.install_at(&roots, false, &required, progress).await?
    };

    // Pins are reapplied afterwards: a pin is state about the user's
    // intent, and applying it before the package exists would be writing
    // state for something not installed.
    let (reapplied_pins, skipped_pins) = self.reapply_pins(file.pins())?;

    // After the install, not before: a failed install then leaves what was
    // there in place instead of an environment with things removed and
    // nothing added.
    let pruned = if prune {
      let candidates = self.prune_candidates(file)?;
      if candidates.is_empty() {
        None
      } else {
        Some(self.remove(&candidates, false)?)
      }
    } else {
      None
    };

    Ok(ImportOutcome {
      installed: outcome,
      reapplied_pins,
      skipped_pins,
      pruned,
    })
  }

  /// Reports what importing `file` would install, without changing anything.
  pub fn plan_import(&self, file: &EnvFile) -> Result<InstallPlan> {
    let roots = file.roots();
    if roots.is_empty() {
      return Ok(InstallPlan::default());
    }
    self.plan_install_at(&roots, false, &file.required_versions())
  }

  /// Installed packages that `file` neither names nor needs: what
  /// `env import --prune` removes.
  ///
  /// Everything the resolved plan for the file touches is kept — its roots,
  /// and every dependency of theirs, installed yet or not — so a package the
  /// file lists only as a dependency survives exactly as long as something
  /// named still needs it. The rest of the state goes, explicit or not,
  /// which also collects dependencies that only a pruned package needed.
  pub fn prune_candidates(&self, file: &EnvFile) -> Result<Vec<PackageId>> {
    let wanted: BTreeSet<String> = self
      .plan_import(file)?
      .steps
      .into_iter()
      .map(|step| step.id)
      .collect();
    let state = crate::state::load(&self.layout)?;
    Ok(
      state
        .packages
        .keys()
        .filter(|id| !wanted.contains(id.as_str()))
        .cloned()
        .collect(),
    )
  }

  /// Resolves a user-supplied name to a package ID, with a useful error.
  pub fn parse_id(&self, raw: &str) -> Result<PackageId> {
    PackageId::new(raw).map_err(|e| Error::InvalidArgument(e.to_string()))
  }

  /// External packages detected on this machine.
  pub fn detected_externals(&self) -> Result<BTreeSet<PackageId>> {
    Ok(self.externals_present(self.index()?).clone())
  }
}

// -------------------------------------------------------------- locations --

/// Where the parts a user may move to another disk are, and moving them.
///
/// Apart from [`Session`] for the reason [`Environments`] is: a session's
/// layout already has the locations applied, and possibly an environment on
/// top, while changing a location needs the layout from before either — a
/// reset has to know what the default was.
///
/// Locations belong to the default environment and to the cache every
/// environment shares. A named environment keeps its plugins and libraries
/// inside its own directory, because `env remove` deletes that directory
/// whole and must not have to go looking on other disks.
pub struct Storage {
  /// The layout with no location applied.
  home: Layout,
}

/// One movable part, as `location show` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct LocationSummary {
  pub kind: LocationKind,
  /// The directories in use: one, or one per plugin format.
  pub paths: Vec<PathBuf>,
  /// What the user chose, when it is not the default.
  pub configured: Option<PathBuf>,
  /// False when the chosen directory is not there — a disk not mounted.
  pub available: bool,
}

impl Storage {
  /// `home` is the base layout, before [`Config::located`] and before any
  /// environment redirect.
  pub fn new(home: Layout) -> Self {
    Self { home }
  }

  fn located(&self, config: &Config) -> Layout {
    self.home.clone().with_locations(&config.locations)
  }

  /// Every movable part and where it is now.
  pub fn show(&self) -> Result<Vec<LocationSummary>> {
    let config = Config::load(&self.home)?;
    Ok(Self::summarise(&self.located(&config)))
  }

  fn summarise(layout: &Layout) -> Vec<LocationSummary> {
    let missing = layout.unavailable_locations();
    LocationKind::ALL
      .into_iter()
      .map(|kind| LocationSummary {
        kind,
        paths: Self::roots(layout, kind),
        configured: layout.locations().get(kind).map(PathBuf::from),
        available: !missing.iter().any(|(k, _)| *k == kind),
      })
      .collect()
  }

  /// The directories `kind` names in `layout`.
  fn roots(layout: &Layout, kind: LocationKind) -> Vec<PathBuf> {
    match kind {
      LocationKind::Cache => vec![layout.cache_dir().to_path_buf()],
      LocationKind::Libraries => vec![layout.library_root().to_path_buf()],
      LocationKind::Plugins => layout
        .plugin_roots()
        .map(|(_, root)| root.to_path_buf())
        .collect(),
    }
  }

  /// Puts `kind` in `path` from now on.
  ///
  /// `path` must be a directory that exists: this is the moment to find out
  /// that a disk is not mounted, rather than after an install has filled the
  /// empty mount point underneath it. Nothing already installed is moved,
  /// and so nothing installed may be left behind either — see
  /// [`Storage::refuse_stranding`].
  pub fn set(&self, kind: LocationKind, path: &Path) -> Result<Vec<LocationSummary>> {
    if !path.is_absolute()
      || path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
      return Err(Error::InvalidArgument(format!(
        "{} is not an absolute path without `..`",
        path.display()
      )));
    }
    if !path.is_dir() {
      return Err(Error::InvalidArgument(format!(
        "{} is not a directory; create it first (and check that its disk is mounted)",
        path.display()
      )));
    }
    self.change(kind, Some(path.to_path_buf()))
  }

  /// Puts `kind` back where it is by default.
  pub fn reset(&self, kind: LocationKind) -> Result<Vec<LocationSummary>> {
    self.change(kind, None)
  }

  fn change(&self, kind: LocationKind, path: Option<PathBuf>) -> Result<Vec<LocationSummary>> {
    // Under the state lock, so an install in another terminal cannot land
    // in the old location between the check and the save.
    let before_config = Config::load(&self.home)?;
    let before = self.located(&before_config);
    let guard = StateGuard::acquire(&before)?;

    let mut config = before_config.clone();
    config.locations.set(kind, path);
    let after = self.located(&config);

    if Self::roots(&before, kind) != Self::roots(&after, kind) {
      self.refuse_nesting(&after, kind)?;
      Self::refuse_stranding(&before, kind, guard.state())?;
    }
    config.save(&self.home)?;
    Ok(Self::summarise(&after))
  }

  /// A location inside another, or around one, would let a package ID name
  /// a directory the manager keeps something else in: libraries in
  /// `/mnt/x` and the cache in `/mnt/x/cache`, then a package called
  /// `cache`. The manager's own data and configuration are off limits for
  /// the same reason.
  fn refuse_nesting(&self, after: &Layout, kind: LocationKind) -> Result<()> {
    let Some(chosen) = after.locations().get(kind) else {
      return Ok(());
    };
    let others = LocationKind::ALL
      .into_iter()
      .filter(|other| *other != kind)
      .flat_map(|other| Self::roots(after, other))
      .chain([
        after.data_dir().to_path_buf(),
        after.config_dir().to_path_buf(),
      ]);
    for other in others {
      if chosen.starts_with(&other) || other.starts_with(chosen) {
        return Err(Error::InvalidArgument(format!(
          "{} overlaps {}, which Luthier already uses; choose a directory of its own",
          chosen.display(),
          other.display()
        )));
      }
    }
    Ok(())
  }

  /// Refuses to move a root that packages are installed under.
  ///
  /// State records absolute paths and removal deletes only inside the
  /// current roots, so a package left in the old place could never be
  /// removed. The cache is exempt: everything in it can be fetched again,
  /// and nothing records a path into it.
  fn refuse_stranding(before: &Layout, kind: LocationKind, state: &State) -> Result<()> {
    if kind == LocationKind::Cache {
      return Ok(());
    }
    let roots = Self::roots(before, kind);
    let stranded: Vec<String> = state
      .packages
      .values()
      .filter(|package| {
        package
          .files
          .iter()
          .any(|file| roots.iter().any(|root| file.path().starts_with(root)))
      })
      .map(|package| package.id.to_string())
      .collect();
    if stranded.is_empty() {
      return Ok(());
    }
    let list = stranded.join(" ");
    Err(Error::InvalidArgument(format!(
      "{} package(s) are installed in the current {kind} location: {list}. \
       Remove them first (`luthier remove {list}`), change the location, then install them again",
      stranded.len()
    )))
  }

  /// The exports that let hosts find plugins in a location the user chose.
  pub fn search_path(&self) -> Result<Activation> {
    let config = Config::load(&self.home)?;
    Ok(env::relocated_search_path(
      &self.home,
      &self.located(&config),
    ))
  }
}

// ----------------------------------------------------------- environments --

/// Managing environments, as opposed to working inside one.
///
/// Deliberately not part of [`Session`]. A session's `Layout` is already
/// redirected into whichever environment was selected, and these operations
/// act *on* environments from outside: `env create` must work when nothing
/// exists yet, and `env remove` must work on one that is not the active one.
/// So this takes the base layout and the selection, and every decision an
/// environment command makes — what a valid name is, what "not there" means,
/// which environment `path` defaults to — lives here rather than in a front
/// end, for the same reason the rest of `api` does.
pub struct Environments {
  layout: Layout,
  active: Option<EnvName>,
}

impl Environments {
  /// `layout` is the base layout, never one already redirected by `into_env`.
  pub fn new(layout: Layout, active: Option<EnvName>) -> Self {
    Self { layout, active }
  }

  /// Validates a user-supplied name.
  ///
  /// The name becomes a path segment, so it is checked rather than trusted;
  /// the error is an invalid argument because that is what it is.
  pub fn name(&self, raw: &str) -> Result<EnvName> {
    EnvName::new(raw.to_owned()).map_err(|e| Error::InvalidArgument(e.to_string()))
  }

  /// Every environment, with the selected one marked.
  pub fn list(&self) -> Result<Vec<EnvSummary>> {
    env::list(&self.layout, self.active.as_ref())
  }

  /// Creates one, returning where it went.
  pub fn create(&self, name: &EnvName) -> Result<PathBuf> {
    env::create(&self.layout, name)
  }

  /// Where an environment lives, whether or not it exists yet.
  pub fn path(&self, name: &EnvName) -> PathBuf {
    env::path(&self.layout, name)
  }

  /// Where an existing environment lives, refusing a name that is not there.
  ///
  /// Separate from [`Environments::remove`] so a front end can put the path
  /// in a confirmation prompt and still refuse a typo before asking anything.
  pub fn locate(&self, name: &EnvName) -> Result<PathBuf> {
    self.must_exist(name, None)?;
    Ok(self.path(name))
  }

  /// Refuses a selection naming an environment that does not exist.
  pub fn require(&self, name: &EnvName) -> Result<()> {
    self.must_exist(name, Some("Create it with: luthier env create"))
  }

  /// The layout a command acts through: the base one, or the selected
  /// environment's.
  ///
  /// This is where `--env` and `LUTHIER_ENV` stop being a name and become
  /// paths, which is why the selection is checked here rather than wherever a
  /// front end happens to read the flag.
  pub fn selected_layout(&self) -> Result<Layout> {
    match self.active.as_ref() {
      Some(name) => {
        self.require(name)?;
        Ok(self.layout.clone().into_env(name))
      }
      None => Ok(self.layout.clone()),
    }
  }

  /// Deletes an environment and everything installed in it.
  pub fn remove(&self, name: &EnvName) -> Result<PathBuf> {
    self.must_exist(name, None)?;
    env::remove(&self.layout, name)
  }

  /// The shell commands that put an environment on the search path.
  pub fn activation(&self, name: &EnvName) -> Result<Activation> {
    self.require(name)?;
    let redirected = self.layout.clone().into_env(name);
    Ok(env::activation(&redirected, name))
  }

  /// The shell commands that undo an activation.
  pub fn deactivation(&self) -> Activation {
    env::deactivation()
  }

  /// The environment in use and where it lives, if one is selected.
  pub fn active(&self) -> Option<(&EnvName, PathBuf)> {
    let name = self.active.as_ref()?;
    Some((name, env::path(&self.layout, name)))
  }

  /// The directory `env path` prints: the one named, else the active one.
  pub fn path_of(&self, name: Option<&EnvName>) -> Result<PathBuf> {
    match name {
      Some(name) => Ok(self.path(name)),
      None => match self.active.as_ref() {
        Some(active) => Ok(self.path(active)),
        None => Err(Error::InvalidArgument(
          "no environment active; name one, or activate it first".into(),
        )),
      },
    }
  }

  fn must_exist(&self, name: &EnvName, hint: Option<&str>) -> Result<()> {
    if env::exists(&self.layout, name) {
      return Ok(());
    }
    Err(Error::InvalidArgument(match hint {
      Some(hint) => format!("no environment named {name}. {hint} {name}"),
      None => format!("no environment named {name}"),
    }))
  }
}

// ------------------------------------------------------------------ results --

/// What a resolution would download, summed and at its peak.
///
/// One value computed once: the planner and the installer have to agree about
/// which packages need work, and about how many bytes that is, or the plan the
/// user approved is not the plan that runs.
struct ArtifactSizes {
  /// Every artifact that would be fetched.
  total: u64,
  /// The largest single one, which is all the workspace ever holds at once.
  largest: u64,
  /// The same bytes, by the root each package is installed under.
  by_root: BTreeMap<PathBuf, u64>,
}

impl ArtifactSizes {
  fn of(resolution: &Resolution<'_>, layout: &Layout) -> Self {
    let mut total = 0;
    let mut largest = 0;
    let mut by_root = BTreeMap::new();
    for artifact in resolution.to_install().filter_map(|p| p.artifact) {
      let Some(size) = artifact.size else {
        continue;
      };
      total += size;
      largest = largest.max(size);
      *by_root.entry(Self::root_of(artifact, layout)).or_default() += size;
    }
    Self {
      total,
      largest,
      by_root,
    }
  }

  /// Where most of an artifact's bytes land. Content goes to the library
  /// root; a plugin to the root of the first format this build installs.
  fn root_of(artifact: &luthier_manifest::Artifact, layout: &Layout) -> PathBuf {
    if artifact.provides.contains(&Format::Library) {
      return layout.library_root().to_path_buf();
    }
    artifact
      .provides
      .iter()
      .find_map(|format| layout.plugin_root(format))
      .unwrap_or(layout.data_dir())
      .to_path_buf()
  }
}

/// Why a plan cannot be carried out.
///
/// Kept in typed form rather than as rendered text so the refusal a front end
/// raises from a plan is the same error, with the same hint and exit code,
/// that the installer would raise for it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum BlockedReason {
  MissingExternal {
    id: PackageId,
    provisioning_hint: Option<String>,
  },
  NotEnoughSpace {
    path: PathBuf,
    required: u64,
    available: u64,
  },
  LocalChanges {
    id: PackageId,
    changes: Vec<String>,
  },
}

impl BlockedReason {
  /// The error an installer would raise for this, built once so the two
  /// cannot drift apart.
  pub fn to_error(&self) -> Error {
    match self {
      BlockedReason::MissingExternal {
        id,
        provisioning_hint,
      } => Error::Resolve(ResolveError::ExternalMissing {
        id: id.clone(),
        provisioning_hint: provisioning_hint.clone(),
      }),
      BlockedReason::NotEnoughSpace {
        path,
        required,
        available,
      } => Error::Install(InstallError::NotEnoughSpace {
        path: path.clone(),
        required: *required,
        available: *available,
      }),
      BlockedReason::LocalChanges { id, changes } => Error::Install(InstallError::LocalChanges {
        id: id.clone(),
        changes: changes.clone(),
      }),
    }
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
  pub installed: InstallOutcome,
  /// Pins carried over from the file, as `"<id> <version>"`.
  pub reapplied_pins: Vec<String>,
  /// Pins the file carried for packages this import did not install.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub skipped_pins: Vec<String>,
  /// What `--prune` removed, when it removed anything.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub pruned: Option<RemoveOutcome>,
}

/// The name an artifact was published under: the last segment of its URL,
/// decoded.
///
/// The cache names a file by its digest, which is right for the cache and
/// wrong for a bare file, whose name is the only thing that says what it is —
/// `LibreKick_linux_x86_64.clap` is a CLAP because of the name it was given.
/// Whatever comes back is untrusted: `archive::place` holds it to the same
/// rules as an archive entry.
fn published_name(url: &url::Url) -> String {
  let last = url
    .path_segments()
    .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
    .unwrap_or_default();
  percent_encoding::percent_decode_str(last)
    .decode_utf8_lossy()
    .into_owned()
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
  pub id: String,
  pub name: String,
  pub kind: String,
  pub category: String,
  pub tags: Vec<String>,
  pub description: Option<String>,
  pub version: Option<String>,
  pub formats: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageInfo {
  pub id: String,
  pub name: String,
  pub kind: String,
  pub description: Option<String>,
  pub category: String,
  pub tags: Vec<String>,
  pub license: String,
  pub license_kind: String,
  pub homepage: Option<String>,
  pub repository: Option<String>,
  pub documentation: Option<String>,
  pub authors: Vec<String>,
  pub latest_version: Option<String>,
  pub available_versions: Vec<String>,
  pub formats: Vec<String>,
  pub target: String,
  pub registry: String,
  pub installed_version: Option<String>,
  /// Where an `external` package was found on this machine, if it is present.
  pub detected_at: Option<String>,
  pub pinned: Option<String>,
  pub dependencies: Vec<String>,
  /// What the package holds that needs an engine, e.g. `SFZ`.
  pub content: Vec<String>,
  /// Engines that play that content, by package ID. Any one is enough.
  pub played_by: Vec<String>,
  /// Whether the install rules were written and reviewed, or read out of the
  /// archive at install time.
  ///
  /// Over four hundred packages reach the manager from a source that says
  /// which formats an archive holds but never which entry is which, so their
  /// rules are derived and nobody has looked at them. That is a deliberate
  /// trade for breadth, and a user is entitled to know which kind they have.
  pub rules_reviewed: Option<bool>,
  pub provisioning_hint: Option<String>,
  pub unknown_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledSummary {
  pub id: String,
  pub name: String,
  pub version: String,
  pub formats: Vec<String>,
  pub reason: String,
  pub pinned: Option<String>,
  pub installed_at: String,
  pub files: Vec<String>,
}

impl From<&InstalledPackage> for InstalledSummary {
  fn from(package: &InstalledPackage) -> Self {
    Self {
      id: package.id.to_string(),
      name: package.name.clone(),
      version: package.version.to_string(),
      formats: package.formats.iter().map(Format::to_string).collect(),
      reason: match package.reason {
        InstallReason::Explicit => "explicit",
        InstallReason::Dependency => "dependency",
      }
      .into(),
      pinned: package.pin.as_ref().map(ToString::to_string),
      installed_at: package.installed_at.to_string(),
      files: package
        .files
        .iter()
        .map(|f| f.path().display().to_string())
        .collect(),
    }
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallStep {
  pub id: String,
  pub name: String,
  pub version: String,
  pub action: String,
  pub reason: String,
  pub download_bytes: Option<u64>,
  pub formats: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissingExternal {
  pub id: String,
  pub name: String,
  pub provisioning_hint: Option<String>,
}

/// A package whose content nothing on this machine appears to play.
///
/// Reported, never refused. What a user does with a folder of samples is
/// their business, and a registry that names no engine for a format is this
/// manager being ignorant rather than the machine being unable — which is
/// exactly the case with a registry that has no field for what plays what.
#[derive(Debug, Clone, Serialize)]
pub struct UnplayableContent {
  pub id: String,
  pub name: String,
  pub content: String,
  /// What kind of software plays this format, in prose. True whatever any
  /// registry knows, so the warning is useful even with an empty `engines`.
  pub played_by: String,
  /// Any one of these would do, in the order the registries list them.
  pub engines: Vec<EngineChoice>,
}

/// An engine that would make some content playable.
#[derive(Debug, Clone, Serialize)]
pub struct EngineChoice {
  pub id: String,
  pub name: String,
  /// Whether this manager can install it for this platform.
  pub installable: bool,
  /// Where to get it otherwise, for an `external` engine.
  pub provisioning_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InstallPlan {
  pub steps: Vec<InstallStep>,
  pub missing_externals: Vec<MissingExternal>,
  /// Installing is refused while this is non-empty.
  pub unplayable: Vec<UnplayableContent>,
  /// Why this plan cannot be carried out, if it cannot.
  ///
  /// The lists above describe the obstacle for a reader; this carries the
  /// refusal itself, so a front end raises the installer's own error rather
  /// than approximating it.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub blocked: Option<BlockedReason>,
  /// What each filesystem involved would have to provide, and what it has.
  ///
  /// Empty when nothing would be downloaded, or when free space could not be
  /// determined.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub space: Vec<SpaceRequirement>,
}

/// One configured bench, and where it sits in the precedence order.
#[derive(Debug, Clone, Serialize)]
pub struct BenchSummary {
  /// 1 is consulted first and wins any ID two benches both carry.
  pub priority: usize,
  pub name: String,
  pub kind: String,
  pub location: String,
}

/// One file in the download cache.
#[derive(Debug, Clone, Serialize)]
pub struct CacheEntry {
  pub sha256: String,
  pub path: PathBuf,
  pub bytes: u64,
  /// True for an interrupted download rather than a verified artifact.
  pub partial: bool,
  /// Installed packages that recorded this digest. Empty means nothing wants it.
  pub used_by: Vec<String>,
}

/// What `clean_cache` removed, or would have.
#[derive(Debug, Clone, Serialize)]
pub struct CacheCleaned {
  pub dry_run: bool,
  pub bytes: u64,
  pub removed: Vec<CacheEntry>,
}

/// Room needed on one filesystem, against room available.
#[derive(Debug, Clone, Serialize)]
pub struct SpaceRequirement {
  /// A directory on the filesystem in question.
  pub path: PathBuf,
  pub required_bytes: u64,
  pub available_bytes: u64,
}

impl SpaceRequirement {
  pub fn is_short(&self) -> bool {
    self.available_bytes < self.required_bytes
  }
}

impl InstallPlan {
  /// Whether installing would be refused before anything is downloaded:
  /// something required cannot be installed by this manager, content would
  /// arrive with nothing to play it, or the disk could not hold it.
  pub fn is_blocked(&self) -> bool {
    self.blocked.is_some()
  }

  /// The error to raise instead of carrying this plan out.
  ///
  /// A front end that has shown a plan refuses it with this, rather than
  /// calling `install` and letting the installer decide again from state it
  /// reads afresh: the second derivation can disagree with the first, and
  /// then something the user was never asked about gets installed.
  pub fn refusal(&self) -> Option<Error> {
    self.blocked.as_ref().map(BlockedReason::to_error)
  }

  /// Filesystems that could not hold what this plan would write.
  pub fn short_of_space(&self) -> impl Iterator<Item = &SpaceRequirement> {
    self.space.iter().filter(|s| s.is_short())
  }

  /// Steps that would actually change something.
  pub fn actionable(&self) -> impl Iterator<Item = &InstallStep> {
    self
      .steps
      .iter()
      .filter(|s| s.action != "already installed")
  }

  pub fn total_download_bytes(&self) -> u64 {
    self.actionable().filter_map(|s| s.download_bytes).sum()
  }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InstallOutcome {
  pub installed: Vec<InstalledSummary>,
  pub skipped: Vec<InstalledSummary>,
  /// Things that were true but not fatal: upstream metadata that did not
  /// match what the archive turned out to hold.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub warnings: Vec<String>,
}

/// Formats a source claimed that the archive did not actually deliver.
fn missing_formats(claimed: &[Format], rules: &[luthier_manifest::InstallRule]) -> Vec<Format> {
  let produced: BTreeSet<&Format> = rules.iter().map(|rule| &rule.format).collect();
  claimed
    .iter()
    .filter(|format| !produced.contains(format))
    .cloned()
    .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct RemovalTarget {
  pub id: String,
  pub name: String,
  pub version: String,
  pub files: Vec<String>,
  /// Installed packages that still need this one.
  pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemovalPlan {
  pub packages: Vec<RemovalTarget>,
  /// Installed content this removal would leave with nothing to play it.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub stranded: Vec<StrandedContent>,
}

/// A library that would be left silent, and what would have played it.
#[derive(Debug, Clone, Serialize)]
pub struct StrandedContent {
  pub id: String,
  pub content: String,
  pub engines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeptFile {
  pub path: String,
  pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoveOutcome {
  pub removed: Vec<InstalledSummary>,
  /// Files left in place because they were changed, added or replaced after
  /// installation. Only those: the rest of a bundle they sit in is removed.
  pub kept_files: Vec<KeptFile>,
  /// Installed content this removal left with nothing to play it.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub stranded: Vec<StrandedContent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileProblem {
  pub path: String,
  pub problem: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
  pub id: String,
  pub name: String,
  pub version: String,
  pub ok: bool,
  pub problems: Vec<FileProblem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableUpdate {
  pub id: String,
  pub name: String,
  pub installed: String,
  pub available: String,
  pub pinned: Option<String>,
}
