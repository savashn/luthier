//! What is installed, and which files belong to it.
//!
//! The manager never infers ownership by scanning plugin directories (§19).
//! Every file it creates is recorded here, and uninstallation consults only
//! this record — so a plugin that happens to share a name with a package is
//! never touched, and a file the user modified is reported rather than deleted.

use crate::error::{Error, Result, StateError};
use crate::fsutil;
use crate::layout::Layout;
use jiff::Timestamp;
use luthier_manifest::{Format, PackageId, Sha256Hash};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// On-disk revision of the state document.
pub const STATE_VERSION: u32 = 1;

/// Why a package is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallReason {
  /// The user asked for it by name. Never an orphan.
  Explicit,
  /// Pulled in to satisfy another package's dependency.
  Dependency,
}

/// One file inside an installed bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleFile {
  /// Path relative to the bundle root.
  pub path: String,
  pub sha256: Sha256Hash,
  pub mode: u32,
}

/// Something the manager created and is therefore responsible for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InstalledEntry {
  /// A single file, such as a `.clap`.
  File {
    path: PathBuf,
    sha256: Sha256Hash,
    mode: u32,
  },
  /// A directory treated as one unit, such as a `.vst3`.
  ///
  /// The contents are recorded individually, not just the root, which is
  /// what lets `luthier verify` detect a tampered-with bundle and what lets
  /// removal notice a file the user edited.
  Bundle {
    path: PathBuf,
    contents: Vec<BundleFile>,
  },
  /// A directory the manager created and may remove if it ends up empty.
  Dir { path: PathBuf },
}

impl InstalledEntry {
  pub fn path(&self) -> &Path {
    match self {
      InstalledEntry::File { path, .. }
      | InstalledEntry::Bundle { path, .. }
      | InstalledEntry::Dir { path } => path,
    }
  }
}

/// Where an artifact came from, kept so `verify` and the cache can find it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
  pub url: String,
  pub sha256: Sha256Hash,
}

/// One installed package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledPackage {
  pub id: PackageId,
  pub name: String,
  pub version: Version,
  /// Which registry the manifest came from.
  pub registry: String,
  /// Digest of the manifest used, so registry drift is detectable.
  pub manifest_digest: Sha256Hash,
  pub reason: InstallReason,
  /// Held at this version by `luthier pin`; normal updates skip it (§27).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub pin: Option<Version>,
  pub installed_at: Timestamp,
  pub formats: Vec<Format>,
  pub artifacts: Vec<ArtifactRecord>,
  pub dependencies: Vec<PackageId>,
  pub files: Vec<InstalledEntry>,
}

impl InstalledPackage {
  pub fn is_pinned(&self) -> bool {
    self.pin.is_some()
  }
}

/// The whole installation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
  pub version: u32,
  #[serde(default)]
  pub packages: BTreeMap<PackageId, InstalledPackage>,
}

impl Default for State {
  fn default() -> Self {
    Self {
      version: STATE_VERSION,
      packages: BTreeMap::new(),
    }
  }
}

impl State {
  pub fn get(&self, id: &PackageId) -> Option<&InstalledPackage> {
    self.packages.get(id)
  }

  pub fn is_installed(&self, id: &PackageId) -> bool {
    self.packages.contains_key(id)
  }

  /// Installed packages that depend on `id`.
  ///
  /// The basis for §23: a dependency is never removed while something still
  /// needs it, and the CLI can say exactly what.
  pub fn dependents_of(&self, id: &PackageId) -> Vec<PackageId> {
    let mut dependents: Vec<PackageId> = self
      .packages
      .values()
      .filter(|p| &p.id != id && p.dependencies.contains(id))
      .map(|p| p.id.clone())
      .collect();
    dependents.sort();
    dependents
  }

  /// Packages installed only as dependencies that nothing needs any more (§24).
  pub fn orphans(&self) -> Vec<PackageId> {
    let mut orphans: Vec<PackageId> = self
      .packages
      .values()
      .filter(|p| p.reason == InstallReason::Dependency)
      .filter(|p| self.dependents_of(&p.id).is_empty())
      .map(|p| p.id.clone())
      .collect();
    orphans.sort();
    orphans
  }

