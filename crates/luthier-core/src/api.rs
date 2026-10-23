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
use crate::envfile::EnvFile;
use crate::error::{Error, InstallError, ResolveError, Result, StateError};
use crate::fsutil;
use crate::install::{self, EntryStatus, InstallTransaction};
use crate::layout::{Layout, LocationKind};
use crate::registry::{RefreshOutcome, RegistryIndex};
use crate::resolver::{self, Disposition, Resolution, ResolveRequest};
use crate::scan::{self, DetectedPlugin};
use crate::selfupdate;
use crate::state::{
  ArtifactRecord, InstallReason, InstalledEntry, InstalledPackage, State, StateGuard, StateLock,
};
use jiff::Timestamp;
use luthier_manifest::{Format, PackageId, PackageKind, Target};
use semver::Version;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the line about a newer Luthier waits for GitHub.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(3);

/// How old the Open Audio Stack index may get before a command that reads it
/// asks for a newer one first. A day: packages change as their authors
/// publish, and asking costs no bytes when nothing has.
pub const AUTOMATIC_REFRESH_AFTER: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// No packages: what every resolution but an upgrade holds back, and what
/// one with nothing to install looks for on disk.
static NO_PACKAGES: BTreeSet<PackageId> = BTreeSet::new();

/// No version required: what every resolution but an import's demands.
static NO_VERSIONS: BTreeMap<PackageId, Version> = BTreeMap::new();

/// What a resolution is for, which decides what each package is recorded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose<'h> {
  /// The roots were asked for, and are recorded as explicitly installed
  /// (§24), as the resolver marks them — a dependency installed by name
  /// becomes one the user wants, as apt marks it manual.
  Install,
  /// The roots are being brought to a newer release, so they keep the
  /// reason they were recorded with. Naming a dependency to upgrade it is
  /// not asking to keep it forever. `held` are the roots whose own upgrade
  /// cannot be resolved ([`Session::resolve_upgrade`]): they stay at the
  /// version installed, with what they need.
  Upgrade { held: &'h BTreeSet<PackageId> },
}

impl Purpose<'_> {
  fn held(&self) -> &BTreeSet<PackageId> {
    match self {
      Purpose::Install => &NO_PACKAGES,
      Purpose::Upgrade { held } => held,
    }
  }

  /// Overrides what the resolver says with what state already records.
  ///
  /// The resolver knows only roots and dependencies; it cannot tell a
  /// package the user asked for last week from one another package pulls
  /// in today. So an explicit package is never demoted: upgraded or
  /// reinstalled as someone else's dependency, it stays explicit, or
  /// `autoremove` would delete something asked for by name.
  fn record_reasons(self, resolution: &mut Resolution<'_>, state: &State) {
    for package in &mut resolution.order {
      let Some(installed) = state.get(package.id()) else {
        continue;
      };
      let asked_for = self == Purpose::Install && package.reason == InstallReason::Explicit;
      if !asked_for {
        package.reason = installed.reason;
      }
    }
  }
}

/// What a plan resolves, and what carrying it out resolves again.
#[derive(Clone, Copy)]
struct Request<'r> {
  roots: &'r [PackageId],
  force: bool,
  required_versions: &'r BTreeMap<PackageId, Version>,
  purpose: Purpose<'r>,
}

impl<'r> Request<'r> {
  fn install(
    roots: &'r [PackageId],
    force: bool,
    required: &'r BTreeMap<PackageId, Version>,
  ) -> Self {
    Self {
      roots,
      force,
      required_versions: required,
      purpose: Purpose::Install,
    }
  }

  /// An upgrade of `roots`, holding `held` back: the plan and carrying it
  /// out build it the same way, so they cannot resolve differently.
  fn upgrade(roots: &'r [PackageId], held: &'r BTreeSet<PackageId>) -> Self {
    Self {
      roots,
      force: false,
      required_versions: &NO_VERSIONS,
      purpose: Purpose::Upgrade { held },
    }
  }
}

/// Installed dependencies this resolution records as asked for by name:
/// an install's roots that are there already, whether resolved through a
/// source or kept from the state (see [`resolver::frozen`]).
fn requested_marks(
  resolution: &Resolution<'_>,
  state: &State,
  purpose: Purpose<'_>,
  roots: &[PackageId],
) -> Vec<PackageId> {
  let was_dependency = |id: &PackageId| {
    state
      .get(id)
      .is_some_and(|installed| installed.reason == InstallReason::Dependency)
  };
  let mut marks: Vec<PackageId> = resolution
    .order
    .iter()
    .filter(|package| {
      package.disposition == Disposition::Satisfied
        && package.reason == InstallReason::Explicit
        && was_dependency(package.id())
    })
    .map(|package| package.id().clone())
    .collect();
  if purpose == Purpose::Install {
    marks.extend(
      resolution
        .kept
        .iter()
        .filter(|kept| roots.contains(&kept.id) && was_dependency(&kept.id))
        .map(|kept| kept.id.clone()),
    );
  }
  marks
}

/// Records `requested` as explicitly installed and `dependency` as a
/// dependency, where each is installed and recorded otherwise, and returns
/// what changed. The caller commits.
fn mark(
  state: &mut State,
  requested: &[PackageId],
  dependency: &[PackageId],
) -> Vec<InstalledSummary> {
  let marks = requested
    .iter()
    .map(|id| (id, InstallReason::Explicit))
    .chain(dependency.iter().map(|id| (id, InstallReason::Dependency)));
  let mut marked = Vec::new();
  for (id, reason) in marks {
    if let Some(record) = state.packages.get_mut(id)
      && record.reason != reason
    {
      record.reason = reason;
      marked.push(InstalledSummary::from(&*record));
    }
  }
  marked
}

/// A configured manager.
pub struct Session {
  layout: Layout,
  config: Config,
  target: Target,
  offline: bool,
  /// Where the latest Luthier release is described.
  releases_api: url::Url,
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
      releases_api: url::Url::parse(selfupdate::RELEASES_API).expect("the releases API is a URL"),
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
  /// A second source the user has not fetched yet does not stop the first
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
  /// first to claim an ID keeps it, so extras corrects a broader
  /// source rather than colliding with it.
  ///
  /// Built once and reused for the rest of the session. Snapshots only change
  /// during `refresh` and [`Session::refresh_due`], neither of which reads the
  /// index, so there is nothing a later call could see that the first did
  /// not — as long as a command refreshes before it reads.
  pub fn index(&self) -> Result<&RegistryIndex> {
    if let Some(index) = self.index.get() {
      return Ok(index);
    }
    let providers = self.config.providers(&self.layout, self.offline);
    let built = RegistryIndex::merge(providers.iter().map(|provider| provider.load_index()))?;
    Ok(self.index.get_or_init(|| built))
  }

  // -------------------------------------------------------------- refresh --

