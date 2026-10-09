//! The `validate` command itself, as the registry's CI runs it.
//!
//! The rules it applies are tested in `luthier-manifest`. What is tested here
//! is everything the command adds on top and that no rule can see: that a
//! manifest is filed under its own ID, that two files do not claim one ID, that
//! `engines.toml` is read and cross-checked, and that `--strict` changes the
//! exit code rather than the output. The CI of extras is exactly this binary and
//! this flag, so a break here is a break in every pull request.

use assert_cmd::Command;
use std::path::{Path, PathBuf};

/// A manifest tree built one file at a time.
struct Tree {
  dir: tempfile::TempDir,
}

impl Tree {
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
fn a_clean_tree_passes_in_both_modes() {
  let tree = Tree::new();
  tree.good("plugins/alpha.toml", "alpha");
  tree.good("plugins/beta.toml", "beta");

  let plain = tree.validate(&[]);
  assert!(plain.status.success(), "{}", text_of(&plain));
  assert!(
    text_of(&plain).contains("Checked 2 manifest(s): 0 error(s), 0 warning(s)."),
    "{}",
    text_of(&plain)
  );

  assert!(tree.validate(&["--strict"]).status.success());
}

#[test]
fn an_empty_tree_is_not_a_failure() {
  let tree = Tree::new();
  let output = tree.validate(&[]);
  assert!(output.status.success(), "{}", text_of(&output));
  assert!(
    text_of(&output).contains("No manifests found"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn a_path_that_is_not_a_directory_is_refused() {
  let tree = Tree::new();
  tree.good("plugins/alpha.toml", "alpha");
  let mut command = Command::cargo_bin("luthier-registry").unwrap();
  let output = command
    .arg("validate")
    .arg(tree.path().join("plugins/alpha.toml"))
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
  let tree = Tree::new();
  tree.write("plugins/wrongname.toml", &good_manifest("alpha"));

  let output = tree.validate(&[]);
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
  let tree = Tree::new();
  tree.good("plugins/alpha.toml", "alpha");
  tree.write("libraries/alpha.toml", &good_manifest("alpha"));

  let output = tree.validate(&[]);
  assert!(!output.status.success());
  assert!(
    text_of(&output).contains("is already defined in"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn a_parse_error_costs_that_file_and_not_the_run() {
  let tree = Tree::new();
  tree.good("plugins/alpha.toml", "alpha");
  tree.write("plugins/broken.toml", "schema = 1\nid = \"broken\"\nname =");

  let output = tree.validate(&[]);
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
  let tree = Tree::new();
  tree.write(
    "plugins/alpha.toml",
    &good_manifest("alpha").replace("name = ", "nmae = \"typo\"\nname = "),
  );
  let output = tree.validate(&[]);
  assert!(!output.status.success(), "{}", text_of(&output));
}

#[test]
fn strict_turns_a_warning_into_a_failing_exit_code() {
  let tree = Tree::new();
  // No description: a warning, not an error.
  tree.write(
    "plugins/alpha.toml",
    &good_manifest("alpha").replace(
      "description = \"A package for testing the validator.\"\n",
      "",
    ),
  );

  let lenient = tree.validate(&[]);
  assert!(lenient.status.success(), "{}", text_of(&lenient));
  assert!(
    text_of(&lenient).contains("0 error(s), 1 warning(s)"),
    "{}",
    text_of(&lenient)
  );

  let strict = tree.validate(&["--strict"]);
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
  // Without an allowance the CI of extras is red on a correct manifest.
  let tree = Tree::new();
  let with_sidecar = good_manifest("alpha").replace(
    r#"  { format = "clap", source = "alpha.clap", kind = "file" },"#,
    "  { format = \"clap\", source = \"alpha.clap\", kind = \"file\" },\n  \
     { format = \"clap\", source = \"libalpha-helper.so\", kind = \"file\" },",
  );
  tree.write("plugins/alpha.toml", &with_sidecar);
  assert!(!tree.validate(&["--strict"]).status.success());

  tree.write(
    "plugins/alpha.toml",
    &with_sidecar.replace(
      r#"source = "libalpha-helper.so", kind = "file" }"#,
      r#"source = "libalpha-helper.so", kind = "file", allow = ["file-extension"] }"#,
    ),
  );
  let allowed = tree.validate(&["--strict"]);
  assert!(allowed.status.success(), "{}", text_of(&allowed));
}

#[test]
fn content_every_build_knows_needs_no_engines_file() {
  // The cross-check the whole tree needs and no single manifest can do —
  // and which a tree now rarely has to answer, because every build carries
  // engines for the content types it knows. A tree adds to that list; it no
  // longer has to restate it.
  let tree = Tree::new();
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
  tree.write("libraries/kit.toml", &library);

  // DrumGizmo kits are played by engines this build already knows, so the
  // tree needs no `engines.toml` to ship one.
  let without = tree.validate(&["--strict"]);
  assert!(without.status.success(), "{}", text_of(&without));

  // Naming another engine still works, and is how a tree covers one that
  // appeared after this build was released.
  tree.good("plugins/drumcraker.toml", "drumcraker");
  tree.write(
    "engines.toml",
    "schema = 1\n\n[[engine]]\npackage = \"drumcraker\"\nplays = [\"drumgizmo\"]\n",
  );
  let with = tree.validate(&["--strict"]);
  assert!(with.status.success(), "{}", text_of(&with));
}

#[test]
fn a_broken_engines_file_is_reported_rather_than_ignored() {
  let tree = Tree::new();
  tree.good("plugins/alpha.toml", "alpha");
  tree.write(
    "engines.toml",
    "schema = 1\n\n[[engine]]\nplays = [\"sfz\"]\n",
  );

  let output = tree.validate(&[]);
  assert!(!output.status.success(), "{}", text_of(&output));
  assert!(
    text_of(&output).contains("engines.toml"),
    "{}",
    text_of(&output)
  );
}

#[test]
fn the_committed_schema_can_be_printed_without_a_tree() {
  // Registry CI regenerates this to check it is current, and does so with
  // the binary built without its authoring features.
  let mut command = Command::cargo_bin("luthier-registry").unwrap();
  let output = command.arg("schema").output().unwrap();
  assert!(output.status.success());
  let text = String::from_utf8_lossy(&output.stdout);
  assert!(text.contains("\"$schema\""), "{text}");
  assert!(text.contains("file-extension"), "{text}");
}

/// Extras, which ships with this repository.
///
/// Unconditional now that `extras/` is in the repository. It used to be skipped
/// when it was a sibling checkout that might be absent, which meant the one
/// test covering real data was the one most likely not to run.
#[test]
fn extras_passes_strict_validation() {
  let extras = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extras");
  assert!(
    extras.join("plugins").is_dir(),
    "{} is missing from the checkout",
    extras.display()
  );
  let mut command = Command::cargo_bin("luthier-registry").unwrap();
  let output = command
    .arg("validate")
    .arg(&extras)
    .arg("--strict")
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "extras fails strict validation: {}",
    text_of(&output)
  );
}
