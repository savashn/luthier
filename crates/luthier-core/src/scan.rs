//! Detecting plugins already on the system.
//!
//! Scanning never establishes ownership — that comes from the state file
//! (§19) — but it answers questions the state cannot: whether a plugin the
//! user installed by hand is sitting where we are about to write (§29, §30),
//! whether an `external` dependency is present, and whether anything that can
//! play a library's content — an SFZ engine, DrumGizmo — is.
//!
//! They have two different scopes, which is why they have different
//! functions. [`scan`] looks only at the roots this manager installs into,
//! because that is the only place a destination collision can happen.
//! [`detect_externals`] and [`detect_engines`] also search the read-only system
//! roots, because an `external` package is by definition something the
//! distribution or a container provided, and an engine very often is.

use crate::error::{Error, Result};
use crate::layout::Layout;
use crate::registry::RegistryIndex;
use crate::state::State;
use luthier_manifest::{DetectRule, Format, Manifest, PackageId, PackageKind};
use semver::Version;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Whether Luthier put a plugin where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
  Managed {
    package: PackageId,
    version: Version,
  },
  /// Present but not ours: installed by hand, by a distribution package, or
  /// by a vendor installer. Never modified without the user asking.
  Unmanaged,
}

/// One plugin found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedPlugin {
  pub path: PathBuf,
  pub format: Format,
  /// The filename, e.g. `Surge XT.clap`.
  pub name: String,
  pub status: PluginStatus,
}

impl DetectedPlugin {
  pub fn is_managed(&self) -> bool {
    matches!(self.status, PluginStatus::Managed { .. })
  }
}

/// Lists every plugin in the layout's plugin roots.
///
/// Missing roots are not an error: a user with no VST3 plugins simply has no
/// `~/.vst3`.
pub fn scan(layout: &Layout, state: &State) -> Result<Vec<DetectedPlugin>> {
  let mut found = Vec::new();

  for (format, root) in layout.plugin_roots() {
    let Some(extension) = format.extension() else {
      continue;
    };
    if !root.is_dir() {
      continue;
    }

    let entries = std::fs::read_dir(root).map_err(|e| Error::io("list", root, e))?;
    for entry in entries {
      let entry = entry.map_err(|e| Error::io("list", root, e))?;
      let path = entry.path();
      let name = entry.file_name().to_string_lossy().into_owned();

      // Our own staging and backup files are not plugins.
      if name.starts_with('.') {
        continue;
      }
      if path.extension().and_then(|e| e.to_str()) != Some(extension) {
        continue;
      }

      let status = match state.owner_of(&path) {
        Some(owner) => PluginStatus::Managed {
          package: owner.id.clone(),
          version: owner.version.clone(),
        },
        None => PluginStatus::Unmanaged,
      };
      found.push(DetectedPlugin {
        path,
        format: format.clone(),
        name,
        status,
      });
    }
  }

  found.sort_by(|a, b| a.path.cmp(&b.path));
  Ok(found)
}

/// Which `external` packages from the registry are present on this machine.
///
/// A package is satisfied when any one of its detect rules matches, which
/// covers software shipping in several formats where only one may be installed.
pub fn detect_externals(layout: &Layout, index: &RegistryIndex) -> BTreeSet<PackageId> {
  index
    .packages
    .values()
    .map(|entry| &entry.manifest)
    .filter(|manifest| manifest.kind == PackageKind::External)
    .filter(|manifest| locate_external(layout, manifest).is_some())
    .map(|manifest| manifest.id.clone())
    .collect()
}

/// Where an `external` package was found, if it is present.
///
/// Searched in order: the roots this manager installs into, then the read-only
/// system roots. The managed root comes first so a user who installed a build
/// by hand into `~/.lv2` sees that one reported rather than a distribution copy.
///
/// Searching the system roots is the whole point for this package kind. sfizz's
/// own manifest tells the user to install it with their distribution's package
/// manager; looking only under `$HOME` would mean never finding what that
/// instruction produces.
pub fn locate_external(layout: &Layout, manifest: &Manifest) -> Option<PathBuf> {
  locate(layout, &manifest.detect)
}

/// Engines from the registry's `engines.toml` that are present on this
/// machine, found by their detect rules.
///
/// An entry's own rules are tried alongside any its package's manifest
/// carries: sfizz's live in `sfizz.toml`, while DrumCraker's registry has no
/// field for them. A copy this manager installed needs no rule at all — the
/// caller already knows it from the state file.
pub fn detect_engines(layout: &Layout, index: &RegistryIndex) -> BTreeSet<PackageId> {
  index
    .engines
    .iter()
    .filter(|engine| {
      let from_manifest = index
        .packages
        .get(&engine.package)
        .map(|entry| entry.manifest.detect.as_slice())
        .unwrap_or_default();
      locate(layout, &engine.detect)
        .or_else(|| locate(layout, from_manifest))
        .is_some()
    })
    .map(|engine| engine.package.clone())
    .collect()
}