  /// Every path the manager owns, for duplicate and conflict detection.
  pub fn owned_paths(&self) -> BTreeMap<PathBuf, PackageId> {
    let mut owned = BTreeMap::new();
    for package in self.packages.values() {
      for entry in &package.files {
        owned.insert(entry.path().to_path_buf(), package.id.clone());
      }
    }
    owned
  }

  /// Which package owns `path`, if any.
  pub fn owner_of(&self, path: &Path) -> Option<&InstalledPackage> {
    self
      .packages
      .values()
      .find(|p| p.files.iter().any(|entry| entry.path() == path))
  }

  fn parse(bytes: &[u8], path: &Path) -> Result<Self> {
    let state: State = serde_json::from_slice(bytes).map_err(|e| {
      Error::State(StateError::Corrupt {
        path: path.to_path_buf(),
        reason: e.to_string(),
      })
    })?;
    if state.version > STATE_VERSION {
      return Err(Error::State(StateError::Corrupt {
        path: path.to_path_buf(),
        reason: format!(
          "written by a newer version of Luthier (state v{}, this build understands \
                     v{STATE_VERSION})",
          state.version
        ),
      }));
    }
    Ok(state)
  }
}

/// Reads the state document, returning an empty state if there is none yet.
///
/// Takes no lock: safe for read-only commands, and a partially written file is
/// impossible because writes go through an atomic rename.
pub fn load(layout: &Layout) -> Result<State> {
  let path = layout.state_file();
  match std::fs::read(&path) {
    Ok(bytes) => State::parse(&bytes, &path),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
    Err(e) => Err(Error::io("read", &path, e)),
  }
}

/// Exclusive access to the state for the duration of a mutating operation.
///
/// The lock is advisory and process-wide; it stops two `luthier install` runs
/// from interleaving writes to the same plugin directories.
pub struct StateGuard {
  _lock: std::fs::File,
  path: PathBuf,
  backup: PathBuf,
  state: State,
}

impl StateGuard {
  /// Acquires the lock and reads current state.
  pub fn acquire(layout: &Layout) -> Result<Self> {
    fsutil::ensure_dir(&layout.state_dir())?;
    let lock_path = layout.lock_file();
    let lock = std::fs::OpenOptions::new()
      .create(true)
      .read(true)
      .write(true)
      .truncate(false)
      .open(&lock_path)
      .map_err(|e| Error::io("open lock file", &lock_path, e))?;

    // std's advisory file locking (stable since 1.89) is enough here and
    // avoids a dependency: `flock` on Unix, `LockFileEx` on Windows.
    lock
      .try_lock()
      .map_err(|_| Error::State(StateError::Locked))?;

    Ok(Self {
      _lock: lock,
      path: layout.state_file(),
      backup: layout.state_backup_file(),
      state: load(layout)?,
    })
  }

  pub fn state(&self) -> &State {
    &self.state
  }

  pub fn state_mut(&mut self) -> &mut State {
    &mut self.state
  }

