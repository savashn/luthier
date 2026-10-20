//! Turning a request into an ordered plan.
//!
//! Determinism is a requirement, not a nicety (§57): the same registry, target
//! and request must always produce the same plan, or lock files and reproducible
//! environments are impossible later. Every point where the algorithm could
//! branch on iteration order instead branches on package ID.

use crate::error::{Error, ResolveError, Result};
use crate::registry::{IndexEntry, RegistryIndex};
use crate::state::{InstallReason, State};
use luthier_manifest::{Artifact, Manifest, PackageId, PackageKind, Release, Target};
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet};

/// Somewhere to look packages up. Lets the resolver be tested with in-memory
/// fixtures and no registry on disk.
pub trait PackageSource {
  fn manifest(&self, id: &PackageId) -> Option<&Manifest>;
  fn entry(&self, id: &PackageId) -> Option<&IndexEntry>;
}

impl PackageSource for RegistryIndex {
  fn manifest(&self, id: &PackageId) -> Option<&Manifest> {
    self.packages.get(id).map(|e| &e.manifest)
  }
  fn entry(&self, id: &PackageId) -> Option<&IndexEntry> {
    self.packages.get(id)
  }
}

/// What resolution decided to do about one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
  /// Not installed; will be.
  Install,
  /// Installed at a different version; will be replaced.
  Upgrade { from: Version },
  /// Already installed at the selected version; nothing to do.
  Satisfied,
}

/// One package in the plan.
#[derive(Debug, Clone)]
pub struct ResolvedPackage<'a> {
  pub entry: &'a IndexEntry,
  pub version: Version,
  pub release: &'a Release,
  /// The artifact chosen for the requested target.
  ///
  /// `None` for a `pack`, which is metadata only: it carries no artifact and
  /// exists to resolve to its dependencies (§49).
  pub artifact: Option<&'a Artifact>,
  pub reason: InstallReason,
  pub dependencies: Vec<PackageId>,
  pub disposition: Disposition,
}

impl ResolvedPackage<'_> {
  pub fn id(&self) -> &PackageId {
    &self.entry.manifest.id
  }
}

/// An `external` dependency: detected, never downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRequirement {
  pub id: PackageId,
  pub name: String,
  pub satisfied: bool,
  pub provisioning_hint: Option<String>,
}

/// The complete plan.
#[derive(Debug, Clone)]
pub struct Resolution<'a> {
  /// Dependencies before dependents.
  pub order: Vec<ResolvedPackage<'a>>,
  pub externals: Vec<ExternalRequirement>,
  /// Installed packages resolved from the state, left as they are: see
  /// [`frozen`]. Nothing is done to them, and nothing records them anew.
  pub kept: Vec<KeptPackage>,
}

/// An installed package a resolution leaves exactly as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptPackage {
  pub id: PackageId,
  pub version: Version,
}

impl<'a> Resolution<'a> {
  /// Packages that actually need work.
  pub fn to_install(&self) -> impl Iterator<Item = &ResolvedPackage<'a>> {
    self
      .order
      .iter()
      .filter(|p| p.disposition != Disposition::Satisfied)
  }

  /// External requirements that are not present.
  pub fn missing_externals(&self) -> impl Iterator<Item = &ExternalRequirement> {
    self.externals.iter().filter(|e| !e.satisfied)
  }
}

/// Inputs to resolution.
pub struct ResolveRequest<'a> {
  /// Packages the user named. Recorded as explicitly installed (§24).
  pub roots: &'a [PackageId],
  pub target: &'a Target,
  pub state: &'a State,
  /// External package IDs the scanner found on this machine.
  pub detected_externals: &'a BTreeSet<PackageId>,
  /// Reinstall packages already at the selected version.
  pub force: bool,
  /// Versions this resolution must land on, from an imported environment
  /// file (§51). Behaves exactly like a pin — including refusing rather
  /// than silently overriding a conflicting dependency requirement — but
  /// applies only to this resolution and is not written to state.
  pub required_versions: &'a BTreeMap<PackageId, Version>,
  /// Installed packages this command is about to remove (`import --prune`):
  /// they hold nothing at the version installed, since they are going.
  pub leaving: &'a BTreeSet<PackageId>,
  /// Installed packages `upgrade` holds back because their own upgrade
  /// cannot be resolved: each stays at the version installed, and so does
  /// what it needs, as for a package no source supplies, so nothing else
  /// moves past what its installed release requires.
  pub held: &'a BTreeSet<PackageId>,
}

/// Why an installed package is resolved from the state rather than a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frozen {
  /// No source carries it any more.
  NotCarried,
  /// A source carries it, and no release of it is built for this target.
  NoReleaseForTarget,
  /// `upgrade` holds it back: its own upgrade cannot be resolved.
  HeldBack,
  /// A package nothing can upgrade any more needs it, so it stays at the
  /// version installed: what that package supports is not known.
  NeededBy(PackageId),
  /// A package `upgrade` holds back needs it, so it stays at the version
  /// installed: moving it could break the release that stays.
  HeldBy(PackageId),
}

