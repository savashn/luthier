//! The operations a front end performs.
//!
//! This is the whole public surface of the manager (§38). The CLI parses
//! arguments and renders results; every decision about what to do lives here,
//! so a GUI added later reuses it rather than reimplementing it. Results are
//! plain owned, serialisable data — never borrowed from an index — so a caller
//! can render them however it likes, including as JSON (§33).

use crate::archive::{self, ExtractLimits};
use crate::config::{Config, RegistryConfig, RegistrySource};
use crate::download::{Downloader, Progress};
use crate::engine;
use crate::env::{self, Activation, EnvName, EnvSummary};
use crate::envfile::EnvFile;
use crate::error::{Error, InstallError, ResolveError, Result, StateError};
use crate::fsutil;
use crate::install::{self, EntryStatus, InstallTransaction};
use crate::layout::Layout;
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
use std::path::PathBuf;

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
    // Nothing is being accepted here: `load_index` reads the snapshot that
    // a previous refresh already verified, so there is no signature for an
    // override to relax.
    let providers = self.config.providers(&self.layout, self.offline, false);
    let built = RegistryIndex::merge(providers.iter().map(|provider| provider.load_index()))?;
    Ok(self.index.get_or_init(|| built))
  }

  // -------------------------------------------------------------- refresh --

  /// Updates every configured registry (§31 `refresh`).
  ///
  /// `allow_unsigned` accepts a snapshot from a bench that was signed before
  /// and is not this time — and nothing else. A signature that fails to
  /// verify, or one made with a key the bench is not trusted to use, is
  /// refused whatever the caller passes: those are claims that did not hold
  /// up rather than absent ones. The flag is a parameter rather than a
  /// setting on the session for the same reason `install` takes `force`:
  /// consent belongs to the operation a user asked for.
  pub async fn refresh(&self, allow_unsigned: bool) -> Result<Vec<RefreshOutcome>> {
    let mut outcomes = Vec::new();
    let mut first_error = None;
    for provider in self
      .config
      .providers(&self.layout, self.offline, allow_unsigned)
    {
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
    let sizes = ArtifactSizes::of(&resolution);
    let space = self.space_needed(&sizes);

    // The typed refusal is decided here, from the resolution this plan was
    // built out of, so a front end refuses the plan it showed the user rather
    // than relying on the installer to derive the same verdict a second time
    // from freshly-read state. Two derivations can disagree; the one the user
    // was shown is the one that must decide.
    let blocked = resolution
      .missing_externals()
      .next()
      .map(|external| BlockedReason::MissingExternal {
        id: external.id.clone(),
        provisioning_hint: external.provisioning_hint.clone(),
      })
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
  /// The cache and the install root are often the same filesystem and
  /// sometimes not, so the requirement is summed per device rather than
  /// checked twice against the same free space.
  fn space_needed(&self, sizes: &ArtifactSizes) -> Vec<SpaceRequirement> {
    if sizes.total == 0 {
      return Vec::new();
    }
    let cache = self.layout.artifact_cache_dir();
    let workspace = self.layout.transactions_dir();
    let install = self.layout.data_dir().to_path_buf();

    let mut by_device: BTreeMap<Option<u64>, (PathBuf, u64)> = BTreeMap::new();
    // Directories sharing a device share one requirement, and only one path
    // can name it. They are visited in the order a user would act on them —
    // the install root, then the cache, then the workspace — so the name a
    // shortage carries is the one worth freeing space in, rather than
    // whichever directory happened to be listed first.
    for (path, needed) in [
      (install, sizes.total),
      (cache, sizes.total),
      (workspace, sizes.largest),
    ] {
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

    // Running out of disk is refused here too, and for the same reason:
    // discovering it mid-extraction leaves a rolled-back transaction and a
    // cache full of bytes the user waited an hour for.
    if let Some(short) = self
      .space_needed(&ArtifactSizes::of(&resolution))
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
      archive::extract(
        &fetched.path,
        &artifact.archive,
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

        match install::verify_entry(entry)? {
          EntryStatus::Missing => {}
          EntryStatus::Intact => fsutil::remove_any(path)?,
          EntryStatus::Modified { detail } => {
            // Someone changed it after we installed it; that is
            // their work, not ours to throw away.
            kept_files.push(KeptFile {
              path: path.display().to_string(),
              reason: detail,
            });
          }
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

  /// The benches recorded in the configuration file, highest priority first.
  ///
  /// Order is the whole mechanism behind correcting a broader source, so it is
  /// what this reports: the first bench to claim an ID keeps it.
  ///
  /// Read from the file rather than from this session, so that all three bench
  /// commands describe the same thing. A session started with
  /// `--registry-path` is running against an override, and listing the
  /// override here would mean `bench add` appeared to do nothing.
  pub fn configured_benches(&self) -> Result<Vec<BenchSummary>> {
    Ok(Self::summarise(&Config::load(&self.layout)?))
  }

  /// Adds a bench to the configuration.
  ///
  /// Appended last unless `first` says otherwise, because priority decides
  /// which manifest wins a collision: a bench added without a word about
  /// precedence must not quietly start overriding the curated one.
  pub fn add_bench(
    &self,
    name: &str,
    source: RegistrySource,
    keys: &[String],
    first: bool,
  ) -> Result<Vec<BenchSummary>> {
    let name = validate_bench_name(name)?;
    let mut config = Config::load(&self.layout)?;
    if config.registries.iter().any(|r| r.name == name) {
      return Err(Error::InvalidArgument(format!(
        "a bench named {name:?} is already configured; remove it first"
      )));
    }
    let mut entry = RegistryConfig::new(name, source);
    for key in keys {
      entry.keys.push(parse_key(&entry, key)?);
    }
    if first {
      config.registries.insert(0, entry);
    } else {
      config.registries.push(entry);
    }
    config.save(&self.layout)?;
    Ok(Self::summarise(&config))
  }

  /// Removes a bench from the configuration. Its snapshot is deleted too.
  pub fn remove_bench(&self, name: &str) -> Result<Vec<BenchSummary>> {
    let mut config = Config::load(&self.layout)?;
    let before = config.registries.len();
    config.registries.retain(|r| r.name != name);
    if config.registries.len() == before {
      return Err(Error::InvalidArgument(format!(
        "no bench named {name:?} is configured"
      )));
    }

    // A snapshot nothing reads is just occupancy, and leaving it would make
    // re-adding the name silently reuse stale metadata. The pinned origin goes
    // with it: the user has discarded this bench, so re-adding it under a
    // different URL is a decision they have already made.
    //
    // The name becomes a path segment of a *recursive* delete, so it is
    // validated here exactly as `add_bench` validates it, rather than trusted
    // because it appeared in a file. Matching an entry in `config.json` proves
    // only that some entry carried that string: a hand-edited or migrated
    // `name = ".."` would otherwise resolve to the whole data directory.
    // Nothing on disk can sit under an unusable name, since nothing could have
    // created it, so the configuration entry still goes.
    if validate_bench_name(name).is_ok() {
      // Deleted before the configuration is saved: a failure here leaves the
      // bench configured and its snapshot intact, which a second attempt
      // fixes. The other order would leave a snapshot no configuration
      // mentions — exactly the stale metadata this deletion exists to avoid.
      fsutil::remove_any(&self.layout.registry_dir(name))?;
      crate::registry::provenance::forget(&self.layout.registries_dir(), name);
    }

    config.save(&self.layout)?;
    Ok(Self::summarise(&config))
  }

  /// Trusts a key for a bench, so a snapshot it signs is accepted and one
  /// anybody else signs is not.
  ///
  /// Adding a key without removing the old one is what a rotation is: both
  /// are accepted until the bench has published under the new key, and
  /// `untrust` retires the old one afterwards. Doing it the other way round
  /// leaves a window where no refresh can succeed.
  pub fn trust_bench(&self, name: &str, key: &str) -> Result<Vec<BenchSummary>> {
    let mut config = Config::load(&self.layout)?;
    let entry = config
      .registries
      .iter_mut()
      .find(|r| r.name == name)
      .ok_or_else(|| Error::InvalidArgument(format!("no bench named {name:?} is configured")))?;

    let key = parse_key(entry, key)?;
    if entry.keys.contains(&key) {
      return Err(Error::InvalidArgument(format!(
        "bench {name:?} already trusts {key}"
      )));
    }
    entry.keys.push(key);
    config.save(&self.layout)?;
    Ok(Self::summarise(&config))
  }

  /// Stops trusting a key, or every key when none is named.
  ///
  /// Dropping the last key also forgets the key pinned on the first fetch.
  /// Otherwise "this bench is one I read unsigned" would be a decision the
  /// user made and the pin quietly overruled.
  pub fn untrust_bench(&self, name: &str, key: Option<&str>) -> Result<Vec<BenchSummary>> {
    let mut config = Config::load(&self.layout)?;
    let entry = config
      .registries
      .iter_mut()
      .find(|r| r.name == name)
      .ok_or_else(|| Error::InvalidArgument(format!("no bench named {name:?} is configured")))?;

    match key {
      Some(raw) => {
        let key = parse_key(entry, raw)?;
        let before = entry.keys.len();
        entry.keys.retain(|k| k != &key);
        if entry.keys.len() == before {
          return Err(Error::InvalidArgument(format!(
            "bench {name:?} does not trust {key}"
          )));
        }
      }
      None => entry.keys.clear(),
    }

    if entry.keys.is_empty() && validate_bench_name(name).is_ok() {
      crate::registry::provenance::forget_key(&self.layout.registries_dir(), name);
    }
    config.save(&self.layout)?;
    Ok(Self::summarise(&config))
  }

  fn summarise(config: &Config) -> Vec<BenchSummary> {
    config
      .registries
      .iter()
      .enumerate()
      .map(|(i, registry)| BenchSummary {
        priority: i + 1,
        name: registry.name.clone(),
        keys: registry.keys.iter().map(ToString::to_string).collect(),
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
  pub async fn import_env(
    &self,
    file: &EnvFile,
    progress: &mut dyn Progress,
  ) -> Result<ImportOutcome> {
    let roots = file.roots();
    if roots.is_empty() {
      return Err(Error::InvalidArgument(
        "the environment file lists no explicitly installed packages".into(),
      ));
    }
    let required = file.required_versions();
    let outcome = self.install_at(&roots, false, &required, progress).await?;

    // Pins are reapplied afterwards: a pin is state about the user's
    // intent, and applying it before the package exists would be writing
    // state for something not installed.
    let (reapplied_pins, skipped_pins) = self.reapply_pins(file.pins())?;

    Ok(ImportOutcome {
      installed: outcome,
      reapplied_pins,
      skipped_pins,
    })
  }

  /// Reports what importing `file` would do, without changing anything.
  pub fn plan_import(&self, file: &EnvFile) -> Result<InstallPlan> {
    self.plan_install_at(&file.roots(), false, &file.required_versions())
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
}

impl ArtifactSizes {
  fn of(resolution: &Resolution<'_>) -> Self {
    let sizes = resolution
      .to_install()
      .filter_map(|p| p.artifact.and_then(|a| a.size));
    let mut total = 0;
    let mut largest = 0;
    for size in sizes {
      total += size;
      largest = largest.max(size);
    }
    Self { total, largest }
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

#[derive(Debug, Clone, Serialize)]
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
  /// Keys this bench is trusted to be signed with, as hex.
  ///
  /// Empty means "whatever signs it first", which is what every bench
  /// starts as. It is reported rather than left implicit because the
  /// difference between a pinned key and no key at all is the difference
  /// between the two threat models this answers.
  pub keys: Vec<String>,
}

/// Reads a key for a bench that could actually use one.
///
/// Refusing it where it is written beats accepting a key that would never be
/// checked: a path bench is a directory the user already controls, and an
/// Open Audio Stack site publishes no signature to check it against.
fn parse_key(bench: &RegistryConfig, raw: &str) -> Result<crate::registry::signature::PublicKey> {
  if !bench.can_be_signed() {
    return Err(Error::InvalidArgument(format!(
      "bench {:?} is a {} bench, and nothing published there carries a signature to check",
      bench.name,
      match bench.source {
        RegistrySource::Path { .. } => "local directory",
        RegistrySource::Oas { .. } => "Open Audio Stack",
        RegistrySource::Snapshot { .. } => "snapshot",
      }
    )));
  }
  raw
    .parse()
    .map_err(|e| Error::InvalidArgument(format!("{raw:?} is not an Ed25519 public key: {e}")))
}

/// Rejects a bench name that could not be a directory.
///
/// The name becomes a path segment under the snapshot directory, so it is
/// validated rather than trusted — the same reason an environment name is.
fn validate_bench_name(raw: &str) -> Result<String> {
  let invalid =
    |reason: &str| Error::InvalidArgument(format!("{raw:?} is not a usable bench name: {reason}"));
  if raw.is_empty() {
    return Err(invalid("it is empty"));
  }
  if raw.len() > 64 {
    return Err(invalid("it is longer than 64 characters"));
  }
  if raw.starts_with('.') {
    return Err(invalid("it starts with a dot"));
  }
  if !raw
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
  {
    return Err(invalid("use letters, digits, '.', '-' and '_'"));
  }
  Ok(raw.to_owned())
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

#[derive(Debug, Clone, Serialize)]
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
  /// Files left in place because they had been modified since installation.
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
