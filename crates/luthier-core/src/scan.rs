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
//! [`detect_externals`] and [`detect_engines`] also search the system roots,
//! which this module only reads, because an `external` package is by
//! definition something the distribution or a container provided, and an
//! engine very often is.
//! [`SystemPlugins`] walks only those, vendors' folders included, for a
//! second copy of a plugin about to be installed.

use crate::error::{Error, Result};
use crate::layout::Layout;
use crate::registry::RegistryIndex;
use crate::state::State;
use luthier_manifest::{DetectRule, Format, Manifest, PackageId, PackageKind};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

/// Engines the merged index knows — the built-in list plus any source's
/// `engines.toml` — that are present on this machine, found by their detect
/// rules.
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

/// How far below a system root a vendor's folders are followed. CLAP and
/// VST3 hosts search their directories recursively, and packaged plugins
/// sit a level or two down (`/usr/lib/vst3/<vendor>/X.vst3`); a bound keeps
/// a link that loops, or a root that is somebody's whole disk, from being
/// walked without end.
const VENDOR_DEPTH: usize = 3;

/// Every plugin the system roots hold, by format and name, found in one
/// pass: a plan asks once for each install rule, and a host finds a plugin
/// in a vendor's folder as readily as at the top.
#[derive(Debug, Default)]
pub struct SystemPlugins {
  found: BTreeMap<(Format, String), Vec<Found>>,
}

impl SystemPlugins {
  /// Walks the system roots of `layout`.
  ///
  /// Below each CLAP and VST3 root, plain folders are followed
  /// [`VENDOR_DEPTH`] deep, never into a plugin, which is a file or a
  /// bundle; an LV2 host loads only the bundles directly in each directory,
  /// so nothing below them is a plugin it finds. The managed root is walked
  /// too, where it is a system root (`~/.clap` until plugins move): a
  /// vendor's installer puts its own folder in `~/.vst3`, which a host
  /// searches like any other.
  pub fn of(layout: &Layout) -> Self {
    let mut found: BTreeMap<(Format, String), Vec<Found>> = BTreeMap::new();
    for (format, root) in layout.all_system_roots() {
      let Some(extension) = format.extension() else {
        continue;
      };
      let depth = if *format == Format::Lv2 {
        0
      } else {
        VENDOR_DEPTH
      };
      walk(root, extension, depth, &mut BTreeSet::new(), &mut |path| {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
          found
            .entry((format.clone(), name.to_string()))
            .or_default()
            .push(Found {
              root: root.to_path_buf(),
              path,
            });
        }
      });
    }
    Self { found }
  }

  /// Every copy of a plugin named `name` the system roots of `format` hold.
  ///
  /// A host searches those roots as well as the one this manager installs
  /// into, so a second copy there means the plugin is listed twice, or
  /// loaded from whichever the host reads first. Matched by name alone, as
  /// detection is: a distribution that names its bundle differently goes
  /// unnoticed. A copy gone since the walk is not reported.
  ///
  /// Each copy once, however many roots reach it: on Arch `/usr/lib64` is a
  /// link to `/usr/lib`. Told apart by the root it was found under, resolved,
  /// and where below it, not by what it links to: two Nix profiles each link
  /// a plugin, or a vendor's folder of them, into the same store path, and
  /// a host finds it through both.
  ///
  /// The managed copy is not a second one, however it is reached: at the
  /// top of the managed root, decided by name so a path that fails to
  /// resolve cannot make it one; through a search path naming the managed
  /// root by another spelling; or through a link to it, as plugins moved
  /// with `location set plugins` are often linked back into `~/.clap` for a
  /// host that searches nowhere else. Decided when asked, by where it really
  /// is, so a copy this manager has just placed is known for its own.
  pub fn copies_of(&self, layout: &Layout, format: &Format, name: &str) -> Vec<PathBuf> {
    let Some(found) = self.found.get(&(format.clone(), name.to_string())) else {
      return Vec::new();
    };
    let managed = layout.plugin_root(format).map(|root| root.join(name));
    let managed_copy = managed.as_ref().and_then(|path| path.canonicalize().ok());
    let mut seen = BTreeSet::new();
    found
      .iter()
      .filter(|found| found.path.exists())
      .filter(|found| managed.as_ref() != Some(&found.path))
      .filter(|found| {
        managed_copy
          .as_ref()
          .is_none_or(|managed| found.path.canonicalize().ok().as_ref() != Some(managed))
      })
      .filter(|found| {
        let root = found
          .root
          .canonicalize()
          .unwrap_or_else(|_| found.root.clone());
        let below = found.path.strip_prefix(&found.root).unwrap_or(&found.path);
        seen.insert(root.join(below))
      })
      .map(|found| found.path.clone())
      .collect()
  }
}