/// The first place any of `rules` matches, managed roots before system ones.
fn locate(layout: &Layout, rules: &[DetectRule]) -> Option<PathBuf> {
  rules.iter().find_map(|rule| {
    let managed = layout
      .plugin_root(&rule.format)
      .map(|root| root.join(rule.name.as_str()));
    let system = layout
      .system_roots(&rule.format)
      .iter()
      .map(|root| root.join(rule.name.as_str()));

    managed.into_iter().chain(system).find(|path| path.exists())
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::state::{InstallReason, InstalledEntry, InstalledPackage};
  use luthier_manifest::{ParseMode, Sha256Hash};
  use std::collections::BTreeMap;

  fn layout_with_plugins(dir: &std::path::Path) -> Layout {
    let layout = Layout::rooted_at(dir);
    for (_, root) in layout.plugin_roots() {
      std::fs::create_dir_all(root).unwrap();
    }
    layout
  }

  #[test]
  fn an_unmanaged_plugin_is_reported_as_such() {
    // Exactly the situation on a machine where Surge XT was installed by
    // hand before Luthier ever ran.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let clap_root = layout.plugin_root(&Format::Clap).unwrap();
    std::fs::write(clap_root.join("Surge XT.clap"), b"\x7fELF").unwrap();

    let found = scan(&layout, &State::default()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Surge XT.clap");
    assert_eq!(found[0].format, Format::Clap);
    assert_eq!(found[0].status, PluginStatus::Unmanaged);
  }

  #[test]
  fn a_recorded_plugin_is_reported_as_managed() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let path = layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .join("Dexed.clap");
    std::fs::write(&path, b"\x7fELF").unwrap();

    let mut state = State::default();
    let id = PackageId::new("dexed").unwrap();
    state.packages.insert(
      id.clone(),
      InstalledPackage {
        id: id.clone(),
        name: "Dexed".into(),
        version: Version::new(1, 0, 1),
        registry: "default".into(),
        manifest_digest: Sha256Hash::from_bytes([0; 32]),
        reason: InstallReason::Explicit,
        pin: None,
        installed_at: jiff::Timestamp::UNIX_EPOCH,
        formats: vec![Format::Clap],
        artifacts: vec![],
        dependencies: vec![],
        files: vec![InstalledEntry::File {
          path: path.clone(),
          sha256: Sha256Hash::from_bytes([1; 32]),
          mode: 0o755,
        }],
      },
    );

    let found = scan(&layout, &state).unwrap();
    assert_eq!(
      found[0].status,
      PluginStatus::Managed {
        package: id,
        version: Version::new(1, 0, 1)
      }
    );
  }

  #[test]
  fn staging_and_backup_files_are_not_mistaken_for_plugins() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let root = layout.plugin_root(&Format::Clap).unwrap();
    std::fs::write(root.join(".luthier-stage-123-Surge XT.clap"), b"x").unwrap();
    std::fs::write(root.join(".luthier-backup-123-Surge XT.clap"), b"x").unwrap();
    std::fs::write(root.join("README.txt"), b"x").unwrap();

    assert!(scan(&layout, &State::default()).unwrap().is_empty());
  }

  #[test]
  fn missing_plugin_directories_are_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    assert!(scan(&layout, &State::default()).unwrap().is_empty());
  }

  /// The sfizz manifest, which is the case this whole mechanism exists for.
  fn external_registry(dir: &std::path::Path) -> crate::registry::RegistryIndex {
    let registry = dir.join("registry");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::write(
      registry.join("sfizz.toml"),
      concat!(
        "schema = 1\n",
        "id = \"sfizz\"\n",
        "name = \"sfizz\"\n",
        "kind = \"external\"\n",
        "category = \"instrument\"\n",
        "license = { kind = \"open-source\", spdx = \"BSD-2-Clause\" }\n",
        "detect = [\n",
        "  { format = \"vst3\", name = \"sfizz.vst3\" },\n",
        "  { format = \"lv2\", name = \"sfizz.lv2\" },\n",
        "]\n",
      ),
    )
    .unwrap();
    use crate::registry::RegistryProvider;
    crate::registry::LocalRegistry::new("default", &registry)
      .load_index()
      .unwrap()
  }

  #[test]
  fn an_external_installed_by_the_system_is_detected() {
    // The bug this covers: sfizz's own manifest tells the user to install
    // it with their distribution's package manager, but detection only
    // looked under $HOME, so following that instruction never satisfied
    // the dependency. A container image that bakes sfizz into
    // /usr/lib/lv2 hit exactly the same wall.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let index = external_registry(dir.path());

    let system = dir.path().join("usr/lib/lv2");
    std::fs::create_dir_all(&system).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Lv2, vec![system.clone()])]));

    assert!(
      detect_externals(&layout, &index).is_empty(),
      "nothing is installed yet"
    );

    std::fs::create_dir_all(system.join("sfizz.lv2")).unwrap();
    assert!(detect_externals(&layout, &index).contains(&PackageId::new("sfizz").unwrap()));
  }

  #[test]
  fn a_managed_copy_is_reported_ahead_of_a_system_one() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let index = external_registry(dir.path());

    let system = dir.path().join("usr/lib/lv2");
    std::fs::create_dir_all(system.join("sfizz.lv2")).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Lv2, vec![system])]));

    let managed = layout.plugin_root(&Format::Lv2).unwrap().join("sfizz.lv2");
    std::fs::create_dir_all(&managed).unwrap();

    let manifest = &index.packages[&PackageId::new("sfizz").unwrap()].manifest;
    assert_eq!(locate_external(&layout, manifest), Some(managed));
  }

  #[test]
  fn scanning_ignores_system_roots() {
    // scan() answers "is our destination occupied". A distribution plugin
    // in /usr/lib is not in any destination, and reporting it would make
    // the install-time collision check refuse valid installs.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());

    let system = dir.path().join("usr/lib/clap");
    std::fs::create_dir_all(&system).unwrap();
    std::fs::write(system.join("Distro.clap"), b"\x7fELF").unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Clap, vec![system])]));

    assert!(scan(&layout, &State::default()).unwrap().is_empty());
  }

  #[test]
  fn an_external_package_is_detected_by_its_rules() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());

    let registry = dir.path().join("registry");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::write(
      registry.join("sfizz.toml"),
      concat!(
        "schema = 1\n",
        "id = \"sfizz\"\n",
        "name = \"sfizz\"\n",
        "kind = \"external\"\n",
        "category = \"instrument\"\n",
        "license = { kind = \"open-source\", spdx = \"BSD-2-Clause\" }\n",
        "detect = [\n",
        "  { format = \"vst3\", name = \"sfizz.vst3\" },\n",
        "  { format = \"lv2\", name = \"sfizz.lv2\" },\n",
        "]\n",
      ),
    )
    .unwrap();
    let index = crate::registry::LocalRegistry::new("default", &registry);
    use crate::registry::RegistryProvider;
    let index = index.load_index().unwrap();
    let _ = ParseMode::Lenient;

    assert!(detect_externals(&layout, &index).is_empty());

    // Only the LV2 build is installed; one matching rule is enough.
    std::fs::create_dir_all(layout.plugin_root(&Format::Lv2).unwrap().join("sfizz.lv2")).unwrap();
    let detected = detect_externals(&layout, &index);
    assert_eq!(detected.len(), 1);
    assert!(detected.contains(&PackageId::new("sfizz").unwrap()));
  }

  #[test]
  fn an_engine_is_found_by_its_own_rules_or_by_its_packages() {
    // DrumCraker comes from a registry with no detect field, so its rule
    // lives in engines.toml; sfizz's lives in its manifest.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let mut index = external_registry(dir.path());
    index.engines = luthier_manifest::EnginesFile::parse(
      r#"
[[engine]]
package = "sfizz"
plays = ["sfz"]

[[engine]]
package = "drumcraker"
plays = ["drumgizmo"]
detect = [{ format = "vst3", name = "DrumCraker.vst3" }]
"#,
    )
    .unwrap()
    .entries;

    assert!(detect_engines(&layout, &index).is_empty());

    let system = dir.path().join("usr/lib/vst3");
    std::fs::create_dir_all(system.join("DrumCraker.vst3")).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Vst3, vec![system])]));
    assert_eq!(
      detect_engines(&layout, &index),
      BTreeSet::from([PackageId::new("drumcraker").unwrap()])
    );

    std::fs::create_dir_all(layout.plugin_root(&Format::Lv2).unwrap().join("sfizz.lv2")).unwrap();
    assert_eq!(detect_engines(&layout, &index).len(), 2);
  }
}
