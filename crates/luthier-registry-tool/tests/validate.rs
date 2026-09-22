//! The `validate` command itself, as the registry's CI runs it.
//!
//! The rules it applies are tested in `luthier-manifest`. What is tested here
//! is everything the command adds on top and that no rule can see: that a
//! manifest is filed under its own ID, that two files do not claim one ID, that
//! `engines.toml` is read and cross-checked, and that `--strict` changes the
//! exit code rather than the output. The bench's CI is exactly this binary and
//! this flag, so a break here is a break in every pull request.

use assert_cmd::Command;
use std::path::{Path, PathBuf};

/// A bench built one file at a time.
struct Bench {
  dir: tempfile::TempDir,
}

impl Bench {
  fn new() -> Self {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
    Self { dir }
  }

  fn path(&self) -> &Path {
    self.dir.path()
  }

  fn write(&self, relative: &str, text: &str) {
    let path = self.dir.path().join(relative);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
  }

  /// A manifest that passes every rule.
  fn good(&self, file: &str, id: &str) {
    self.write(file, &good_manifest(id));
  }

  fn validate(&self, extra: &[&str]) -> std::process::Output {
    let mut command = Command::cargo_bin("luthier-registry").expect("the binary is built");
    command.arg("validate").arg(self.path());
    for argument in extra {
      command.arg(argument);
    }
    command.output().expect("the command runs")
  }
}