/// The installed packages every resolution leaves where they are.
///
/// A package no source can supply any more still works where it is, and
/// whatever needs it must not fail to resolve over it; what it needs in
/// turn must not be moved past what it was built against, because with
/// its manifest gone nothing says what it supports. Both are fixed points:
/// resolved at the installed version, from the state, needing no release
/// in any registry. A requirement they do not meet is a conflict, said as
/// one ([`ResolveError::KeptConflict`]). A package `upgrade` holds back
/// (`held`) is fixed the same way, and so is what it needs.
pub fn frozen<S: PackageSource>(
  source: &S,
  state: &State,
  target: &Target,
  leaving: &BTreeSet<PackageId>,
  roots: &[PackageId],
  held: &BTreeSet<PackageId>,
) -> BTreeMap<PackageId, Frozen> {
  let mut frozen = BTreeMap::new();
  for package in state.packages.values() {
    let why = match source.manifest(&package.id) {
      None => Frozen::NotCarried,
      // A pack has no artifacts to be built for anything.
      Some(manifest) if manifest.kind == PackageKind::Pack => continue,
      Some(manifest) if newest_installable(manifest, target).is_none() => {
        Frozen::NoReleaseForTarget
      }
      Some(_) => continue,
    };
    frozen.insert(package.id.clone(), why);
  }
  for id in held {
    if state.is_installed(id) && !frozen.contains_key(id) {
      frozen.insert(id.clone(), Frozen::HeldBack);
    }
  }
  // Only what stays needs what it was built against: a package nothing
  // explicit reaches is `autoremove`'s, and one about to be removed holds
  // nothing either. One named in this command stays, explicit or not: it
  // is about to be recorded as asked for.
  let explicit = state
    .packages
    .values()
    .filter(|package| {
      (package.reason == InstallReason::Explicit && !leaving.contains(&package.id))
        || roots.contains(&package.id)
    })
    .map(|package| &package.id);
  let staying = state.reachable_from(explicit);
  let holders: Vec<PackageId> = frozen
    .keys()
    .filter(|id| staying.contains(id) && !leaving.contains(*id))
    .cloned()
    .collect();
  for id in &holders {
    let why = if frozen.get(id) == Some(&Frozen::HeldBack) {
      Frozen::HeldBy(id.clone())
    } else {
      Frozen::NeededBy(id.clone())
    };
    for reached in state.reachable_from([id]) {
      if !frozen.contains_key(reached) {
        frozen.insert(reached.clone(), why.clone());
      }
    }
  }
  frozen
}

/// The newest release of `manifest` with something this build installs
/// for `target`: what an unconstrained resolution picks. Yanked releases
/// are never picked, so they do not count.
pub fn newest_installable<'a>(manifest: &'a Manifest, target: &Target) -> Option<&'a Release> {
  manifest
    .releases_newest_first()
    .into_iter()
    .find(|release| select_artifact(release, target).is_some())
}

/// The conflict a requirement makes with a package held where it is.
fn kept_conflict(id: &PackageId, version: &Version, requirement: String, why: &Frozen) -> Error {
  Error::Resolve(ResolveError::KeptConflict {
    id: id.clone(),
    detail: format!(
      "{requirement}, but {id} stays at {version}: {}",
      held_because(why)
    ),
  })
}

/// Why a package stays where it is, as a clause. "No source that could be
/// read": the resolver cannot tell a package gone from every source from
/// one whose source failed to load.
fn held_because(why: &Frozen) -> String {
  match why {
    Frozen::NotCarried => "no source that could be read carries it".to_string(),
    Frozen::NoReleaseForTarget => "no release of it is built for this machine any more".to_string(),
    Frozen::NeededBy(by) => format!("{by} needs it, and can no longer be upgraded"),
    Frozen::HeldBack => "its own upgrade is held back".to_string(),
    Frozen::HeldBy(by) => format!("{by} needs it, and is held back"),
  }
}