/// One plugin a walk found, and the system root it was found under.
#[derive(Debug)]
struct Found {
  root: PathBuf,
  path: PathBuf,
}

/// Every copy of a plugin named `name` the system roots of `format` hold;
/// [`SystemPlugins::copies_of`], for one question. A walk of every root
/// for each, so for tests: a command asks the session's `SystemPlugins`.
#[cfg(test)]
fn on_system(layout: &Layout, format: &Format, name: &str) -> Vec<PathBuf> {
  SystemPlugins::of(layout).copies_of(layout, format, name)
}

/// Calls `found` for each plugin under `dir` with `extension`, in name
/// order, following plain folders `depth` levels down.
///
/// Links are followed, since a Nix profile is made of them, but no folder
/// twice: `visited` holds where each one really is, so a link back up the
/// tree ends at once instead of at the depth. One set per root — two roots
/// reaching one folder are two places a host finds what is in it.
fn walk(
  dir: &Path,
  extension: &str,
  depth: usize,
  visited: &mut BTreeSet<PathBuf>,
  found: &mut impl FnMut(PathBuf),
) {
  if !visited.insert(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())) {
    return;
  }
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  // Sorted: what is found, and in which order, must not depend on the
  // order a filesystem lists a directory in.
  let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
  paths.sort();
  for path in paths {
    let hidden = path
      .file_name()
      .is_some_and(|n| n.to_string_lossy().starts_with('.'));
    if hidden {
      continue;
    }
    if path.extension().and_then(|e| e.to_str()) == Some(extension) {
      // A link to nothing is no plugin a host loads.
      if path.exists() {
        found(path);
      }
    } else if depth > 0 && path.is_dir() {
      walk(&path, extension, depth - 1, visited, found);
    }
  }
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
        content: vec![],
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

  #[test]
  fn a_plugin_a_system_root_holds_is_found_there() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let system = dir.path().join("usr/lib/lv2");
    std::fs::create_dir_all(system.join("lsp-plugins.lv2")).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Lv2, vec![system.clone()])]));

    assert_eq!(
      on_system(&layout, &Format::Lv2, "lsp-plugins.lv2"),
      [system.join("lsp-plugins.lv2")]
    );
    assert!(on_system(&layout, &Format::Lv2, "other.lv2").is_empty());
    // Another format's roots are not searched for it.
    assert!(on_system(&layout, &Format::Clap, "lsp-plugins.lv2").is_empty());
  }

  #[test]
  fn every_copy_is_found_and_each_only_once() {
    // NixOS with the plugin in the system profile and in the user's, each
    // a link to the same store path: a host finds it through both, so both
    // are named, and removing one does not reveal the other a run later.
    // Arch links /usr/lib64 to /usr/lib: one copy, however it is reached.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let store = dir
      .path()
      .join("nix/store/abc-lsp-plugins/lib/lv2/lsp-plugins.lv2");
    std::fs::create_dir_all(&store).unwrap();
    let system = dir.path().join("run/current-system/sw/lib/lv2");
    let profile = dir.path().join("nix-profile/lib/lv2");
    for root in [&system, &profile] {
      std::fs::create_dir_all(root).unwrap();
      std::os::unix::fs::symlink(&store, root.join("lsp-plugins.lv2")).unwrap();
    }
    let link = dir.path().join("lib64-lv2");
    std::os::unix::fs::symlink(&system, &link).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(
      Format::Lv2,
      vec![system.clone(), link, profile.clone()],
    )]));

    assert_eq!(
      on_system(&layout, &Format::Lv2, "lsp-plugins.lv2"),
      [
        system.join("lsp-plugins.lv2"),
        profile.join("lsp-plugins.lv2")
      ]
    );
  }

  #[test]
  fn a_managed_root_on_the_search_path_is_not_the_system() {
    // `LV2_PATH=~/.lv2:/usr/lib/lv2` is a common setting. The copy in
    // `~/.lv2` is the one this manager installed, not a second one.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let managed = layout.plugin_root(&Format::Lv2).unwrap().to_path_buf();
    std::fs::create_dir_all(managed.join("lsp-plugins.lv2")).unwrap();
    // Spelt differently, through a symlink, which only canonical paths
    // tell apart.
    let alias = dir.path().join("lv2-alias");
    std::os::unix::fs::symlink(&managed, &alias).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Lv2, vec![managed, alias])]));

    assert!(on_system(&layout, &Format::Lv2, "lsp-plugins.lv2").is_empty());
  }

  #[test]
  fn an_external_left_in_the_home_directory_is_found_once_plugins_move() {
    // sfizz installed by hand in ~/.lv2, then `location set plugins`: a
    // host still loads it from there, so it is still present.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let index = external_registry(dir.path());
    let home_lv2 = layout.plugin_root(&Format::Lv2).unwrap().to_path_buf();
    std::fs::create_dir_all(home_lv2.join("sfizz.lv2")).unwrap();
    let disk = dir.path().join("disk");
    std::fs::create_dir_all(&disk).unwrap();

    let moved = layout
      .with_system_roots(BTreeMap::from([(Format::Lv2, vec![home_lv2])]))
      .with_locations(&crate::layout::Locations {
        plugins: Some(disk),
        ..Default::default()
      });
    assert!(detect_externals(&moved, &index).contains(&PackageId::new("sfizz").unwrap()));
  }

  #[test]
  fn a_link_to_the_managed_copy_is_not_a_second_one() {
    // Plugins moved to another disk, linked back into ~/.clap for a host
    // that looks nowhere else: one copy, reached twice.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let managed = layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .join("Linked.clap");
    std::fs::write(&managed, b"\x7fELF").unwrap();
    let system = dir.path().join("home-clap");
    std::fs::create_dir_all(&system).unwrap();
    std::os::unix::fs::symlink(&managed, system.join("Linked.clap")).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Clap, vec![system.clone()])]));

    assert!(on_system(&layout, &Format::Clap, "Linked.clap").is_empty());

    // A file of its own there is a second copy.
    std::fs::remove_file(system.join("Linked.clap")).unwrap();
    std::fs::write(system.join("Linked.clap"), b"\x7fELF").unwrap();
    assert_eq!(
      on_system(&layout, &Format::Clap, "Linked.clap"),
      [system.join("Linked.clap")]
    );
  }

  #[test]
  fn a_copy_left_in_the_home_directory_is_second_once_plugins_move() {
    // Before `location set plugins`, ~/.clap is where this manager
    // installs and a copy there is its own; after, a host still finds it,
    // beside the one on the other disk. Whichever order the layout was
    // built in.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let left = home.join(".clap/Left.clap");
    std::fs::create_dir_all(left.parent().unwrap()).unwrap();
    std::fs::write(&left, b"\x7fELF").unwrap();
    let disk = dir.path().join("disk/plugins");
    std::fs::create_dir_all(&disk).unwrap();
    let system = BTreeMap::from([(Format::Clap, vec![home.join(".clap")])]);
    let moved = crate::layout::Locations {
      plugins: Some(disk),
      ..Default::default()
    };

    let at_home = Layout::rooted_at(&home).with_system_roots(system.clone());
    assert!(on_system(&at_home, &Format::Clap, "Left.clap").is_empty());

    for layout in [
      at_home.clone().with_locations(&moved),
      Layout::rooted_at(&home)
        .with_locations(&moved)
        .with_system_roots(system.clone()),
    ] {
      assert_eq!(
        on_system(&layout, &Format::Clap, "Left.clap"),
        std::slice::from_ref(&left)
      );
      // Still never somewhere a delete may reach.
      assert!(!layout.is_managed_location(&left));
    }
  }

  #[test]
  fn a_copy_in_a_vendor_s_folder_is_found_as_a_host_finds_it() {
    // CLAP and VST3 hosts search below their directories, and packages put
    // plugins in a vendor's folder; an LV2 host looks only at the top.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let system = dir.path().join("usr/lib");
    let vst3 = system.join("vst3/Vendor/Suite/Deep.vst3");
    let clap = system.join("clap/Vendor/Deep.clap");
    let lv2 = system.join("lv2/vendor/deep.lv2");
    std::fs::create_dir_all(&vst3).unwrap();
    std::fs::create_dir_all(&lv2).unwrap();
    std::fs::create_dir_all(clap.parent().unwrap()).unwrap();
    std::fs::write(&clap, b"\x7fELF").unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([
      (Format::Vst3, vec![system.join("vst3")]),
      (Format::Clap, vec![system.join("clap")]),
      (Format::Lv2, vec![system.join("lv2")]),
    ]));

    assert_eq!(on_system(&layout, &Format::Vst3, "Deep.vst3"), [vst3]);
    assert_eq!(on_system(&layout, &Format::Clap, "Deep.clap"), [clap]);
    assert!(on_system(&layout, &Format::Lv2, "deep.lv2").is_empty());
  }

  #[test]
  fn a_plugin_is_never_looked_inside_and_the_walk_has_a_floor() {
    // A bundle is a plugin, not a folder of them; and a vendor's folder is
    // followed only so far, so a link that loops ends.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let root = dir.path().join("vst3");
    std::fs::create_dir_all(root.join("Outer.vst3/Contents/Inner.vst3")).unwrap();
    std::fs::create_dir_all(root.join("a/b/c/d/TooDeep.vst3")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Vst3, vec![root.clone()])]));

    assert_eq!(
      on_system(&layout, &Format::Vst3, "Outer.vst3"),
      [root.join("Outer.vst3")]
    );
    assert!(on_system(&layout, &Format::Vst3, "Inner.vst3").is_empty());
    assert!(on_system(&layout, &Format::Vst3, "TooDeep.vst3").is_empty());
  }

  #[test]
  fn a_vendor_s_folder_in_the_managed_root_is_searched_too() {
    // A vendor's installer puts its folder in ~/.vst3, which a host
    // searches; the bundle this manager placed at the top is its own.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let managed = layout.plugin_root(&Format::Vst3).unwrap().to_path_buf();
    std::fs::create_dir_all(managed.join("Diva.vst3")).unwrap();
    std::fs::create_dir_all(managed.join("u-he/Diva.vst3")).unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Vst3, vec![managed.clone()])]));

    assert_eq!(
      on_system(&layout, &Format::Vst3, "Diva.vst3"),
      [managed.join("u-he/Diva.vst3")]
    );
  }

  #[test]
  fn two_profiles_linking_one_vendor_folder_are_two_copies() {
    // Nix links a vendor's folder, not each bundle, into every profile
    // that has the package: a host finds the plugin through each.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let store = dir.path().join("nix/store/abc-suite/lib/vst3/Vendor");
    std::fs::create_dir_all(store.join("Suite.vst3")).unwrap();
    let system = dir.path().join("run/current-system/sw/lib/vst3");
    let profile = dir.path().join("nix-profile/lib/vst3");
    for root in [&system, &profile] {
      std::fs::create_dir_all(root).unwrap();
      std::os::unix::fs::symlink(&store, root.join("Vendor")).unwrap();
    }
    let layout = layout.with_system_roots(BTreeMap::from([(
      Format::Vst3,
      vec![system.clone(), profile.clone()],
    )]));

    assert_eq!(
      on_system(&layout, &Format::Vst3, "Suite.vst3"),
      [
        system.join("Vendor/Suite.vst3"),
        profile.join("Vendor/Suite.vst3")
      ]
    );
  }

  #[test]
  fn a_copy_gone_since_the_walk_is_not_reported() {
    // The walk is the session's; a copy removed while the user read the
    // plan — `pacman -R` in another terminal — is no longer there to name.
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_with_plugins(dir.path());
    let system = dir.path().join("usr/lib/clap");
    std::fs::create_dir_all(&system).unwrap();
    std::fs::write(system.join("Gone.clap"), b"\x7fELF").unwrap();
    let layout = layout.with_system_roots(BTreeMap::from([(Format::Clap, vec![system.clone()])]));

    let walked = SystemPlugins::of(&layout);
    std::fs::remove_file(system.join("Gone.clap")).unwrap();
    assert!(
      walked
        .copies_of(&layout, &Format::Clap, "Gone.clap")
        .is_empty()
    );
  }
}
