//! Refreshing a signed bench, and refusing one that cannot prove who it is.
//!
//! The unit tests in `registry::signature` cover the policy in isolation.
//! What is checked here is that the policy is wired into the one place it
//! matters: between the download and the extractor, so a snapshot nothing
//! vouched for is never opened, and the snapshot already on disk survives
//! every refusal.
//!
//! Benches are served over `file://`, which is what the suite does wherever
//! the behaviour under test is not HTTP itself (§55).

mod support;

use luthier_core::Layout;
use luthier_core::registry::signature::SecretKey;
use luthier_core::registry::{HttpSnapshotRegistry, RegistryProvider, provenance};
use luthier_manifest::Sha256Hash;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
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
    self.dir.path().join("bench.tar.gz.sig")
  }

  fn url(&self) -> Url {
    Url::from_file_path(self.snapshot_path()).unwrap()
  }

  /// Publishes a snapshot carrying one manifest, and returns its digest.
  fn publish(&self, id: &str) -> Sha256Hash {
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
    Sha256Hash::from_bytes(Sha256::digest(&bytes).into())
  }

  fn sign(&self, key: &SecretKey, digest: &Sha256Hash) {
    std::fs::write(self.signature_path(), key.sign(digest).render()).unwrap();
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

  fn registries_dir(&self) -> PathBuf {
    self.layout.registries_dir()
  }

  fn signed_by(&self) -> Option<String> {
    provenance::load(&self.registries_dir(), "bench")?
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

fn exists(path: &Path) -> bool {
  path.exists()
}

#[tokio::test]
async fn a_bench_that_publishes_no_signature_still_refreshes() {
  // Every bench in existence today. Nothing to check against is not the
  // same as a failed check, and refusing here would leave the manager
  // unable to read any registry that has not started signing yet.
  let published = Published::new();
  published.publish("sfizz");
  assert!(!exists(&published.signature_path()));

  let client = Client::new();
  let outcome = client.bench(&published.url()).refresh().await.unwrap();

  assert_eq!(outcome.packages, 1);
  assert_eq!(client.signed_by(), None);
}

#[tokio::test]
async fn the_first_signature_pins_the_key_and_the_next_refresh_needs_it() {
  // Trust on first use, applied to the key exactly as it is to the origin:
  // the first fetch cannot prove anything, and every one after it can.
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  let digest = published.publish("sfizz");
  published.sign(&key, &digest);

  let client = Client::new();
  client.bench(&published.url()).refresh().await.unwrap();
  assert_eq!(client.signed_by(), Some(key.public().to_string()));

  // The bench stops signing. That is the downgrade this pin exists to
  // notice, and noticing it is worth more than the refresh it costs.
  published.publish("sfizz-2");
  published.unpublish_signature();

  let err = client
    .bench(&published.url())
    .refresh()
    .await
    .unwrap_err()
    .to_string();
  assert!(err.contains("unsigned"), "{err}");

  // Refused before extraction, so what was already there is untouched.
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
  assert_eq!(client.signed_by(), Some(key.public().to_string()));
}

#[tokio::test]
async fn a_signature_that_covers_other_bytes_is_refused() {
  // What a tampered snapshot looks like from here: a real signature by the
  // real key, over a snapshot that is not the one served.
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  published.publish("sfizz");
  published.sign(&key, &Sha256Hash::from_bytes([0xab; 32]));

  let client = Client::new();
  let err = client
    .bench(&published.url())
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
async fn a_configured_key_is_checked_on_the_very_first_fetch() {
  // What a key in the configuration buys over the pin: the first fetch is
  // checked too, which is the fetch an attacker would otherwise aim at.
  let published = Published::new();
  let trusted = SecretKey::generate().unwrap();
  let stranger = SecretKey::generate().unwrap();
  let digest = published.publish("sfizz");
  published.sign(&stranger, &digest);

  let client = Client::new();
  let err = client
    .bench(&published.url())
    .keys(vec![trusted.public()])
    .refresh()
    .await
    .unwrap_err()
    .to_string();

  assert!(err.contains("not one of the keys"), "{err}");
  assert_eq!(client.installed_manifest(), None);

  // The same bench, signed by the key it was trusted with.
  published.sign(&trusted, &digest);
  client
    .bench(&published.url())
    .keys(vec![trusted.public()])
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
}

#[tokio::test]
async fn a_rotation_works_because_both_keys_are_trusted_at_once() {
  // Retiring the old key first would leave a window in which no refresh
  // can succeed, so a rotation overlaps: trust the new key, publish under
  // it, then drop the old one.
  let published = Published::new();
  let old = SecretKey::generate().unwrap();
  let new = SecretKey::generate().unwrap();

  let digest = published.publish("sfizz");
  published.sign(&old, &digest);
  let client = Client::new();
  client
    .bench(&published.url())
    .keys(vec![old.public(), new.public()])
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.signed_by(), Some(old.public().to_string()));

  let digest = published.publish("sfizz-2");
  published.sign(&new, &digest);
  client
    .bench(&published.url())
    .keys(vec![old.public(), new.public()])
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.signed_by(), Some(new.public().to_string()));
}

#[tokio::test]
async fn allow_unsigned_accepts_the_absence_once_and_keeps_the_pin() {
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  let digest = published.publish("sfizz");
  published.sign(&key, &digest);

  let client = Client::new();
  client.bench(&published.url()).refresh().await.unwrap();

  published.publish("sfizz-2");
  published.unpublish_signature();
  client
    .bench(&published.url())
    .allow_unsigned(true)
    .refresh()
    .await
    .unwrap();
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz-2.toml"));

  // One run, not a setting: the key is still pinned, so the next plain
  // refresh asks the same question again.
  assert_eq!(client.signed_by(), Some(key.public().to_string()));
  assert!(
    client
      .bench(&published.url())
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
  let digest = published.publish("sfizz");
  published.sign(&key, &digest);

  let client = Client::new();
  client.bench(&published.url()).refresh().await.unwrap();

  published.publish("sfizz-2");
  published.sign(&key, &Sha256Hash::from_bytes([7; 32]));

  let err = client
    .bench(&published.url())
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
  // verification off.
  let published = Published::new();
  let key = SecretKey::generate().unwrap();
  let digest = published.publish("sfizz");
  published.sign(&key, &digest);

  let client = Client::new();
  client.bench(&published.url()).refresh().await.unwrap();

  published.publish("sfizz-2");
  std::fs::write(published.signature_path(), b"algorithm rsa\n").unwrap();

  let err = client
    .bench(&published.url())
    .refresh()
    .await
    .unwrap_err()
    .to_string();
  assert!(err.contains("cannot be read"), "{err}");
  assert_eq!(client.installed_manifest().as_deref(), Some("sfizz.toml"));
}
