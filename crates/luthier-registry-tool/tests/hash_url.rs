// The command under test only exists with the authoring features, so without
// them this file compiles to nothing rather than failing to resolve its
// dependencies.
#![cfg(feature = "authoring")]

//! `hash-url` against a release served locally.
//!
//! This is the command that keeps §26 true: a checksum in a manifest comes
//! from the real file, never from a release page or a previous version. What
//! the tests below pin down is the shape of what it prints — a contributor
//! pastes it into a manifest, so a changed field name is a broken workflow —
//! and that a failed download can never be mistaken for a digest.
//!
//! The artifact is served by `wiremock` on localhost, which is what the suite
//! does wherever the behaviour under test is HTTP (§55). Nothing reaches the
//! network.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Bytes with a digest computed outside this suite, so the assertion is a
/// real expectation rather than the same computation run twice.
///
/// It is not a tarball, and does not need to be: `hash-url` hashes bytes and
/// never opens them. What it says about the container comes from the URL.
const FIXTURE: &[u8] = b"luthier hash-url fixture\n";
const DIGEST: &str = "e95b2bbc291748e5aa4c79acadf447d5cfec720e511467766e866d8d36313bd5";

async fn serve(server: &MockServer, at: &str) {
  Mock::given(method("GET"))
    .and(path(at.to_owned()))
    .respond_with(ResponseTemplate::new(200).set_body_bytes(FIXTURE))
    .mount(server)
    .await;
}

fn run(url: &str) -> std::process::Output {
  assert_cmd::Command::cargo_bin("luthier-registry")
    .expect("the luthier-registry binary is built")
    .arg("hash-url")
    .arg(url)
    .output()
    .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
  String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn what_it_prints_is_what_a_manifest_needs() {
  let server = MockServer::start().await;
  serve(&server, "/releases/proj-1.0.0.tar.gz").await;
  let url = format!("{}/releases/proj-1.0.0.tar.gz", server.uri());

  let output = run(&url);
  let text = stdout(&output);
  assert!(output.status.success(), "{text}");

  // The four fields a release artifact carries, in the shape they are
  // pasted in. `size` matters as much as the digest: it is what bounds the
  // download, per artifact rather than globally.
  assert!(
    text.contains(&format!("source: {{ type: http, url: \"{url}\" }}")),
    "{text}"
  );
  assert!(text.contains("archive: tar.gz"), "{text}");
  assert!(text.contains(&format!("size: {}", FIXTURE.len())), "{text}");
  assert!(
    text.contains(&format!("checksum: {{ sha256: \"{DIGEST}\" }}")),
    "{text}"
  );
}

#[tokio::test]
async fn every_container_the_registry_uses_is_named_from_the_url() {
  let server = MockServer::start().await;
  for (file, format) in [
    ("proj.tar.gz", "tar.gz"),
    ("proj.tar.xz", "tar.xz"),
    ("proj.zip", "zip"),
    ("proj.7z", "7z"),
  ] {
    serve(&server, &format!("/{file}")).await;
    let text = stdout(&run(&format!("{}/{file}", server.uri())));
    assert!(
      text.contains(&format!("archive: {format}")),
      "{file}: {text}"
    );
  }
}

#[tokio::test]
async fn a_url_that_names_no_container_prints_no_archive_line() {
  // Some projects serve downloads from a path that says nothing about the
  // file. Guessing from the bytes would be the obvious next step and is
  // exactly what this must not do: `archive` is what the extractor trusts,
  // so a contributor writes it having seen what upstream actually ships.
  let server = MockServer::start().await;
  serve(&server, "/download").await;

  let text = stdout(&run(&format!("{}/download", server.uri())));
  assert!(text.contains(&format!("sha256: \"{DIGEST}\"")), "{text}");
  assert!(!text.contains("archive:"), "{text}");
}

#[tokio::test]
async fn a_failed_download_prints_no_digest_at_all() {
  // The failure worth testing: an error page hashes to something, and a
  // digest printed for one would end up in a manifest and verify forever
  // against 404 bytes.
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/gone.tar.gz"))
    .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
    .mount(&server)
    .await;

  let output = run(&format!("{}/gone.tar.gz", server.uri()));
  let text = stdout(&output);
  assert!(!output.status.success(), "{text}");
  assert!(!text.contains("sha256"), "{text}");
  assert!(
    String::from_utf8_lossy(&output.stderr).contains("error:"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
}

#[tokio::test]
async fn a_url_that_is_not_one_is_refused_before_anything_is_fetched() {
  let output = run("not-a-url");
  assert!(!output.status.success());
  assert!(!stdout(&output).contains("sha256"));
}