fn good_manifest(id: &str) -> String {
  format!(
    r#"schema = 1
id = "{id}"
name = "Test {id}"
kind = "plugin"
category = "instrument"
description = "A package for testing the validator."
license = {{ kind = "open-source", spdx = "GPL-3.0-or-later" }}

[[releases]]
version = "1.0.0"

[[releases.artifacts]]
target = {{ os = "linux", arch = "x86_64" }}
source = {{ type = "http", url = "https://example.invalid/{id}.tar.gz" }}
archive = "tar.gz"
checksum = {{ sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" }}
provides = ["clap"]
install = [
  {{ format = "clap", source = "{id}.clap", kind = "file" }},
]
"#
  )
}

fn text_of(output: &std::process::Output) -> String {
  format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  )
}

#[test]
fn a_clean_bench_passes_in_both_modes() {
  let bench = Bench::new();
  bench.good("plugins/alpha.toml", "alpha");
  bench.good("plugins/beta.toml", "beta");

  let plain = bench.validate(&[]);
  assert!(plain.status.success(), "{}", text_of(&plain));
  assert!(
    text_of(&plain).contains("Checked 2 manifest(s): 0 error(s), 0 warning(s)."),
    "{}",
    text_of(&plain)
  );

  assert!(bench.validate(&["--strict"]).status.success());
}

#[test]
fn an_empty_tree_is_not_a_failure() {
  let bench = Bench::new();
  let output = bench.validate(&[]);
  assert!(output.status.success(), "{}", text_of(&output));
  assert!(
    text_of(&output).contains("No manifests found"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn a_path_that_is_not_a_directory_is_refused() {
  let bench = Bench::new();
  bench.good("plugins/alpha.toml", "alpha");
  let mut command = Command::cargo_bin("luthier-registry").unwrap();
  let output = command
    .arg("validate")
    .arg(bench.path().join("plugins/alpha.toml"))
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(
    text_of(&output).contains("is not a directory"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn a_manifest_filed_under_the_wrong_name_is_an_error() {
  // Nothing in the rules can catch this: it is about the filename, which a
  // manifest cannot see.
  let bench = Bench::new();
  bench.write("plugins/wrongname.toml", &good_manifest("alpha"));

  let output = bench.validate(&[]);
  assert!(!output.status.success());
  assert!(
    text_of(&output).contains("must be filed as alpha.toml"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn two_files_claiming_one_id_is_an_error() {
  // The second file wins or the first does, depending on read order; either
  // way one package silently disappears, so it is refused.
  let bench = Bench::new();
  bench.good("plugins/alpha.toml", "alpha");
  bench.write("libraries/alpha.toml", &good_manifest("alpha"));

  let output = bench.validate(&[]);
  assert!(!output.status.success());
  assert!(
    text_of(&output).contains("is already defined in"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn a_parse_error_costs_that_file_and_not_the_run() {
  let bench = Bench::new();
  bench.good("plugins/alpha.toml", "alpha");
  bench.write("plugins/broken.toml", "schema = 1\nid = \"broken\"\nname =");

  let output = bench.validate(&[]);
  assert!(!output.status.success());
  let text = text_of(&output);
  assert!(text.contains("broken.toml"), "{text}");
  // The good one was still checked: the count includes both.
  assert!(text.contains("Checked 2 manifest(s)"), "{text}");
}

#[test]
fn an_unknown_field_is_caught_because_parsing_is_strict_here() {
  // The manager parses leniently so an older client can read a newer
  // registry (§7). The validator must not, or a contributor's typo lands.
  let bench = Bench::new();
  bench.write(
    "plugins/alpha.toml",
    &good_manifest("alpha").replace("name = ", "nmae = \"typo\"\nname = "),
  );
  let output = bench.validate(&[]);
  assert!(!output.status.success(), "{}", text_of(&output));
}

#[test]
fn strict_turns_a_warning_into_a_failing_exit_code() {
  let bench = Bench::new();
  // No description: a warning, not an error.
  bench.write(
    "plugins/alpha.toml",
    &good_manifest("alpha").replace(
      "description = \"A package for testing the validator.\"\n",
      "",
    ),
  );

  let lenient = bench.validate(&[]);
  assert!(lenient.status.success(), "{}", text_of(&lenient));
  assert!(
    text_of(&lenient).contains("0 error(s), 1 warning(s)"),
    "{}",
    text_of(&lenient)
  );

  let strict = bench.validate(&["--strict"]);
  assert!(!strict.status.success(), "{}", text_of(&strict));
  // Same findings, different verdict.
  assert!(
    text_of(&strict).contains("0 error(s), 1 warning(s)"),
    "{}",
    text_of(&strict)
  );
}

#[test]
fn a_rule_may_accept_a_warning_so_strict_stays_usable() {
  // LSP Plugins' case: a `.so` that has to live in the CLAP directory.
  // Without an allowance the bench's CI is red on a correct manifest.
  let bench = Bench::new();
  let with_sidecar = good_manifest("alpha").replace(
    r#"  { format = "clap", source = "alpha.clap", kind = "file" },"#,
    "  { format = \"clap\", source = \"alpha.clap\", kind = \"file\" },\n  \
     { format = \"clap\", source = \"libalpha-helper.so\", kind = \"file\" },",
  );
  bench.write("plugins/alpha.toml", &with_sidecar);
  assert!(!bench.validate(&["--strict"]).status.success());

  bench.write(
    "plugins/alpha.toml",
    &with_sidecar.replace(
      r#"source = "libalpha-helper.so", kind = "file" }"#,
      r#"source = "libalpha-helper.so", kind = "file", allow = ["file-extension"] }"#,
    ),
  );
  let allowed = bench.validate(&["--strict"]);
  assert!(allowed.status.success(), "{}", text_of(&allowed));
}

#[test]
fn content_every_build_knows_needs_no_engines_file() {
  // The cross-check the whole bench needs and no single manifest can do —
  // and which a bench now rarely has to answer, because every build carries
  // engines for the content types it knows. A bench adds to that list; it no
  // longer has to restate it.
  let bench = Bench::new();
  let library = good_manifest("kit")
    .replace(
      "kind = \"plugin\"",
      "kind = \"library\"\ncontent = [\"drumgizmo\"]",
    )
    .replace("category = \"instrument\"", "category = \"sample-library\"")
    .replace("provides = [\"clap\"]", "provides = [\"library\"]")
    .replace(
      r#"{ format = "clap", source = "kit.clap", kind = "file" }"#,
      r#"{ format = "library", source = "Kit", kind = "bundle" }"#,
    );
  bench.write("libraries/kit.toml", &library);

  // DrumGizmo kits are played by engines this build already knows, so the
  // bench needs no `engines.toml` to ship one.
  let without = bench.validate(&["--strict"]);
  assert!(without.status.success(), "{}", text_of(&without));

  // Naming another engine still works, and is how a bench covers one that
  // appeared after this build was released.
  bench.good("plugins/drumcraker.toml", "drumcraker");
  bench.write(
    "engines.toml",
    "schema = 1\n\n[[engine]]\npackage = \"drumcraker\"\nplays = [\"drumgizmo\"]\n",
  );
  let with = bench.validate(&["--strict"]);
  assert!(with.status.success(), "{}", text_of(&with));
}

#[test]
fn a_broken_engines_file_is_reported_rather_than_ignored() {
  let bench = Bench::new();
  bench.good("plugins/alpha.toml", "alpha");
  bench.write(
    "engines.toml",
    "schema = 1\n\n[[engine]]\nplays = [\"sfz\"]\n",
  );

  let output = bench.validate(&[]);
  assert!(!output.status.success(), "{}", text_of(&output));
  assert!(
    text_of(&output).contains("engines.toml"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn the_committed_schema_can_be_printed_without_a_bench() {
  // Registry CI regenerates this to check it is current, and does so with
  // the binary built without its authoring features.
  let mut command = Command::cargo_bin("luthier-registry").unwrap();
  let output = command.arg("schema").output().unwrap();
  assert!(output.status.success());
  let text = String::from_utf8_lossy(&output.stdout);
  assert!(text.contains("\"$schema\""), "{text}");
  assert!(text.contains("file-extension"), "{text}");
}

/// The real bench, when it is checked out beside this repository.
///
/// Skipped rather than failed when it is not: a contributor with only this
/// repository should still get a green suite.
#[test]
fn the_real_bench_passes_strict_validation() {
  let bench = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../luthier-pkgs");
  if !bench.join("plugins").is_dir() {
    eprintln!("skipping: {} is not checked out", bench.display());
    return;
  }
  let mut command = Command::cargo_bin("luthier-registry").unwrap();
  let output = command
    .arg("validate")
    .arg(&bench)
    .arg("--strict")
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "the bench CI runs exactly this: {}",
    text_of(&output)
  );
}
