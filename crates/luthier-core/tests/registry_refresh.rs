//! Refreshing the sources: a snapshot bench fetched and extracted, and one
//! source failing without costing the others.
//!
//! Benches are served over `file://`, which is what the suite does wherever
//! the behaviour under test is not HTTP itself (§55).

mod support;

use luthier_core::Layout;
use luthier_core::registry::{HttpSnapshotRegistry, RegistryProvider, provenance};
use std::path::PathBuf;
use support::{TarEntry, build_tar, gzip};
use url::Url;

/// A bench published as a tarball in a directory, the way a forge serves one.
struct Published {
  dir: tempfile::TempDir,
}

impl Published {
  fn new() -> Self {
    Self {
      dir: tempfile::tempdir().unwrap(),
    }
  }

  fn snapshot_path(&self) -> PathBuf {
    self.dir.path().join("bench.tar.gz")
  }

  fn url(&self) -> Url {
    Url::from_file_path(self.snapshot_path()).unwrap()
  }

  /// Publishes a snapshot carrying one manifest, wrapped in one directory as
  /// a forge wraps a tarball.
  fn publish(&self, id: &str) {
    let manifest = format!(
      "schema = 1\nid = \"{id}\"\nname = \"{id}\"\nkind = \"external\"\n\
       category = \"effect\"\nlicense = {{ kind = \"open-source\", spdx = \"MIT\" }}\n\
       provisioning_hint = \"Install it from your distribution.\"\n"
    );
    let bytes = gzip(&build_tar(&[
      TarEntry::dir("bench-main"),
      TarEntry::dir("bench-main/plugins"),
      TarEntry::file(
        &format!("bench-main/plugins/{id}.toml"),
        manifest.as_bytes(),
      ),
    ]));
    std::fs::write(self.snapshot_path(), &bytes).unwrap();
  }
}

/// Where the manager keeps what it fetched.
struct Client {
  _root: tempfile::TempDir,
  layout: Layout,
}

impl Client {
  fn new() -> Self {
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(root.path());
    Self {
      _root: root,
      layout,
    }
  }

  fn bench(&self, url: &Url) -> HttpSnapshotRegistry {
    HttpSnapshotRegistry::new(
      "bench",
      url.clone(),
      self.layout.registry_dir("bench"),
      self.layout.cache_dir(),
    )
  }
}

#[tokio::test]
async fn a_snapshot_is_fetched_unwrapped_and_recorded() {
  let published = Published::new();
  published.publish("sfizz");

  let client = Client::new();
  let outcome = client.bench(&published.url()).refresh().await.unwrap();
  assert_eq!(outcome.packages, 1);
  // The forge's wrapper directory is gone, so paths stay stable across
  // refreshes whatever the wrapper was called.
  assert!(
    client
      .layout
      .registry_dir("bench")
      .join("plugins/sfizz.toml")
      .is_file()
  );

  let recorded = provenance::load(&client.layout.registries_dir(), "bench").unwrap();
  assert_eq!(
    recorded.bytes,
    std::fs::metadata(published.snapshot_path()).unwrap().len()
  );

  // A later snapshot replaces the earlier one rather than merging into it.
  published.publish("sfizz-2");
  client.bench(&published.url()).refresh().await.unwrap();
  let plugins = client.layout.registry_dir("bench").join("plugins");
  assert!(plugins.join("sfizz-2.toml").is_file());
  assert!(!plugins.join("sfizz.toml").exists());
}

// ------------------------------------------------------------------ refresh --

/// A bench nobody can reach must not cost the user the ones they can.
///
/// The default configuration alone lists two benches, and until this the
/// first one failing aborted the command before the second was asked — so an
/// unpublished or briefly unreachable bench made `refresh` useless rather
/// than partial.
#[tokio::test]
async fn one_unreachable_bench_does_not_stop_the_others() {
  use luthier_core::api::Session;
  use luthier_core::config::{Config, RegistryConfig, RegistrySource};

  let published = Published::new();
  published.publish("sfizz");
  let client = Client::new();

  let config = Config {
    registries: vec![
      RegistryConfig::new(
        "gone",
        RegistrySource::Snapshot {
          url: Url::parse("file:///nonexistent/bench.tar.gz").unwrap(),
        },
      ),
      RegistryConfig::new(
        "reachable",
        RegistrySource::Snapshot {
          url: published.url(),
        },
      ),
    ],
    locations: Default::default(),
  };

  let session = Session::new(client.layout.clone(), config).unwrap();
  let outcomes = session.refresh().await.unwrap();

  assert_eq!(outcomes.len(), 2);
  assert!(outcomes[0].failure.is_some(), "{:?}", outcomes[0]);
  assert_eq!(outcomes[0].registry, "gone");
  assert_eq!(outcomes[1].failure, None);
  assert_eq!(outcomes[1].packages, 1);
}

#[tokio::test]
async fn a_refresh_that_updated_nothing_at_all_is_an_error() {
  // Partial is a warning; total failure is what a script has to see.
  use luthier_core::api::Session;
  use luthier_core::config::{Config, RegistryConfig, RegistrySource};

  let client = Client::new();
  let config = Config {
    registries: vec![RegistryConfig::new(
      "gone",
      RegistrySource::Snapshot {
        url: Url::parse("file:///nonexistent/bench.tar.gz").unwrap(),
      },
    )],
    locations: Default::default(),
  };

  let session = Session::new(client.layout.clone(), config).unwrap();
  assert!(session.refresh().await.is_err());
}
