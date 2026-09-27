//! Refreshing a signed bench, and refusing one that cannot prove who it is.
//!
//! The unit tests in `registry::signature` cover the format and the policy in
//! isolation. What is checked here is that the policy is wired into the one
//! place it matters: between the download and the extractor, so a snapshot
//! nothing vouched for is never opened, and the snapshot already on disk
//! survives every refusal.
//!
//! Benches are served over `file://`, which is what the suite does wherever
//! the behaviour under test is not HTTP itself (§55).
//!
//! The last two cases are about refreshing rather than signing, and share
//! these fixtures: one bench failing must leave the others refreshed.

mod support;

use luthier_core::Layout;
use luthier_core::registry::signature::{SecretKey, prehash};
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

  fn signature_path(&self) -> PathBuf {
    self.dir.path().join("bench.tar.gz.minisig")
  }

  fn url(&self) -> Url {
    Url::from_file_path(self.snapshot_path()).unwrap()
  }

  /// Publishes a snapshot carrying one manifest.
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

  /// Signs what is published now.
  fn sign(&self, key: &SecretKey) {
    let bytes = std::fs::read(self.snapshot_path()).unwrap();
    self.sign_bytes(key, &bytes);
  }

  /// Publishes a genuine signature over bytes that are not the snapshot's.
  fn sign_bytes(&self, key: &SecretKey, bytes: &[u8]) {
    let signed = key.sign(&prehash(bytes), "bench.tar.gz", 1_790_000_000);
    std::fs::write(self.signature_path(), signed.render()).unwrap();
  }

  fn unpublish_signature(&self) {
    let _ = std::fs::remove_file(self.signature_path());
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

  /// The bench as the default one is built: with its key.
  fn signed_bench(&self, url: &Url, key: &SecretKey) -> HttpSnapshotRegistry {
    self.bench(url).keys(vec![key.public()])
  }

  fn signed_by(&self) -> Option<String> {
    provenance::load(&self.layout.registries_dir(), "bench")?
      .signed_by
      .map(|k| k.to_string())
  }

  /// Which manifest the snapshot on disk carries, if any.
  fn installed_manifest(&self) -> Option<String> {
    let plugins = self.layout.registry_dir("bench").join("plugins");
    let mut names: Vec<String> = std::fs::read_dir(plugins)
      .ok()?
      .flatten()
      .map(|e| e.file_name().to_string_lossy().into_owned())
      .collect();
    names.sort();
    names.first().cloned()
  }
}

#[tokio::test]
async fn a_source_with_no_key_is_read_unsigned() {
  // What a `--registry-path` checkout or a test bench is: nothing to check
  // against, which is not the same as a failed check.
  let published = Published::new();
  published.publish("sfizz");

  let client = Client::new();
  let outcome = client.bench(&published.url()).refresh().await.unwrap();
  assert_eq!(outcome.packages, 1);
  assert_eq!(client.signed_by(), None);

  // A signature beside it changes nothing without a key to check it with:
  // a minisign file names only a key ID, so it cannot vouch for itself.
  let key = SecretKey::generate().unwrap();
  published.sign(&key);
  client.bench(&published.url()).refresh().await.unwrap();
  assert_eq!(client.signed_by(), None);
}

#[tokio::test]
async fn a_bench_with_a_key_is_verified_from_the_first_fetch() {
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  published.publish("sfizz");
  published.sign(&key);

  let client = Client::new();
  client
    .signed_bench(&published.url(), &key)
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.signed_by(), Some(key.public().to_string()));

  // The bench stops signing: refused, and what was there is untouched.
  published.publish("sfizz-2");
  published.unpublish_signature();
  let err = client
    .signed_bench(&published.url(), &key)
    .refresh()
    .await
    .unwrap_err()
    .to_string();
  assert!(err.contains("unsigned"), "{err}");
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
}

