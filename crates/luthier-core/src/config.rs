//! User configuration.

use crate::error::{Error, Result};
use crate::fsutil;
use crate::layout::{Layout, Locations};
use crate::registry::signature::PublicKey;
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
  /// Ed25519 keys this bench is trusted to be signed with.
  ///
  /// Empty is the common case and not a weakness by itself: the first
  /// signature a bench serves pins the key it names, exactly as the first
  /// fetch pins the origin. What a key here adds is that the pin is right
  /// from the first fetch rather than from the second, which is the fetch an
  /// attacker would have to beat.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub keys: Vec<PublicKey>,
  #[serde(flatten)]
  pub source: RegistrySource,
}

impl RegistryConfig {
  pub fn new(name: impl Into<String>, source: RegistrySource) -> Self {
    Self {
      name: name.into(),
      keys: Vec::new(),
      source,
    }
  }

  /// Whether a signature could mean anything for this bench.
  ///
  /// A local checkout is a directory the user already controls, and an Open
  /// Audio Stack site publishes no signatures. Saying so where a key is
  /// added beats accepting one that would never be checked.
  pub fn can_be_signed(&self) -> bool {
    matches!(self.source, RegistrySource::Snapshot { .. })
  }
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
  #[serde(default = "default_registries")]
  pub registries: Vec<RegistryConfig>,
  /// Directories chosen in place of the defaults. Left out of the file
  /// while none is set, like a bench's keys.
  #[serde(default, skip_serializing_if = "Locations::is_empty")]
  pub locations: Locations,
}

/// The bench shipped with the client, published as a release asset.
///
/// A release asset rather than the forge's branch tarball, for a reason that
/// only shows up one phase later: a signature is fetched from the snapshot's
/// own URL with `.sig` appended, and nothing can be published under
/// `/archive/refs/heads/`. A branch tarball is therefore a snapshot that can
/// never be signed. An asset is a path this project controls, so
/// `bench.tar.gz.sig` sits beside it.
///
/// It is built from `bench/` by the release workflow rather than being a
/// tarball of the repository: the manager's own `Cargo.toml` files would
/// otherwise be read as manifests, since discovery walks whatever it is given.
///
/// It is signed with [`DEFAULT_BENCH_KEY`], and the key is required from the
/// first fetch.
///
/// Until the first release is tagged this URL will not resolve; use
/// `--registry-path bench` (or a `path` entry in `config.json`) to point at
/// the checkout in the meantime.
pub const DEFAULT_REGISTRY_URL: &str =
  "https://github.com/savashn/luthier/releases/latest/download/bench.tar.gz";

/// The key the default bench is signed with, as the release publishes it.
///
/// Built in so the first fetch is verified too. Without it the first
/// signature a user ever saw would pin whatever key it named, and that first
/// fetch is exactly the one an attacker who controls the forge account would
/// aim at. Rotation is `docs/REGISTRY.md`'s procedure, and a release that
/// changes this constant is the announcement.
pub const DEFAULT_BENCH_KEY: &str =
  "e3796d9892f200f145a5befbb421a66fb9d6ba5a68afe6246044dfba716a99aa";

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
      keys: vec![PublicKey::parse(DEFAULT_BENCH_KEY).expect("the built-in key is valid")],
      ..RegistryConfig::new(
        "luthier-extras",
        RegistrySource::Snapshot {
          url: Url::parse(DEFAULT_REGISTRY_URL).expect("the built-in URL is valid"),
        },
      )
    },
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

  /// All configured registries as providers, in configuration order.
  ///
  /// `allow_unsigned` is a property of one refresh rather than of the
  /// configuration, which is why it arrives here rather than being stored:
  /// accepting an unsigned snapshot once must not be a setting anyone can
  /// forget they turned on. Everything that only reads a local snapshot
  /// passes `false`, because nothing is being accepted.
  pub fn providers(
    &self,
    layout: &Layout,
    offline: bool,
    allow_unsigned: bool,
  ) -> Vec<Box<dyn RegistryProvider>> {
    self
      .registries
      .iter()
      .map(|config| build_provider(layout, config, offline, allow_unsigned))
      .collect()
  }
}

fn build_provider(
  layout: &Layout,
  config: &RegistryConfig,
  offline: bool,
  allow_unsigned: bool,
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
      .offline(offline)
      .keys(config.keys.clone())
      .allow_unsigned(allow_unsigned),
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
    let config = from_path("/srv/luthier-extras");
    config.save(&layout).unwrap();
    assert_eq!(Config::load(&layout).unwrap(), config);
  }

  #[test]
  fn the_default_bench_is_verified_from_its_first_fetch() {
    // A key configured up front is required from the first fetch; with none
    // the first signature would pin whatever key it named.
    let config = Config::default();
    let bench = &config.registries[0];
    assert_eq!(bench.name, "luthier-extras");
    assert_eq!(bench.keys.len(), 1);
    assert_eq!(bench.keys[0].to_string(), DEFAULT_BENCH_KEY);
    // The Open Audio Stack publishes no signatures; a key there would be a
    // promise nothing checks.
    assert!(config.registries[1].keys.is_empty());
  }

  #[test]
  fn a_configuration_written_before_keys_existed_still_reads() {
    // Every `config.json` in existence predates signatures, and an upgrade
    // that could not read one would be worse than no verification at all.
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    std::fs::create_dir_all(layout.config_dir()).unwrap();
    std::fs::write(
      layout.config_file(),
      br#"{"registries":[{"name":"bench","type":"snapshot","url":"https://example.com/p.tar.gz"}]}"#,
    )
    .unwrap();

    let config = Config::load(&layout).unwrap();
    assert!(config.registries[0].keys.is_empty());
    // And a bench with no keys writes none back, rather than growing an
    // empty field in everyone's configuration.
    config.save(&layout).unwrap();
    let text = std::fs::read_to_string(layout.config_file()).unwrap();
    assert!(!text.contains("keys"), "{text}");
  }

  #[test]
  fn only_a_snapshot_bench_can_carry_a_signature() {
    // A local checkout is the user's own directory and an OAS site
    // publishes nothing to check, so a key on either would never be used.
    let snapshot = RegistryConfig::new(
      "bench",
      RegistrySource::Snapshot {
        url: Url::parse("https://example.com/p.tar.gz").unwrap(),
      },
    );
    assert!(snapshot.can_be_signed());
    assert!(!from_path("/srv/pkgs").registries[0].can_be_signed());
    assert!(
      !RegistryConfig::new(
        "oas",
        RegistrySource::Oas {
          url: Url::parse(DEFAULT_OAS_URL).unwrap(),
        },
      )
      .can_be_signed()
    );
  }

  #[test]
  fn a_path_registry_builds_a_local_provider() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let config = from_path(dir.path());
    let provider = config.providers(&layout, false, false).remove(0);
    assert_eq!(provider.name(), "local");
  }
}