/// Resolves `request` against `source`.
pub fn resolve<'a, S: PackageSource>(
  source: &'a S,
  request: &ResolveRequest<'_>,
) -> Result<Resolution<'a>> {
  let mut requirements: BTreeMap<PackageId, Vec<(PackageId, VersionReq)>> = BTreeMap::new();
  let mut chosen: BTreeMap<PackageId, Version> = BTreeMap::new();
  let mut edges: BTreeMap<PackageId, Vec<PackageId>> = BTreeMap::new();
  let mut externals: BTreeMap<PackageId, ExternalRequirement> = BTreeMap::new();
  let mut reasons: BTreeMap<PackageId, InstallReason> = BTreeMap::new();
  let mut kept: BTreeMap<PackageId, Version> = BTreeMap::new();
  // Worked out before the walk, not as it goes: whether a dependency is
  // held must not depend on whether the package holding it was reached
  // first. It stays installed either way.
  let frozen = frozen(
    source,
    request.state,
    request.target,
    request.leaving,
    request.roots,
    request.held,
  );

  let mut pending: BTreeSet<PackageId> = request.roots.iter().cloned().collect();
  for root in request.roots {
    reasons.insert(root.clone(), InstallReason::Explicit);
    requirements.entry(root.clone()).or_default();
  }

  // A worklist rather than recursion, always taking the lexicographically
  // smallest pending package, so the traversal order is fixed.
  while let Some(id) = pending.iter().next().cloned() {
    pending.remove(&id);
    let constraints = requirements.get(&id).cloned().unwrap_or_default();

    // A fixed point: the version installed, whatever is released.
    let mut held_at = None;
    if let (Some(installed), Some(why)) = (request.state.get(&id), frozen.get(&id)) {
      for (requirer, req) in &constraints {
        if !req.matches(&installed.version) {
          return Err(kept_conflict(
            &id,
            &installed.version,
            format!("{requirer} needs {id} {req}"),
            why,
          ));
        }
      }
      if let Some(wanted) = request.required_versions.get(&id)
        && wanted != &installed.version
      {
        return Err(kept_conflict(
          &id,
          &installed.version,
          format!("the file needs {id} {wanted}"),
          why,
        ));
      }
      // Resolved through its source when that still publishes what is
      // installed — a yanked release included, since it is what is there —
      // so a reinstall still has something to fetch; from the state
      // otherwise.
      let published = source.manifest(&id).is_some_and(|manifest| {
        manifest.release(&installed.version).is_some_and(|release| {
          manifest.kind == PackageKind::Pack || select_artifact(release, request.target).is_some()
        })
      });
      if !published {
        // Asked to reinstall it, which is what `verify` says repairs a
        // package: there is nothing to fetch, and saying "already
        // installed" would leave it broken without a word.
        if request.force && request.roots.contains(&id) {
          return Err(Error::Resolve(ResolveError::NotReinstallable {
            id: id.clone(),
            detail: format!(
              "{id} {} cannot be reinstalled: {}",
              installed.version,
              match why {
                Frozen::NotCarried => held_because(why),
                _ => "that version is no longer published for this machine".to_string(),
              }
            ),
          }));
        }
        kept.insert(id.clone(), installed.version.clone());
        continue;
      }
      held_at = Some(installed.version.clone());
    }

    let manifest = source
      .manifest(&id)
      .ok_or_else(|| Error::Resolve(ResolveError::NotFound(id.clone())))?;

    if manifest.kind == PackageKind::External {
      externals.insert(
        id.clone(),
        ExternalRequirement {
          id: id.clone(),
          name: manifest.name.clone(),
          satisfied: request.detected_externals.contains(&id),
          provisioning_hint: manifest.provisioning_hint.clone(),
        },
      );
      continue;
    }

    let version = match held_at {
      Some(version) => version,
      None => {
        // A version demanded by an imported environment file outranks a
        // stored pin: the user asked for this exact set just now. The two
        // are kept apart because they fail differently — a stored pin
        // naming a version the registry has dropped falls back, a
        // required one cannot.
        let required = request.required_versions.get(&id);
        let pin = request.state.get(&id).and_then(|p| p.pin.clone());
        select_version(
          manifest,
          &constraints,
          request.target,
          required,
          pin.as_ref(),
        )?
      }
    };

    if let Some(previous) = chosen.get(&id) {
      if previous != &version {
        return Err(Error::Resolve(ResolveError::Conflict {
          id: id.clone(),
          requirers: constraints,
        }));
      }
      continue;
    }
    chosen.insert(id.clone(), version.clone());

    let release = manifest
      .release(&version)
      .expect("select_version returned a version from this manifest");

    let mut dependencies: Vec<PackageId> =
      release.dependencies.iter().map(|d| d.id.clone()).collect();
    dependencies.sort();
    dependencies.dedup();
    edges.insert(id.clone(), dependencies.clone());

    for dependency in &release.dependencies {
      let req = dependency.version.clone().unwrap_or(VersionReq::STAR);
      requirements
        .entry(dependency.id.clone())
        .or_default()
        .push((id.clone(), req.clone()));
      reasons
        .entry(dependency.id.clone())
        .or_insert(InstallReason::Dependency);
      // Already decided: check the new constraint still holds rather than
      // silently keeping an incompatible version.
      let decided = kept
        .get(&dependency.id)
        .or_else(|| chosen.get(&dependency.id));
      match decided {
        Some(selected) if !req.matches(selected) => {
          return Err(match frozen.get(&dependency.id) {
            Some(why) => kept_conflict(
              &dependency.id,
              selected,
              format!("{id} needs {} {req}", dependency.id),
              why,
            ),
            None => Error::Resolve(ResolveError::Conflict {
              id: dependency.id.clone(),
              requirers: requirements
                .get(&dependency.id)
                .cloned()
                .unwrap_or_default(),
            }),
          });
        }
        Some(_) => {}
        None if !externals.contains_key(&dependency.id) => {
          pending.insert(dependency.id.clone());
        }
        None => {}
      }
    }
  }

  let ordered_ids = topological_order(&edges)?;

  let mut order = Vec::new();
  for id in ordered_ids {
    let Some(version) = chosen.get(&id) else {
      continue;
    };
    let entry = source
      .entry(&id)
      .ok_or_else(|| Error::Resolve(ResolveError::NotFound(id.clone())))?;
    let release = entry
      .manifest
      .release(version)
      .expect("chosen version exists");
    let artifact = match select_artifact(release, request.target) {
      Some(artifact) => Some(artifact),
      // A pack legitimately has nothing to download.
      None if entry.manifest.kind == PackageKind::Pack => None,
      None => {
        return Err(Error::Resolve(ResolveError::NoArtifactForTarget {
          id: id.clone(),
          version: version.clone(),
          target: request.target.clone(),
        }));
      }
    };

    let installed = request.state.get(&id);
    let disposition = match installed {
      Some(p) if &p.version == version && !request.force => Disposition::Satisfied,
      Some(p) => Disposition::Upgrade {
        from: p.version.clone(),
      },
      None => Disposition::Install,
    };

    order.push(ResolvedPackage {
      entry,
      version: version.clone(),
      release,
      artifact,
      reason: reasons
        .get(&id)
        .copied()
        .unwrap_or(InstallReason::Dependency),
      dependencies: edges.get(&id).cloned().unwrap_or_default(),
      disposition,
    });
  }

  Ok(Resolution {
    order,
    externals: externals.into_values().collect(),
    kept: kept
      .into_iter()
      .map(|(id, version)| KeptPackage { id, version })
      .collect(),
  })
}

