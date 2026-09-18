//! User configuration.

use crate::error::{Error, Result};
use crate::fsutil;
use crate::layout::Layout;
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

impl RegistrySource {
  /// Reads a bench location the way a user would write one.
  ///
  /// A local checkout is a path, a snapshot is a tarball URL, and anything
  /// else over HTTP is an Open Audio Stack site root. The guess is only a
  /// default: `kind` overrides it, because a site can be served from a URL
  /// that looks like anything.
  pub fn parse(spec: &str, kind: Option<&str>) -> Result<Self> {
    let as_url = Url::parse(spec).ok().filter(|u| u.scheme() != "file");
    let inferred = match (&as_url, kind) {
      (_, Some(explicit)) => explicit,
      (Some(url), None) => {
        let path = url.path().to_ascii_lowercase();
        if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
          "snapshot"
        } else {
          "oas"
        }
      }
      (None, None) => "path",
    };

    match inferred {
      "path" => Ok(RegistrySource::Path {
        path: PathBuf::from(spec),
      }),
      "snapshot" | "oas" => {
        let url = as_url
          .ok_or_else(|| Error::InvalidArgument(format!("{spec:?} is not an http or https URL")))?;
        Ok(if inferred == "snapshot" {
          RegistrySource::Snapshot { url }
        } else {
          RegistrySource::Oas { url }
        })
      }
      other => Err(Error::InvalidArgument(format!(
        "unknown bench type {other:?}; use path, snapshot or oas"
      ))),
    }
  }
}

/// One configured registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryConfig {
  pub name: String,
  #[serde(flatten)]
  pub source: RegistrySource,
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
  #[serde(default = "default_registries")]
  pub registries: Vec<RegistryConfig>,
}

/// The registry shipped with the client.
///
/// Until the registry repository is published this URL will not resolve; use
/// `--registry-path` (or a `path` entry in `config.json`) to point at a local
/// checkout in the meantime.
pub const DEFAULT_REGISTRY_URL: &str =
  "https://github.com/luthier/luthier-pkgs/archive/refs/heads/main.tar.gz";

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
/// That order is the whole mechanism behind correcting a broader source: where
/// a derived entry would install the wrong thing, the bench's manifest is the
/// one the resolver sees.
fn default_registries() -> Vec<RegistryConfig> {
  vec![
    RegistryConfig {
      name: "luthier-pkgs".into(),
      source: RegistrySource::Snapshot {
        url: Url::parse(DEFAULT_REGISTRY_URL).expect("the built-in URL is valid"),
      },
    },
    RegistryConfig {
      name: "oas".into(),
      source: RegistrySource::Oas {
        url: Url::parse(DEFAULT_OAS_URL).expect("the built-in URL is valid"),
      },
    },
  ]
}

impl Default for Config {
  fn default() -> Self {
    Self {
      registries: default_registries(),
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

  pub fn save(&self, layout: &Layout) -> Result<()> {
    let path = layout.config_file();
    let mut bytes = serde_json::to_vec_pretty(self)
      .map_err(|e| Error::io("serialise configuration", &path, std::io::Error::other(e)))?;
    bytes.push(b'\n');
    fsutil::ensure_dir(layout.config_dir())?;
    fsutil::write_atomic(&path, &bytes)
  }

  /// All configured registries as providers, in configuration order.
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
    registries: vec![RegistryConfig {
      name: "local".into(),
      source: RegistrySource::Path { path: path.into() },
    }],
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
    assert_eq!(names, vec!["luthier-pkgs", "oas"]);
  }

  #[test]
  fn config_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let config = from_path("/srv/luthier-pkgs");
    config.save(&layout).unwrap();
    assert_eq!(Config::load(&layout).unwrap(), config);
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
