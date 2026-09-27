//! Portable environment files: what `luthier env export` writes and
//! `luthier env import` reads (§51).
//!
//! The point of the format is that a creative setup survives a move to another
//! machine. That means it has to record enough to *reproduce* an installation,
//! not merely list what was in it — so an export pins the exact version of
//! every package, including the ones that arrived as dependencies, and an
//! import feeds those versions to the resolver as hard requirements.
//!
//! It deliberately records no absolute paths, no plugin directories and no
//! environment name to install into. Where things land is the receiving
//! machine's business, decided by its own [`crate::layout::Layout`]; a file
//! that could name destinations would be the same arbitrary-write primitive
//! that manifests are forbidden from being.

use crate::error::{Error, Result, StateError};
use crate::state::{InstallReason, State};
use jiff::Timestamp;
use luthier_manifest::PackageId;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// On-disk revision of the environment-file format.
pub const ENV_FILE_VERSION: u32 = 1;

/// A serialised environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvFile {
  pub meta: Meta,
  /// Every package, sorted by ID so two exports of the same installation are
  /// byte-identical and diff cleanly in version control.
  #[serde(default, rename = "package")]
  pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
  /// Format revision, so a future reader can refuse a file it predates.
  pub schema: u32,
  /// Version of the binary that wrote the file. Informational.
  pub luthier: String,
  #[serde(with = "timestamp_string")]
  pub exported: Timestamp,
  /// The environment this came from. Informational only — import never
  /// switches environments on the strength of a file's contents.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub environment: Option<String>,
  /// Whether `version` fields are authoritative.
  ///
  /// `true` for a normal export: the import must land on exactly these
  /// versions or fail. `false` for `--loose`: versions are absent and the
  /// import takes whatever the registry currently offers.
  pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageEntry {
  pub id: PackageId,
  /// Absent in a `--loose` export.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub version: Option<Version>,
  /// Which registry the manifest came from. Recorded for a reader; an import
  /// resolves by ID against the built-in sources whatever this says, so a
  /// hand-written or generated file may leave it out.
  #[serde(default, skip_serializing_if = "String::is_empty")]
  pub registry: String,
  /// `explicit` packages are what the import asks for by name; dependencies
  /// are recorded so their versions can be reproduced, but are not roots.
  pub reason: InstallReason,
  /// A pin the user had applied, reapplied verbatim on import.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub pin: Option<Version>,
}

/// jiff's `Timestamp` has no serde impl that round-trips through TOML's own
/// datetime type cleanly, so it is written as an RFC 3339 string.
mod timestamp_string {
  use jiff::Timestamp;
  use serde::{Deserialize, Deserializer, Serializer};

  pub fn serialize<S: Serializer>(ts: &Timestamp, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(&ts.to_string())
  }

  pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Timestamp, D::Error> {
    let raw = String::deserialize(de)?;
    raw.parse().map_err(serde::de::Error::custom)
  }
}

impl EnvFile {
  /// Builds an environment file from what is installed.
  ///
  /// `pinned` false produces the `--loose` shape: package identities without
  /// versions, for "give me these, current is fine".
  pub fn from_state(state: &State, environment: Option<&str>, pinned: bool) -> Self {
    let mut packages: Vec<PackageEntry> = state
      .packages
      .values()
      .map(|p| PackageEntry {
        id: p.id.clone(),
        version: pinned.then(|| p.version.clone()),
        registry: p.registry.clone(),
        reason: p.reason,
        pin: p.pin.clone(),
      })
      .collect();
    packages.sort_by(|a, b| a.id.cmp(&b.id));

    Self {
      meta: Meta {
        schema: ENV_FILE_VERSION,
        luthier: env!("CARGO_PKG_VERSION").to_string(),
        exported: Timestamp::now(),
        environment: environment.map(ToOwned::to_owned),
        pinned,
      },
      packages,
    }
  }

  /// The packages an import should ask for by name.
  ///
  /// Dependencies are deliberately excluded: asking for them as roots would
  /// record them as explicitly installed, and `luthier cleanup` would then
  /// never offer to remove them once nothing needed them (§24).
  pub fn roots(&self) -> Vec<PackageId> {
    self
      .packages
      .iter()
      .filter(|p| p.reason == InstallReason::Explicit)
      .map(|p| p.id.clone())
      .collect()
  }

  /// Versions the resolver must land on: every package that names one.
  ///
  /// A `--loose` export names none. A hand-written or generated file may name
  /// some and not others, which is how the Home Manager module holds one
  /// package at a version and lets the rest follow the registry; `pinned`
  /// only asserts that every package names one.
  pub fn required_versions(&self) -> BTreeMap<PackageId, Version> {
    self
      .packages
      .iter()
      .filter_map(|p| p.version.clone().map(|v| (p.id.clone(), v)))
      .collect()
  }

  /// Pins to reapply once the install has succeeded.
  pub fn pins(&self) -> Vec<(PackageId, Version)> {
    self
      .packages
      .iter()
      .filter_map(|p| p.pin.clone().map(|v| (p.id.clone(), v)))
      .collect()
  }

  pub fn to_toml(&self) -> Result<String> {
    toml::to_string_pretty(self).map_err(|e| {
      Error::State(StateError::EnvFile {
        source_name: "<export>".into(),
        reason: format!("could not serialise the environment: {e}"),
      })
    })
  }