/// Picks the highest release satisfying every constraint and this target.
fn select_version(
  manifest: &Manifest,
  constraints: &[(PackageId, VersionReq)],
  target: &Target,
  required: Option<&Version>,
  pin: Option<&Version>,
) -> Result<Version> {
  // A required version comes from an imported environment file. Unlike a
  // pin it cannot degrade into "something close": an import that silently
  // installed a different version would not have reproduced anything.
  if let Some(wanted) = required {
    for (requirer, req) in constraints {
      if !req.matches(wanted) {
        return Err(Error::Resolve(ResolveError::PinConflict {
          id: manifest.id.clone(),
          pinned: wanted.clone(),
          requirer: requirer.clone(),
          req: req.clone(),
        }));
      }
    }
    return if manifest.release(wanted).is_some() {
      Ok(wanted.clone())
    } else {
      Err(Error::Resolve(ResolveError::RequiredVersionMissing {
        id: manifest.id.clone(),
        version: wanted.clone(),
      }))
    };
  }

  // Pinned packages are held where they are, and a conflicting requirement is
  // reported rather than quietly overriding the user's choice (§27).
  if let Some(pinned) = pin {
    for (requirer, req) in constraints {
      if !req.matches(pinned) {
        return Err(Error::Resolve(ResolveError::PinConflict {
          id: manifest.id.clone(),
          pinned: pinned.clone(),
          requirer: requirer.clone(),
          req: req.clone(),
        }));
      }
    }
    if manifest.release(pinned).is_some() {
      return Ok(pinned.clone());
    }
  }

  let candidates = manifest.releases_newest_first();
  if candidates.is_empty() {
    return Err(Error::Resolve(ResolveError::NoReleaseForTarget {
      id: manifest.id.clone(),
      target: target.clone(),
    }));
  }

  // A pack has no artifacts to be compatible with, so any release will do.
  let needs_artifact = manifest.kind != PackageKind::Pack;

  let mut satisfies_constraints = false;
  for release in &candidates {
    if !constraints
      .iter()
      .all(|(_, req)| req.matches(&release.version))
    {
      continue;
    }
    satisfies_constraints = true;
    if !needs_artifact || select_artifact(release, target).is_some() {
      return Ok(release.version.clone());
    }
  }

  if !satisfies_constraints {
    let req = constraints
      .first()
      .map(|(_, r)| r.clone())
      .unwrap_or(VersionReq::STAR);
    return Err(Error::Resolve(ResolveError::NoMatchingVersion {
      id: manifest.id.clone(),
      req,
    }));
  }

  // "Nothing for this target" and "something for this target that this build
  // does not install" are different answers, and only the second one tells a
  // user why. The newest release that publishes anything at all is the one
  // worth naming: it is what they would have got.
  if let Some(artifact) = candidates
    .iter()
    .filter(|release| {
      constraints
        .iter()
        .all(|(_, req)| req.matches(&release.version))
    })
    .find_map(|release| release.artifacts_for(target).next())
  {
    return Err(Error::Resolve(ResolveError::NothingInstallable {
      id: manifest.id.clone(),
      target: target.clone(),
      declared: artifact.provides.clone(),
    }));
  }

  Err(Error::Resolve(ResolveError::NoReleaseForTarget {
    id: manifest.id.clone(),
    target: target.clone(),
  }))
}

/// Chooses the artifact to use for `target`.
///
/// Declaration order is the registry's preference. Surge XT, for instance,
/// publishes both a 92 MB plugins-only tarball and a 333 MB one with the full
/// content set; listing the smaller first is how the manifest says which to
/// prefer. The client does not guess.
fn select_artifact<'a>(release: &'a Release, target: &Target) -> Option<&'a Artifact> {
  release
    .artifacts_for(target)
    .find(|artifact| crate::install::installable(artifact))
}