#[tokio::test]
async fn a_signature_that_covers_other_bytes_is_refused() {
  // What a tampered snapshot looks like from here: a real signature by the
  // real key, over a snapshot that is not the one served.
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  published.publish("sfizz");
  published.sign_bytes(&key, b"some other snapshot");

  let client = Client::new();
  let err = client
    .signed_bench(&published.url(), &key)
    .refresh()
    .await
    .unwrap_err()
    .to_string();

  assert!(err.contains("does not verify"), "{err}");
  // Nothing was extracted: a first refresh that fails leaves no snapshot.
  assert_eq!(client.installed_manifest(), None);
  assert_eq!(client.signed_by(), None);
}

#[tokio::test]
async fn a_stranger_s_signature_is_refused() {
  let published = Published::new();
  let trusted = SecretKey::generate().unwrap();
  let stranger = SecretKey::generate().unwrap();
  published.publish("sfizz");
  published.sign(&stranger);

  let client = Client::new();
  let err = client
    .signed_bench(&published.url(), &trusted)
    .refresh()
    .await
    .unwrap_err()
    .to_string();
  assert!(err.contains("not one of the keys"), "{err}");
  assert_eq!(client.installed_manifest(), None);

  // The same bench, signed by the key the build carries.
  published.sign(&trusted);
  client
    .signed_bench(&published.url(), &trusted)
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
}

#[tokio::test]
async fn a_rotation_works_because_both_keys_are_trusted_at_once() {
  // Retiring the old key first would leave a window in which no refresh
  // can succeed, so a rotation overlaps: a release carries both keys, the
  // bench is signed with the new one, and a later release drops the old.
  let published = Published::new();
  let old = SecretKey::generate().unwrap();
  let new = SecretKey::generate().unwrap();
  let client = Client::new();

  for (id, key) in [("sfizz", &old), ("sfizz-2", &new)] {
    published.publish(id);
    published.sign(key);
    client
      .bench(&published.url())
      .keys(vec![old.public(), new.public()])
      .refresh()
      .await
      .unwrap();
    assert_eq!(client.signed_by(), Some(key.public().to_string()));
  }
}

#[tokio::test]
async fn allow_unsigned_accepts_the_absence_once() {
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  published.publish("sfizz");

  let client = Client::new();
  client
    .signed_bench(&published.url(), &key)
    .allow_unsigned(true)
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));

  // One run, not a setting: the next plain refresh asks again.
  assert!(
    client
      .signed_bench(&published.url(), &key)
      .refresh()
      .await
      .unwrap_err()
      .to_string()
      .contains("unsigned")
  );
}

#[tokio::test]
async fn allow_unsigned_does_not_wave_through_a_signature_that_fails() {
  // The flag says "there is no claim here"; it cannot be made to mean "the
  // claim did not hold up, carry on".
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  published.publish("sfizz");
  published.sign(&key);

  let client = Client::new();
  client
    .signed_bench(&published.url(), &key)
    .refresh()
    .await
    .unwrap();

  published.publish("sfizz-2");
  published.sign_bytes(&key, b"not this snapshot");
  let err = client
    .signed_bench(&published.url(), &key)
    .allow_unsigned(true)
    .refresh()
    .await
    .unwrap_err()
    .to_string();
  assert!(err.contains("does not verify"), "{err}");
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
}

#[tokio::test]
async fn a_signature_file_that_cannot_be_read_is_refused_not_ignored() {
  // The tempting failure mode: an unreadable signature treated as no
  // signature, which would make "publish garbage" a way to turn
  // verification off. Refused with or without a key to check it against.
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  published.publish("sfizz");
  published.sign(&key);

  let client = Client::new();
  client
    .signed_bench(&published.url(), &key)
    .refresh()
    .await
    .unwrap();

  published.publish("sfizz-2");
  std::fs::write(published.signature_path(), b"algorithm rsa\n").unwrap();
  for bench in [
    client.signed_bench(&published.url(), &key),
    client.bench(&published.url()),
  ] {
    let err = bench.refresh().await.unwrap_err().to_string();
    assert!(err.contains("cannot be read"), "{err}");
  }
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
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
  let outcomes = session.refresh(false).await.unwrap();

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
  assert!(session.refresh(false).await.is_err());
}