  /// Writes the state out, keeping the previous copy as `.bak`.
  pub fn commit(&mut self) -> Result<()> {
    if let Ok(previous) = std::fs::read(&self.path) {
      fsutil::write_atomic(&self.backup, &previous)?;
    }
    self.state.version = STATE_VERSION;
    let mut bytes = serde_json::to_vec_pretty(&self.state)
      .map_err(|e| Error::io("serialise state", &self.path, std::io::Error::other(e)))?;
    bytes.push(b'\n');
    fsutil::write_atomic(&self.path, &bytes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn package(id: &str, reason: InstallReason, deps: &[&str]) -> InstalledPackage {
    InstalledPackage {
      id: PackageId::new(id).unwrap(),
      name: id.to_owned(),
      version: Version::new(1, 0, 0),
      registry: "default".into(),
      manifest_digest: Sha256Hash::from_bytes([0; 32]),
      reason,
      pin: None,
      installed_at: Timestamp::UNIX_EPOCH,
      formats: vec![Format::Clap],
      artifacts: vec![],
      dependencies: deps.iter().map(|d| PackageId::new(*d).unwrap()).collect(),
      files: vec![InstalledEntry::File {
        path: PathBuf::from(format!("/home/u/.clap/{id}.clap")),
        sha256: Sha256Hash::from_bytes([1; 32]),
        mode: 0o755,
      }],
    }
  }

  fn state_with(packages: Vec<InstalledPackage>) -> State {
    let mut state = State::default();
    for p in packages {
      state.packages.insert(p.id.clone(), p);
    }
    state
  }

  #[test]
  fn a_shared_dependency_reports_every_dependent() {
    // §23's example: removing VSCO 2 must not take sfizz with it, because
    // another-piano still needs it.
    let state = state_with(vec![
      package("vsco2", InstallReason::Explicit, &["sfizz"]),
      package("another-piano", InstallReason::Explicit, &["sfizz"]),
      package("sfizz", InstallReason::Dependency, &[]),
    ]);
    let sfizz = PackageId::new("sfizz").unwrap();
    assert_eq!(
      state.dependents_of(&sfizz),
      vec![
        PackageId::new("another-piano").unwrap(),
        PackageId::new("vsco2").unwrap()
      ]
    );
    assert!(state.orphans().is_empty());
  }

  #[test]
  fn orphans_are_dependencies_nothing_needs_any_more() {
    let state = state_with(vec![
      package("sfizz", InstallReason::Dependency, &[]),
      package("old-preset-pack", InstallReason::Dependency, &[]),
      package("surge-xt", InstallReason::Explicit, &[]),
    ]);
    // An explicitly requested package is never an orphan, even with no dependents.
    assert_eq!(
      state.orphans(),
      vec![
        PackageId::new("old-preset-pack").unwrap(),
        PackageId::new("sfizz").unwrap()
      ]
    );
  }

  #[test]
  fn ownership_lookup_finds_the_right_package() {
    let state = state_with(vec![package("surge-xt", InstallReason::Explicit, &[])]);
    let owned = state
      .owner_of(Path::new("/home/u/.clap/surge-xt.clap"))
      .unwrap();
    assert_eq!(owned.id.as_str(), "surge-xt");
    // A plugin with a similar name is not ours.
    assert!(
      state
        .owner_of(Path::new("/home/u/.clap/surge-xt-effects.clap"))
        .is_none()
    );
  }

  #[test]
  fn state_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    assert!(load(&layout).unwrap().packages.is_empty());

    let mut guard = StateGuard::acquire(&layout).unwrap();
    guard.state_mut().packages.insert(
      PackageId::new("surge-xt").unwrap(),
      package("surge-xt", InstallReason::Explicit, &[]),
    );
    guard.commit().unwrap();
    drop(guard);

    let reloaded = load(&layout).unwrap();
    assert_eq!(reloaded.packages.len(), 1);
    assert!(reloaded.is_installed(&PackageId::new("surge-xt").unwrap()));
  }

  #[test]
  fn a_second_process_cannot_take_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let _first = StateGuard::acquire(&layout).unwrap();
    match StateGuard::acquire(&layout) {
      Err(Error::State(StateError::Locked)) => {}
      Err(other) => panic!("expected a lock conflict, got {other}"),
      Ok(_) => panic!("a second guard must not be able to take the lock"),
    }
  }

  #[test]
  fn committing_keeps_the_previous_copy() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    {
      let mut guard = StateGuard::acquire(&layout).unwrap();
      guard.state_mut().packages.insert(
        PackageId::new("dexed").unwrap(),
        package("dexed", InstallReason::Explicit, &[]),
      );
      guard.commit().unwrap();
    }
    {
      let mut guard = StateGuard::acquire(&layout).unwrap();
      guard.state_mut().packages.clear();
      guard.commit().unwrap();
    }
    assert!(layout.state_backup_file().exists());
    let backup: State =
      serde_json::from_slice(&std::fs::read(layout.state_backup_file()).unwrap()).unwrap();
    assert_eq!(backup.packages.len(), 1);
  }

  #[test]
  fn a_corrupt_state_file_is_reported_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    fsutil::ensure_dir(&layout.state_dir()).unwrap();
    std::fs::write(layout.state_file(), b"{ not json").unwrap();
    let err = load(&layout).unwrap_err();
    assert!(
      matches!(err, Error::State(StateError::Corrupt { .. })),
      "{err}"
    );
    assert!(err.hint().unwrap().contains(".bak"));
  }

  #[test]
  fn state_from_a_newer_build_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    fsutil::ensure_dir(&layout.state_dir()).unwrap();
    std::fs::write(layout.state_file(), br#"{"version": 99, "packages": {}}"#).unwrap();
    let err = load(&layout).unwrap_err();
    assert!(err.to_string().contains("newer version"), "{err}");
  }
}