/// Orders packages so dependencies come first, reporting any cycle.
fn topological_order(edges: &BTreeMap<PackageId, Vec<PackageId>>) -> Result<Vec<PackageId>> {
  #[derive(Clone, Copy, PartialEq)]
  enum Mark {
    Visiting,
    Done,
  }

  fn visit(
    id: &PackageId,
    edges: &BTreeMap<PackageId, Vec<PackageId>>,
    marks: &mut BTreeMap<PackageId, Mark>,
    stack: &mut Vec<PackageId>,
    out: &mut Vec<PackageId>,
  ) -> Result<()> {
    match marks.get(id) {
      Some(Mark::Done) => return Ok(()),
      Some(Mark::Visiting) => {
        // Report the cycle itself, not just that one exists.
        let start = stack.iter().position(|p| p == id).unwrap_or(0);
        let mut path: Vec<PackageId> = stack[start..].to_vec();
        path.push(id.clone());
        return Err(Error::Resolve(ResolveError::Cycle(path)));
      }
      None => {}
    }

    marks.insert(id.clone(), Mark::Visiting);
    stack.push(id.clone());
    for dependency in edges.get(id).into_iter().flatten() {
      if edges.contains_key(dependency) {
        visit(dependency, edges, marks, stack, out)?;
      }
    }
    stack.pop();
    marks.insert(id.clone(), Mark::Done);
    out.push(id.clone());
    Ok(())
  }

  let mut marks = BTreeMap::new();
  let mut out = Vec::new();
  // BTreeMap iteration is sorted, so the output order is fixed.
  for id in edges.keys() {
    visit(id, edges, &mut marks, &mut Vec::new(), &mut out)?;
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use luthier_manifest::{ParseMode, Sha256Hash};
  use std::path::PathBuf;

  struct Fixture {
    packages: BTreeMap<PackageId, IndexEntry>,
  }

  impl PackageSource for Fixture {
    fn manifest(&self, id: &PackageId) -> Option<&Manifest> {
      self.packages.get(id).map(|e| &e.manifest)
    }
    fn entry(&self, id: &PackageId) -> Option<&IndexEntry> {
      self.packages.get(id)
    }
  }

  impl Fixture {
    fn new() -> Self {
      Self {
        packages: BTreeMap::new(),
      }
    }

    /// Adds a plugin with the given releases and per-release dependencies.
    fn add(self, id: &str, releases: &[(&str, &[&str])]) -> Self {
      let mut lines = vec![
        "schema = 1".to_string(),
        format!("id = \"{id}\""),
        format!("name = \"{id}\""),
        "kind = \"plugin\"".to_string(),
        "category = \"instrument\"".to_string(),
        "license = { kind = \"open-source\", spdx = \"MIT\" }".to_string(),
      ];
      for (version, deps) in releases {
        lines.push("\n[[releases]]".to_string());
        lines.push(format!("version = \"{version}\""));
        if !deps.is_empty() {
          let quoted: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
          lines.push(format!("dependencies = [{}]", quoted.join(", ")));
        }
        lines.extend(artifact_lines(id));
      }
      self.insert(&lines.join("\n"), id)
    }

    fn add_external(self, id: &str) -> Self {
      let lines = [
        "schema = 1".to_string(),
        format!("id = \"{id}\""),
        format!("name = \"{id}\""),
        "kind = \"external\"".to_string(),
        "category = \"instrument\"".to_string(),
        format!("provisioning_hint = \"Install {id} from your distribution.\""),
        "license = { kind = \"open-source\", spdx = \"BSD-2-Clause\" }".to_string(),
        "\n[[detect]]".to_string(),
        "format = \"vst3\"".to_string(),
        format!("name = \"{id}.vst3\""),
      ];
      self.insert(&lines.join("\n"), id)
    }

    fn insert(mut self, text: &str, id: &str) -> Self {
      let manifest = luthier_manifest::from_toml(text, id, ParseMode::Strict)
        .unwrap_or_else(|e| panic!("fixture {id} should parse: {e}"))
        .manifest;
      self.packages.insert(
        manifest.id.clone(),
        IndexEntry {
          manifest,
          path: PathBuf::from(format!("{id}.toml")),
          digest: Sha256Hash::from_bytes([0; 32]),
          unknown_fields: vec![],
          registry: "test".into(),
          notes: Vec::new(),
        },
      );
      self
    }

    /// Adds a package whose rules must be derived from its archive, as
    /// everything from a source that carries none arrives.
    fn add_derived(mut self, id: &str, provides: &[&str]) -> Self {
      let quoted: Vec<String> = provides.iter().map(|p| format!("\"{p}\"")).collect();
      let text = [
        "schema = 1".to_string(),
        format!("id = \"{id}\""),
        format!("name = \"{id}\""),
        "kind = \"plugin\"".to_string(),
        "category = \"instrument\"".to_string(),
        "license = { kind = \"open-source\", spdx = \"MIT\" }".to_string(),
        "\n[[releases]]".to_string(),
        "version = \"1.0.0\"".to_string(),
        "\n[[releases.artifacts]]".to_string(),
        "target = { os = \"linux\", arch = \"x86_64\" }".to_string(),
        format!("source = {{ type = \"http\", url = \"https://e.invalid/{id}.zip\" }}"),
        "archive = \"zip\"".to_string(),
        "checksum = { sha256 = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\" }".to_string(),
        format!("provides = [{}]", quoted.join(", ")),
        "derive_install = true".to_string(),
      ]
      .join("\n");
      let manifest = luthier_manifest::from_toml(&text, id, ParseMode::Strict)
        .unwrap_or_else(|e| panic!("fixture {id} should parse: {e}"))
        .manifest;
      self.packages.insert(
        manifest.id.clone(),
        IndexEntry {
          manifest,
          path: PathBuf::from(format!("{id}.toml")),
          digest: Sha256Hash::from_bytes([0; 32]),
          unknown_fields: vec![],
          registry: "test".into(),
          notes: Vec::new(),
        },
      );
      self
    }
  }

  /// The artifact stanza every fixture release shares.
  fn artifact_lines(id: &str) -> Vec<String> {
    vec![
            "\n[[releases.artifacts]]".to_string(),
            "target = { os = \"linux\", arch = \"x86_64\" }".to_string(),
            format!("source = {{ type = \"http\", url = \"https://e.invalid/{id}.tar.gz\" }}"),
            "archive = \"tar.gz\"".to_string(),
            "checksum = { sha256 = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\" }".to_string(),
            "provides = [\"clap\"]".to_string(),
            format!("install = [{{ format = \"clap\", source = \"{id}.clap\", kind = \"file\" }}]"),
        ]
  }

  /// Resolving one package on its own, for the error rather than the order.
  fn resolve_one(fixture: &Fixture, id_: &str) -> Result<Vec<String>> {
    resolve_ids(fixture, &[id_])
  }

  #[test]
  fn a_release_with_nothing_derivable_is_refused_before_it_is_fetched() {
    // The case this exists for: the Open Audio Stack registry says a
    // release's Linux archive holds an `elf` or a `so`, which is a VST2
    // build or a standalone program. Neither yields a rule, so the install
    // used to download the whole thing and then refuse it.
    let fixture = Fixture::new().add_derived("standalone", &[]);
    let err = resolve_one(&fixture, "standalone").unwrap_err();

    assert!(err.to_string().contains("nothing"), "{err}");
    assert!(err.to_string().contains("standalone"), "{err}");
    // And it says why, rather than leaving the user to guess at a flag.
    let hint = err.hint().unwrap_or_default();
    assert!(hint.contains("VST2"), "{hint}");
  }

  #[test]
  fn sample_content_resolves_because_its_rule_is_read_from_the_archive_shape() {
    // A library declares `library` and nothing else. The archive's shape
    // says where the content is, so there is something to plan.
    let fixture = Fixture::new().add_derived("kit", &["library"]);
    assert_eq!(resolve_one(&fixture, "kit").unwrap(), vec!["kit"]);
  }

  #[test]
  fn a_release_that_declares_a_derivable_format_resolves() {
    // The claim is upstream's and unverified, which is the point: it is
    // enough to plan on, and the derivation checks it against the real
    // archive once the bytes are there.
    let fixture = Fixture::new().add_derived("plug", &["clap"]);
    assert_eq!(resolve_one(&fixture, "plug").unwrap(), vec!["plug"]);
  }

  fn id(s: &str) -> PackageId {
    PackageId::new(s).unwrap()
  }

  fn linux() -> Target {
    Target::new(luthier_manifest::Os::Linux, luthier_manifest::Arch::X86_64)
  }

  fn resolve_ids(fixture: &Fixture, roots: &[&str]) -> Result<Vec<String>> {
    let roots: Vec<PackageId> = roots.iter().map(|r| id(r)).collect();
    let state = State::default();
    let detected = BTreeSet::new();
    let resolution = resolve(
      fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )?;
    Ok(
      resolution
        .order
        .iter()
        .map(|p| p.id().to_string())
        .collect(),
    )
  }

  #[test]
  fn dependencies_are_installed_before_dependents() {
    let fixture = Fixture::new()
      .add("vsco2", &[("1.0.0", &["engine"])])
      .add("engine", &[("1.0.0", &[])]);
    assert_eq!(
      resolve_ids(&fixture, &["vsco2"]).unwrap(),
      vec!["engine", "vsco2"]
    );
  }

  #[test]
  fn a_diamond_installs_each_package_once() {
    let fixture = Fixture::new()
      .add("top", &[("1.0.0", &["left", "right"])])
      .add("left", &[("1.0.0", &["base"])])
      .add("right", &[("1.0.0", &["base"])])
      .add("base", &[("1.0.0", &[])]);
    let order = resolve_ids(&fixture, &["top"]).unwrap();
    assert_eq!(order.len(), 4);
    assert_eq!(order[0], "base");
    assert_eq!(order[3], "top");
    assert!(order.iter().position(|p| p == "base") < order.iter().position(|p| p == "left"));
  }

  #[test]
  fn resolution_is_deterministic() {
    // §57: the same inputs must always give the same plan, or a future lock
    // file means nothing.
    let fixture = Fixture::new()
      .add("top", &[("1.0.0", &["a", "b", "c"])])
      .add("a", &[("1.0.0", &["shared"])])
      .add("b", &[("1.0.0", &["shared"])])
      .add("c", &[("1.0.0", &["shared"])])
      .add("shared", &[("1.0.0", &[])]);
    let first = resolve_ids(&fixture, &["top"]).unwrap();
    for _ in 0..10 {
      assert_eq!(resolve_ids(&fixture, &["top"]).unwrap(), first);
    }
  }

  #[test]
  fn a_cycle_is_reported_with_the_path() {
    let fixture = Fixture::new()
      .add("a", &[("1.0.0", &["b"])])
      .add("b", &[("1.0.0", &["c"])])
      .add("c", &[("1.0.0", &["a"])]);
    let err = resolve_ids(&fixture, &["a"]).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("dependency cycle"), "{text}");
    assert!(
      text.contains("->"),
      "the cycle path should be shown: {text}"
    );
  }

  #[test]
  fn the_highest_matching_release_is_selected() {
    let fixture = Fixture::new().add(
      "surge-xt",
      &[("1.3.2", &[]), ("1.3.4", &[]), ("1.3.3", &[])],
    );
    let roots = vec![id("surge-xt")];
    let state = State::default();
    let detected = BTreeSet::new();
    let resolution = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap();
    // Declaration order in the file must not matter.
    assert_eq!(resolution.order[0].version, Version::new(1, 3, 4));
  }

  #[test]
  fn an_unknown_package_suggests_searching() {
    let fixture = Fixture::new();
    let err = resolve_ids(&fixture, &["nope"]).unwrap_err();
    assert!(
      matches!(err, Error::Resolve(ResolveError::NotFound(_))),
      "{err}"
    );
    assert!(err.hint().unwrap().contains("luthier search"));
  }

  #[test]
  fn an_external_dependency_is_reported_not_downloaded() {
    // The sfizz case: no redistributable Linux binary exists, so the
    // resolver records the requirement and leaves it to the user.
    let fixture = Fixture::new()
      .add("vsco2", &[("1.0.0", &["sfizz"])])
      .add_external("sfizz");
    let roots = vec![id("vsco2")];
    let state = State::default();
    let detected = BTreeSet::new();
    let resolution = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap();

    assert_eq!(resolution.order.len(), 1, "only vsco2 is downloadable");
    assert_eq!(resolution.order[0].id().as_str(), "vsco2");
    let missing: Vec<&ExternalRequirement> = resolution.missing_externals().collect();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].id.as_str(), "sfizz");
    assert!(
      missing[0]
        .provisioning_hint
        .as_ref()
        .unwrap()
        .contains("distribution")
    );
  }

  #[test]
  fn a_detected_external_counts_as_satisfied() {
    let fixture = Fixture::new()
      .add("vsco2", &[("1.0.0", &["sfizz"])])
      .add_external("sfizz");
    let roots = vec![id("vsco2")];
    let state = State::default();
    let detected = BTreeSet::from([id("sfizz")]);
    let resolution = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap();
    assert_eq!(resolution.missing_externals().count(), 0);
  }

  #[test]
  fn roots_are_explicit_and_dependencies_are_not() {
    let fixture = Fixture::new()
      .add("vsco2", &[("1.0.0", &["engine"])])
      .add("engine", &[("1.0.0", &[])]);
    let roots = vec![id("vsco2")];
    let state = State::default();
    let detected = BTreeSet::new();
    let resolution = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap();
    let reasons: BTreeMap<&str, InstallReason> = resolution
      .order
      .iter()
      .map(|p| (p.id().as_str(), p.reason))
      .collect();
    assert_eq!(reasons["vsco2"], InstallReason::Explicit);
    // This is what later lets `luthier autoremove` call engine an orphan (§24).
    assert_eq!(reasons["engine"], InstallReason::Dependency);
  }

  #[test]
  fn a_version_requirement_narrows_the_selection() {
    let fixture = Fixture::new()
      .add("app", &[("1.0.0", &[])])
      .add("lib", &[("1.0.0", &[]), ("2.0.0", &[])]);
    // Rebuild `app` with a constrained dependency on lib.
    let mut lines = vec![
      "schema = 1".to_string(),
      "id = \"app\"".to_string(),
      "name = \"app\"".to_string(),
      "kind = \"plugin\"".to_string(),
      "category = \"instrument\"".to_string(),
      "license = { kind = \"open-source\", spdx = \"MIT\" }".to_string(),
      "\n[[releases]]".to_string(),
      "version = \"1.0.0\"".to_string(),
      "dependencies = [{ id = \"lib\", version = \"<2.0.0\" }]".to_string(),
    ];
    lines.extend(artifact_lines("app"));
    let fixture = fixture.insert(&lines.join("\n"), "app");

    let roots = vec![id("app")];
    let state = State::default();
    let detected = BTreeSet::new();
    let resolution = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap();
    let lib = resolution
      .order
      .iter()
      .find(|p| p.id().as_str() == "lib")
      .unwrap();
    assert_eq!(
      lib.version,
      Version::new(1, 0, 0),
      "2.0.0 is excluded by the requirement"
    );
  }

  #[test]
  fn a_package_with_no_artifact_for_this_target_is_reported() {
    let fixture = Fixture::new().add("mac-only", &[("1.0.0", &[])]);
    let roots = vec![id("mac-only")];
    let state = State::default();
    let detected = BTreeSet::new();
    let other_target = Target::new(luthier_manifest::Os::Macos, luthier_manifest::Arch::Aarch64);
    let err = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &other_target,
        state: &state,
        detected_externals: &detected,
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap_err();
    assert!(err.to_string().contains("no release for"), "{err}");
  }

  /// A state with `packages` installed, as `(id, version, dependencies)`,
  /// every one explicit.
  fn installed(packages: &[(&str, &str, &[&str])]) -> State {
    let mut state = State::default();
    for (name, version, deps) in packages {
      state.packages.insert(
        id(name),
        crate::state::InstalledPackage {
          id: id(name),
          name: (*name).into(),
          version: version.parse().unwrap(),
          registry: "test".into(),
          manifest_digest: Sha256Hash::from_bytes([0; 32]),
          reason: InstallReason::Explicit,
          pin: None,
          installed_at: jiff::Timestamp::UNIX_EPOCH,
          formats: vec![],
          content: vec![],
          artifacts: vec![],
          dependencies: deps.iter().map(|d| id(d)).collect(),
          files: vec![],
        },
      );
    }
    state
  }

  fn resolve_in<'a>(fixture: &'a Fixture, state: &State, roots: &[&str]) -> Result<Resolution<'a>> {
    let roots: Vec<PackageId> = roots.iter().map(|r| id(r)).collect();
    resolve(
      fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state,
        detected_externals: &BTreeSet::new(),
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
  }

  fn version_of(resolution: &Resolution<'_>, name: &str) -> String {
    resolution
      .order
      .iter()
      .find(|p| p.id().as_str() == name)
      .map(|p| p.version.to_string())
      .or_else(|| {
        resolution
          .kept
          .iter()
          .find(|k| k.id.as_str() == name)
          .map(|k| k.version.to_string())
      })
      .unwrap_or_else(|| panic!("{name} is not in the resolution"))
  }

  #[test]
  fn an_installed_dependency_no_source_carries_is_kept_as_it_is() {
    // `gone` left every source. `app` still needs it and still resolves,
    // against the copy installed.
    let fixture = Fixture::new().add("app", &[("2.0.0", &["gone"])]);
    let state = installed(&[("app", "1.0.0", &["gone"]), ("gone", "1.0.0", &[])]);
    let resolution = resolve_in(&fixture, &state, &["app"]).unwrap();
    assert_eq!(version_of(&resolution, "app"), "2.0.0");
    assert_eq!(
      resolution.kept,
      vec![KeptPackage {
        id: id("gone"),
        version: Version::new(1, 0, 0),
      }]
    );
  }

  #[test]
  fn what_a_gone_package_needs_stays_whichever_is_reached_first() {
    // `old` left every source and needs `lib`. `alpha` and `zeta` need
    // `lib` too, and sort on either side of it: held either way, and from
    // the copy installed, though the registry dropped 1.0.0.
    let fixture = Fixture::new()
      .add("lib", &[("2.0.0", &[])])
      .add("alpha", &[("1.0.0", &["lib"])])
      .add("zeta", &[("1.0.0", &["lib"])]);
    let state = installed(&[("old", "1.0.0", &["lib"]), ("lib", "1.0.0", &[])]);
    for roots in [&["alpha"][..], &["zeta"][..], &["alpha", "zeta"][..]] {
      let resolution = resolve_in(&fixture, &state, roots).unwrap();
      assert_eq!(version_of(&resolution, "lib"), "1.0.0", "{roots:?}");
    }
  }

  #[test]
  fn a_requirement_a_held_package_cannot_meet_names_what_holds_it() {
    // `lib` is decided before `zeta` asks for more: the conflict is still
    // said as the hold it is, not as a plain conflict.
    let fixture = Fixture::new()
      .add("lib", &[("1.0.0", &[]), ("2.0.0", &[])])
      .add("alpha", &[("1.0.0", &["lib"])])
      .insert(
        &[
          "schema = 1",
          "id = \"zeta\"",
          "name = \"zeta\"",
          "kind = \"plugin\"",
          "category = \"instrument\"",
          "license = { kind = \"open-source\", spdx = \"MIT\" }",
          "\n[[releases]]",
          "version = \"1.0.0\"",
          "dependencies = [{ id = \"lib\", version = \">=2\" }]",
        ]
        .iter()
        .map(ToString::to_string)
        .chain(artifact_lines("zeta"))
        .collect::<Vec<_>>()
        .join("\n"),
        "zeta",
      );
    let state = installed(&[("old", "1.0.0", &["lib"]), ("lib", "1.0.0", &[])]);
    let err = resolve_in(&fixture, &state, &["alpha", "zeta"]).unwrap_err();
    let message = err.to_string();
    assert!(
      message.contains("zeta needs lib >=2, but lib stays at 1.0.0: old needs it"),
      "{message}"
    );
  }

  #[test]
  fn a_package_whose_only_build_here_is_yanked_is_frozen() {
    // Resolution never picks a yanked release, so it is no build at all.
    let fixture = Fixture::new().add("foo", &[("1.0.0", &[])]);
    let mut fixture = fixture;
    let entry = fixture.packages.get_mut(&id("foo")).unwrap();
    entry.manifest.releases[0].yanked = Some("broken".into());
    let state = installed(&[("foo", "1.0.0", &[])]);
    let frozen = frozen(
      &fixture,
      &state,
      &linux(),
      &BTreeSet::new(),
      &[],
      &BTreeSet::new(),
    );
    assert_eq!(frozen.get(&id("foo")), Some(&Frozen::NoReleaseForTarget));
    assert!(newest_installable(&fixture.packages[&id("foo")].manifest, &linux()).is_none());
  }

  #[test]
  fn reinstalling_a_package_no_source_carries_is_refused_and_says_why() {
    // `install --force` is what `verify` says repairs a package; with
    // nothing to fetch, "already installed" would hide that it cannot.
    let fixture = Fixture::new();
    let state = installed(&[("gone", "1.0.0", &[])]);
    let roots = [id("gone")];
    let err = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &BTreeSet::new(),
        force: true,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap_err();
    assert!(
      err.to_string().contains("gone 1.0.0 cannot be reinstalled"),
      "{err}"
    );
  }

  #[test]
  fn an_orphan_no_source_carries_holds_nothing() {
    // Nothing explicit needs `gone`: it is `autoremove`'s, and must not keep
    // `lib` from moving for what stays.
    let fixture = Fixture::new()
      .add("lib", &[("2.0.0", &[])])
      .add("plugin", &[("1.0.0", &["lib"])]);
    let mut state = installed(&[("gone", "1.0.0", &["lib"]), ("lib", "1.0.0", &[])]);
    for name in ["gone", "lib"] {
      state.packages.get_mut(&id(name)).unwrap().reason = InstallReason::Dependency;
    }
    let resolution = resolve_in(&fixture, &state, &["plugin"]).unwrap();
    assert_eq!(version_of(&resolution, "lib"), "2.0.0");
  }

  #[test]
  fn a_package_about_to_be_removed_holds_nothing() {
    let fixture = Fixture::new()
      .add("lib", &[("2.0.0", &[])])
      .add("plugin", &[("1.0.0", &["lib"])]);
    let state = installed(&[("old", "1.0.0", &["lib"]), ("lib", "1.0.0", &[])]);
    let roots = [id("plugin")];
    let leaving = BTreeSet::from([id("old"), id("lib")]);
    let resolution = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &BTreeSet::new(),
        force: false,
        required_versions: &BTreeMap::new(),
        leaving: &leaving,
        held: &BTreeSet::new(),
      },
    )
    .unwrap();
    assert_eq!(version_of(&resolution, "lib"), "2.0.0");
  }

  #[test]
  fn a_gone_package_named_in_this_command_holds_what_it_needs() {
    // `gone` is an orphan, so on its own it holds nothing; named here, it is
    // about to be recorded as asked for, and `lib` stays for it.
    let fixture = Fixture::new()
      .add("lib", &[("2.0.0", &[])])
      .add("plugin", &[("1.0.0", &["lib"])]);
    let mut state = installed(&[("gone", "1.0.0", &["lib"]), ("lib", "1.0.0", &[])]);
    for name in ["gone", "lib"] {
      state.packages.get_mut(&id(name)).unwrap().reason = InstallReason::Dependency;
    }
    let resolution = resolve_in(&fixture, &state, &["gone", "plugin"]).unwrap();
    assert_eq!(version_of(&resolution, "lib"), "1.0.0");
  }

  #[test]
  fn reinstalling_a_held_version_no_longer_published_says_so() {
    let fixture = Fixture::new().add("lib", &[("2.0.0", &[])]);
    let state = installed(&[("old", "1.0.0", &["lib"]), ("lib", "1.0.0", &[])]);
    let roots = [id("lib")];
    let err = resolve(
      &fixture,
      &ResolveRequest {
        roots: &roots,
        target: &linux(),
        state: &state,
        detected_externals: &BTreeSet::new(),
        force: true,
        required_versions: &BTreeMap::new(),
        leaving: &BTreeSet::new(),
        held: &BTreeSet::new(),
      },
    )
    .unwrap_err();
    assert!(
      err
        .to_string()
        .contains("that version is no longer published"),
      "{err}"
    );
  }
}