  /// Updates every registry (§31; the `update` command).
  pub async fn refresh(&self) -> Result<Vec<RefreshOutcome>> {
    self.require_locations(&[LocationKind::Cache])?;
    // Waits for a refresh another command is making on its own, rather than
    // writing the same snapshots alongside it.
    let _lock = fsutil::Lock::acquire(&self.layout.registries_lock_file())?;

    let mut outcomes = Vec::new();
    let mut first_error = None;
    // A source built into the binary has nothing to fetch, and counting it
    // would make a refresh that reached nothing look like a partial success.
    let providers = self.config.providers(&self.layout, self.offline);
    for provider in providers.iter().filter(|p| !p.is_built_in()) {
      match provider.refresh().await {
        Ok(outcome) => outcomes.push(outcome),
        // One source that cannot be reached does not cost a user the others.
        // A failed refresh leaves that source's previous snapshot alone, so
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

  /// Refreshes, before a command reads them, the registries that keep
  /// themselves fresh and are due: never fetched, or last asked more than
  /// [`AUTOMATIC_REFRESH_AFTER`] ago. Returns what was attempted.
  ///
  /// Never an error: a source that cannot be reached keeps the snapshot on
  /// disk, and its outcome says why, for the command to warn with. Nothing
  /// is attempted under `--offline`, nor while another Luthier is refreshing
  /// the same snapshots, which leaves them as fresh as this would.
  pub async fn refresh_due(&self) -> Vec<RefreshOutcome> {
    if self.offline || self.index.get().is_some() {
      return Vec::new();
    }
    // The registries' lock, not the state's: a command that only reads
    // packages must not stop an install in another terminal from starting.
    let _lock = match fsutil::Lock::try_acquire(&self.layout.registries_lock_file()) {
      Ok(Some(lock)) => lock,
      Ok(None) => return Vec::new(),
      Err(error) => return vec![RefreshOutcome::failed("registries", error.to_string())],
    };
    let mut outcomes = Vec::new();
    for provider in self.config.providers(&self.layout, self.offline) {
      match provider.refresh_if_due(AUTOMATIC_REFRESH_AFTER).await {
        Some(Ok(outcome)) => outcomes.push(outcome),
        Some(Err(error)) => {
          outcomes.push(RefreshOutcome::failed(provider.name(), error.to_string()))
        }
        None => {}
      }
    }
    outcomes
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
      files: installed
        .map(|p| {
          p.files
            .iter()
            .map(|f| f.path().display().to_string())
            .collect()
        })
        .unwrap_or_default(),
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

  /// The named packages as installed, in the order named, each once.
  ///
  /// What `luthier list <package>` prints the files of. A package that is
  /// not installed is an error rather than an empty answer, so a script
  /// asking where something is cannot mistake a typo for "nowhere".
  pub fn installed(&self, ids: &[PackageId]) -> Result<Vec<InstalledSummary>> {
    let state = crate::state::load(&self.layout)?;
    let mut seen = BTreeSet::new();
    ids
      .iter()
      .filter(|id| seen.insert(*id))
      .map(|id| {
        state
          .get(id)
          .map(InstalledSummary::from)
          .ok_or_else(|| Error::State(StateError::NotInstalled { id: id.clone() }))
      })
      .collect()
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
    let state = crate::state::load(&self.layout)?;
    self.plan_for(
      &Request::install(ids, force, required_versions),
      false,
      &state,
    )
  }

  /// What [`Session::upgrade`] would do with `ids`: the named packages, or
  /// with none named every explicitly installed one, and what it leaves
  /// where it is (`held_back`).
  pub fn plan_upgrade(&self, ids: &[PackageId]) -> Result<InstallPlan> {
    self.require_locations(&LocationKind::ALL)?;
    let state = crate::state::load(&self.layout)?;
    let upgrade = self.upgrade_roots(ids, &state)?;
    let request = Request::upgrade(&upgrade.roots, &upgrade.holding);
    let mut plan = match upgrade.resolution {
      Some(resolution) => self.plan_from(resolution, &request, BTreeSet::new(), false, &state)?,
      None => self.plan_for(&request, false, &state)?,
    };
    plan.held_back = upgrade.held_back;
    Ok(plan)
  }

  fn plan_for(&self, request: &Request<'_>, prune: bool, state: &State) -> Result<InstallPlan> {
    let Request {
      roots: ids,
      force,
      required_versions,
      purpose,
    } = *request;
    let index = self.index()?;
    // Nothing to install needs nothing looked for on disk.
    let detected = if ids.is_empty() {
      &NO_PACKAGES
    } else {
      self.externals_present(index)
    };
    let leaving = if prune {
      self.import_leaving(ids, required_versions, state)?
    } else {
      BTreeSet::new()
    };
    let resolution = resolver::resolve(
      index,
      &ResolveRequest {
        roots: ids,
        target: &self.target,
        state,
        detected_externals: detected,
        force,
        required_versions,
        leaving: &leaving,
        held: purpose.held(),
      },
    )?;
    self.plan_from(resolution, request, leaving, prune, state)
  }

  /// The plan `resolution` makes: what `request` resolved to, with
  /// `leaving` what an `import --prune` removes.
  fn plan_from(
    &self,
    mut resolution: Resolution<'_>,
    request: &Request<'_>,
    leaving: BTreeSet<PackageId>,
    prune: bool,
    state: &State,
  ) -> Result<InstallPlan> {
    let Request {
      roots: ids,
      force,
      purpose,
      ..
    } = *request;
    let index = self.index()?;
    purpose.record_reasons(&mut resolution, state);

    let unplayable = self.unplayable(&resolution, index, state);
    let sizes = ArtifactSizes::of(&resolution, &self.layout);
    let space = self.space_needed(&sizes);

    // The typed refusal is decided here, from the resolution this plan was
    // built out of, so a front end refuses the plan it showed the user rather
    // than relying on the installer to derive the same verdict a second time
    // from freshly-read state. Two derivations can disagree; the one the user
    // was shown is the one that must decide.
    let local_changes = self.local_changes(&resolution, state, force)?;
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
      marked_requested: requested_marks(&resolution, state, purpose, ids),
      kept: resolution.kept.iter().map(|kept| kept.id.clone()).collect(),
      prune: leaving.into_iter().collect(),
      prune_withheld: if prune { self.prune_withheld()? } else { None },
      marked_dependency: Vec::new(),
      roots: ids.to_vec(),
      holding: purpose.held().clone(),
      held_back: Outdated::default(),
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
          // Named by a source but carried by no configured registry: it
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
    self
      .install_for(&Request::install(ids, force, required_versions), progress)
      .await
  }

  /// Carries out an upgrade [`Session::plan_upgrade`] planned: the packages
  /// it resolved, and their dependencies, to their newest compatible
  /// releases.
  ///
  /// The plan's roots, not ones worked out again: a package installed in
  /// another terminal while the user read the plan is not upgraded by an
  /// answer to a question that never named it, and one removed meanwhile
  /// is not installed again (`InstallOutcome::no_longer_installed`). What
  /// the plan held back stays held. Unlike an install, it records every
  /// package as what it already was: a dependency upgraded by name is still
  /// a dependency, and `autoremove` still collects it once nothing needs it
  /// (§24).
  pub async fn upgrade(
    &self,
    plan: &InstallPlan,
    progress: &mut dyn Progress,
  ) -> Result<InstallOutcome> {
    self
      .install_for(&Request::upgrade(&plan.roots, &plan.holding), progress)
      .await
  }

  /// Records what the plan marks: `marked_requested` as explicitly
  /// installed, `marked_dependency` as a dependency. For a plan with
  /// nothing else to do; carrying out one that has does this as it goes.
  ///
  /// No confirmation is wanted for it, as apt asks none: it places nothing.
  /// Marking a package requested only keeps `autoremove` from removing what
  /// the user just asked for; marking one a dependency comes only from an
  /// `import --prune` file, which describes the installation.
  pub fn record_marks(&self, plan: &InstallPlan) -> Result<Vec<InstalledSummary>> {
    self.apply_marks(&plan.marked_requested, &plan.marked_dependency)
  }

  fn apply_marks(
    &self,
    requested: &[PackageId],
    dependency: &[PackageId],
  ) -> Result<Vec<InstalledSummary>> {
    if requested.is_empty() && dependency.is_empty() {
      return Ok(Vec::new());
    }
    let mut guard = StateGuard::acquire(&self.layout)?;
    let marked = mark(guard.state_mut(), requested, dependency);
    if !marked.is_empty() {
      guard.commit()?;
    }
    Ok(marked)
  }

  async fn install_for(
    &self,
    request: &Request<'_>,
    progress: &mut dyn Progress,
  ) -> Result<InstallOutcome> {
    self.require_locations(&LocationKind::ALL)?;
    let mut guard = StateGuard::acquire(&self.layout)?;
    self
      .install_locked(&mut guard, request, &BTreeSet::new(), progress)
      .await
  }

  /// Installs what `request` resolves to, under the lock the caller holds.
  /// `leaving` is what an `import --prune` is about to remove, which holds
  /// nothing ([`ResolveRequest::leaving`]).
  async fn install_locked(
    &self,
    guard: &mut StateGuard,
    request: &Request<'_>,
    leaving: &BTreeSet<PackageId>,
    progress: &mut dyn Progress,
  ) -> Result<InstallOutcome> {
    let Request {
      roots: ids,
      force,
      required_versions,
      purpose,
    } = *request;

    // An upgrade's roots come from a plan the user read, and a root another
    // command removed meanwhile is not brought back, as explicitly
    // installed, by an answer to a question about upgrading it. It is said
    // to be gone, or the command would claim to have upgraded it.
    let (ids, no_longer_installed): (Vec<PackageId>, Vec<PackageId>) = match purpose {
      Purpose::Install => (ids.to_vec(), Vec::new()),
      Purpose::Upgrade { .. } => ids
        .iter()
        .cloned()
        .partition(|id| guard.state().is_installed(id)),
    };
    let ids = ids.as_slice();

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
    let mut resolution = resolver::resolve(
      index,
      &ResolveRequest {
        roots: ids,
        target: &self.target,
        state: guard.state(),
        detected_externals: detected,
        force,
        required_versions,
        leaving,
        held: purpose.held(),
      },
    )?;
    purpose.record_reasons(&mut resolution, guard.state());

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
    let mut unsaved = false;

    // Before anything is fetched, and in one write: a dependency asked for
    // by name is one the user wants, whether or not a later download in
    // this command fails, and whether it resolved through a source or
    // was kept from the state.
    let requested = requested_marks(&resolution, guard.state(), purpose, ids);
    if !mark(guard.state_mut(), &requested, &[]).is_empty() {
      guard.commit()?;
    }

    for package in &resolution.order {
      if package.disposition == Disposition::Satisfied {
        let record = guard
          .state_mut()
          .packages
          .get_mut(package.id())
          .expect("satisfied means installed");
        // A record from before content was recorded learns it here, while
        // the manifest is at hand.
        let learned = record.content.is_empty() && !package.entry.manifest.content.is_empty();
        if learned {
          record.content = package.entry.manifest.content.clone();
        }
        let summary = InstalledSummary::from(&*record);
        // Written with the next package, or once after the last: a state
        // write per record would rewrite the whole file for each, and
        // losing this to a later failure only means learning it again.
        unsaved |= learned;
        skipped.push(summary);
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
          content: package.entry.manifest.content.clone(),
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
      // What went wrong with an archive names the package it was for: an
      // import of twenty that stops on one must say which.
      let named = |error: Error| match error {
        Error::Archive(error) => Error::PackageArchive {
          id: package.id().clone(),
          version: package.version.clone(),
          error: Box::new(error),
        },
        other => other,
      };
      let extract_root = transaction.workspace().join("extract");
      fsutil::ensure_dir(&extract_root)?;
      archive::unpack(
        &fetched.path,
        &artifact.archive,
        &published_name(&artifact.source.url),
        &extract_root,
        ExtractLimits::for_download(fetched.bytes),
      )
      .map_err(|e| named(e.into()))?;

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
      )
      .map_err(named)?;
      if let Some(content) = &content {
        items.push(
          install::plan_content(
            &self.layout,
            &extract_root,
            content,
            package.id(),
            guard.state(),
          )
          .map_err(named)?,
        );
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
        content: package.entry.manifest.content.clone(),
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

    if unsaved {
      guard.commit()?;
    }

    Ok(InstallOutcome {
      installed,
      skipped,
      warnings,
      no_longer_installed: no_longer_installed
        .iter()
        .map(ToString::to_string)
        .collect(),
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

    Ok(RemovalPlan {
      packages,
      stranded,
      kept: Vec::new(),
      ids: ids.to_vec(),
    })
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
    self.remove_locked(&mut guard, ids, force)
  }

  /// [`Session::remove`], under the lock the caller holds.
  fn remove_locked(
    &self,
    guard: &mut StateGuard,
    ids: &[PackageId],
    force: bool,
  ) -> Result<RemoveOutcome> {
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
      no_longer_installed: Vec::new(),
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

  /// What `outdated` reports (§26): the packages `upgrade` would bring to a
  /// newer release, and those it would leave where they are, with why.
  /// Changes nothing.
  pub fn outdated(&self) -> Result<Outdated> {
    let state = crate::state::load(&self.layout)?;
    // Whether an external is present changes no version, so none is
    // looked for: `outdated` reads the index and the state, not the disk.
    let scope = Scope::Everything {
      detected: &NO_PACKAGES,
    };
    Ok(self.survey(&state, scope)?.0.outdated)
  }

  /// Works out, once, what `outdated` says and what `upgrade` resolves, so
  /// the two cannot disagree.
  ///
  /// `upgrade` with nothing named resolves every explicit package. What no
  /// source can supply any more, what `upgrade` holds back because its own
  /// upgrade cannot be resolved, and what either needs, the resolver keeps
  /// where it is ([`resolver::frozen`]), so this reads the same list rather
  /// than predicting it. The update listed is the version that resolution
  /// lands on, so a requirement that holds a dependency back holds it back
  /// here too ([`Self::resolve_upgrade`]). An update is listed where an
  /// explicit package reaches it; one nothing explicit reaches is an
  /// orphan's, which only `upgrade <package>` brings.
  ///
  /// With [`Scope::Everything`] the resolution is returned too, for the
  /// plan to be built from rather than resolved again.
  fn survey(&self, state: &State, scope: Scope<'_>) -> Result<(Survey, Option<Resolution<'_>>)> {
    let index = self.index()?;
    // A source that could not be read may well carry what is missing.
    let complete = index.problems.is_empty();
    let mut survey = Survey::default();

    let explicit = state
      .packages
      .values()
      .filter(|package| package.reason == InstallReason::Explicit)
      .map(|package| &package.id);
    let reach = state.reachable_from(explicit.clone());
    survey.roots = explicit.cloned().collect();
    let (resolution, holding) = match scope {
      Scope::Everything { detected } => {
        self.resolve_upgrade(index, state, &survey.roots, detected)?
      }
      Scope::Named => (None, BTreeMap::new()),
    };
    let resolved: BTreeMap<&PackageId, &Version> = resolution
      .iter()
      .flat_map(|resolution| &resolution.order)
      .map(|package| (package.id(), &package.version))
      .collect();
    survey.holding = holding.keys().cloned().collect();
    let frozen = resolver::frozen(
      index,
      state,
      &self.target,
      &BTreeSet::new(),
      &[],
      &survey.holding,
    );

    for package in state.packages.values() {
      let newest = self.newest_release(index, package);
      // What keeps it where it is, if anything: why, who, and what the
      // resolver said. One a package needs is said to be held only where
      // there is something newer to be held from.
      let (reason, by, detail) = match frozen.get(&package.id) {
        None => (None, None, None),
        Some(resolver::Frozen::NotCarried) if complete => {
          (Some(UnavailableReason::NotCarried), None, None)
        }
        Some(resolver::Frozen::NotCarried) => {
          (Some(UnavailableReason::SourceUnreadable), None, None)
        }
        Some(resolver::Frozen::NoReleaseForTarget) => {
          (Some(UnavailableReason::NoReleaseForTarget), None, None)
        }
        Some(resolver::Frozen::HeldBack) => {
          let held = &holding[&package.id];
          // Missing while a source could not be read: that source may
          // well carry it, and `update` be all it takes.
          let reason = if !complete && held.not_found {
            UnavailableReason::SourceUnreadable
          } else {
            UnavailableReason::Unresolvable
          };
          (Some(reason), None, Some(held.detail.clone()))
        }
        Some(resolver::Frozen::NeededBy(by)) => (
          newest
            .is_some()
            .then_some(UnavailableReason::NeededByUnavailable),
          Some(by),
          None,
        ),
        Some(resolver::Frozen::HeldBy(by)) => (
          newest
            .is_some()
            .then_some(UnavailableReason::NeededByHeldBack),
          Some(by),
          None,
        ),
      };
      let held = reason.map(|reason| {
        let mut held = Unavailable::of(package, reason, by, newest.as_ref());
        held.detail = detail;
        held
      });
      if frozen.contains_key(&package.id) {
        survey.held.insert(package.id.clone());
      }
      if let Some(held) = held {
        survey.outdated.unavailable.push(held);
      } else if let Some(available) = &newest
        && !frozen.contains_key(&package.id)
      {
        if reach.contains(&package.id) {
          // §27: a pinned package resolves to its pin, and the release it
          // is held from is reported, not hidden. Otherwise the release
          // the resolution lands on; where what stays installed requires
          // an older one, `upgrade` leaves it too, and so does this.
          let lands_on = if package.is_pinned() {
            Some(available)
          } else {
            resolved
              .get(&package.id)
              .copied()
              .filter(|version| **version > package.version)
          };
          if let Some(version) = lands_on {
            survey
              .outdated
              .updates
              .push(AvailableUpdate::of(package, version));
          }
        } else {
          survey.outdated.unavailable.push(Unavailable::of(
            package,
            UnavailableReason::Orphaned,
            None,
            Some(available),
          ));
        }
      }
      if let Some(available) = newest {
        survey.newest.insert(package.id.clone(), available);
      }
    }
    Ok((survey, resolution))
  }

  /// The newest release of `package` built for this machine, where it is
  /// newer than the one installed.
  fn newest_release(&self, index: &RegistryIndex, package: &InstalledPackage) -> Option<Version> {
    let entry = index.packages.get(&package.id)?;
    // A pack has no artifacts; resolving it is what brings its members up
    // to date.
    if entry.manifest.kind == PackageKind::Pack {
      return None;
    }
    // The resolver's own choice, so this never names a release it would
    // pass over.
    resolver::newest_installable(&entry.manifest, &self.target)
      .map(|release| release.version.clone())
      .filter(|version| version > &package.version)
  }

  /// How upgrading every one of `roots` resolves, and the roots it holds
  /// back, with why. `None` when even holding every root back resolves
  /// nothing: what is installed cannot be resolved any more, and `upgrade`
  /// says why when it tries.
  ///
  /// A root whose own upgrade cannot be resolved — it needs a newer release
  /// of something kept where it is, say — must not stop every other package
  /// from moving, so it is held back, as apt keeps back a package: it stays
  /// at the version installed, and so does what it needs
  /// ([`ResolveRequest::held`]), so nothing else moves past what its
  /// installed release requires. Held back is each root that fails to
  /// resolve on its own, given what is held already, until the whole set
  /// resolves — a property of the root, not of where its ID sorts. Where
  /// every root resolves alone, they clash with one another, and a
  /// minimal set of them that will not resolve together, found in order
  /// of ID, is held back whole, each with the clash: which of them should move is the user's
  /// call, and `upgrade <package>` makes it. Then the next, so one clash
  /// holds back only the packages in it. What succeeds at once costs one
  /// resolution.
  fn resolve_upgrade<'a>(
    &self,
    index: &'a RegistryIndex,
    state: &State,
    roots: &[PackageId],
    detected: &BTreeSet<PackageId>,
  ) -> Result<(Option<Resolution<'a>>, BTreeMap<PackageId, HeldBack>)> {
    let resolve = |roots: &[PackageId], held: &BTreeSet<PackageId>| {
      resolver::resolve(
        index,
        &ResolveRequest {
          roots,
          target: &self.target,
          state,
          detected_externals: detected,
          force: false,
          required_versions: &NO_VERSIONS,
          leaving: &NO_PACKAGES,
          held,
        },
      )
    };
    // Whether `roots` resolve with `held` held: the error if not.
    let clashes = |roots: &[PackageId], held: &BTreeSet<PackageId>| match resolve(roots, held) {
      Ok(_) => Ok(None),
      Err(Error::Resolve(error)) => Ok(Some(error)),
      Err(other) => Err(other),
    };
    let mut holding: BTreeMap<PackageId, HeldBack> = BTreeMap::new();
    loop {
      let held: BTreeSet<PackageId> = holding.keys().cloned().collect();
      let failure = match resolve(roots, &held) {
        Ok(resolution) => return Ok((Some(resolution), holding)),
        Err(Error::Resolve(error)) => error,
        Err(other) => return Err(other),
      };
      let (base, moving): (Vec<PackageId>, Vec<PackageId>) =
        roots.iter().cloned().partition(|root| held.contains(root));
      // What is held back fails even held: what is installed cannot be
      // resolved any more, and no root still moving is to blame for it.
      if moving.is_empty() || (!base.is_empty() && clashes(&base, &held)?.is_some()) {
        return Ok((None, holding));
      }
      let mut failing = BTreeMap::new();
      for root in &moving {
        if let Some(error) = clashes(std::slice::from_ref(root), &held)? {
          failing.insert(root.clone(), HeldBack::of(&error));
        }
      }
      if failing.is_empty() {
        let with = |part: &[PackageId]| [&base[..], part].concat();
        // The shortest run of roots, in order of ID, that clashes — the
        // whole run does, as just seen — whose last one takes part in the
        // clash. Then each earlier one the clash does without is let go,
        // which leaves a minimal set: every root in it is needed for it.
        let mut found = None;
        for end in 1..moving.len() {
          if let Some(error) = clashes(&with(&moving[..end]), &held)? {
            found = Some((end - 1, error));
            break;
          }
        }
        let (last, mut clash) = found.unwrap_or((moving.len() - 1, failure));
        let mut part: Vec<PackageId> = Vec::new();
        for (i, root) in moving[..last].iter().enumerate() {
          let without: Vec<PackageId> = part.iter().chain(&moving[i + 1..=last]).cloned().collect();
          match clashes(&with(&without), &held)? {
            Some(error) => clash = error,
            None => part.push(root.clone()),
          }
        }
        part.push(moving[last].clone());
        for root in part {
          failing.insert(root, HeldBack::of(&clash));
        }
      }
      holding.extend(failing);
    }
  }

  /// What `upgrade` resolves, what it says it leaves, and what it holds
  /// back: the named packages, or with none named every explicit one.
  ///
  /// A named package has to be installed already. Upgrading something that
  /// is not there is an install, and saying so beats doing one unasked. A
  /// named package is never held back: one that cannot be upgraded is
  /// refused, with why.
  fn upgrade_roots(&self, ids: &[PackageId], state: &State) -> Result<UpgradeRoots<'_>> {
    if let Some(missing) = ids.iter().find(|id| !state.is_installed(id)) {
      return Err(Error::State(StateError::NotInstalled {
        id: missing.clone(),
      }));
    }
    let anything_explicit = state
      .packages
      .values()
      .any(|package| package.reason == InstallReason::Explicit);
    let scope = if !ids.is_empty() {
      Scope::Named
    } else if anything_explicit {
      Scope::Everything {
        detected: self.externals_present(self.index()?),
      }
    } else {
      // Nothing to resolve needs nothing looked for on disk.
      Scope::Everything {
        detected: &NO_PACKAGES,
      }
    };
    let (survey, resolution) = self.survey(state, scope)?;
    let named = |id: &str| ids.is_empty() || ids.iter().any(|named| named.as_str() == id);
    // §27: a pin is a decision to stay put, and is reported rather than
    // silently obeyed; so is what the resolver will keep where it is. An
    // orphan's update is only left behind when nothing is named, since
    // naming it is what upgrades it.
    let mut held = Outdated {
      updates: survey
        .outdated
        .updates
        .iter()
        .filter(|update| update.pinned.is_some() && named(&update.id))
        .cloned()
        .collect(),
      unavailable: survey
        .outdated
        .unavailable
        .iter()
        .filter(|package| {
          named(&package.id) && (ids.is_empty() || package.reason != UnavailableReason::Orphaned)
        })
        .cloned()
        .collect(),
    };
    // A named orphan that is pinned is held by its pin, which `updates`
    // does not list, since nothing explicit reaches it.
    for id in ids {
      let package = state.get(id).expect("checked above");
      if package.is_pinned()
        && !survey.held.contains(id)
        && !held.updates.iter().any(|update| update.id == id.as_str())
        && let Some(available) = survey.newest.get(id)
      {
        held.updates.push(AvailableUpdate::of(package, available));
      }
    }
    let roots = if ids.is_empty() {
      survey.roots
    } else {
      ids.to_vec()
    };
    Ok(UpgradeRoots {
      roots,
      held_back: held,
      holding: survey.holding,
      resolution,
    })
  }

  // ----------------------------------------------------------- autoremove --

  /// What `autoremove` removes: packages installed as dependencies that
  /// nothing explicitly installed needs any more (§24). Changes nothing.
  ///
  /// Except an engine that is the last thing able to play content that
  /// stays installed. Content does not depend on an engine — any one of
  /// several plays it — so a library never makes its player reachable, and
  /// the player of a kit could otherwise be collected the moment the pack
  /// that brought it went. Kept with whatever it needs in turn, and every
  /// engine that could play the content is kept, not one picked: choosing
  /// between players is the user's call, and `remove` makes it.
  ///
  /// The plan is a removal plan, with `kept` filled in. A front end shows
  /// it and, once confirmed, hands it to [`Session::autoremove`].
  pub fn plan_autoremove(&self) -> Result<RemovalPlan> {
    let state = crate::state::load(&self.layout)?;
    let (remove, kept) = self.autoremove_candidates(&state)?;
    let mut plan = self.plan_remove(&remove)?;
    plan.kept = kept;
    Ok(plan)
  }

  /// Removes what [`Session::plan_autoremove`] listed: that list, not one
  /// worked out again after the user answered, which could hold something
  /// nobody was shown.
  ///
  /// Refused, before anything is deleted, if one of them is no longer
  /// something `autoremove` would take: in between, another command may
  /// have asked for it by name, installed something that needs it, or
  /// installed content only it plays. Checking for dependents alone, as
  /// `remove` does, would delete the first and the last. One another
  /// command removed meanwhile is already gone, which is what was asked:
  /// it is skipped, and said to be (`no_longer_installed`).
  pub fn autoremove(&self, plan: &RemovalPlan) -> Result<RemoveOutcome> {
    self.require_locations(&[LocationKind::Libraries, LocationKind::Plugins])?;
    let mut guard = StateGuard::acquire(&self.layout)?;
    let (present, gone): (Vec<PackageId>, Vec<PackageId>) = plan
      .ids
      .iter()
      .cloned()
      .partition(|id| guard.state().is_installed(id));
    let (still, _) = self.autoremove_candidates(guard.state())?;
    if let Some(id) = present.iter().find(|id| !still.contains(id)) {
      return Err(Error::State(StateError::NoLongerUnneeded {
        id: id.clone(),
      }));
    }
    let mut outcome = self.remove_locked(&mut guard, &present, false)?;
    outcome.no_longer_installed = gone.iter().map(ToString::to_string).collect();
    Ok(outcome)
  }

  fn autoremove_candidates(&self, state: &State) -> Result<(Vec<PackageId>, Vec<KeptEngine>)> {
    let mut remove: BTreeSet<PackageId> = state.orphans().into_iter().collect();
    if remove.is_empty() {
      return Ok((Vec::new(), Vec::new()));
    }
    let index = self.index()?;
    let detected = self.externals_present(index);

    let mut kept = Vec::new();
    // Each pass takes at least one package out of `remove`, so this ends.
    loop {
      let stranded = engine::stranded_by_removal(index, state, &remove, detected);
      let mut keeping: BTreeSet<&PackageId> = BTreeSet::new();
      for found in &stranded {
        for engine in found.engines.iter().filter(|e| remove.contains(*e)) {
          keeping.insert(engine);
          kept.push(KeptEngine {
            id: engine.to_string(),
            plays: found.content.label().to_owned(),
            for_package: found.package.to_string(),
          });
        }
      }
      if keeping.is_empty() {
        break;
      }
      let needed: BTreeSet<PackageId> =
        state.reachable_from(keeping).into_iter().cloned().collect();
      remove.retain(|id| !needed.contains(id));
    }

    Ok((remove.into_iter().collect(), kept))
  }

  // -------------------------------------------------------------- sources --

  /// The sources this build reads, highest priority first.
  ///
  /// A fixed list: extras, then the Open Audio Stack registry it
  /// corrects. Order is the whole mechanism behind that correction, so it is
  /// what this reports: the first source to claim an ID keeps it.
  ///
  /// The built-in list rather than this session's, so that a session started
  /// with `--registry-path` still says where manifests normally come from.
  pub fn sources(&self) -> Vec<SourceSummary> {
    Self::summarise(&Config::default())
  }

  fn summarise(config: &Config) -> Vec<SourceSummary> {
    config
      .registries
      .iter()
      .enumerate()
      .map(|(i, registry)| SourceSummary {
        priority: i + 1,
        name: registry.name.clone(),
        kind: match registry.source {
          RegistrySource::BuiltIn => "built-in",
          RegistrySource::Path { .. } => "path",
          RegistrySource::Snapshot { .. } => "snapshot",
          RegistrySource::Oas { .. } => "oas",
        }
        .into(),
        location: match &registry.source {
          // Where it is: in this version of Luthier.
          RegistrySource::BuiltIn => format!("luthier {}", crate::selfupdate::this_version()),
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
  /// invented version is not a harmless note: it makes `outdated` report a
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

  /// Captures what is installed as a portable file (§51).
  ///
  /// `pinned` records the exact version of every package, including ones
  /// that arrived as dependencies, so the file reproduces an installation
  /// rather than merely listing it. `false` produces the `--loose` shape.
  pub fn export(&self, pinned: bool) -> Result<EnvFile> {
    let state = crate::state::load(&self.layout)?;
    Ok(EnvFile::from_state(&state, pinned))
  }

  /// Installs everything an exported file describes, then reapplies its
  /// pins.
  ///
  /// Only `explicit` entries become roots; dependency versions are supplied
  /// to the resolver as requirements so the same graph comes back, and are
  /// recorded as dependencies, eligible for `autoremove` — with `prune`,
  /// even one installed by name here before ([`Session::plan_import`]).
  ///
  /// With `prune`, the import converges rather than adds: once what the file
  /// names is installed, every package it neither names nor needs is
  /// removed (`InstallPlan::prune`). That is what makes the file a
  /// description of the installation rather than a lower bound on it, and
  /// what a declarative front end — the Home Manager module — relies on.
  /// A file naming nothing is refused without `prune`, since it would do
  /// nothing; with it, it removes everything.
  ///
  /// `plan` is what [`Session::plan_import`] showed the user, and only what
  /// its `prune` lists is removed, where it is still unneeded: a package
  /// installed in another terminal since then is not taken unseen, and one
  /// something installed since needs is kept, with what it needs. The
  /// install and the removal happen under one lock, so nothing changes in
  /// between.
  pub async fn import(
    &self,
    file: &EnvFile,
    prune: bool,
    plan: &InstallPlan,
    progress: &mut dyn Progress,
  ) -> Result<ImportOutcome> {
    let roots = file.roots();
    if roots.is_empty() && !prune {
      return Err(Error::InvalidArgument(
        "the file lists no explicitly installed packages".into(),
      ));
    }
    let required = file.required_versions();
    // A file naming nothing installs nothing, and needs only the locations
    // a removal does.
    if roots.is_empty() {
      self.require_locations(&[LocationKind::Libraries, LocationKind::Plugins])?;
    } else {
      self.require_locations(&LocationKind::ALL)?;
    }
    let prune_withheld = if prune { self.prune_withheld()? } else { None };

    let (outcome, pruned, prune_kept) = {
      let mut guard = StateGuard::acquire(&self.layout)?;
      // From the state under the lock, so what an `import --prune` frees
      // is worked out from the state it then changes. Only what is then
      // removed holds nothing while the install resolves: a package it
      // keeps, shown or not, stays, and so must what it needs.
      let removing: BTreeSet<PackageId> = if prune {
        let leaving = self.import_leaving(&roots, &required, guard.state())?;
        Self::shown_and_unneeded(&plan.prune, &leaving, guard.state())
      } else {
        BTreeSet::new()
      };
      let outcome = if roots.is_empty() {
        InstallOutcome::default()
      } else {
        self
          .install_locked(
            &mut guard,
            &Request::install(&roots, false, &required),
            &removing,
            progress,
          )
          .await?
      };
      // After the install, not before: a failed install then leaves what
      // was there in place instead of an environment with things removed
      // and nothing added. Asked again of the state the install left, in
      // case a release it kept at the version installed still needs one.
      let removing = Self::still_unneeded(removing, guard.state());
      let pruned = if prune_withheld.is_none() && !removing.is_empty() {
        Some(self.remove_locked(&mut guard, &removing, false)?)
      } else {
        None
      };
      // Shown as removed and still here: a package that stays needs it.
      // Said, so an unattended import does not read as having converged
      // on the file.
      let prune_kept: Vec<String> = if prune && prune_withheld.is_none() {
        plan
          .prune
          .iter()
          .filter(|id| guard.state().is_installed(id))
          .map(ToString::to_string)
          .collect()
      } else {
        Vec::new()
      };
      (outcome, pruned, prune_kept)
    };

    // Pins are reapplied afterwards: a pin is state about the user's
    // intent, and applying it before the package exists would be writing
    // state for something not installed.
    let (reapplied_pins, skipped_pins) = self.reapply_pins(file.pins())?;
    let state = crate::state::load(&self.layout)?;
    let (requested, dependency) = Self::import_marks(file, prune, &state);
    self.apply_marks(&requested, &dependency)?;

    Ok(ImportOutcome {
      installed: outcome,
      reapplied_pins,
      skipped_pins,
      pruned,
      prune_withheld,
      prune_kept,
    })
  }

  /// What of `shown`, the prune the user confirmed, an import removes
  /// before it installs: what `leaving` still holds, less what a package
  /// in `leaving` the plan did not show needs. That one was installed
  /// since the plan, and stays, so what it needs stays too. A package the
  /// resolution reaches is not asked: its new release decides what it
  /// needs, and `leaving` holds nothing it does.
  fn shown_and_unneeded(
    shown: &[PackageId],
    leaving: &BTreeSet<PackageId>,
    state: &State,
  ) -> BTreeSet<PackageId> {
    let unshown = leaving.iter().filter(|id| !shown.contains(id));
    let needed: BTreeSet<PackageId> = state.reachable_from(unshown).into_iter().cloned().collect();
    shown
      .iter()
      .filter(|id| leaving.contains(*id) && !needed.contains(*id))
      .cloned()
      .collect()
  }

  /// What of `removing` nothing outside it needs, asked again of the state
  /// an install left: a release it kept at the version installed may still
  /// need one. Removing that would be refused as still required, after the
  /// install had already gone ahead, and the Home Manager module runs this
  /// unattended.
  fn still_unneeded(mut removing: BTreeSet<PackageId>, state: &State) -> Vec<PackageId> {
    removing.retain(|id| state.is_installed(id));
    loop {
      let needed: Vec<PackageId> = removing
        .iter()
        .filter(|id| {
          state
            .dependents_of(id)
            .iter()
            .any(|dependent| !removing.contains(dependent))
        })
        .cloned()
        .collect();
      if needed.is_empty() {
        return removing.into_iter().collect();
      }
      for id in needed {
        removing.remove(&id);
      }
    }
  }

  /// What an import of `file` marks among the packages already installed:
  /// those it asks for by name, as requested, and with `prune` those it
  /// lists as dependencies, as dependencies.
  ///
  /// Without `prune` an import adds to what is there, and demoting
  /// something the user asked for by name would hand it to the next
  /// `autoremove`; with it the file is the description of the
  /// installation.
  fn import_marks(file: &EnvFile, prune: bool, state: &State) -> (Vec<PackageId>, Vec<PackageId>) {
    let mut requested = Vec::new();
    let mut dependency = Vec::new();
    for entry in &file.packages {
      let Some(record) = state.get(&entry.id) else {
        continue;
      };
      match entry.reason {
        InstallReason::Explicit if record.reason == InstallReason::Dependency => {
          requested.push(entry.id.clone());
        }
        InstallReason::Dependency if prune && record.reason == InstallReason::Explicit => {
          dependency.push(entry.id.clone());
        }
        _ => {}
      }
    }
    (requested, dependency)
  }

  /// Why `import --prune` must remove nothing now, if it must: a source
  /// could not be read. A package that source carries would be missing
  /// from the resolution, which is how a package the file does not need
  /// looks, and the Home Manager module runs `import --prune` unattended.
  fn prune_withheld(&self) -> Result<Option<String>> {
    Ok(self.index()?.problems.first().cloned())
  }

  /// What an `import --prune` of `file` removes: the installed packages the
  /// resolution of its roots neither reaches nor keeps from the state, nor
  /// reaches through what it keeps. Empty while [`Self::prune_withheld`].
  ///
  /// The same set is what the resolution frees from holding anything at
  /// the version installed ([`ResolveRequest::leaving`](resolver::ResolveRequest)),
  /// or a file that drops a package no source carries, and moves what it
  /// needed, could never be imported. The two depend on each other — a
  /// lifted hold can change what a root's new release reaches — so this
  /// starts from everything but what the file names, and resolves until
  /// the set stops changing: what a resolution reaches comes back, holds
  /// and all. Not from what the file reaches through the state, which is
  /// the graph before the upgrade: a release that dropped a gone package
  /// would be held by it and refuse.
  fn import_leaving(
    &self,
    roots: &[PackageId],
    required: &BTreeMap<PackageId, Version>,
    state: &State,
  ) -> Result<BTreeSet<PackageId>> {
    if self.prune_withheld()?.is_some() {
      return Ok(BTreeSet::new());
    }
    let index = self.index()?;
    let detected = self.externals_present(index);
    let mut leaving: BTreeSet<PackageId> = state
      .packages
      .keys()
      .filter(|id| !roots.contains(id))
      .cloned()
      .collect();
    // Bounded: each pass that changes the set is a different resolution,
    // and there are only so many packages to free or hold.
    for _ in 0..=state.packages.len() {
      let unreached: BTreeSet<PackageId> = if roots.is_empty() {
        state.packages.keys().cloned().collect()
      } else {
        let resolution = resolver::resolve(
          index,
          &ResolveRequest {
            roots,
            target: &self.target,
            state,
            detected_externals: detected,
            force: false,
            required_versions: required,
            leaving: &leaving,
            held: &NO_PACKAGES,
          },
        )?;
        let mut reached: BTreeSet<&PackageId> = resolution
          .order
          .iter()
          .map(|package| package.id())
          .collect();
        reached.extend(state.reachable_from(resolution.kept.iter().map(|kept| &kept.id)));
        state
          .packages
          .keys()
          .filter(|id| !reached.contains(id))
          .cloned()
          .collect()
      };
      if unreached == leaving {
        break;
      }
      leaving = unreached;
    }
    Ok(leaving)
  }

  /// Reports what importing `file` would do, without changing anything:
  /// what it installs, what it marks, and with `prune` what it removes.
  pub fn plan_import(&self, file: &EnvFile, prune: bool) -> Result<InstallPlan> {
    let roots = file.roots();
    if roots.is_empty() && !prune {
      return Err(Error::InvalidArgument(
        "the file lists no explicitly installed packages".into(),
      ));
    }
    let state = crate::state::load(&self.layout)?;
    // A file naming nothing installs nothing, and needs only the locations
    // `remove` does, which it checks itself.
    if !roots.is_empty() {
      self.require_locations(&LocationKind::ALL)?;
    }
    let required = file.required_versions();
    let mut plan = self.plan_for(&Request::install(&roots, false, &required), prune, &state)?;
    let (requested, dependency) = Self::import_marks(file, prune, &state);
    plan.marked_requested = requested;
    plan.marked_dependency = dependency;
    Ok(plan)
  }

  // ---------------------------------------------------------- self-update --

  /// Asks `url` for the latest release instead of GitHub; the suite's
  /// `file://` one, so no test depends on the network.
  pub fn releases_api(mut self, url: &str) -> Result<Self> {
    self.releases_api = url::Url::parse(url)
      .map_err(|e| Error::InvalidArgument(format!("--releases-api {url}: {e}")))?;
    Ok(self)
  }

  /// The latest release. Read fresh each time and in one request: it is the
  /// answer to "is there a newer Luthier", so a cached one would be no
  /// answer.
  pub async fn latest_release(&self) -> Result<selfupdate::Release> {
    let body = Downloader::new(self.layout.self_update_dir())
      .offline(self.offline)
      .get(&self.releases_api, selfupdate::RELEASE_DESCRIPTION_LIMIT)
      .await?;
    selfupdate::parse_release(&body)
  }

  /// A newer release than this one, if there is one and the answer comes
  /// quickly; for the line after `update` and `outdated`.
  ///
  /// Never an error, and never a wait: the command has done its work by
  /// then, so no answer from GitHub within [`NOTICE_TIMEOUT`], or none at
  /// all under `--offline`, leaves the line out. Nor is one given for a
  /// Luthier in the Nix store, whose version whatever installs it there
  /// decides, and which would otherwise be told on every rebuild; or under
  /// `--root` for a binary outside the root, which it could not update.
  pub async fn newer_luthier(&self) -> Option<selfupdate::NewerLuthier> {
    if self.offline {
      return None;
    }
    let binary = selfupdate::running_binary().ok()?;
    let installation = selfupdate::Installation::of(&binary);
    if installation == selfupdate::Installation::Nix
      || self.outside_root(&binary, &installation).is_some()
    {
      return None;
    }
    let release = match tokio::time::timeout(NOTICE_TIMEOUT, self.latest_release()).await {
      Ok(Ok(release)) => release,
      Ok(Err(error)) => {
        tracing::debug!("could not check for a newer Luthier: {error}");
        return None;
      }
      Err(_) => {
        tracing::debug!("no answer about a newer Luthier within {NOTICE_TIMEOUT:?}");
        return None;
      }
    };
    let current = selfupdate::this_version();
    (release.version > current).then(|| selfupdate::NewerLuthier {
      current,
      latest: release.version,
      updates_itself: installation.updates_itself(),
    })
  }

  /// What updating the running binary to `release` would do, without doing
  /// it.
  ///
  /// Under `--root`, which confines everything Luthier writes, only a binary
  /// inside the root is updated: a package install writes system
  /// directories, and anything else is outside it.
  pub fn plan_self_update(
    &self,
    release: &selfupdate::Release,
  ) -> Result<selfupdate::SelfUpdatePlan> {
    let binary = selfupdate::running_binary()?;
    let installation = selfupdate::Installation::of(&binary);
    let current = selfupdate::this_version();
    // Already the latest is an answer wherever the binary is.
    if release.version > current
      && let Some(root) = self.outside_root(&binary, &installation)
    {
      return Err(Error::SelfUpdate(
        crate::error::SelfUpdateError::OutsideRoot { binary, root },
      ));
    }
    selfupdate::plan(
      release,
      &current,
      installation,
      &selfupdate::Host::this_machine(),
    )
  }

  /// The `--root` directory, when `binary` is outside it: only a standalone
  /// binary inside the root may be replaced, because a package install
  /// writes system directories.
  fn outside_root(
    &self,
    binary: &Path,
    installation: &selfupdate::Installation,
  ) -> Option<PathBuf> {
    let root = self.layout.root()?;
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let inside = matches!(installation, selfupdate::Installation::Standalone { .. })
      && binary.starts_with(&root);
    (!inside).then_some(root)
  }

  /// Carries out a [`Session::plan_self_update`] plan. The download is
  /// checked against the SHA-256 GitHub publishes for it, kept apart from
  /// the artifact cache, which holds only what packages name, and removed
  /// afterwards whether the update went through or not.
  pub async fn self_update(
    &self,
    plan: &selfupdate::SelfUpdatePlan,
    progress: &mut dyn Progress,
  ) -> Result<()> {
    self.require_locations(&[LocationKind::Cache])?;
    // Under the state lock, like everything else that writes: a second
    // `upgrade --self`, or the cache being moved by `location`, would
    // otherwise pull the download out from under this one. The lock alone:
    // a state file this build cannot read must not stop the update that
    // would read it.
    let _lock = StateLock::acquire(&self.layout)?;
    let dir = self.layout.self_update_dir();
    let downloader = Downloader::new(&dir).offline(self.offline);
    let applied = selfupdate::apply(plan, &downloader, &dir, progress).await;
    let removed = fsutil::remove_any(&dir);
    applied.and(removed)
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
/// Apart from [`Session`] because a session's layout already has the
/// locations applied, while changing a location needs the layout from before
/// — a reset has to know what the default was.
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
  pub fn search_path(&self) -> Result<crate::layout::SearchPath> {
    let config = Config::load(&self.home)?;
    Ok(crate::layout::search_path(
      &self.home,
      &self.located(&config),
    ))
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
  /// Why `--prune` removed nothing this time: a source could not be read.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub prune_withheld: Option<String>,
  /// What the plan said `--prune` would remove and it kept: a package that
  /// stays still needs it — one installed after the plan was made, or one
  /// the install kept at the version installed.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub prune_kept: Vec<String>,
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
  /// Where Luthier put it: one path per file or bundle it installed. Empty
  /// when it is not installed. Sample libraries live in a hidden directory,
  /// and this is how a user finds the folder to open in a player.
  pub files: Vec<String>,
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
  /// Packages installed as dependencies and now asked for by name: nothing
  /// is placed for them, and they are recorded as explicitly installed, so
  /// `autoremove` keeps them.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub marked_requested: Vec<PackageId>,
  /// Packages installed by name and now, by an `import --prune` file,
  /// recorded as dependencies.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub marked_dependency: Vec<PackageId>,
  /// Installed packages it leaves exactly as they are, resolved from the
  /// state because no source can supply them, or because one of those
  /// needs them at the version installed.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub kept: Vec<PackageId>,
  /// For `import --prune`, the installed packages it removes once the file
  /// is installed: those the file neither names nor needs.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub prune: Vec<PackageId>,
  /// Why `import --prune` removes nothing this time: a source could not be
  /// read. The install still goes ahead.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub prune_withheld: Option<String>,
  /// What the plan resolved, which is what carrying it out resolves again:
  /// for an upgrade with nothing named, the explicit packages as they were
  /// when the plan was made, not as they are once the user has answered.
  #[serde(skip)]
  pub roots: Vec<PackageId>,
  /// For an upgrade with nothing named, the explicit packages it holds
  /// back at the version installed, with what they need, since their own
  /// upgrade cannot be resolved; carrying the plan out holds the same ones.
  #[serde(skip)]
  pub holding: BTreeSet<PackageId>,
  /// For an upgrade, what it leaves where it is: updates a pin holds back,
  /// and packages nothing can upgrade any more.
  #[serde(skip_serializing_if = "Outdated::is_empty")]
  pub held_back: Outdated,
}

/// One source, and where it sits in the precedence order.
#[derive(Debug, Clone, Serialize)]
pub struct SourceSummary {
  /// 1 is consulted first and wins any ID two sources both carry.
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
  /// Packages an upgrade plan named that another command removed before
  /// the user answered, and that were therefore not upgraded.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub no_longer_installed: Vec<String>,
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
  /// For `autoremove`, the orphans it keeps because content still
  /// installed needs them.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub kept: Vec<KeptEngine>,
  /// The packages this plan removes, as [`Session::remove`] takes them.
  #[serde(skip)]
  pub ids: Vec<PackageId>,
}

/// An orphan `autoremove` keeps: the last engine for content still installed.
#[derive(Debug, Clone, Serialize)]
pub struct KeptEngine {
  pub id: String,
  /// The content it plays, as `sfz`.
  pub plays: String,
  /// The installed package holding that content.
  pub for_package: String,
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
  /// Packages `autoremove` listed that another command removed before the
  /// user answered: nothing was left to remove.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub no_longer_installed: Vec<String>,
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

/// What [`Session::outdated`] found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Outdated {
  /// Packages `upgrade` brings to a newer release, pinned ones included.
  pub updates: Vec<AvailableUpdate>,
  /// Packages that stay at the version installed, and why.
  pub unavailable: Vec<Unavailable>,
}

impl Outdated {
  pub fn is_empty(&self) -> bool {
    self.updates.is_empty() && self.unavailable.is_empty()
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableUpdate {
  pub id: String,
  pub name: String,
  pub installed: String,
  pub available: String,
  pub pinned: Option<String>,
}

impl AvailableUpdate {
  fn of(package: &InstalledPackage, available: &Version) -> Self {
    Self {
      id: package.id.to_string(),
      name: package.name.clone(),
      installed: package.version.to_string(),
      available: available.to_string(),
      // §27: a pin is a decision to stay put, and is reported rather
      // than silently overridden.
      pinned: package.pin.as_ref().map(ToString::to_string),
    }
  }
}

/// An installed package `upgrade` leaves at the version installed. It
/// keeps working; it is reported so that it does not fall silently behind.
#[derive(Debug, Clone, Serialize)]
pub struct Unavailable {
  pub id: String,
  pub name: String,
  pub installed: String,
  /// A newer release, when there is one it is held back from.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub available: Option<String>,
  pub reason: UnavailableReason,
  /// For [`UnavailableReason::NeededByUnavailable`] and
  /// [`UnavailableReason::NeededByHeldBack`], the package that needs it.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub because_of: Option<String>,
  /// For [`UnavailableReason::Unresolvable`], what the resolver said.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub detail: Option<String>,
}

impl Unavailable {
  fn of(
    package: &InstalledPackage,
    reason: UnavailableReason,
    because_of: Option<&PackageId>,
    available: Option<&Version>,
  ) -> Self {
    Self {
      id: package.id.to_string(),
      name: package.name.clone(),
      installed: package.version.to_string(),
      available: available.map(ToString::to_string),
      reason,
      because_of: because_of.map(ToString::to_string),
      detail: None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnavailableReason {
  /// Neither source carries the package any more.
  NotCarried,
  /// It is missing from the sources that could be read, and one could not
  /// be: that one may well still carry it.
  SourceUnreadable,
  /// A source carries it, with no release built for this machine.
  NoReleaseForTarget,
  /// A package nothing can upgrade needs it, so it is kept at the version
  /// installed rather than moved past what that package supports.
  NeededByUnavailable,
  /// Explicitly installed, and upgrading it cannot be resolved with what
  /// else is installed, so `upgrade` leaves it, and what it needs, where
  /// they are and upgrades the rest. `detail` says why.
  Unresolvable,
  /// A package held back as [`UnavailableReason::Unresolvable`] needs it,
  /// so it stays at the version installed with that package.
  NeededByHeldBack,
  /// Installed as a dependency, and nothing explicitly installed needs it
  /// now, so upgrading everything never reaches it. `autoremove` removes
  /// it; `upgrade <package>` upgrades it.
  Orphaned,
}

/// What [`Session::survey`] resolves.
#[derive(Clone, Copy)]
enum Scope<'d> {
  /// Only what is named, as `upgrade <package>` resolves: nothing else is
  /// resolved, and the updates listed are only those a pin holds back.
  Named,
  /// Every explicit package, as `upgrade` with nothing named resolves,
  /// with `detected` the externals present.
  Everything { detected: &'d BTreeSet<PackageId> },
}

/// What [`Session::upgrade_roots`] works out for a plan.
struct UpgradeRoots<'a> {
  roots: Vec<PackageId>,
  held_back: Outdated,
  holding: BTreeSet<PackageId>,
  /// How those roots resolve, with nothing named: the plan is built from
  /// it rather than resolving the same request again.
  resolution: Option<Resolution<'a>>,
}

/// Why `upgrade` holds a root back ([`Session::resolve_upgrade`]).
struct HeldBack {
  /// What the resolver said.
  detail: String,
  /// Something it needs was found in no source.
  not_found: bool,
}

impl HeldBack {
  fn of(error: &ResolveError) -> Self {
    Self {
      detail: error.to_string(),
      not_found: matches!(error, ResolveError::NotFound(_)),
    }
  }
}

/// What [`Session::survey`] worked out.
#[derive(Default)]
struct Survey {
  outdated: Outdated,
  /// The explicit packages `upgrade` with nothing named resolves.
  roots: Vec<PackageId>,
  /// The explicit packages it holds back at the version installed
  /// ([`Session::resolve_upgrade`]).
  holding: BTreeSet<PackageId>,
  /// The newest release built for this machine, where it is newer.
  newest: BTreeMap<PackageId, Version>,
  /// What the resolver keeps where it is.
  held: BTreeSet<PackageId>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::RegistryConfig;
  use luthier_manifest::Sha256Hash;

  /// A session over `sources`, with `gone` installed and carried by none.
  fn session_with(dir: &Path, sources: Vec<RegistryConfig>) -> Session {
    let layout = Layout::rooted_at(dir);
    record(&layout, "gone", InstallReason::Explicit);
    let config = Config {
      registries: sources,
      ..Config::default()
    };
    Session::new(layout, config).unwrap()
  }

  /// Records `id` as installed, with no files, as another command would.
  fn record(layout: &Layout, id: &str, reason: InstallReason) {
    record_needing(layout, id, reason, &[]);
  }

  /// [`record`], needing `dependencies`.
  fn record_needing(layout: &Layout, id: &str, reason: InstallReason, dependencies: &[&str]) {
    let mut guard = StateGuard::acquire(layout).unwrap();
    let id = PackageId::new(id).unwrap();
    guard.state_mut().packages.insert(
      id.clone(),
      InstalledPackage {
        name: id.to_string(),
        id,
        version: Version::new(1, 0, 0),
        registry: "second".into(),
        manifest_digest: Sha256Hash::from_bytes([0; 32]),
        reason,
        pin: None,
        installed_at: Timestamp::UNIX_EPOCH,
        formats: vec![Format::Clap],
        content: vec![],
        artifacts: vec![],
        dependencies: dependencies
          .iter()
          .map(|d| PackageId::new(*d).unwrap())
          .collect(),
        files: vec![],
      },
    );
    guard.commit().unwrap();
  }

  fn installed_ids(session: &Session) -> Vec<String> {
    crate::state::load(session.layout())
      .unwrap()
      .packages
      .keys()
      .map(ToString::to_string)
      .collect()
  }

  /// A session over one readable source, empty or holding `manifests`.
  fn session_reading(dir: &Path, manifests: &[(&str, &str)]) -> Session {
    let readable = dir.join("readable");
    std::fs::create_dir_all(&readable).unwrap();
    for (name, text) in manifests {
      std::fs::write(readable.join(name), text).unwrap();
    }
    session_with(
      dir,
      vec![RegistryConfig::new(
        "first",
        RegistrySource::Path { path: readable },
      )],
    )
  }

  fn empty_env_file() -> EnvFile {
    EnvFile::from_toml(
      "[meta]\nschema = 1\nluthier = \"0.4.0\"\nexported = \"2026-09-27T00:00:00Z\"\n\
       pinned = false\n",
      "empty.toml",
    )
    .unwrap()
  }

  #[test]
  fn autoremove_refuses_what_was_asked_for_after_it_was_listed() {
    // `dep` is an orphan when the plan is made; by the time the user
    // answers, `install dep` in another terminal has made it explicit.
    // Nothing it has no dependents for would stop `remove`.
    let dir = tempfile::tempdir().unwrap();
    let session = session_reading(dir.path(), &[]);
    record(session.layout(), "dep", InstallReason::Dependency);
    let plan = session.plan_autoremove().unwrap();
    assert_eq!(plan.ids, [PackageId::new("dep").unwrap()]);

    record(session.layout(), "dep", InstallReason::Explicit);
    let refused = session.autoremove(&plan).unwrap_err();
    assert!(
      matches!(
        refused,
        Error::State(StateError::NoLongerUnneeded { ref id }) if id.as_str() == "dep"
      ),
      "{refused:?}"
    );
    assert_eq!(installed_ids(&session), ["dep", "gone"]);

    // Still unneeded, it goes.
    record(session.layout(), "dep", InstallReason::Dependency);
    let plan = session.plan_autoremove().unwrap();
    session.autoremove(&plan).unwrap();
    assert_eq!(installed_ids(&session), ["gone"]);
  }

  #[test]
  fn autoremove_skips_what_another_command_removed_after_it_was_listed() {
    // Gone already is what was asked: the rest still goes, and that one
    // is said to have gone meanwhile.
    let dir = tempfile::tempdir().unwrap();
    let session = session_reading(dir.path(), &[]);
    record(session.layout(), "a", InstallReason::Dependency);
    record(session.layout(), "b", InstallReason::Dependency);
    let plan = session.plan_autoremove().unwrap();
    assert_eq!(plan.ids.len(), 2);

    session
      .remove(&[PackageId::new("a").unwrap()], false)
      .unwrap();
    let outcome = session.autoremove(&plan).unwrap();
    let removed: Vec<&str> = outcome.removed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(removed, ["b"]);
    assert_eq!(outcome.no_longer_installed, ["a"]);
    assert_eq!(installed_ids(&session), ["gone"]);
  }

  #[tokio::test]
  async fn import_prune_removes_only_what_its_plan_showed() {
    // The plan removes `gone`. A package installed in another terminal
    // before the user answered is not in the file either, and was never
    // shown: it stays.
    let dir = tempfile::tempdir().unwrap();
    let session = session_reading(dir.path(), &[]);
    let file = empty_env_file();
    let plan = session.plan_import(&file, true).unwrap();
    assert_eq!(plan.prune, [PackageId::new("gone").unwrap()]);

    record(session.layout(), "late", InstallReason::Explicit);
    let outcome = session
      .import(&file, true, &plan, &mut crate::download::NoProgress)
      .await
      .unwrap();
    let pruned: Vec<String> = outcome
      .pruned
      .unwrap()
      .removed
      .iter()
      .map(|p| p.id.clone())
      .collect();
    assert_eq!(pruned, ["gone"]);
    assert_eq!(installed_ids(&session), ["late"]);
  }

  #[tokio::test]
  async fn import_prune_keeps_what_was_shown_once_something_needs_it_again() {
    // `late`, installed after the plan, needs `gone`. Removing `gone` would
    // be refused as still required after the install had gone ahead, and
    // the Home Manager module runs this unattended: it is kept instead.
    let dir = tempfile::tempdir().unwrap();
    let session = session_reading(dir.path(), &[]);
    let file = empty_env_file();
    let plan = session.plan_import(&file, true).unwrap();
    assert_eq!(plan.prune, [PackageId::new("gone").unwrap()]);

    record_needing(session.layout(), "late", InstallReason::Explicit, &["gone"]);
    let outcome = session
      .import(&file, true, &plan, &mut crate::download::NoProgress)
      .await
      .unwrap();
    assert!(outcome.pruned.is_none(), "{:?}", outcome.pruned);
    assert_eq!(outcome.prune_kept, ["gone"]);
    assert_eq!(installed_ids(&session), ["gone", "late"]);
  }

  #[tokio::test]
  async fn upgrade_does_not_install_a_root_removed_after_the_plan() {
    // `fresh` was installed when the plan was made, and removed before the
    // user answered. Upgrading it must not put it back: nothing is fetched,
    // and the artifact here could not be.
    let manifest = format!(
      "schema = 1\nid = \"fresh\"\nname = \"Fresh\"\nkind = \"plugin\"\n\
       category = \"instrument\"\ntags = [\"synthesizer\"]\n\
       description = \"Test package fresh.\"\n\
       license = {{ kind = \"open-source\", spdx = \"GPL-3.0-or-later\" }}\n\
       \n[[releases]]\nversion = \"2.0.0\"\n\
       \n[[releases.artifacts]]\n\
       target = {{ os = \"linux\", arch = \"{}\" }}\n\
       source = {{ type = \"file\", url = \"file:///nonexistent/fresh.tar.gz\" }}\n\
       archive = \"tar.gz\"\nsize = 1\n\
       checksum = {{ sha256 = \"{}\" }}\n\
       provides = [\"clap\"]\n\
       install = [{{ format = \"clap\", source = \"Fresh.clap\", kind = \"file\" }}]\n",
      std::env::consts::ARCH,
      "0".repeat(64),
    );
    let dir = tempfile::tempdir().unwrap();
    let session = session_reading(dir.path(), &[("fresh.toml", &manifest)]);
    assert!(
      session
        .index()
        .unwrap()
        .packages
        .contains_key(&PackageId::new("fresh").unwrap())
    );
    let plan = InstallPlan {
      roots: vec![PackageId::new("fresh").unwrap()],
      ..InstallPlan::default()
    };
    let outcome = session
      .upgrade(&plan, &mut crate::download::NoProgress)
      .await
      .unwrap();
    assert!(outcome.installed.is_empty(), "{outcome:?}");
    assert_eq!(outcome.no_longer_installed, ["fresh"]);
    assert_eq!(installed_ids(&session), ["gone"]);
  }

  fn reason_for_gone(session: &Session) -> UnavailableReason {
    let outdated = session.outdated().unwrap();
    assert_eq!(outdated.unavailable.len(), 1, "{outdated:?}");
    outdated.unavailable[0].reason
  }

  #[test]
  fn a_package_missing_while_a_source_cannot_be_read_is_not_said_to_be_gone() {
    // The source that could not be read may well carry it: "no source
    // carries it any more" would be a guess, and `update` the fix.
    let dir = tempfile::tempdir().unwrap();
    let readable = dir.path().join("readable");
    std::fs::create_dir_all(&readable).unwrap();
    let session = session_with(
      dir.path(),
      vec![
        RegistryConfig::new("first", RegistrySource::Path { path: readable }),
        RegistryConfig::new(
          "second",
          RegistrySource::Path {
            path: dir.path().join("never-fetched"),
          },
        ),
      ],
    );
    assert_eq!(
      reason_for_gone(&session),
      UnavailableReason::SourceUnreadable
    );
  }

  #[test]
  fn a_package_missing_from_every_source_that_was_read_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let readable = dir.path().join("readable");
    std::fs::create_dir_all(&readable).unwrap();
    let session = session_with(
      dir.path(),
      vec![RegistryConfig::new(
        "first",
        RegistrySource::Path { path: readable },
      )],
    );
    assert_eq!(reason_for_gone(&session), UnavailableReason::NotCarried);
  }

  #[test]
  fn import_prune_removes_nothing_while_a_source_cannot_be_read() {
    // A package that source carries would look like one the file does not
    // need, and `import --prune` runs unattended. The install still goes
    // ahead; only the removal waits.
    let dir = tempfile::tempdir().unwrap();
    let readable = dir.path().join("readable");
    std::fs::create_dir_all(&readable).unwrap();
    let session = session_with(
      dir.path(),
      vec![
        RegistryConfig::new("first", RegistrySource::Path { path: readable }),
        RegistryConfig::new(
          "second",
          RegistrySource::Path {
            path: dir.path().join("never-fetched"),
          },
        ),
      ],
    );
    let plan = session.plan_import(&empty_env_file(), true).unwrap();
    assert!(plan.prune.is_empty(), "{:?}", plan.prune);
    assert!(plan.prune_withheld.is_some());
  }
}
