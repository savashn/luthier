//! User configuration.

use crate::error::{Error, Result};
use crate::fsutil;
use crate::layout::{Layout, Locations};
use crate::registry::{HttpSnapshotRegistry, LocalRegistry, OasRegistry, RegistryProvider};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

/// Where a registry's manifests come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RegistrySource {
  /// A directory on this machine: a git checkout, or a test fixture.
  Path { path: PathBuf },
  /// A snapshot tarball fetched over HTTPS.
  Snapshot { url: Url },
  /// An Open Audio Stack registry, published as static JSON.
  ///
  /// `url` is the site root, not the index file: the layout below it is this
  /// build's business, not the user's.
  Oas { url: Url },
}

/// One registry this build reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryConfig {
  pub name: String,
  #[serde(flatten)]
  pub source: RegistrySource,
}

impl RegistryConfig {
  pub fn new(name: impl Into<String>, source: RegistrySource) -> Self {
    Self {
      name: name.into(),
      source,
    }
  }
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
  /// Where manifests come from: always [`default_registries`] for a user.
  ///
  /// Never read from or written to `config.json`. Luthier reads the Open
  /// Audio Stack registry and the one bench that corrects it, and nothing
  /// else; a `registries` list that 0.1 wrote into the file is ignored. The
  /// field exists so a front end or a test can hand a session a different
  /// list — `--registry-path`, for developing the bench, is the one way the
  /// command line does.
  #[serde(skip, default = "default_registries")]
  pub registries: Vec<RegistryConfig>,
  /// Directories chosen in place of the defaults. Left out of the file
  /// while none is set.
  #[serde(default, skip_serializing_if = "Locations::is_empty")]
  pub locations: Locations,
}

/// The bench shipped with the client, published as a release asset.
///
/// A release asset rather than the forge's branch tarball, so the bench a
/// user reads is the one a release published rather than whatever `main`
/// holds at that moment.
///
/// It is built from `bench/` by the release workflow rather than being a
/// tarball of the repository: the manager's own `Cargo.toml` files would
/// otherwise be read as manifests, since discovery walks whatever it is given.
///
/// Not signed: it is trusted on HTTPS and on GitHub, exactly as the binary
/// that reads it was downloaded.
pub const DEFAULT_REGISTRY_URL: &str =
  "https://github.com/savashn/luthier/releases/latest/download/bench.tar.gz";

/// The Open Audio Stack registry, published as static JSON under CC0.
///
/// It carries hundreds of packages against this project's handful, and it is
/// where anything downloadable belongs: a plugin with a release binary is
/// expressible there, so duplicating it in the bench would only be a second
/// copy to keep current. What it cannot express — software packaged only by
/// distributions, sample content that needs an engine, curated sets — is what
/// the bench is for.
pub const DEFAULT_OAS_URL: &str = "https://open-audio-stack.github.io/open-audio-stack-registry/";

/// The curated bench first, so it wins any ID both registries carry.
///
/// This is the whole list. Users do not add benches: anything downloadable
/// belongs in the Open Audio Stack registry, and the bench holds only what
/// cannot be expressed there.
///
/// That order is the whole mechanism behind correcting a broader source: where
/// a derived entry would install the wrong thing, the bench's manifest is the
/// one the resolver sees.
fn default_registries() -> Vec<RegistryConfig> {
  vec![
    RegistryConfig::new(
      "luthier-extras",
      RegistrySource::Snapshot {
        url: Url::parse(DEFAULT_REGISTRY_URL).expect("the built-in URL is valid"),
      },
    ),
    RegistryConfig::new(
      "oas",
      RegistrySource::Oas {
        url: Url::parse(DEFAULT_OAS_URL).expect("the built-in URL is valid"),
      },
    ),
  ]
}

impl Default for Config {
  fn default() -> Self {
    Self {
      registries: default_registries(),
      locations: Locations::default(),
    }
  }
}

