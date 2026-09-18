// The command under test only exists with the authoring features, so without
// them this file compiles to nothing rather than failing to resolve its
// dependencies.
#![cfg(feature = "authoring")]

//! `check-updates` against a forge served locally.
//!
//! The suite stays offline (§55): `wiremock` serves the GitHub API on
//! localhost and the command is pointed at it with the hidden `--api` flag.
//! No test reaches the network, so a run gives the same answer on a laptop, in
//! CI, and on a machine with no route out.

use std::path::Path;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A manifest with one release whose artifact points at `url`.
fn manifest(id: &str, version: &str, url: &str) -> String {
  format!(
    r#"schema = 1
id = "{id}"
name = "{id}"
kind = "plugin"
category = "instrument"
license = {{ kind = "open-source", spdx = "MIT" }}

[[releases]]
version = "{version}"

[[releases.artifacts]]
target = {{ os = "linux", arch = "x86_64" }}
source = {{ type = "http", url = "{url}" }}
archive = "tar.gz"
checksum = {{ sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" }}
provides = ["clap"]
install = [{{ format = "clap", source = "{id}.clap", kind = "file" }}]
"#
  )
}

fn write(dir: &Path, id: &str, body: &str) {
  let plugins = dir.join("plugins");
  std::fs::create_dir_all(&plugins).unwrap();
  std::fs::write(plugins.join(format!("{id}.toml")), body).unwrap();
}

fn run(registry: &Path, api: &str, extra: &[&str]) -> std::process::Output {
  let mut command = assert_cmd::Command::cargo_bin("luthier-registry")
    .expect("the luthier-registry binary is built");
  command
    .arg("check-updates")
    .arg(registry)
    .arg("--api")
    .arg(api)
    .args(extra)
    // A token in the developer's environment must not change the result.
    .env_remove("GITHUB_TOKEN");
  command.output().unwrap()
}

fn stdout(output: &std::process::Output) -> String {
  String::from_utf8_lossy(&output.stdout).into_owned()
}

async fn release_tag(server: &MockServer, owner: &str, repo: &str, tag: &str) {
  Mock::given(method("GET"))
    .and(path(format!("/repos/{owner}/{repo}/releases/latest")))
    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "tag_name": tag
    })))
    .mount(server)
    .await;
}

#[tokio::test]
async fn a_package_behind_upstream_is_reported_with_both_versions() {
  let server = MockServer::start().await;
  release_tag(&server, "owner", "proj", "v2.1.0").await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "proj",
    &manifest(
      "proj",
      "1.0.0",
      "https://github.com/owner/proj/releases/download/v1.0.0/proj.tar.gz",
    ),
  );

  let output = run(dir.path(), &server.uri(), &[]);
  let text = stdout(&output);
  assert!(output.status.success(), "{text}");
  assert!(text.contains("proj"), "{text}");
  assert!(text.contains("1.0.0"), "{text}");
  assert!(text.contains("2.1.0"), "{text}");
  assert!(text.contains("1 behind"), "{text}");
  // The checksum still has to come from the real file (§26).
  assert!(text.contains("hash-url"), "{text}");
}

#[tokio::test]
async fn a_current_package_is_silent_unless_asked_for() {
  let server = MockServer::start().await;
  release_tag(&server, "owner", "proj", "v1.0.0").await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "proj",
    &manifest(
      "proj",
      "1.0.0",
      "https://github.com/owner/proj/releases/download/v1.0.0/proj.tar.gz",
    ),
  );

  let quiet = stdout(&run(dir.path(), &server.uri(), &[]));
  assert!(
    quiet.contains("Every tracked package is current."),
    "{quiet}"
  );
  assert!(!quiet.contains("current  "), "{quiet}");

  let verbose = stdout(&run(dir.path(), &server.uri(), &["--all"]));
  assert!(verbose.contains("proj"), "{verbose}");
  assert!(verbose.contains("current"), "{verbose}");
}

#[tokio::test]
async fn the_publishing_repository_is_asked_not_the_source_repository() {
  // Surge XT's real shape: `repository` names the source, artifacts come
  // from a different repo. Asking the wrong one reports wrong versions
  // forever, so the check derives the repo from the artifact URL.
  let server = MockServer::start().await;
  release_tag(&server, "surge-synthesizer", "releases-xt", "1.3.5").await;
  Mock::given(method("GET"))
    .and(path("/repos/surge-synthesizer/surge/releases/latest"))
    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "tag_name": "nightly"
    })))
    .mount(&server)
    .await;

  let dir = tempfile::tempdir().unwrap();
  let body = manifest(
    "surge-xt",
    "1.3.4",
    "https://github.com/surge-synthesizer/releases-xt/releases/download/1.3.4/surge.tar.gz",
  )
  .replace(
    "[[releases]]",
    "repository = \"https://github.com/surge-synthesizer/surge\"\n\n[[releases]]",
  );
  write(dir.path(), "surge-xt", &body);

  let text = stdout(&run(dir.path(), &server.uri(), &[]));
  assert!(text.contains("1.3.5"), "{text}");
  assert!(text.contains("releases-xt"), "{text}");
  assert!(!text.contains("nightly"), "{text}");
}