  /// Parses an environment file, refusing one written by a newer format.
  pub fn from_toml(text: &str, source_name: &str) -> Result<Self> {
    let file: EnvFile = toml::from_str(text).map_err(|e| {
      Error::State(StateError::EnvFile {
        source_name: source_name.to_owned(),
        reason: e.to_string(),
      })
    })?;
    if file.meta.schema > ENV_FILE_VERSION {
      return Err(Error::State(StateError::EnvFile {
        source_name: source_name.to_owned(),
        reason: format!(
          "environment file is schema v{} but this build understands up to \
                     v{ENV_FILE_VERSION}; upgrade luthier",
          file.meta.schema
        ),
      }));
    }
    if file.meta.pinned && file.packages.iter().any(|p| p.version.is_none()) {
      return Err(Error::State(StateError::EnvFile {
        source_name: source_name.to_owned(),
        reason: "file claims to be pinned but at least one package has no version".into(),
      }));
    }
    Ok(file)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::state::{ArtifactRecord, InstalledPackage};
  use luthier_manifest::{Format, Sha256Hash};

  fn package(id: &str, version: &str, reason: InstallReason) -> InstalledPackage {
    InstalledPackage {
      id: PackageId::new(id).unwrap(),
      name: id.to_string(),
      version: version.parse().unwrap(),
      registry: "default".into(),
      manifest_digest: Sha256Hash::from_bytes([1; 32]),
      reason,
      pin: None,
      installed_at: Timestamp::now(),
      formats: vec![Format::Clap],
      artifacts: vec![ArtifactRecord {
        url: "https://example.invalid/a.zip".into(),
        sha256: Sha256Hash::from_bytes([2; 32]),
      }],
      dependencies: vec![],
      files: vec![],
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
  fn a_pinned_export_round_trips() {
    let state = state_with(vec![
      package("surge-xt", "1.3.4", InstallReason::Explicit),
      package("engine", "0.9.0", InstallReason::Dependency),
    ]);
    let file = EnvFile::from_state(&state, Some("mixing"), true);
    let text = file.to_toml().unwrap();
    let back = EnvFile::from_toml(&text, "env.toml").unwrap();
    assert_eq!(back, file);
    assert_eq!(back.meta.environment.as_deref(), Some("mixing"));
  }

  #[test]
  fn only_explicit_packages_become_roots() {
    // A dependency reimported as a root would be recorded as explicitly
    // installed, and `cleanup` would stop offering to remove it (§24).
    let state = state_with(vec![
      package("surge-xt", "1.3.4", InstallReason::Explicit),
      package("engine", "0.9.0", InstallReason::Dependency),
    ]);
    let file = EnvFile::from_state(&state, None, true);
    let roots = file.roots();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].as_str(), "surge-xt");
    // ...but its version is still reproduced.
    assert_eq!(file.required_versions().len(), 2);
  }

  #[test]
  fn a_loose_export_records_no_versions() {
    let state = state_with(vec![package("dexed", "1.0.1", InstallReason::Explicit)]);
    let file = EnvFile::from_state(&state, None, false);
    assert!(file.packages[0].version.is_none());
    assert!(file.required_versions().is_empty());
    let text = file.to_toml().unwrap();
    assert!(!text.contains("1.0.1"), "{text}");
  }

  #[test]
  fn packages_are_sorted_so_two_exports_are_byte_identical() {
    let a = state_with(vec![
      package("zam-plugins", "4.5.0", InstallReason::Explicit),
      package("dexed", "1.0.1", InstallReason::Explicit),
    ]);
    let file = EnvFile::from_state(&a, None, true);
    let ids: Vec<&str> = file.packages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["dexed", "zam-plugins"]);
  }

  #[test]
  fn pins_survive_the_round_trip() {
    let mut state = state_with(vec![package("surge-xt", "1.3.3", InstallReason::Explicit)]);
    state
      .packages
      .values_mut()
      .next()
      .unwrap()
      .pin
      .replace("1.3.3".parse().unwrap());
    let file = EnvFile::from_state(&state, None, true);
    let back = EnvFile::from_toml(&file.to_toml().unwrap(), "env.toml").unwrap();
    assert_eq!(back.pins().len(), 1);
    assert_eq!(back.pins()[0].1.to_string(), "1.3.3");
  }

  #[test]
  fn a_file_from_a_newer_format_is_refused() {
    let state = state_with(vec![package("dexed", "1.0.1", InstallReason::Explicit)]);
    let text = EnvFile::from_state(&state, None, true)
      .to_toml()
      .unwrap()
      .replace("schema = 1", "schema = 99");
    let err = EnvFile::from_toml(&text, "env.toml").unwrap_err();
    assert!(err.to_string().contains("upgrade luthier"), "{err}");
  }

  #[test]
  fn a_pinned_file_missing_a_version_is_refused() {
    // Otherwise the import would silently install "whatever is current"
    // from a file that claims to reproduce an exact set.
    let text = "\
[meta]
schema = 1
luthier = \"0.1.0\"
exported = \"2026-09-06T21:15:00Z\"
pinned = true

[[package]]
id = \"dexed\"
registry = \"default\"
reason = \"explicit\"
";
    let err = EnvFile::from_toml(text, "env.toml").unwrap_err();
    assert!(err.to_string().contains("claims to be pinned"), "{err}");
  }
}