impl Config {
  /// Reads the config file, falling back to defaults when there is none.
  pub fn load(layout: &Layout) -> Result<Self> {
    let path = layout.config_file();
    match std::fs::read(&path) {
      Ok(bytes) => serde_json::from_slice(&bytes)
        .map_err(|e| Error::io("parse configuration", &path, std::io::Error::other(e))),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
      Err(e) => Err(Error::io("read", &path, e)),
    }
  }

  /// `layout` with the configured locations applied.
  ///
  /// Read from the file whatever registry override a command was given:
  /// `--registry-path` replaces where manifests come from, not where
  /// anything is installed.
  pub fn located(layout: Layout) -> Result<Layout> {
    let locations = Self::load(&layout)?.locations;
    Ok(layout.with_locations(&locations))
  }

  pub fn save(&self, layout: &Layout) -> Result<()> {
    let path = layout.config_file();
    let mut bytes = serde_json::to_vec_pretty(self)
      .map_err(|e| Error::io("serialise configuration", &path, std::io::Error::other(e)))?;
    bytes.push(b'\n');
    fsutil::ensure_dir(layout.config_dir())?;
    fsutil::write_atomic(&path, &bytes)
  }

  /// All registries as providers, in precedence order.
  pub fn providers(&self, layout: &Layout, offline: bool) -> Vec<Box<dyn RegistryProvider>> {
    self
      .registries
      .iter()
      .map(|config| build_provider(layout, config, offline))
      .collect()
  }
}

fn build_provider(
  layout: &Layout,
  config: &RegistryConfig,
  offline: bool,
) -> Box<dyn RegistryProvider> {
  match &config.source {
    RegistrySource::Path { path } => Box::new(LocalRegistry::new(&config.name, path)),
    RegistrySource::Snapshot { url } => Box::new(
      HttpSnapshotRegistry::new(
        &config.name,
        url.clone(),
        layout.registry_dir(&config.name),
        layout.cache_dir(),
      )
      .offline(offline),
    ),
    RegistrySource::Oas { url } => Box::new(
      OasRegistry::new(
        &config.name,
        url.clone(),
        layout.registry_dir(&config.name),
        layout.cache_dir(),
      )
      .offline(offline),
    ),
  }
}

/// A configuration using one local directory. Used by `--registry-path`.
pub fn from_path(path: impl Into<PathBuf>) -> Config {
  Config {
    registries: vec![RegistryConfig::new(
      "local",
      RegistrySource::Path { path: path.into() },
    )],
    locations: Locations::default(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_missing_config_file_yields_the_built_in_registries_in_precedence_order() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let config = Config::load(&layout).unwrap();

    // The order is the precedence rule, so it is worth asserting rather
    // than only counting: the bench must come first or it cannot correct
    // anything.
    let names: Vec<&str> = config.registries.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["luthier-extras", "oas"]);
  }

  #[test]
  fn config_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let mut config = Config::default();
    config.locations.cache = Some(dir.path().join("elsewhere"));
    config.save(&layout).unwrap();
    assert_eq!(Config::load(&layout).unwrap(), config);
  }

  #[test]
  fn registries_in_the_file_are_neither_read_nor_written() {
    // 0.1 wrote the registry list into `config.json` and let users add to
    // it. An upgrade reads the built-in list whatever the file says, and a
    // save stops carrying one.
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    std::fs::create_dir_all(layout.config_dir()).unwrap();
    std::fs::write(
      layout.config_file(),
      br#"{"registries":[{"name":"bench","type":"snapshot","url":"https://example.com/p.tar.gz"}]}"#,
    )
    .unwrap();

    let config = Config::load(&layout).unwrap();
    assert_eq!(config.registries, Config::default().registries);
    config.save(&layout).unwrap();
    let text = std::fs::read_to_string(layout.config_file()).unwrap();
    assert!(!text.contains("registries"), "{text}");
  }

  #[test]
  fn a_path_registry_builds_a_local_provider() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let config = from_path(dir.path());
    let provider = config.providers(&layout, false).remove(0);
    assert_eq!(provider.name(), "local");
  }
}