#[tokio::test]
async fn a_project_with_no_releases_falls_back_to_tags() {
  // How VSCO 2 ships: a generated tag archive, no uploaded release asset.
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/repos/sgossner/VSCO-2-CE/releases/latest"))
    .respond_with(ResponseTemplate::new(404))
    .mount(&server)
    .await;
  Mock::given(method("GET"))
    .and(path("/repos/sgossner/VSCO-2-CE/tags"))
    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
        { "name": "1.2.0" },
        { "name": "1.1.0" }
    ])))
    .mount(&server)
    .await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "vsco2",
    &manifest(
      "vsco2",
      "1.1.0",
      "https://github.com/sgossner/VSCO-2-CE/archive/refs/tags/1.1.0.zip",
    ),
  );

  let text = stdout(&run(dir.path(), &server.uri(), &[]));
  assert!(text.contains("1.2.0"), "{text}");
  assert!(text.contains("1 behind"), "{text}");
}

#[tokio::test]
async fn a_host_with_no_api_is_reported_rather_than_guessed_at() {
  // The DrumGizmo kits are served from a plain website.
  let server = MockServer::start().await;
  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "drskit",
    &manifest(
      "drskit",
      "2.1.0",
      "https://drumgizmo.org/kits/DRSKit/DRSKit2_1.zip",
    ),
  );

  let text = stdout(&run(dir.path(), &server.uri(), &["--all"]));
  assert!(text.contains("drumgizmo.org"), "{text}");
  assert!(text.contains("1 on unsupported hosts"), "{text}");
  assert!(text.contains("0 behind"), "{text}");
}

#[tokio::test]
async fn external_packages_are_skipped_without_a_request() {
  // No releases means no version to compare, and no reason to spend a
  // request. The mock server is given no routes at all, so any request 404s
  // and would surface as a failure in the summary.
  let server = MockServer::start().await;
  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "sfizz",
    r#"schema = 1
id = "sfizz"
name = "sfizz"
kind = "external"
category = "instrument"
license = { kind = "open-source", spdx = "BSD-2-Clause" }
provisioning_hint = "Install sfizz from your distribution."

[[detect]]
format = "vst3"
name = "sfizz.vst3"
"#,
  );

  let text = stdout(&run(dir.path(), &server.uri(), &["--all"]));
  assert!(text.contains("Checked 0 package(s)"), "{text}");
}

#[tokio::test]
async fn a_tag_that_is_not_a_version_is_reported_as_unreadable() {
  let server = MockServer::start().await;
  release_tag(&server, "owner", "proj", "nightly-2026-09-01").await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "proj",
    &manifest(
      "proj",
      "1.0.0",
      "https://github.com/owner/proj/releases/download/v1.0.0/proj.tar.gz",
    ),
  );

  let text = stdout(&run(dir.path(), &server.uri(), &["--all"]));
  assert!(text.contains("unreadable tag"), "{text}");
  // Not counted as behind: we do not know that it is.
  assert!(text.contains("0 behind"), "{text}");
}

#[tokio::test]
async fn a_rate_limit_is_named_rather_than_reported_as_up_to_date() {
  // The failure that would otherwise look like "everything is current",
  // which is the worst possible way for this command to be wrong.
  let server = MockServer::start().await;
  Mock::given(method("GET"))
    .and(path("/repos/owner/proj/releases/latest"))
    .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
    .mount(&server)
    .await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "proj",
    &manifest(
      "proj",
      "1.0.0",
      "https://github.com/owner/proj/releases/download/v1.0.0/proj.tar.gz",
    ),
  );

  let text = stdout(&run(dir.path(), &server.uri(), &["--all"]));
  assert!(text.contains("rate limit"), "{text}");
  assert!(text.contains("1 failed"), "{text}");
  assert!(text.contains("0 behind"), "{text}");
}

#[tokio::test]
async fn json_output_is_one_document_ci_can_act_on() {
  let server = MockServer::start().await;
  release_tag(&server, "owner", "proj", "v2.0.0").await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "proj",
    &manifest(
      "proj",
      "1.0.0",
      "https://github.com/owner/proj/releases/download/v1.0.0/proj.tar.gz",
    ),
  );

  let text = stdout(&run(dir.path(), &server.uri(), &["--json"]));
  let parsed: serde_json::Value = serde_json::from_str(&text).expect("one JSON document");
  let entry = &parsed.as_array().unwrap()[0];
  assert_eq!(entry["id"], "proj");
  assert_eq!(entry["state"], "behind");
  assert_eq!(entry["current"], "1.0.0");
  assert_eq!(entry["latest"], "2.0.0");
  assert_eq!(entry["tag"], "v2.0.0");
  assert_eq!(entry["source"], "github:owner/proj");
}

#[tokio::test]
async fn being_behind_only_fails_the_run_when_asked() {
  let server = MockServer::start().await;
  release_tag(&server, "owner", "proj", "v2.0.0").await;

  let dir = tempfile::tempdir().unwrap();
  write(
    dir.path(),
    "proj",
    &manifest(
      "proj",
      "1.0.0",
      "https://github.com/owner/proj/releases/download/v1.0.0/proj.tar.gz",
    ),
  );

  // A scheduled CI run should report, not go red.
  assert!(run(dir.path(), &server.uri(), &[]).status.success());
  assert!(
    !run(dir.path(), &server.uri(), &["--exit-code"])
      .status
      .success()
  );
}
