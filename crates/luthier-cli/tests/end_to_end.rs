//! The full package lifecycle, end to end (§61).
//!
//! Every test builds its own registry and its own artifacts, served over
//! `file://`, so the suite never depends on a live GitHub release (§55) and can
//! run offline. `--root` confines all state and plugin directories to a
//! temporary directory, so nothing here can reach the real `~/.clap`.

use assert_cmd::Command;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The architecture the binary under test resolves for (`Target::host()`) and
/// whose VST3 directory it expects. The fixtures publish for it, so the suite
/// passes on x86_64 and on aarch64 alike.
const ARCH: &str = std::env::consts::ARCH;

/// A throwaway installation with its own registry.
struct Fixture {
  dir: tempfile::TempDir,
}

impl Fixture {
  fn new() -> Self {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("registry/plugins")).unwrap();
    std::fs::create_dir_all(dir.path().join("registry/packs")).unwrap();
    std::fs::create_dir_all(dir.path().join("artifacts")).unwrap();
    Self { dir }
  }

  fn root(&self) -> PathBuf {
    self.dir.path().join("root")
  }

  fn registry(&self) -> PathBuf {
    self.dir.path().join("registry")
  }

  fn clap_dir(&self) -> PathBuf {
    self.root().join(".clap")
  }

  fn vst3_dir(&self) -> PathBuf {
    self.root().join(".vst3")
  }

  /// A `.tar.gz` holding a directory of sample content.
  fn make_library_artifact(&self, name: &str) -> Artifact {
    let mut builder = tar::Builder::new(Vec::new());
    append(
      &mut builder,
      &format!("{name}/Strings/violin.sfz"),
      b"<region> sample=violin.wav",
      0o644,
    );
    append(
      &mut builder,
      &format!("{name}/Strings/violin.wav"),
      b"RIFF....WAVE",
      0o644,
    );

    let tar = builder.into_inner().expect("tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    let bytes = encoder.finish().unwrap();

    let path = self
      .dir
      .path()
      .join("artifacts")
      .join(format!("{name}-library.tar.gz"));
    std::fs::write(&path, &bytes).unwrap();

    Artifact {
      url: url::Url::from_file_path(&path).unwrap().to_string(),
      sha256: luthier_core::fsutil::hash_file(&path).unwrap().to_string(),
      size: bytes.len() as u64,
      path,
    }
  }

  /// A `.tar.gz` whose content sits at the root, with no wrapper directory —
  /// how half the Open Audio Stack registry's libraries are published.
  fn make_flat_library_artifact(&self, name: &str) -> Artifact {
    let mut builder = tar::Builder::new(Vec::new());
    append(&mut builder, "kit.sfz", b"<region> sample=kick.wav", 0o644);
    append(&mut builder, "samples/kick.wav", b"RIFF....WAVE", 0o644);
    append(&mut builder, "LICENSE", b"CC0", 0o644);

    let tar = builder.into_inner().expect("tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    let bytes = encoder.finish().unwrap();

    let path = self
      .dir
      .path()
      .join("artifacts")
      .join(format!("{name}-flat.tar.gz"));
    std::fs::write(&path, &bytes).unwrap();

    Artifact {
      url: url::Url::from_file_path(&path).unwrap().to_string(),
      sha256: luthier_core::fsutil::hash_file(&path).unwrap().to_string(),
      size: bytes.len() as u64,
      path,
    }
  }

  /// A library manifest carrying no install rules, as everything from a
  /// source that publishes none arrives.
  fn add_library_derived(&self, id: &str, version: &str, artifact: &Artifact) {
    let name = capitalise(id);
    let manifest = format!(
      "schema = 1\nid = \"{id}\"\nname = \"{name}\"\nkind = \"library\"\n\
             category = \"sample-library\"\ntags = [\"orchestral\"]\n\
             description = \"Test library {id}.\"\n\
             license = {{ kind = \"open-source\", spdx = \"CC0-1.0\" }}\n\
             \n[[releases]]\nversion = \"{version}\"\n\
             \n[[releases.artifacts]]\n\
             target = {{ os = \"linux\", arch = \"{ARCH}\" }}\n\
             source = {{ type = \"file\", url = \"{}\" }}\n\
             archive = \"tar.gz\"\n\
             size = {}\n\
             checksum = {{ sha256 = \"{}\" }}\n\
             provides = [\"library\"]\n\
             derive_install = true\n",
      artifact.url, artifact.size, artifact.sha256,
    );
    std::fs::write(self.registry().join(format!("plugins/{id}.toml")), manifest).unwrap();
  }

  /// Writes a sample-library manifest.
  fn add_library(&self, id: &str, version: &str, source: &str, artifact: &Artifact) {
    let name = capitalise(id);
    let manifest = format!(
      "schema = 1\nid = \"{id}\"\nname = \"{name}\"\nkind = \"library\"\n\
             category = \"sample-library\"\ntags = [\"orchestral\"]\n\
             description = \"Test library {id}.\"\n\
             license = {{ kind = \"open-source\", spdx = \"CC0-1.0\" }}\n\
             \n[[releases]]\nversion = \"{version}\"\n\
             \n[[releases.artifacts]]\n\
             target = {{ os = \"linux\", arch = \"{ARCH}\" }}\n\
             source = {{ type = \"file\", url = \"{}\" }}\n\
             archive = \"tar.gz\"\n\
             size = {}\n\
             checksum = {{ sha256 = \"{}\" }}\n\
             provides = [\"library\"]\n\
             install = [{{ format = \"library\", source = \"{source}\", kind = \"bundle\" }}]\n",
      artifact.url, artifact.size, artifact.sha256,
    );
    std::fs::write(self.registry().join(format!("plugins/{id}.toml")), manifest).unwrap();
  }

  fn luthier(&self) -> Command {
    let mut command = Command::cargo_bin("luthier").expect("the luthier binary is built");
    command
      .arg("--root")
      .arg(self.root())
      .arg("--registry-path")
      .arg(self.registry())
      .arg("--no-system-plugins")
      // `update` and `outdated` ask whether a newer Luthier is out; the suite
      // never reaches GitHub, so the question goes nowhere and is dropped.
      .arg("--releases-api")
      .arg("file:///nonexistent/luthier-release.json")
      .arg("--yes");
    command
  }

  /// Builds a `.tar.gz` holding a CLAP file and a VST3 bundle, and returns
  /// its `file://` URL, SHA-256 and size.
  fn make_artifact(&self, name: &str) -> Artifact {
    let mut builder = tar::Builder::new(Vec::new());

    let clap = elf_shared_object();
    append(&mut builder, &format!("{name}.clap"), &clap, 0o755);
    append(
      &mut builder,
      &format!("{name}.vst3/Contents/{ARCH}-linux/{name}.so"),
      &elf_shared_object(),
      0o755,
    );
    append(
      &mut builder,
      &format!("{name}.vst3/Contents/Resources/moduleinfo.json"),
      b"{}",
      0o644,
    );

    let tar = builder.into_inner().expect("tar");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    let bytes = encoder.finish().unwrap();

    let path = self
      .dir
      .path()
      .join("artifacts")
      .join(format!("{name}.tar.gz"));
    std::fs::write(&path, &bytes).unwrap();

    Artifact {
      url: url::Url::from_file_path(&path).unwrap().to_string(),
      sha256: luthier_core::fsutil::hash_file(&path).unwrap().to_string(),
      size: bytes.len() as u64,
      path,
    }
  }

  /// Writes a plugin manifest with one release.
  fn add_plugin(&self, id: &str, version: &str, artifact: &Artifact, dependencies: &[&str]) {
    let name = capitalise(id);
    let mut manifest = format!(
      "schema = 1\nid = \"{id}\"\nname = \"{name}\"\nkind = \"plugin\"\n\
             category = \"instrument\"\ntags = [\"synthesizer\"]\n\
             description = \"Test package {id}.\"\n\
             license = {{ kind = \"open-source\", spdx = \"GPL-3.0-or-later\" }}\n\
             \n[[releases]]\nversion = \"{version}\"\n"
    );
    if !dependencies.is_empty() {
      let quoted: Vec<String> = dependencies.iter().map(|d| format!("\"{d}\"")).collect();
      manifest.push_str(&format!("dependencies = [{}]\n", quoted.join(", ")));
    }
    manifest.push_str(&format!(
      "\n[[releases.artifacts]]\n\
             target = {{ os = \"linux\", arch = \"{ARCH}\" }}\n\
             source = {{ type = \"file\", url = \"{}\" }}\n\
             archive = \"tar.gz\"\n\
             size = {}\n\
             checksum = {{ sha256 = \"{}\" }}\n\
             provides = [\"clap\", \"vst3\"]\n\
             install = [\n\
             \x20 {{ format = \"clap\", source = \"{name}.clap\", kind = \"file\" }},\n\
             \x20 {{ format = \"vst3\", source = \"{name}.vst3\", kind = \"bundle\" }},\n\
             ]\n",
      artifact.url, artifact.size, artifact.sha256,
    ));
    std::fs::write(self.registry().join(format!("plugins/{id}.toml")), manifest).unwrap();
  }

  /// Writes a manifest that carries no install rules, the way a provider
  /// reading a source without them hands one over.
  fn add_plugin_derived(&self, id: &str, version: &str, artifact: &Artifact) {
    self.add_plugin_derived_claiming(id, version, artifact, &["clap", "vst3"]);
  }

  /// The same, with a stated claim about what the archive holds. A source
  /// that carries no install rules carries no verified claim either.
  fn add_plugin_derived_claiming(
    &self,
    id: &str,
    version: &str,
    artifact: &Artifact,
    claims: &[&str],
  ) {
    let name = capitalise(id);
    let provides = claims
      .iter()
      .map(|f| format!("\"{f}\""))
      .collect::<Vec<_>>()
      .join(", ");
    let manifest = format!(
      "schema = 1\nid = \"{id}\"\nname = \"{name}\"\nkind = \"plugin\"\n\
             category = \"instrument\"\ntags = [\"synthesizer\"]\n\
             description = \"Test package {id}.\"\n\
             license = {{ kind = \"open-source\", spdx = \"GPL-3.0-or-later\" }}\n\
             \n[[releases]]\nversion = \"{version}\"\n\
             \n[[releases.artifacts]]\n\
             target = {{ os = \"linux\", arch = \"{ARCH}\" }}\n\
             source = {{ type = \"file\", url = \"{}\" }}\n\
             archive = \"tar.gz\"\n\
             size = {}\n\
             checksum = {{ sha256 = \"{}\" }}\n\
             provides = [{provides}]\n\
             derive_install = true\n",
      artifact.url, artifact.size, artifact.sha256,
    );
    std::fs::write(self.registry().join(format!("plugins/{id}.toml")), manifest).unwrap();
  }

  /// Overwrites a manifest's checksum, simulating a corrupted download.
  fn corrupt_checksum(&self, id: &str) {
    let path = self.registry().join(format!("plugins/{id}.toml"));
    let text = std::fs::read_to_string(&path).unwrap();
    let replaced = text
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("checksum") {
                    "checksum = { sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\" }"
                        .to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    std::fs::write(&path, replaced).unwrap();
  }
}

struct Artifact {
  url: String,
  sha256: String,
  size: u64,
  path: PathBuf,
}

fn capitalise(id: &str) -> String {
  let mut chars = id.chars();
  match chars.next() {
    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    None => String::new(),
  }
}

fn append<W: Write>(builder: &mut tar::Builder<W>, name: &str, data: &[u8], mode: u32) {
  let mut header = tar::Header::new_gnu();
  header.set_size(data.len() as u64);
  header.set_mode(mode);
  header.set_mtime(0);
  header.set_entry_type(tar::EntryType::Regular);
  header.set_path(name).expect("path fits");
  header.set_cksum();
  builder.append(&header, data).expect("append");
}

/// A minimal but genuine ELF64 shared-object header, so the installer's real
/// validation passes.
fn elf_shared_object() -> Vec<u8> {
  let mut bytes = vec![0u8; 128];
  bytes[..4].copy_from_slice(b"\x7fELF");
  bytes[4] = 2;
  bytes[5] = 1;
  bytes[6] = 1;
  bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
  let machine: u16 = if ARCH == "aarch64" { 0xb7 } else { 0x3e };
  bytes[18..20].copy_from_slice(&machine.to_le_bytes());
  bytes
}

fn stdout_of(output: &std::process::Output) -> String {
  String::from_utf8_lossy(&output.stdout).into_owned()
}

// --------------------------------------------------------------------------

#[test]
fn the_full_lifecycle_works() {
  // §61's definition of done, start to finish.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);

  // search
  let search = fixture
    .luthier()
    .args(["search", "synth"])
    .output()
    .unwrap();
  assert!(search.status.success());
  assert!(
    stdout_of(&search).contains("testsynth"),
    "{}",
    stdout_of(&search)
  );

  // info
  let info = fixture
    .luthier()
    .args(["info", "testsynth"])
    .output()
    .unwrap();
  let text = stdout_of(&info);
  assert!(text.contains("1.2.3"), "{text}");
  assert!(text.contains("GPL-3.0-or-later"), "{text}");
  assert!(text.contains("clap, vst3"), "{text}");

  // install
  let install = fixture
    .luthier()
    .args(["install", "testsynth"])
    .output()
    .unwrap();
  assert!(
    install.status.success(),
    "{}",
    String::from_utf8_lossy(&install.stderr)
  );
  assert!(fixture.clap_dir().join("Testsynth.clap").is_file());
  assert!(
    fixture
      .vst3_dir()
      .join(format!("Testsynth.vst3/Contents/{ARCH}-linux/Testsynth.so"))
      .is_file()
  );

  // list
  let list = fixture.luthier().arg("list").output().unwrap();
  let listing = stdout_of(&list);
  assert!(listing.contains("Testsynth"), "{listing}");
  assert!(listing.contains("1.2.3"), "{listing}");
  assert!(listing.contains("clap,vst3"), "{listing}");

  // verify
  let verify = fixture
    .luthier()
    .args(["verify", "testsynth"])
    .output()
    .unwrap();
  assert!(verify.status.success(), "{}", stdout_of(&verify));
  assert!(stdout_of(&verify).contains("ok"));

  // remove
  let remove = fixture
    .luthier()
    .args(["remove", "testsynth"])
    .output()
    .unwrap();
  assert!(
    remove.status.success(),
    "{}",
    String::from_utf8_lossy(&remove.stderr)
  );
  assert!(!fixture.clap_dir().join("Testsynth.clap").exists());
  assert!(!fixture.vst3_dir().join("Testsynth.vst3").exists());

  let after = fixture.luthier().arg("list").output().unwrap();
  assert!(stdout_of(&after).contains("No packages installed"));
}

#[test]
fn a_checksum_mismatch_aborts_before_installing_anything() {
  // §12/§13: nothing reaches a plugin directory before verification passes.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Badsum");
  fixture.add_plugin("badsum", "1.0.0", &artifact, &[]);
  fixture.corrupt_checksum("badsum");

  let output = fixture
    .luthier()
    .args(["install", "badsum"])
    .output()
    .unwrap();
  assert_eq!(
    output.status.code(),
    Some(4),
    "checksum failure is exit code 4"
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("checksum verification failed"), "{stderr}");
  assert!(stderr.contains("Expected:"), "{stderr}");
  assert!(stderr.contains("Received:"), "{stderr}");
  assert!(stderr.contains("nothing was written"), "{stderr}");

  assert!(
    !fixture.clap_dir().join("Badsum.clap").exists(),
    "a corrupted artifact must never be installed"
  );
}

#[test]
fn dependencies_are_installed_first_and_recorded_as_such() {
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  fixture.add_plugin("library", "2.0.0", &library, &["engine"]);

  let output = fixture
    .luthier()
    .args(["install", "library"])
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );

  let text = stdout_of(&output);
  let engine_at = text.find("Installed Engine").expect("engine installed");
  let library_at = text.find("Installed Library").expect("library installed");
  assert!(
    engine_at < library_at,
    "the dependency must be installed first:\n{text}"
  );

  let list = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let packages: serde_json::Value = serde_json::from_str(&list).unwrap();
  let reasons: Vec<(&str, &str)> = packages
    .as_array()
    .unwrap()
    .iter()
    .map(|p| (p["id"].as_str().unwrap(), p["reason"].as_str().unwrap()))
    .collect();
  assert!(reasons.contains(&("library", "explicit")), "{reasons:?}");
  assert!(reasons.contains(&("engine", "dependency")), "{reasons:?}");
}

#[test]
fn a_still_needed_dependency_is_not_removed() {
  // §23: removing the dependent must not take the dependency with it.
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  fixture.add_plugin("library", "2.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();

  let output = fixture
    .luthier()
    .args(["remove", "engine"])
    .output()
    .unwrap();
  assert_eq!(
    output.status.code(),
    Some(6),
    "dependency failures are exit code 6"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("required by library"), "{stderr}");
  assert!(
    fixture.clap_dir().join("Engine.clap").is_file(),
    "the dependency must still be installed"
  );

  // Removing the dependent leaves the dependency behind as an orphan.
  fixture
    .luthier()
    .args(["remove", "library"])
    .assert()
    .success();
  let dry_run = stdout_of(
    &fixture
      .luthier()
      .args(["autoremove", "--dry-run"])
      .output()
      .unwrap(),
  );
  assert!(dry_run.contains("Engine.clap"), "{dry_run}");
  assert!(
    fixture.clap_dir().join("Engine.clap").is_file(),
    "a dry run must delete nothing (§24)"
  );
}

#[test]
fn autoremove_removes_a_whole_chain_nothing_needs_and_keeps_the_rest() {
  // §24: a dependency nothing explicit needs goes, and so does the
  // dependency it alone needed, in one pass: a library over an engine over
  // a runtime, beside a package whose own dependency must survive.
  let fixture = Fixture::new();
  for (id, name, deps) in [
    ("runtime", "Runtime", &[][..]),
    ("engine", "Engine", &["runtime"][..]),
    ("library", "Library", &["engine"][..]),
    ("shared", "Shared", &[][..]),
    ("keeper", "Keeper", &["shared"][..]),
  ] {
    let artifact = fixture.make_artifact(name);
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "library", "keeper"])
    .assert()
    .success();
  fixture
    .luthier()
    .args(["remove", "library"])
    .assert()
    .success();

  // One document under `--json`, with the removal in it.
  let output = fixture
    .luthier()
    .args(["autoremove", "--json"])
    .output()
    .unwrap();
  assert!(output.status.success(), "{}", stdout_of(&output));
  let outcome: serde_json::Value = serde_json::from_str(&stdout_of(&output))
    .unwrap_or_else(|e| panic!("autoremove --json did not emit one document: {e}"));
  let removed: Vec<&str> = outcome["removed"]
    .as_array()
    .unwrap()
    .iter()
    .map(|p| p["id"].as_str().unwrap())
    .collect();
  // `runtime` was needed only by `engine`, and goes in the same pass;
  // `shared` is a dependency too, and stays, because `keeper` needs it.
  assert_eq!(removed, ["engine", "runtime"]);
  assert!(!fixture.clap_dir().join("Engine.clap").exists());
  assert!(!fixture.clap_dir().join("Runtime.clap").exists());
  assert!(fixture.clap_dir().join("Shared.clap").is_file());

  let again = stdout_of(&fixture.luthier().arg("autoremove").output().unwrap());
  assert!(again.contains("Nothing to remove"), "{again}");
}

#[test]
fn upgrade_with_nothing_named_upgrades_everything_but_a_pin() {
  let fixture = Fixture::new();
  for (id, name, deps) in [
    ("engine", "Engine", &[][..]),
    ("library", "Library", &["engine"][..]),
    ("held", "Held", &[][..]),
  ] {
    let artifact = fixture.make_artifact(name);
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "library", "held"])
    .assert()
    .success();
  fixture.luthier().args(["pin", "held"]).assert().success();
  // `held` keeps its 1.0.0 release beside the new one: a pin on a version
  // the registry dropped falls back, which is not what is under test.
  let held_manifest = fixture.registry().join("plugins/held.toml");
  let held_before = std::fs::read_to_string(&held_manifest).unwrap();
  for (id, name, deps) in [
    ("engine", "Engine", &[][..]),
    ("library", "Library", &["engine"][..]),
    ("held", "Held", &[][..]),
  ] {
    let artifact = fixture.make_artifact(name);
    fixture.add_plugin(id, "1.1.0", &artifact, deps);
  }
  let first_release = &held_before[held_before.find("\n[[releases]]").unwrap()..];
  let held_after = std::fs::read_to_string(&held_manifest).unwrap();
  std::fs::write(&held_manifest, held_after + first_release).unwrap();

  // `update` fetches and counts; it installs nothing.
  let updated = stdout_of(&fixture.luthier().arg("update").output().unwrap());
  assert!(updated.contains("2 packages can be upgraded"), "{updated}");
  let outdated = stdout_of(&fixture.luthier().arg("outdated").output().unwrap());
  assert!(outdated.contains("pinned to 1.0.0"), "{outdated}");

  let upgraded = fixture.luthier().arg("upgrade").output().unwrap();
  let stdout = stdout_of(&upgraded);
  assert!(upgraded.status.success(), "{stdout}");
  assert!(stdout.contains("held is pinned to 1.0.0"), "{stdout}");

  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let listed: serde_json::Value = serde_json::from_str(&json).unwrap();
  let row = |id: &str| {
    listed
      .as_array()
      .unwrap()
      .iter()
      .find(|p| p["id"] == id)
      .unwrap_or_else(|| panic!("{id} is not listed: {json}"))
      .clone()
  };
  assert_eq!(row("library")["version"], "1.1.0");
  assert_eq!(row("engine")["version"], "1.1.0");
  assert_eq!(row("held")["version"], "1.0.0");
  // Upgraded as a dependency, it is still one, so `autoremove` still owns it.
  assert_eq!(row("engine")["reason"], "dependency");

  // Not "up to date": the pin is still holding one back, and says so.
  let again = stdout_of(&fixture.luthier().arg("upgrade").output().unwrap());
  assert!(again.contains("Nothing to upgrade."), "{again}");
  assert!(again.contains("held is pinned to 1.0.0"), "{again}");
}

#[test]
fn outdated_names_what_nothing_can_upgrade_any_more() {
  let fixture = Fixture::new();
  for id in ["current", "gone", "foreign"] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, &[]);
  }
  fixture
    .luthier()
    .args(["install", "current", "gone", "foreign"])
    .assert()
    .success();
  let artifact = fixture.make_artifact("Current");
  fixture.add_plugin("current", "1.1.0", &artifact, &[]);
  std::fs::remove_file(fixture.registry().join("plugins/gone.toml")).unwrap();
  // Still carried, with nothing built for this machine.
  let other = if ARCH == "x86_64" {
    "aarch64"
  } else {
    "x86_64"
  };
  let foreign = fixture.registry().join("plugins/foreign.toml");
  let text = std::fs::read_to_string(&foreign).unwrap();
  std::fs::write(
    &foreign,
    text.replace(
      &format!("arch = \"{ARCH}\""),
      &format!("arch = \"{other}\""),
    ),
  )
  .unwrap();

  let updated = stdout_of(&fixture.luthier().arg("update").output().unwrap());
  assert!(
    updated.contains(
      "1 package can be upgraded. 2 installed packages can no longer be upgraded. \
       Run `luthier outdated` to see them."
    ),
    "{updated}"
  );

  let outdated = stdout_of(&fixture.luthier().arg("outdated").output().unwrap());
  assert!(outdated.contains("1.0.0 -> 1.1.0"), "{outdated}");
  assert!(
    outdated.contains("Staying at the version installed"),
    "{outdated}"
  );
  assert!(
    outdated.contains("no source carries it any more"),
    "{outdated}"
  );
  assert!(
    outdated.contains("no release is built for this machine any more"),
    "{outdated}"
  );

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["outdated", "--json"])
      .output()
      .unwrap(),
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  assert_eq!(found["updates"][0]["id"], "current");
  let reasons: Vec<(&str, &str)> = found["unavailable"]
    .as_array()
    .unwrap()
    .iter()
    .map(|p| (p["id"].as_str().unwrap(), p["reason"].as_str().unwrap()))
    .collect();
  assert_eq!(
    reasons,
    [
      ("foreign", "no-release-for-target"),
      ("gone", "not-carried")
    ]
  );

  // Upgrading everything upgrades what it can, and says what it left.
  let upgraded = fixture.luthier().arg("upgrade").output().unwrap();
  let stdout = stdout_of(&upgraded);
  assert!(upgraded.status.success(), "{stdout}");
  assert!(
    stdout.contains("Gone stays at 1.0.0: no source carries it any more."),
    "{stdout}"
  );
  let listed = stdout_of(&fixture.luthier().arg("list").output().unwrap());
  assert!(listed.contains("1.1.0"), "{listed}");
  assert!(listed.contains("Gone"), "{listed}");
}

/// The reason `list --json` records for `id`.
fn reason_of(fixture: &Fixture, id: &str) -> String {
  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let list: serde_json::Value = serde_json::from_str(&json).unwrap();
  list
    .as_array()
    .unwrap()
    .iter()
    .find(|p| p["id"] == id)
    .unwrap_or_else(|| panic!("{id} is not listed: {json}"))["reason"]
    .as_str()
    .unwrap()
    .to_owned()
}

#[test]
fn a_package_asked_for_by_name_is_never_demoted_to_a_dependency() {
  // Installed by name, then upgraded because another package needs it: it
  // is still the user's, and `autoremove` must not take it with the other.
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  fixture
    .luthier()
    .args(["install", "engine"])
    .assert()
    .success();
  let engine = fixture.make_artifact("Engine");
  fixture.add_plugin("engine", "1.1.0", &engine, &[]);
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);

  let output = fixture
    .luthier()
    .args(["install", "library"])
    .output()
    .unwrap();
  assert!(output.status.success(), "{}", stdout_of(&output));
  assert_eq!(reason_of(&fixture, "engine"), "explicit");

  fixture
    .luthier()
    .args(["remove", "library"])
    .assert()
    .success();
  let json = stdout_of(
    &fixture
      .luthier()
      .args(["autoremove", "--dry-run", "--json"])
      .output()
      .unwrap(),
  );
  let planned: serde_json::Value = serde_json::from_str(&json).unwrap();
  assert_eq!(planned["packages"], serde_json::json!([]), "{json}");
}

#[test]
fn installing_a_dependency_by_name_keeps_it_from_autoremove() {
  // apt marks such a package manual. Nothing is downloaded or asked: the
  // package is there, and only why it is there changes.
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();
  assert_eq!(reason_of(&fixture, "engine"), "dependency");

  let output = fixture
    .luthier()
    .args(["install", "engine"])
    .output()
    .unwrap();
  let stdout = stdout_of(&output);
  assert!(output.status.success(), "{stdout}");
  assert!(stdout.contains("records it as requested"), "{stdout}");
  assert_eq!(reason_of(&fixture, "engine"), "explicit");

  fixture
    .luthier()
    .args(["remove", "library"])
    .assert()
    .success();
  fixture.luthier().arg("autoremove").assert().success();
  assert!(fixture.clap_dir().join("Engine.clap").is_file());
}

#[test]
fn a_package_nothing_can_upgrade_stays_and_everything_else_upgrades() {
  // No source builds `dep` for this machine any more. It still works where
  // it is, so `plugin`, which needs it, resolves against the copy
  // installed, and one gone package no longer stops every upgrade.
  let fixture = Fixture::new();
  for (id, deps) in [
    ("dep", &[][..]),
    ("plugin", &["dep"][..]),
    ("other", &[][..]),
    ("engine", &[][..]),
    ("library", &["engine"][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "plugin", "other", "library"])
    .assert()
    .success();
  // `engine` is an orphan from here on: nothing installed needs it.
  fixture
    .luthier()
    .args(["remove", "library"])
    .assert()
    .success();
  for (id, deps) in [
    ("plugin", &["dep"][..]),
    ("other", &[][..]),
    ("engine", &[][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.1.0", &artifact, deps);
  }
  let other_arch = if ARCH == "x86_64" {
    "aarch64"
  } else {
    "x86_64"
  };
  let dep = fixture.registry().join("plugins/dep.toml");
  let text = std::fs::read_to_string(&dep).unwrap();
  std::fs::write(
    &dep,
    text.replace(
      &format!("arch = \"{ARCH}\""),
      &format!("arch = \"{other_arch}\""),
    ),
  )
  .unwrap();

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["outdated", "--json"])
      .output()
      .unwrap(),
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  let updates: Vec<&str> = found["updates"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| u["id"].as_str().unwrap())
    .collect();
  assert_eq!(updates, ["other", "plugin"], "{json}");
  let held: Vec<(&str, &str)> = found["unavailable"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| (u["id"].as_str().unwrap(), u["reason"].as_str().unwrap()))
    .collect();
  assert_eq!(
    held,
    [("dep", "no-release-for-target"), ("engine", "orphaned")],
    "{json}"
  );
  let updated = stdout_of(&fixture.luthier().arg("update").output().unwrap());
  assert!(
    updated.contains(
      "2 packages can be upgraded. 1 installed package can no longer be upgraded. \
       1 update is for a dependency nothing needs now."
    ),
    "{updated}"
  );

  let output = fixture.luthier().arg("upgrade").output().unwrap();
  let stdout = stdout_of(&output);
  assert!(
    output.status.success(),
    "{stdout}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    stdout.contains("Dep stays at 1.0.0: no release is built for this machine any more."),
    "{stdout}"
  );
  let listed = || {
    let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
    serde_json::from_str::<serde_json::Value>(&json).unwrap()
  };
  let version = |listed: &serde_json::Value, id: &str| {
    listed
      .as_array()
      .unwrap()
      .iter()
      .find(|p| p["id"] == id)
      .unwrap()["version"]
      .as_str()
      .unwrap()
      .to_owned()
  };
  let now = listed();
  assert_eq!(version(&now, "other"), "1.1.0");
  assert_eq!(version(&now, "plugin"), "1.1.0");
  assert_eq!(version(&now, "dep"), "1.0.0");
  assert_eq!(version(&now, "engine"), "1.0.0", "an orphan is not reached");

  // A release that needs more than the copy installed is a conflict, said
  // with what holds it. With nothing named, that package is held back and
  // the rest still upgrades; `outdated` says the same.
  let artifact = fixture.make_artifact("Plugin");
  fixture.add_plugin("plugin", "1.2.0", &artifact, &["dep"]);
  let manifest = fixture.registry().join("plugins/plugin.toml");
  let text = std::fs::read_to_string(&manifest).unwrap().replace(
    "dependencies = [\"dep\"]",
    "dependencies = [{ id = \"dep\", version = \">=2\" }]",
  );
  std::fs::write(&manifest, text).unwrap();
  let artifact = fixture.make_artifact("Other");
  fixture.add_plugin("other", "1.2.0", &artifact, &[]);
  let conflict = "plugin needs dep >=2, but dep stays at 1.0.0: no release of it is built for \
                  this machine any more";

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["outdated", "--json"])
      .output()
      .unwrap(),
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  let updates: Vec<&str> = found["updates"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| u["id"].as_str().unwrap())
    .collect();
  assert_eq!(updates, ["other"], "{json}");
  let plugin = found["unavailable"]
    .as_array()
    .unwrap()
    .iter()
    .find(|u| u["id"] == "plugin")
    .unwrap_or_else(|| panic!("{json}"));
  assert_eq!(plugin["reason"], "unresolvable", "{json}");
  assert_eq!(plugin["available"], "1.2.0", "{json}");
  assert_eq!(plugin["detail"], conflict, "{json}");

  let output = fixture.luthier().arg("upgrade").output().unwrap();
  let stdout = stdout_of(&output);
  assert!(
    output.status.success(),
    "{stdout}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    stdout.contains(&format!(
      "Plugin stays at 1.1.0: its upgrade cannot be resolved: {conflict}."
    )),
    "{stdout}"
  );
  let now = listed();
  assert_eq!(version(&now, "plugin"), "1.1.0");
  assert_eq!(version(&now, "other"), "1.2.0");

  // Named, it is not held back but refused, with the same reason.
  let output = fixture
    .luthier()
    .args(["upgrade", "plugin"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains(conflict), "{stderr}");
  assert_eq!(version(&listed(), "plugin"), "1.1.0");

  // Named, `dep` itself is left and said to be.
  let output = fixture.luthier().args(["upgrade", "dep"]).output().unwrap();
  let stdout = stdout_of(&output);
  assert!(output.status.success(), "{stdout}");
  assert!(stdout.contains("Nothing to upgrade."), "{stdout}");
  assert!(stdout.contains("Dep stays at 1.0.0"), "{stdout}");
}

#[test]
fn what_a_package_nothing_can_upgrade_needs_stays_where_it_is() {
  // `old` left every source, and needs `shared`. Upgrading `new`, which
  // needs `shared` too, must not move `shared` past what `old` was built
  // against: it is held, and said to be, and the rest upgrades.
  let fixture = Fixture::new();
  for (id, deps) in [
    ("shared", &[][..]),
    ("old", &["shared"][..]),
    ("new", &["shared"][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "old", "new"])
    .assert()
    .success();
  std::fs::remove_file(fixture.registry().join("plugins/old.toml")).unwrap();
  for (id, deps) in [("shared", &[][..]), ("new", &["shared"][..])] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "2.0.0", &artifact, deps);
  }
  // The registry keeps only 2.0.0, as extras does: what is held is
  // resolved from the copy installed, not from a release.

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["outdated", "--json"])
      .output()
      .unwrap(),
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  assert_eq!(found["updates"][0]["id"], "new", "{json}");
  assert_eq!(found["updates"].as_array().unwrap().len(), 1, "{json}");
  let held = found["unavailable"]
    .as_array()
    .unwrap()
    .iter()
    .find(|u| u["id"] == "shared")
    .unwrap_or_else(|| panic!("{json}"));
  assert_eq!(held["reason"], "needed-by-unavailable");
  assert_eq!(held["because_of"], "old");

  let output = fixture.luthier().arg("upgrade").output().unwrap();
  let stdout = stdout_of(&output);
  assert!(
    output.status.success(),
    "{stdout}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    stdout.contains("Shared stays at 1.0.0: old needs it, and can no longer be upgraded."),
    "{stdout}"
  );
  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let listed: serde_json::Value = serde_json::from_str(&json).unwrap();
  let version = |id: &str| {
    listed
      .as_array()
      .unwrap()
      .iter()
      .find(|p| p["id"] == id)
      .unwrap()["version"]
      .as_str()
      .unwrap()
      .to_owned()
  };
  assert_eq!(version("new"), "2.0.0");
  assert_eq!(version("shared"), "1.0.0");
}

/// Adds `version` of `id` beside the release its manifest already has.
fn add_release(fixture: &Fixture, id: &str, version: &str, dependencies: &[&str]) {
  let path = fixture.registry().join(format!("plugins/{id}.toml"));
  let existing = std::fs::read_to_string(&path).unwrap();
  let artifact = fixture.make_artifact(&capitalise(id));
  fixture.add_plugin(id, version, &artifact, dependencies);
  let added = std::fs::read_to_string(&path).unwrap();
  let release = &added[added.find("\n[[releases]]").unwrap()..];
  std::fs::write(&path, format!("{existing}{release}")).unwrap();
}

/// Rewrites a manifest's dependency list.
fn require(fixture: &Fixture, id: &str, from: &str, to: &str) {
  let path = fixture.registry().join(format!("plugins/{id}.toml"));
  let text = std::fs::read_to_string(&path).unwrap();
  assert!(text.contains(from), "{text}");
  std::fs::write(&path, text.replace(from, to)).unwrap();
}

#[test]
fn a_package_held_back_keeps_what_it_needs_where_it_is() {
  // `p` 1.1.0 needs a newer `x` than the one no source builds here any
  // more, so `upgrade` holds `p` back. Its installed release needs `lib <2`,
  // and `q`, upgrading, needs `lib` too: `lib` stays at 1.0.0 rather than
  // moving to 2.0.0 underneath the `p` still installed.
  let fixture = Fixture::new();
  for (id, deps) in [
    ("lib", &[][..]),
    ("x", &[][..]),
    ("p", &["x", "lib"][..]),
    ("q", &["lib"][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  require(
    &fixture,
    "p",
    "dependencies = [\"x\", \"lib\"]",
    "dependencies = [\"x\", { id = \"lib\", version = \"<2\" }]",
  );
  fixture
    .luthier()
    .args(["install", "p", "q"])
    .assert()
    .success();

  let other_arch = if ARCH == "x86_64" {
    "aarch64"
  } else {
    "x86_64"
  };
  let x = fixture.registry().join("plugins/x.toml");
  let text = std::fs::read_to_string(&x).unwrap();
  std::fs::write(
    &x,
    text.replace(
      &format!("arch = \"{ARCH}\""),
      &format!("arch = \"{other_arch}\""),
    ),
  )
  .unwrap();
  add_release(&fixture, "lib", "2.0.0", &[]);
  let artifact = fixture.make_artifact("P");
  fixture.add_plugin("p", "1.1.0", &artifact, &["x", "lib"]);
  require(
    &fixture,
    "p",
    "dependencies = [\"x\", \"lib\"]",
    "dependencies = [{ id = \"x\", version = \">=2\" }, { id = \"lib\", version = \"<2\" }]",
  );
  let artifact = fixture.make_artifact("Q");
  fixture.add_plugin("q", "1.1.0", &artifact, &["lib"]);

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["outdated", "--json"])
      .output()
      .unwrap(),
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  let updates: Vec<&str> = found["updates"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| u["id"].as_str().unwrap())
    .collect();
  assert_eq!(updates, ["q"], "{json}");
  let held: Vec<(&str, &str)> = found["unavailable"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| (u["id"].as_str().unwrap(), u["reason"].as_str().unwrap()))
    .collect();
  assert_eq!(
    held,
    [
      ("lib", "needed-by-held-back"),
      ("p", "unresolvable"),
      ("x", "no-release-for-target")
    ],
    "{json}"
  );

  let output = fixture.luthier().arg("upgrade").output().unwrap();
  let stdout = stdout_of(&output);
  assert!(
    output.status.success(),
    "{stdout}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let listed: serde_json::Value = serde_json::from_str(&json).unwrap();
  let version = |id: &str| {
    listed
      .as_array()
      .unwrap()
      .iter()
      .find(|p| p["id"] == id)
      .unwrap()["version"]
      .as_str()
      .unwrap()
      .to_owned()
  };
  assert_eq!(version("q"), "1.1.0");
  assert_eq!(version("p"), "1.0.0");
  assert_eq!(version("lib"), "1.0.0", "{json}");
}

#[test]
fn packages_whose_upgrades_clash_with_each_other_are_both_held_back() {
  // `alpha` 2.0.0 needs `c <2` and `beta` 2.0.0 needs `c >=2`. Each
  // upgrades alone; together they cannot. Neither is the one to blame, so
  // both stay, said to be, `outdated` still answers, and `gamma` upgrades.
  let fixture = Fixture::new();
  for (id, deps) in [
    ("c", &[][..]),
    ("alpha", &["c"][..]),
    ("beta", &["c"][..]),
    ("gamma", &[][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "alpha", "beta", "gamma"])
    .assert()
    .success();
  add_release(&fixture, "c", "2.0.0", &[]);
  for (id, wanted) in [("alpha", "<2"), ("beta", ">=2")] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "2.0.0", &artifact, &["c"]);
    require(
      &fixture,
      id,
      "dependencies = [\"c\"]",
      &format!("dependencies = [{{ id = \"c\", version = \"{wanted}\" }}]"),
    );
  }
  let artifact = fixture.make_artifact("Gamma");
  fixture.add_plugin("gamma", "1.1.0", &artifact, &[]);

  let output = fixture
    .luthier()
    .args(["outdated", "--json"])
    .output()
    .unwrap();
  let json = stdout_of(&output);
  assert!(
    output.status.success(),
    "{json}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  let updates: Vec<&str> = found["updates"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| u["id"].as_str().unwrap())
    .collect();
  assert_eq!(updates, ["gamma"], "{json}");
  let held: Vec<(&str, &str)> = found["unavailable"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| (u["id"].as_str().unwrap(), u["reason"].as_str().unwrap()))
    .collect();
  assert_eq!(
    held,
    [
      ("alpha", "unresolvable"),
      ("beta", "unresolvable"),
      ("c", "needed-by-held-back")
    ],
    "{json}"
  );

  fixture.luthier().arg("upgrade").assert().success();
  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let listed: serde_json::Value = serde_json::from_str(&json).unwrap();
  let version = |id: &str| {
    listed
      .as_array()
      .unwrap()
      .iter()
      .find(|p| p["id"] == id)
      .unwrap()["version"]
      .as_str()
      .unwrap()
      .to_owned()
  };
  assert_eq!(version("gamma"), "1.1.0");
  assert_eq!(version("alpha"), "1.0.0");
  assert_eq!(version("beta"), "1.0.0");
  assert_eq!(version("c"), "1.0.0");

  // Which one moves is the user's call.
  fixture
    .luthier()
    .args(["upgrade", "beta"])
    .assert()
    .success();
}

#[test]
fn outdated_lists_the_release_upgrade_lands_on() {
  // `plugin` needs `lib <2`. With 1.5.0 and 2.0.0 of `lib` out, `upgrade`
  // brings it to 1.5.0, so that is the update `outdated` and `update` count,
  // not 2.0.0, which nothing would ever install.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Lib");
  fixture.add_plugin("lib", "1.0.0", &artifact, &[]);
  let artifact = fixture.make_artifact("Plugin");
  fixture.add_plugin("plugin", "1.0.0", &artifact, &["lib"]);
  require(
    &fixture,
    "plugin",
    "dependencies = [\"lib\"]",
    "dependencies = [{ id = \"lib\", version = \"<2\" }]",
  );
  fixture
    .luthier()
    .args(["install", "plugin"])
    .assert()
    .success();

  let artifact = fixture.make_artifact("Lib");
  fixture.add_plugin("lib", "1.5.0", &artifact, &[]);
  add_release(&fixture, "lib", "2.0.0", &[]);

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["outdated", "--json"])
      .output()
      .unwrap(),
  );
  let found: serde_json::Value = serde_json::from_str(&json).unwrap();
  let updates: Vec<(&str, &str)> = found["updates"]
    .as_array()
    .unwrap()
    .iter()
    .map(|u| (u["id"].as_str().unwrap(), u["available"].as_str().unwrap()))
    .collect();
  assert_eq!(updates, [("lib", "1.5.0")], "{json}");

  fixture.luthier().arg("upgrade").assert().success();
  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let listed: serde_json::Value = serde_json::from_str(&json).unwrap();
  let lib = listed
    .as_array()
    .unwrap()
    .iter()
    .find(|p| p["id"] == "lib")
    .unwrap();
  assert_eq!(lib["version"], "1.5.0", "{json}");

  // Nothing `upgrade` would do is left, so nothing is counted.
  let updated = stdout_of(&fixture.luthier().arg("update").output().unwrap());
  assert!(updated.contains("Everything is up to date."), "{updated}");
}

#[test]
fn an_import_records_what_its_file_asked_for() {
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();
  assert_eq!(reason_of(&fixture, "engine"), "dependency");

  // Nothing to install, and still the file's word on what was asked for.
  let file = loose_env_file(&fixture, &["engine", "library"]);
  fixture
    .luthier()
    .arg("import")
    .arg(&file)
    .assert()
    .success();
  assert_eq!(reason_of(&fixture, "engine"), "explicit");

  // With `--prune` the file describes the installation, so what it lists
  // as a dependency is one.
  let file = fixture.dir.path().join("declared.toml");
  std::fs::write(
    &file,
    "[meta]\nschema = 1\nluthier = \"0.4.0\"\nexported = \"2026-09-27T00:00:00Z\"\n\
     pinned = false\n\n[[package]]\nid = \"library\"\nreason = \"explicit\"\n\n\
     [[package]]\nid = \"engine\"\nreason = \"dependency\"\n",
  )
  .unwrap();
  fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .assert()
    .success();
  assert_eq!(reason_of(&fixture, "engine"), "dependency");
  assert_eq!(reason_of(&fixture, "library"), "explicit");
}

#[test]
fn import_prune_keeps_what_the_file_names_when_no_source_carries_it() {
  // The Home Manager module runs `import --prune --yes` on every switch.
  // A package the file names, which its source then dropped, still works
  // where it is; removing it would delete what cannot be installed again.
  let fixture = Fixture::new();
  for id in ["keep", "stray"] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, &[]);
  }
  fixture
    .luthier()
    .args(["install", "keep", "stray"])
    .assert()
    .success();
  std::fs::remove_file(fixture.registry().join("plugins/keep.toml")).unwrap();

  let file = loose_env_file(&fixture, &["keep"]);
  let output = fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}{}",
    stdout_of(&output),
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(installed(&fixture), ["keep"]);
  assert!(fixture.clap_dir().join("Keep.clap").is_file());
}

#[test]
fn import_prune_converges_when_the_file_drops_what_held_a_dependency() {
  // `old` left every source and holds `lib` at 1.0.0. The file drops it
  // and asks for `plugin`, which needs lib >=2: the hold goes with `old`,
  // so the import upgrades `lib` and removes `old` rather than refusing.
  let fixture = Fixture::new();
  for (id, deps) in [("lib", &[][..]), ("old", &["lib"][..])] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "old"])
    .assert()
    .success();
  std::fs::remove_file(fixture.registry().join("plugins/old.toml")).unwrap();
  let artifact = fixture.make_artifact("Lib");
  fixture.add_plugin("lib", "2.0.0", &artifact, &[]);
  let artifact = fixture.make_artifact("Plugin");
  fixture.add_plugin("plugin", "1.0.0", &artifact, &["lib"]);
  let manifest = fixture.registry().join("plugins/plugin.toml");
  let text = std::fs::read_to_string(&manifest).unwrap().replace(
    "dependencies = [\"lib\"]",
    "dependencies = [{ id = \"lib\", version = \">=2\" }]",
  );
  std::fs::write(&manifest, text).unwrap();

  let file = loose_env_file(&fixture, &["plugin"]);
  let output = fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}{}",
    stdout_of(&output),
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(installed(&fixture), ["lib", "plugin"]);
}

/// An export-shaped file, `pinned = false`, naming `(id, reason)`.
fn env_file_with(fixture: &Fixture, name: &str, entries: &[(&str, &str)]) -> PathBuf {
  let mut text = String::from(
    "[meta]\nschema = 1\nluthier = \"0.4.0\"\nexported = \"2026-09-27T00:00:00Z\"\npinned = false\n",
  );
  for (id, reason) in entries {
    text.push_str(&format!(
      "\n[[package]]\nid = \"{id}\"\nreason = \"{reason}\"\n"
    ));
  }
  let file = fixture.dir.path().join(name);
  std::fs::write(&file, text).unwrap();
  file
}

#[test]
fn import_prune_keeps_what_a_kept_package_needs() {
  // `gone` left every source and needs `lib`, which nothing else reaches.
  // Kept, it keeps `lib` too, rather than offering it up and then
  // refusing to remove it.
  let fixture = Fixture::new();
  for (id, deps) in [("lib", &[][..]), ("gone", &["lib"][..])] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "gone"])
    .assert()
    .success();
  std::fs::remove_file(fixture.registry().join("plugins/gone.toml")).unwrap();

  let file = env_file_with(
    &fixture,
    "kept.toml",
    &[("gone", "explicit"), ("lib", "dependency")],
  );
  let output = fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}{}",
    stdout_of(&output),
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(installed(&fixture), ["gone", "lib"]);
}

#[test]
fn import_prune_lets_go_of_a_gone_package_the_new_release_no_longer_needs() {
  // The file still lists `old` as a dependency, as an export made before
  // the upgrade would. `plugin` 2.0 needs neither `old` nor lib 1.0: the
  // hold `old` had goes with it, and the import converges.
  let fixture = Fixture::new();
  for (id, deps) in [
    ("lib", &[][..]),
    ("old", &["lib"][..]),
    ("plugin", &["old"][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "plugin"])
    .assert()
    .success();
  std::fs::remove_file(fixture.registry().join("plugins/old.toml")).unwrap();
  let artifact = fixture.make_artifact("Lib");
  fixture.add_plugin("lib", "2.0.0", &artifact, &[]);
  let artifact = fixture.make_artifact("Plugin");
  fixture.add_plugin("plugin", "2.0.0", &artifact, &["lib"]);
  let manifest = fixture.registry().join("plugins/plugin.toml");
  let text = std::fs::read_to_string(&manifest).unwrap().replace(
    "dependencies = [\"lib\"]",
    "dependencies = [{ id = \"lib\", version = \">=2\" }]",
  );
  std::fs::write(&manifest, text).unwrap();

  let file = env_file_with(
    &fixture,
    "stale.toml",
    &[
      ("plugin", "explicit"),
      ("old", "dependency"),
      ("lib", "dependency"),
    ],
  );
  let output = fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}{}",
    stdout_of(&output),
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(installed(&fixture), ["lib", "plugin"]);
}

#[test]
fn reinstalling_a_yanked_release_still_fetches_it() {
  // Yanked is "do not pick this", not "this is gone": the copy installed is
  // still published, so `install --force` can repair it.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Foo");
  fixture.add_plugin("foo", "1.0.0", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "foo"])
    .assert()
    .success();
  let manifest = fixture.registry().join("plugins/foo.toml");
  let text = std::fs::read_to_string(&manifest).unwrap().replace(
    "version = \"1.0.0\"\n",
    "version = \"1.0.0\"\nyanked = \"broken\"\n",
  );
  std::fs::write(&manifest, text).unwrap();
  std::fs::remove_file(fixture.clap_dir().join("Foo.clap")).unwrap();

  let output = fixture
    .luthier()
    .args(["install", "--force", "foo"])
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "{}{}",
    stdout_of(&output),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(fixture.clap_dir().join("Foo.clap").is_file());
}

#[test]
fn asking_for_a_kept_dependency_by_name_records_it_as_requested() {
  // `engine` came in with `library` and then left every source. Asked for
  // by name, it is kept from the state — and is still the user's now.
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();
  std::fs::remove_file(fixture.registry().join("plugins/engine.toml")).unwrap();

  let output = fixture
    .luthier()
    .args(["install", "engine"])
    .output()
    .unwrap();
  assert!(output.status.success(), "{}", stdout_of(&output));
  assert_eq!(reason_of(&fixture, "engine"), "explicit");
}

#[test]
fn a_dependency_asked_for_by_name_stays_requested_when_a_later_download_fails() {
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();
  let broken = fixture.make_artifact("Broken");
  fixture.add_plugin("broken", "1.0.0", &broken, &[]);
  std::fs::remove_file(&broken.path).unwrap();

  let output = fixture
    .luthier()
    .args(["install", "engine", "broken"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert_eq!(reason_of(&fixture, "engine"), "explicit");
}

#[test]
fn json_is_not_consent_and_shows_no_plan_to_confirm() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.0.0", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();
  // The fixture passes `--yes`; this command must not.
  let output = Command::cargo_bin("luthier")
    .unwrap()
    .arg("--root")
    .arg(fixture.root())
    .arg("--registry-path")
    .arg(fixture.registry())
    .args(["--no-system-plugins", "--json", "remove", "testsynth"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(
    String::from_utf8_lossy(&output.stderr).contains("--yes"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(fixture.clap_dir().join("Testsynth.clap").is_file());
}

#[test]
fn update_takes_no_packages_and_upgrade_only_installed_ones() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);

  // `update` fetches lists; it never installs or upgrades anything.
  let output = fixture
    .luthier()
    .args(["update", "testsynth"])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(2));

  // Upgrading something not installed is an install, and is not done unasked.
  let output = fixture
    .luthier()
    .args(["upgrade", "testsynth"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(!fixture.clap_dir().join("Testsynth.clap").exists());
}

#[test]
fn a_dependency_upgraded_by_name_is_still_a_dependency() {
  // Naming it is asking for a newer release, not asking to keep it after
  // nothing needs it: `autoremove` must still collect it (§24).
  let fixture = Fixture::new();
  let engine = fixture.make_artifact("Engine");
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();
  fixture.add_plugin("engine", "1.1.0", &engine, &[]);

  let upgraded = fixture
    .luthier()
    .args(["upgrade", "engine"])
    .output()
    .unwrap();
  assert!(upgraded.status.success(), "{}", stdout_of(&upgraded));

  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let listed: serde_json::Value = serde_json::from_str(&json).unwrap();
  let engine_row = listed
    .as_array()
    .unwrap()
    .iter()
    .find(|p| p["id"] == "engine")
    .unwrap();
  assert_eq!(engine_row["version"], "1.1.0", "{json}");
  assert_eq!(engine_row["reason"], "dependency", "{json}");

  fixture
    .luthier()
    .args(["remove", "library"])
    .assert()
    .success();
  fixture.luthier().arg("autoremove").assert().success();
  assert!(!fixture.clap_dir().join("Engine.clap").exists());
}

#[test]
fn an_unmanaged_plugin_is_never_overwritten() {
  // §30, and the situation on any machine where plugins were installed by hand.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);

  std::fs::create_dir_all(fixture.clap_dir()).unwrap();
  let existing = fixture.clap_dir().join("Testsynth.clap");
  std::fs::write(&existing, b"INSTALLED BY HAND").unwrap();

  let output = fixture
    .luthier()
    .args(["install", "testsynth"])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(5));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("was not installed by Luthier"), "{stderr}");

  assert_eq!(std::fs::read(&existing).unwrap(), b"INSTALLED BY HAND");
  assert!(
    !fixture.vst3_dir().join("Testsynth.vst3").exists(),
    "a blocked install must not leave the other half behind"
  );
}

#[test]
fn removal_deletes_only_files_the_manager_installed() {
  // §20/§21: a similarly named plugin the user put there is not ours to touch.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  let bystander = fixture.clap_dir().join("Testsynth Extras.clap");
  std::fs::write(&bystander, b"NOT OURS").unwrap();

  fixture
    .luthier()
    .args(["remove", "testsynth"])
    .assert()
    .success();

  assert!(!fixture.clap_dir().join("Testsynth.clap").exists());
  assert_eq!(
    std::fs::read(&bystander).unwrap(),
    b"NOT OURS",
    "a plugin with a similar name must survive"
  );
}

#[test]
fn an_unknown_package_exits_three_and_suggests_search() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);

  let output = fixture
    .luthier()
    .args(["install", "nosuchthing"])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(3), "not found is exit code 3");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("luthier search"), "{stderr}");
}

#[test]
fn an_external_dependency_is_reported_rather_than_downloaded() {
  // The sfizz case: depended on, detected, never installed.
  let fixture = Fixture::new();
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("library", "1.0.0", &library, &["engine"]);
  std::fs::write(
    fixture.registry().join("plugins/engine.toml"),
    concat!(
      "schema = 1\n",
      "id = \"engine\"\n",
      "name = \"Engine\"\n",
      "kind = \"external\"\n",
      "category = \"instrument\"\n",
      "license = { kind = \"open-source\", spdx = \"BSD-2-Clause\" }\n",
      "description = \"An engine with no redistributable binary.\"\n",
      "provisioning_hint = \"Install engine from your distribution.\"\n",
      "\n[[detect]]\n",
      "format = \"clap\"\n",
      "name = \"engine.clap\"\n",
    ),
  )
  .unwrap();

  let output = fixture
    .luthier()
    .args(["install", "library"])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(6));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("Install engine from your distribution"),
    "{stderr}"
  );
  assert!(!fixture.clap_dir().join("Library.clap").exists());

  // Once it is present, the install proceeds.
  std::fs::create_dir_all(fixture.clap_dir()).unwrap();
  std::fs::write(fixture.clap_dir().join("engine.clap"), elf_shared_object()).unwrap();
  fixture
    .luthier()
    .args(["install", "library"])
    .assert()
    .success();
  assert!(fixture.clap_dir().join("Library.clap").is_file());
}

#[test]
fn content_nothing_can_play_is_reported_and_installed_anyway() {
  // A DrumGizmo kit: DrumGizmo plays it, and so does DrumCraker. With
  // neither present the kit still installs, and the plan says what it will
  // take to hear it.
  let fixture = Fixture::new();
  let kit = fixture.make_library_artifact("Kit");
  fixture.add_library("kit", "1.0.0", "Kit", &kit);
  let path = fixture.registry().join("plugins/kit.toml");
  let text = std::fs::read_to_string(&path).unwrap().replace(
    "kind = \"library\"",
    "kind = \"library\"\ncontent = [\"drumgizmo\"]",
  );
  std::fs::write(&path, text).unwrap();

  let drumcraker = fixture.make_artifact("Drumcraker");
  fixture.add_plugin("drumcraker", "1.3.4", &drumcraker, &[]);
  std::fs::write(
    fixture.registry().join("plugins/drumgizmo.toml"),
    concat!(
      "schema = 1\n",
      "id = \"drumgizmo\"\n",
      "name = \"DrumGizmo\"\n",
      "kind = \"external\"\n",
      "category = \"instrument\"\n",
      "license = { kind = \"open-source\", spdx = \"LGPL-3.0-or-later\" }\n",
      "description = \"A drum sampler.\"\n",
      "provisioning_hint = \"Install drumgizmo from your distribution.\"\n",
      "\n[[detect]]\n",
      "format = \"lv2\"\n",
      "name = \"drumgizmo.lv2\"\n",
    ),
  )
  .unwrap();
  std::fs::write(
    fixture.registry().join("engines.toml"),
    concat!(
      "schema = 1\n\n",
      "[[engine]]\npackage = \"drumgizmo\"\nplays = [\"drumgizmo\"]\n\n",
      "[[engine]]\npackage = \"drumcraker\"\nplays = [\"drumgizmo\"]\n",
    ),
  )
  .unwrap();
  let installed = fixture.root().join("share/luthier/libraries/Kit");

  // Nothing here plays it. That is said, in a sentence that names what the
  // format needs and what would supply it — and then the install proceeds,
  // because what a user does with a folder of samples is their business.
  let output = fixture.luthier().args(["install", "kit"]).output().unwrap();
  assert!(output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  for expected in [
    "kit holds DrumGizmo content, and playing it needs DrumGizmo",
    "luthier install drumcraker",
    "Install drumgizmo from your distribution.",
  ] {
    assert!(stderr.contains(expected), "{expected}: {stderr}");
  }
  assert!(installed.join("Strings/violin.sfz").is_file());
  fixture.luthier().args(["remove", "kit"]).assert().success();

  // An engine arriving in the same run leaves nothing to say.
  let output = fixture
    .luthier()
    .args(["install", "kit", "drumcraker"])
    .output()
    .unwrap();
  assert!(output.status.success());
  assert!(
    !String::from_utf8_lossy(&output.stderr).contains("holds DrumGizmo content"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(installed.join("Strings/violin.sfz").is_file());
  fixture
    .luthier()
    .args(["remove", "kit", "drumcraker"])
    .assert()
    .success();

  // So does one this manager did not install.
  let lv2 = fixture.root().join(".lv2/drumgizmo.lv2");
  std::fs::create_dir_all(&lv2).unwrap();
  let output = fixture.luthier().args(["install", "kit"]).output().unwrap();
  assert!(output.status.success());
  assert!(
    !String::from_utf8_lossy(&output.stderr).contains("holds DrumGizmo content"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(installed.is_dir());

  let info = fixture.luthier().args(["info", "kit"]).output().unwrap();
  let stdout = stdout_of(&info);
  assert!(stdout.contains("DrumGizmo"), "{stdout}");
  assert!(stdout.contains("drumgizmo, drumcraker"), "{stdout}");
}

#[test]
fn a_reinstall_reuses_the_verified_cache_and_is_offline_capable() {
  // §28: a removed and reinstalled package need not be downloaded again.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();
  fixture
    .luthier()
    .args(["remove", "testsynth"])
    .assert()
    .success();

  // Delete the source so only a genuine cache hit can succeed.
  std::fs::remove_file(&artifact.path).unwrap();
  let output = fixture
    .luthier()
    .args(["--offline", "install", "testsynth"])
    .output()
    .unwrap();
  assert!(
    output.status.success(),
    "a cached artifact should install offline: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(fixture.clap_dir().join("Testsynth.clap").is_file());
}

#[test]
fn scanning_distinguishes_managed_from_unmanaged_plugins() {
  // §29's two statuses.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  std::fs::write(
    fixture.clap_dir().join("Handmade.clap"),
    elf_shared_object(),
  )
  .unwrap();

  let output = fixture
    .luthier()
    .args(["list", "--unmanaged"])
    .output()
    .unwrap();
  let text = stdout_of(&output);
  assert!(text.contains("Installed by Luthier"), "{text}");
  assert!(text.contains("Installed outside Luthier"), "{text}");

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["list", "--unmanaged", "--json"])
      .output()
      .unwrap(),
  );
  let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
  let statuses: Vec<(&str, &str)> = rows
    .as_array()
    .unwrap()
    .iter()
    .map(|r| (r["name"].as_str().unwrap(), r["status"].as_str().unwrap()))
    .collect();
  assert!(
    statuses.contains(&("Testsynth.clap", "managed")),
    "{statuses:?}"
  );
  assert!(
    statuses.contains(&("Handmade.clap", "unmanaged")),
    "{statuses:?}"
  );
}

#[test]
fn json_output_is_valid_for_every_read_only_command() {
  // §33: `--json` must be parseable, not merely present.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  for args in [
    vec!["search", "synth"],
    vec!["info", "testsynth"],
    vec!["list"],
    vec!["verify"],
    vec!["autoremove", "--dry-run"],
    vec!["outdated"],
  ] {
    let mut command = fixture.luthier();
    command.args(&args).arg("--json");
    let output = command.output().unwrap();
    let text = stdout_of(&output);
    serde_json::from_str::<serde_json::Value>(&text)
      .unwrap_or_else(|e| panic!("`{}` did not emit valid JSON: {e}\n{text}", args.join(" ")));
  }
}

#[test]
fn a_pack_resolves_to_its_members_and_installs_no_files_itself() {
  // §49: a pack is metadata that resolves to dependencies.
  let fixture = Fixture::new();
  let one = fixture.make_artifact("One");
  let two = fixture.make_artifact("Two");
  fixture.add_plugin("one", "1.0.0", &one, &[]);
  fixture.add_plugin("two", "1.0.0", &two, &[]);
  std::fs::write(
    fixture.registry().join("packs/studio.toml"),
    concat!(
      "schema = 1\n",
      "id = \"studio\"\n",
      "name = \"Studio Pack\"\n",
      "kind = \"pack\"\n",
      "category = \"pack\"\n",
      "license = { kind = \"open-source\", spdx = \"CC0-1.0\" }\n",
      "description = \"A curated set.\"\n",
      "\n[[releases]]\n",
      "version = \"1.0.0\"\n",
      "dependencies = [\"one\", \"two\"]\n",
    ),
  )
  .unwrap();

  fixture
    .luthier()
    .args(["install", "studio"])
    .assert()
    .success();
  assert!(fixture.clap_dir().join("One.clap").is_file());
  assert!(fixture.clap_dir().join("Two.clap").is_file());

  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let packages: serde_json::Value = serde_json::from_str(&json).unwrap();
  let pack = packages
    .as_array()
    .unwrap()
    .iter()
    .find(|p| p["id"] == "studio")
    .expect("the pack is recorded");
  assert_eq!(
    pack["files"].as_array().unwrap().len(),
    0,
    "a pack owns no files"
  );
}

#[test]
fn the_state_directory_stays_inside_the_given_root() {
  // The guarantee the whole suite depends on: nothing escapes `--root`.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  let state = fixture.root().join("share/luthier/state/state.json");
  assert!(state.is_file(), "state should live under the root");
  let text = std::fs::read_to_string(&state).unwrap();
  let root = fixture.root().display().to_string();
  for path in ["/.clap/", "/.vst3/"] {
    assert!(
      !text.contains(&format!("\"{path}")),
      "state should only contain rooted paths"
    );
  }
  assert!(
    text.contains(&root),
    "recorded paths should be under {root}"
  );
}

/// Paths recorded in state must be absolute and inside the root.
#[test]
fn recorded_paths_are_absolute() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let packages: serde_json::Value = serde_json::from_str(&json).unwrap();
  for file in packages[0]["files"].as_array().unwrap() {
    let path = file.as_str().unwrap();
    assert!(Path::new(path).is_absolute(), "{path} should be absolute");
    assert!(
      path.starts_with(&fixture.root().display().to_string()),
      "{path}"
    );
  }
}

#[test]
fn a_sample_library_installs_under_the_library_root() {
  // The question `kind: library` answers: content has no plugin format, so
  // no format root can supply its destination. It must land in the library
  // root and nowhere near ~/.clap or ~/.vst3.
  let fixture = Fixture::new();
  let artifact = fixture.make_library_artifact("VSCO-2-CE");
  fixture.add_library("vsco2", "1.1.0", "VSCO-2-CE", &artifact);

  fixture
    .luthier()
    .args(["install", "vsco2"])
    .assert()
    .success();

  let installed = fixture.root().join("share/luthier/libraries/VSCO-2-CE");
  assert!(installed.join("Strings/violin.sfz").is_file());
  assert!(installed.join("Strings/violin.wav").is_file());
  assert!(!fixture.clap_dir().join("VSCO-2-CE").exists());
  assert!(!fixture.vst3_dir().join("VSCO-2-CE").exists());

  // And it is owned, so removal takes it away again.
  fixture
    .luthier()
    .args(["remove", "vsco2"])
    .assert()
    .success();
  assert!(!installed.exists());
}

#[test]
fn a_library_with_no_rules_installs_under_its_package_id() {
  // What the Open Audio Stack registry publishes: `contains: sfz` and no
  // install rules. The archive's shape says where the content is, and the
  // package ID says what to call it — the directory in the archive is named
  // after a commit and changes on every release, so installing under it
  // would move the content out from under whatever plays it.
  let fixture = Fixture::new();

  let wrapped = fixture.make_library_artifact("BillieDrum-48fadc0");
  fixture.add_library_derived("billiedrum", "1.0.0", &wrapped);
  let flat = fixture.make_flat_library_artifact("avl");
  fixture.add_library_derived("avl-percussions", "1.1.0", &flat);

  fixture
    .luthier()
    .args(["install", "billiedrum", "avl-percussions"])
    .assert()
    .success();

  let libraries = fixture.root().join("share/luthier/libraries");
  // The wrapper directory is descended into, and its name is not used.
  assert!(libraries.join("billiedrum/Strings/violin.sfz").is_file());
  assert!(!libraries.join("BillieDrum-48fadc0").exists());
  // A flat archive installs whole, under the same predictable name.
  assert!(libraries.join("avl-percussions/kit.sfz").is_file());
  assert!(libraries.join("avl-percussions/samples/kick.wav").is_file());

  // Nothing went near a plugin root.
  assert!(!fixture.clap_dir().join("billiedrum").exists());
  assert!(!fixture.vst3_dir().join("avl-percussions").exists());

  // Owned like anything else: verified and removed by recorded path.
  fixture.luthier().args(["verify"]).assert().success();
  fixture
    .luthier()
    .args(["remove", "billiedrum", "avl-percussions"])
    .assert()
    .success();
  assert!(!libraries.join("billiedrum").exists());
  assert!(!libraries.join("avl-percussions").exists());
}

/// §51: an export reproduces an installation somewhere else,
/// which is the whole reason the file records versions rather than names.
#[test]
fn an_export_reproduces_the_same_set_elsewhere() {
  let source = Fixture::new();
  let one = source.make_artifact("One");
  let two = source.make_artifact("Two");
  // `two` arrives only as a dependency, so the file has to carry it even
  // though the import will not ask for it by name.
  source.add_plugin("two", "1.0.0", &two, &[]);
  source.add_plugin("one", "1.0.0", &one, &["two"]);
  source.luthier().args(["install", "one"]).assert().success();

  let exported = stdout_of(&source.luthier().args(["export"]).output().unwrap());
  assert!(exported.contains("pinned = true"), "{exported}");
  assert!(exported.contains(r#"id = "two""#), "{exported}");
  assert!(exported.contains(r#"reason = "dependency""#), "{exported}");

  // A second machine: same registry, empty root.
  let target = Fixture::new();
  for id in ["one", "two"] {
    std::fs::copy(
      source.registry().join(format!("plugins/{id}.toml")),
      target.registry().join(format!("plugins/{id}.toml")),
    )
    .unwrap();
  }
  let file = target.dir.path().join("env.toml");
  std::fs::write(&file, &exported).unwrap();

  target
    .luthier()
    .args(["import"])
    .arg(&file)
    .assert()
    .success();

  assert!(target.clap_dir().join("One.clap").is_file());
  assert!(target.clap_dir().join("Two.clap").is_file());

  // The dependency stays a dependency, so `autoremove` still owns it (§24).
  let json = stdout_of(&target.luthier().args(["list", "--json"]).output().unwrap());
  let packages: serde_json::Value = serde_json::from_str(&json).unwrap();
  let two_entry = packages
    .as_array()
    .unwrap()
    .iter()
    .find(|p| p["id"] == "two")
    .expect("the dependency is installed");
  assert_eq!(two_entry["reason"], "dependency");
}

/// A `--loose` export deliberately records no versions.
#[test]
fn a_loose_export_omits_versions() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("One");
  fixture.add_plugin("one", "1.0.0", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "one"])
    .assert()
    .success();

  let exported = stdout_of(
    &fixture
      .luthier()
      .args(["export", "--loose"])
      .output()
      .unwrap(),
  );
  assert!(exported.contains("pinned = false"), "{exported}");
  assert!(!exported.contains("1.0.0"), "{exported}");
}

/// An environment file naming a version the registry cannot supply must fail
/// rather than quietly installing something else.
#[test]
fn an_import_refuses_a_version_the_registry_no_longer_has() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("One");
  fixture.add_plugin("one", "1.0.0", &artifact, &[]);

  let file = fixture.dir.path().join("env.toml");
  std::fs::write(
    &file,
    concat!(
      "[meta]\n",
      "schema = 1\n",
      "luthier = \"0.1.0\"\n",
      "exported = \"2026-09-06T21:15:00Z\"\n",
      "pinned = true\n",
      "\n[[package]]\n",
      "id = \"one\"\n",
      "version = \"9.9.9\"\n",
      "registry = \"default\"\n",
      "reason = \"explicit\"\n",
    ),
  )
  .unwrap();

  let output = fixture
    .luthier()
    .args(["import"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(!fixture.clap_dir().join("One.clap").exists());
}

/// The IDs installed, sorted.
fn installed(fixture: &Fixture) -> Vec<String> {
  let json = stdout_of(&fixture.luthier().args(["list", "--json"]).output().unwrap());
  let list: serde_json::Value = serde_json::from_str(&json).unwrap();
  let mut ids: Vec<String> = list
    .as_array()
    .unwrap()
    .iter()
    .map(|p| p["id"].as_str().unwrap().to_owned())
    .collect();
  ids.sort();
  ids
}

/// A loose environment file naming `ids` as explicit.
fn loose_env_file(fixture: &Fixture, ids: &[&str]) -> PathBuf {
  let mut text = String::from(
    "[meta]\nschema = 1\nluthier = \"0.2.0\"\nexported = \"2026-09-27T00:00:00Z\"\npinned = false\n",
  );
  for id in ids {
    text.push_str(&format!(
      "\n[[package]]\nid = \"{id}\"\nregistry = \"local\"\nreason = \"explicit\"\n"
    ));
  }
  let file = fixture.dir.path().join("env.toml");
  std::fs::write(&file, text).unwrap();
  file
}

#[test]
fn an_import_with_prune_converges_on_the_file() {
  // Without --prune an import only adds, so the file is a lower bound on
  // the installation. A declaration — the Home Manager module is one — needs
  // it to be the installation: what it does not name, and nothing it names
  // needs, goes.
  let fixture = Fixture::new();
  for (id, deps) in [
    ("one", &[][..]),
    ("two", &[][..]),
    ("three", &[][..]),
    ("base", &[][..]),
    ("host", &["base"][..]),
  ] {
    let artifact = fixture.make_artifact(&capitalise(id));
    fixture.add_plugin(id, "1.0.0", &artifact, deps);
  }
  fixture
    .luthier()
    .args(["install", "one", "two", "host"])
    .assert()
    .success();

  // Additive: three arrives, nothing leaves.
  let file = loose_env_file(&fixture, &["one", "three"]);
  fixture
    .luthier()
    .args(["import"])
    .arg(&file)
    .assert()
    .success();
  assert_eq!(installed(&fixture), ["base", "host", "one", "three", "two"]);

  // Converging: two and host go, and base with them, since only host
  // needed it.
  fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .assert()
    .success();
  assert_eq!(installed(&fixture), ["one", "three"]);
  assert!(!fixture.clap_dir().join("Two.clap").exists());
  assert!(!fixture.clap_dir().join("Base.clap").exists());

  // Applying it again is no work at all.
  let again = fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(again.status.success());
  let text = format!(
    "{}{}",
    stdout_of(&again),
    String::from_utf8_lossy(&again.stderr)
  );
  assert!(text.contains("Nothing to do"), "{text}");

  // A dependency the file names stays exactly as long as something named
  // needs it; naming host keeps base.
  let file = loose_env_file(&fixture, &["host"]);
  fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&file)
    .assert()
    .success();
  assert_eq!(installed(&fixture), ["base", "host"]);

  // A file naming nothing removes everything with --prune, and is
  // refused as pointless without it.
  let empty = loose_env_file(&fixture, &[]);
  fixture
    .luthier()
    .args(["import", "--prune"])
    .arg(&empty)
    .assert()
    .success();
  assert!(installed(&fixture).is_empty());
}

#[test]
fn an_artifact_without_rules_installs_what_the_archive_holds() {
  // The same archive, installed both ways, must land the same files: a
  // source that carries no rules is not a second class of install.
  let fixture = Fixture::new();
  let declared = fixture.make_artifact("Declared");
  let derived = fixture.make_artifact("Derived");
  fixture.add_plugin("declared", "1.0.0", &declared, &[]);
  fixture.add_plugin_derived("derived", "1.0.0", &derived);

  for id in ["declared", "derived"] {
    fixture.luthier().args(["install", id]).assert().success();
  }

  for name in ["Declared", "Derived"] {
    assert!(
      fixture.clap_dir().join(format!("{name}.clap")).is_file(),
      "{name}.clap missing"
    );
    assert!(
      fixture.vst3_dir().join(format!("{name}.vst3")).is_dir(),
      "{name}.vst3 missing"
    );
  }
}

#[test]
fn a_derived_install_that_finds_nothing_fails_rather_than_recording_an_empty_package() {
  // sfizz's entry in the Open Audio Stack registry points at the source
  // tarball while claiming to contain LV2 and VST3. Extracting it succeeds
  // and yields no plugin, and recording that as a successful install would
  // hide the upstream error behind a package that does nothing.
  let fixture = Fixture::new();
  let artifact = fixture.make_library_artifact("Samples");
  fixture.add_plugin_derived("samples", "1.0.0", &artifact);

  let output = fixture
    .luthier()
    .args(["install", "samples"])
    .assert()
    .failure();

  let stderr = String::from_utf8_lossy(&output.get_output().stderr).to_string();
  assert!(
    stderr.contains("no CLAP, VST3 or LV2 plugin"),
    "unexpected failure: {stderr}"
  );
  assert!(!fixture.clap_dir().exists());
}

#[test]
fn a_claim_the_archive_does_not_deliver_is_reported() {
  // sfizz's Open Audio Stack entry says the file contains LV2 and VST3 and
  // points at the source tarball. Here the archive delivers two of the three
  // claimed formats, which installs fine and still means upstream metadata
  // is wrong — worth saying, not worth refusing.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Claimant");
  fixture.add_plugin_derived_claiming("claimant", "1.0.0", &artifact, &["clap", "vst3", "lv2"]);

  let assert = fixture
    .luthier()
    .args(["install", "claimant"])
    .assert()
    .success();
  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

  assert!(
    stderr.contains("claims to provide lv2"),
    "unexpected stderr: {stderr}"
  );
  assert!(fixture.clap_dir().join("Claimant.clap").is_file());
}

#[test]
fn a_claim_the_archive_does_deliver_says_nothing() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Quiet");
  fixture.add_plugin_derived_claiming("quiet", "1.0.0", &artifact, &["clap", "vst3"]);

  let assert = fixture
    .luthier()
    .args(["install", "quiet"])
    .assert()
    .success();
  let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

  assert!(!stderr.contains("claims to provide"), "{stderr}");
}

#[test]
fn the_cache_can_be_inspected_and_pruned() {
  // A 5 GiB sample library otherwise occupies the disk twice over for ever:
  // once installed, once as the archive it came from.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Cached");
  fixture.add_plugin("cached", "1.0.0", &artifact, &[]);

  fixture
    .luthier()
    .args(["install", "cached"])
    .assert()
    .success();

  // While it is installed, its archive is in use and stays.
  let listed = fixture.luthier().args(["cache", "list"]).output().unwrap();
  let text = stdout_of(&listed);
  assert!(text.contains("in use"), "{text}");
  assert!(text.contains("cached"), "{text}");

  let kept = fixture.luthier().args(["cache", "clean"]).output().unwrap();
  assert!(
    stdout_of(&kept).contains("Nothing in the cache to reclaim"),
    "{}",
    stdout_of(&kept)
  );

  // Once the package is gone, nothing records that digest any more.
  fixture
    .luthier()
    .args(["remove", "cached"])
    .assert()
    .success();

  let dry = fixture
    .luthier()
    .args(["cache", "clean", "--dry-run"])
    .output()
    .unwrap();
  assert!(
    stdout_of(&dry).contains("Would remove 1 file"),
    "{}",
    stdout_of(&dry)
  );
  // A dry run deletes nothing, so the reinstall path (§28) still works.
  let still_there = fixture.luthier().args(["cache", "list"]).output().unwrap();
  assert!(
    stdout_of(&still_there).contains("unused"),
    "{}",
    stdout_of(&still_there)
  );

  let cleaned = fixture.luthier().args(["cache", "clean"]).output().unwrap();
  assert!(
    stdout_of(&cleaned).contains("Removed 1 file"),
    "{}",
    stdout_of(&cleaned)
  );

  let empty = fixture.luthier().args(["cache", "list"]).output().unwrap();
  assert!(
    stdout_of(&empty).contains("cache is empty"),
    "{}",
    stdout_of(&empty)
  );
}

#[test]
fn removing_the_last_engine_warns_about_the_content_it_leaves_silent() {
  // The check that runs at install time has to run here too, or a kit goes
  // quiet with nothing said about why.
  let fixture = Fixture::new();
  let kit = fixture.make_library_artifact("Kit");
  fixture.add_library("kit", "1.0.0", "Kit", &kit);
  let path = fixture.registry().join("plugins/kit.toml");
  let text = std::fs::read_to_string(&path).unwrap().replace(
    "kind = \"library\"",
    "kind = \"library\"\ncontent = [\"drumgizmo\"]",
  );
  std::fs::write(&path, text).unwrap();

  let drumcraker = fixture.make_artifact("Drumcraker");
  fixture.add_plugin("drumcraker", "1.3.4", &drumcraker, &[]);
  std::fs::write(
    fixture.registry().join("engines.toml"),
    "schema = 1\n\n[[engine]]\npackage = \"drumcraker\"\nplays = [\"drumgizmo\"]\n",
  )
  .unwrap();

  fixture
    .luthier()
    .args(["install", "kit", "drumcraker"])
    .assert()
    .success();

  // Removing is still allowed: the user may be about to install an engine
  // from their distribution. What must not happen is silence.
  let removed = fixture
    .luthier()
    .args(["remove", "drumcraker"])
    .output()
    .unwrap();
  assert!(removed.status.success());
  let text = format!(
    "{}{}",
    stdout_of(&removed),
    String::from_utf8_lossy(&removed.stderr)
  );
  // The plan says it before anything is deleted, and the outcome after.
  assert!(
    text.contains("kit would be left with nothing to play"),
    "{text}"
  );
  assert!(text.contains("kit is left with nothing to play"), "{text}");
  assert!(text.contains("drumcraker"), "{text}");

  // Taking both together says nothing: the kit is going too.
  fixture
    .luthier()
    .args(["install", "drumcraker"])
    .assert()
    .success();
  let both = fixture
    .luthier()
    .args(["remove", "kit", "drumcraker"])
    .output()
    .unwrap();
  let both_text = format!(
    "{}{}",
    stdout_of(&both),
    String::from_utf8_lossy(&both.stderr)
  );
  assert!(!both_text.contains("nothing to play"), "{both_text}");
}

#[test]
fn autoremove_keeps_the_last_engine_for_content_still_installed() {
  // A kit does not depend on its player, so the player a pack brought in
  // is an orphan once the pack goes. Collecting it would leave the kit
  // silent; it stays, and so does what it needs in turn.
  let fixture = Fixture::new();
  let kit = fixture.make_library_artifact("Kit");
  fixture.add_library("kit", "1.0.0", "Kit", &kit);
  let path = fixture.registry().join("plugins/kit.toml");
  let text = std::fs::read_to_string(&path).unwrap().replace(
    "kind = \"library\"",
    "kind = \"library\"\ncontent = [\"drumgizmo\"]",
  );
  std::fs::write(&path, text).unwrap();
  let runtime = fixture.make_artifact("Runtime");
  fixture.add_plugin("runtime", "1.0.0", &runtime, &[]);
  let drumcraker = fixture.make_artifact("Drumcraker");
  fixture.add_plugin("drumcraker", "1.3.4", &drumcraker, &["runtime"]);
  let stray = fixture.make_artifact("Stray");
  fixture.add_plugin("stray", "1.0.0", &stray, &[]);
  std::fs::write(
    fixture.registry().join("engines.toml"),
    "schema = 1\n\n[[engine]]\npackage = \"drumcraker\"\nplays = [\"drumgizmo\"]\n",
  )
  .unwrap();
  std::fs::write(
    fixture.registry().join("packs/bundle.toml"),
    concat!(
      "schema = 1\n",
      "id = \"bundle\"\n",
      "name = \"Bundle\"\n",
      "kind = \"pack\"\n",
      "category = \"pack\"\n",
      "license = { kind = \"open-source\", spdx = \"CC0-1.0\" }\n",
      "description = \"A player and something else.\"\n",
      "\n[[releases]]\n",
      "version = \"1.0.0\"\n",
      "dependencies = [\"drumcraker\", \"stray\"]\n",
    ),
  )
  .unwrap();

  fixture
    .luthier()
    .args(["install", "kit", "bundle"])
    .assert()
    .success();
  fixture
    .luthier()
    .args(["remove", "bundle"])
    .assert()
    .success();
  // What the kit holds was recorded when it was installed, so the player
  // stays even once no source carries the kit, or its source cannot be read.
  std::fs::remove_file(&path).unwrap();

  let json = stdout_of(
    &fixture
      .luthier()
      .args(["autoremove", "--dry-run", "--json"])
      .output()
      .unwrap(),
  );
  let planned: serde_json::Value = serde_json::from_str(&json).unwrap();
  assert_eq!(planned["kept"][0]["id"], "drumcraker", "{json}");
  assert_eq!(planned["packages"][0]["id"], "stray", "{json}");

  let output = fixture.luthier().arg("autoremove").output().unwrap();
  let stdout = stdout_of(&output);
  assert!(output.status.success(), "{stdout}");
  assert!(
    stdout.contains("Keeping drumcraker: it plays the DrumGizmo content in kit"),
    "{stdout}"
  );
  assert!(!fixture.clap_dir().join("Stray.clap").exists(), "{stdout}");
  assert!(fixture.clap_dir().join("Drumcraker.clap").is_file());
  assert!(
    fixture.clap_dir().join("Runtime.clap").is_file(),
    "what the kept engine needs stays with it"
  );
}

#[test]
fn the_sources_are_built_in_and_cannot_be_added_to() {
  // Luthier reads extras, its own list, then the Open Audio Stack registry,
  // in that precedence, and nothing else.
  let fixture = Fixture::new();

  let listed = stdout_of(&fixture.luthier().args(["sources"]).output().unwrap());
  let extras = listed.find("extras").expect(&listed);
  let oas = listed.find("oas").expect(&listed);
  assert!(extras < oas, "{listed}");
  assert!(listed.contains("built-in"), "{listed}");

  // `sources` takes nothing to add one with, or to take one away.
  for command in [
    vec!["sources", "add", "second", "/tmp"],
    vec!["sources", "remove", "oas"],
  ] {
    let output = fixture.luthier().args(&command).output().unwrap();
    assert!(!output.status.success(), "{command:?}");
  }
}

#[test]
fn the_extras_are_there_before_anything_is_fetched() {
  // Built into the binary: found offline, on a machine that has fetched
  // nothing, and nothing is written for them.
  let root = tempfile::tempdir().unwrap();
  let output = Command::cargo_bin("luthier")
    .unwrap()
    .arg("--root")
    .arg(root.path())
    .args([
      "--offline",
      "--no-system-plugins",
      "--json",
      "info",
      "sfizz",
    ])
    .output()
    .unwrap();
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    output.status.success(),
    "{stdout}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let info: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
  assert_eq!(info["registry"], "extras", "{stdout}");
  // Nothing fetched, nothing recorded: no source has been written at all.
  assert!(
    !root.path().join("share/luthier/registries").exists(),
    "{:?}",
    std::fs::read_dir(root.path().join("share/luthier/registries"))
      .map(|d| d.map(|e| e.unwrap().file_name()).collect::<Vec<_>>())
  );
}

#[test]
fn info_says_whether_the_install_rules_were_reviewed() {
  // Over four hundred packages arrive with rules read out of the archive
  // and no human in the loop. That trade is fine; hiding it is not.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Reviewed");
  fixture.add_plugin("reviewed", "1.0.0", &artifact, &[]);
  fixture.add_plugin_derived("derived", "1.0.0", &artifact);

  let reviewed = fixture
    .luthier()
    .args(["info", "reviewed"])
    .output()
    .unwrap();
  assert!(
    stdout_of(&reviewed).contains("written and reviewed"),
    "{}",
    stdout_of(&reviewed)
  );

  let derived = fixture
    .luthier()
    .args(["info", "derived"])
    .output()
    .unwrap();
  assert!(
    stdout_of(&derived).contains("derived from the archive, not reviewed"),
    "{}",
    stdout_of(&derived)
  );
}

#[test]
fn completions_and_the_man_page_need_nothing_to_be_configured() {
  // Both run before a session exists, so they work on a machine with no
  // registry fetched and no state file — which is when a user installs them.
  let dir = tempfile::tempdir().unwrap();

  for (shell, marker) in [
    ("zsh", "#compdef luthier"),
    ("bash", "_luthier"),
    ("fish", "complete"),
  ] {
    let output = Command::cargo_bin("luthier")
      .unwrap()
      .args(["completions", shell])
      .current_dir(dir.path())
      .output()
      .unwrap();
    assert!(output.status.success(), "{shell}");
    let text = stdout_of(&output);
    assert!(text.contains(marker), "{shell}: {text}");
    // Every subcommand a user can type is in there.
    assert!(text.contains("install"), "{shell}");
    assert!(text.contains("sources"), "{shell}");
    assert!(text.contains("cache"), "{shell}");
  }

  let man = Command::cargo_bin("luthier")
    .unwrap()
    .arg("man")
    .current_dir(dir.path())
    .output()
    .unwrap();
  assert!(man.status.success());
  let roff = stdout_of(&man);
  assert!(roff.contains(".TH luthier 1"), "{roff}");
  assert!(roff.contains("SYNOPSIS"), "{roff}");
}

#[test]
fn a_refused_removal_deletes_nothing_at_all() {
  // The state file is the authority on what is installed (§20), so a refusal
  // partway down a list must not leave an earlier package deleted from disk
  // and still recorded. That state disagrees with itself: `verify` reports
  // every file missing, and `install` reads the package as satisfied and
  // declines to put it back.
  let fixture = Fixture::new();
  let alpha = fixture.make_artifact("Alpha");
  let engine = fixture.make_artifact("Engine");
  let library = fixture.make_artifact("Library");
  fixture.add_plugin("alpha", "1.0.0", &alpha, &[]);
  fixture.add_plugin("engine", "1.0.0", &engine, &[]);
  fixture.add_plugin("library", "2.0.0", &library, &["engine"]);
  fixture
    .luthier()
    .args(["install", "alpha", "library"])
    .assert()
    .success();

  // `alpha` is removable and `engine` is not, in that order: the refusal
  // arrives after the first package would have been deleted.
  let output = fixture
    .luthier()
    .args(["remove", "alpha", "engine"])
    .output()
    .unwrap();
  assert_eq!(
    output.status.code(),
    Some(6),
    "a still-required dependency is a dependency failure"
  );

  assert!(
    fixture.clap_dir().join("Alpha.clap").is_file(),
    "the package the refusal did not name must still be on disk"
  );
  assert!(fixture.vst3_dir().join("Alpha.vst3").is_dir());

  // And what is on disk is what the state file claims.
  let listed = stdout_of(&fixture.luthier().arg("list").output().unwrap());
  assert!(listed.contains("Alpha"), "{listed}");
  fixture.luthier().arg("verify").assert().success();
}

#[test]
fn naming_a_package_twice_removes_it_once() {
  // The second pass finds it already gone from state. Handled as one removal
  // rather than a failure after the files are deleted.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.0.0", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  fixture
    .luthier()
    .args(["remove", "testsynth", "testsynth"])
    .assert()
    .success();

  assert!(!fixture.clap_dir().join("Testsynth.clap").exists());
  let listed = stdout_of(&fixture.luthier().arg("list").output().unwrap());
  assert!(!listed.contains("testsynth"), "{listed}");
}

#[test]
fn an_install_under_json_emits_one_document_not_two() {
  // §33: the plan and the outcome are both complete documents, and printing
  // both leaves stdout holding a stream that no JSON parser accepts.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);

  let output = fixture
    .luthier()
    .args(["install", "testsynth", "--json"])
    .output()
    .unwrap();
  assert!(output.status.success());
  let text = stdout_of(&output);
  serde_json::from_str::<serde_json::Value>(&text)
    .unwrap_or_else(|e| panic!("install --json did not emit one document: {e}\n{text}"));

  // The same again, now that there is nothing to do: the plan becomes the
  // answer, and is still one document.
  let output = fixture
    .luthier()
    .args(["install", "testsynth", "--json"])
    .output()
    .unwrap();
  let text = stdout_of(&output);
  serde_json::from_str::<serde_json::Value>(&text)
    .unwrap_or_else(|e| panic!("a plan with nothing to do is not one document: {e}\n{text}"));
}

#[test]
fn a_pin_names_a_version_that_exists() {
  // A pin is read back by every later resolution, so an invented version is
  // not a harmless note: `outdated` would report a package held above the
  // newest release there is.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &artifact, &[]);
  fixture
    .luthier()
    .args(["install", "testsynth"])
    .assert()
    .success();

  let output = fixture
    .luthier()
    .args(["pin", "testsynth", "99.0.0"])
    .output()
    .unwrap();
  assert_eq!(
    output.status.code(),
    Some(2),
    "an invented version is an argument error"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("1.2.3"),
    "the error says what there is: {stderr}"
  );

  // The installed version is always pinnable, named or not.
  fixture
    .luthier()
    .args(["pin", "testsynth", "1.2.3"])
    .assert()
    .success();
  fixture
    .luthier()
    .args(["pin", "testsynth"])
    .assert()
    .success();
}

#[test]
fn removal_keeps_what_the_user_changed_in_a_library_and_nothing_else() {
  // Keeping the whole directory over one edit left gigabytes of samples on
  // disk that no command could reach again: the package was gone from state.
  let fixture = Fixture::new();
  let artifact = fixture.make_library_artifact("VSCO-2-CE");
  fixture.add_library("vsco2", "1.1.0", "VSCO-2-CE", &artifact);
  fixture
    .luthier()
    .args(["install", "vsco2"])
    .assert()
    .success();

  let installed = fixture.root().join("share/luthier/libraries/VSCO-2-CE");
  std::fs::write(installed.join("Strings/violin.sfz"), b"<region> edited").unwrap();
  std::fs::write(installed.join("Strings/mine.sfz"), b"<region> mine").unwrap();

  let output = fixture
    .luthier()
    .args(["remove", "vsco2"])
    .output()
    .unwrap();
  assert!(output.status.success());
  let stdout = stdout_of(&output);
  assert!(stdout.contains("Kept"), "{stdout}");

  assert!(!installed.join("Strings/violin.wav").exists());
  assert_eq!(
    std::fs::read(installed.join("Strings/violin.sfz")).unwrap(),
    b"<region> edited"
  );
  assert_eq!(
    std::fs::read(installed.join("Strings/mine.sfz")).unwrap(),
    b"<region> mine"
  );
}

#[test]
fn an_upgrade_does_not_delete_what_the_user_changed() {
  // Replacing a package moves the old files aside and deletes them, so an
  // upgrade used to throw away exactly what `remove` keeps.
  let fixture = Fixture::new();
  let artifact = fixture.make_library_artifact("VSCO-2-CE");
  fixture.add_library("vsco2", "1.1.0", "VSCO-2-CE", &artifact);
  fixture
    .luthier()
    .args(["install", "vsco2"])
    .assert()
    .success();
  let mine = fixture
    .root()
    .join("share/luthier/libraries/VSCO-2-CE/Strings/mine.sfz");
  std::fs::write(&mine, b"<region> mine").unwrap();

  fixture.add_library("vsco2", "1.2.0", "VSCO-2-CE", &artifact);
  let output = fixture
    .luthier()
    .args(["upgrade", "vsco2"])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(5), "{}", stdout_of(&output));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("mine.sfz"), "{stderr}");
  assert!(stderr.contains("--force"), "{stderr}");
  assert!(mine.is_file());
  assert!(stdout_of(&fixture.luthier().args(["list"]).output().unwrap()).contains("1.1.0"));

  // Saying so is enough.
  fixture
    .luthier()
    .args(["install", "--force", "vsco2"])
    .assert()
    .success();
  assert!(!mine.exists());
  assert!(stdout_of(&fixture.luthier().args(["list"]).output().unwrap()).contains("1.2.0"));
}

impl Fixture {
  /// A file published as itself, under `name`, with no container around it.
  fn make_bare(&self, name: &str, bytes: &[u8]) -> Artifact {
    let dir = self.dir.path().join("artifacts/bare");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    Artifact {
      url: url::Url::from_file_path(&path).unwrap().to_string(),
      sha256: luthier_core::fsutil::hash_file(&path).unwrap().to_string(),
      size: bytes.len() as u64,
      path,
    }
  }

  /// A manifest for a bare artifact. `rules` is the TOML after `provides`:
  /// an `install` line, or `derive_install = true` for one from a source
  /// that publishes none.
  fn add_bare(
    &self,
    id: &str,
    kind: &str,
    extra: &str,
    artifact: &Artifact,
    provides: &str,
    rules: &str,
  ) {
    let name = capitalise(id);
    let manifest = format!(
      "schema = 1\nid = \"{id}\"\nname = \"{name}\"\nkind = \"{kind}\"\n\
             category = \"instrument\"\ntags = [\"test\"]\n\
             description = \"Test package {id}.\"\n{extra}\
             license = {{ kind = \"open-source\", spdx = \"MIT\" }}\n\
             \n[[releases]]\nversion = \"1.0.0\"\n\
             \n[[releases.artifacts]]\n\
             target = {{ os = \"linux\", arch = \"{ARCH}\" }}\n\
             source = {{ type = \"file\", url = \"{}\" }}\n\
             archive = \"none\"\n\
             size = {}\n\
             checksum = {{ sha256 = \"{}\" }}\n\
             provides = [{provides}]\n{rules}\n",
      artifact.url, artifact.size, artifact.sha256,
    );
    std::fs::write(self.registry().join(format!("plugins/{id}.toml")), manifest).unwrap();
  }
}

#[test]
fn a_bare_clap_with_a_written_rule_installs_and_removes() {
  // `archive = "none"` was in the schema and the validator from the start,
  // and the installer refused every one of them.
  let fixture = Fixture::new();
  let artifact = fixture.make_bare("Kick.clap", &elf_shared_object());
  fixture.add_bare(
    "kick",
    "plugin",
    "",
    &artifact,
    "\"clap\"",
    r#"install = [{ format = "clap", source = "Kick.clap", kind = "file" }]"#,
  );

  fixture
    .luthier()
    .args(["install", "kick"])
    .assert()
    .success();
  assert_eq!(
    std::fs::read(fixture.clap_dir().join("Kick.clap")).unwrap(),
    elf_shared_object()
  );
  fixture.luthier().args(["verify"]).assert().success();

  fixture
    .luthier()
    .args(["remove", "kick"])
    .assert()
    .success();
  assert!(!fixture.clap_dir().join("Kick.clap").exists());
}

#[test]
fn a_bare_clap_from_a_source_without_rules_installs_under_its_published_name() {
  // How LibreKick, FreqChain and Delax arrive from the Open Audio Stack
  // registry: the name in the URL is the only thing saying what it is.
  let fixture = Fixture::new();
  let artifact = fixture.make_bare("LibreKick_linux_x86_64.clap", &elf_shared_object());
  fixture.add_bare(
    "librekick",
    "plugin",
    "",
    &artifact,
    "\"clap\"",
    "derive_install = true",
  );

  fixture
    .luthier()
    .args(["install", "librekick"])
    .assert()
    .success();
  assert!(
    fixture
      .clap_dir()
      .join("LibreKick_linux_x86_64.clap")
      .is_file()
  );
}

#[test]
fn a_bare_clap_that_is_not_a_plugin_is_refused() {
  // Placed as a file is not the same as trusted as a plugin: the CLAP
  // installer still checks it is a shared object.
  let fixture = Fixture::new();
  let artifact = fixture.make_bare("Fake.clap", b"#!/bin/sh\necho hi\n");
  fixture.add_bare(
    "fake",
    "plugin",
    "",
    &artifact,
    "\"clap\"",
    "derive_install = true",
  );

  let output = fixture
    .luthier()
    .args(["install", "fake"])
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(!fixture.clap_dir().join("Fake.clap").exists());
}

#[test]
fn a_bare_soundfont_installs_as_a_library_under_its_package_id() {
  // `modernkit`: a SoundFont carries its samples inside it, so the one
  // file is the whole library.
  let fixture = Fixture::new();
  let artifact = fixture.make_bare("Modern.Kit.sf2", b"RIFF....sfbk");
  fixture.add_bare(
    "modernkit",
    "library",
    "content = [\"sf2\"]\n",
    &artifact,
    "\"library\"",
    "derive_install = true",
  );

  fixture
    .luthier()
    .args(["install", "modernkit"])
    .assert()
    .success();
  let installed = fixture
    .root()
    .join("share/luthier/libraries/modernkit/Modern.Kit.sf2");
  assert_eq!(std::fs::read(&installed).unwrap(), b"RIFF....sfbk");

  fixture
    .luthier()
    .args(["remove", "modernkit"])
    .assert()
    .success();
  assert!(!installed.exists());
}

#[test]
fn chosen_locations_receive_downloads_libraries_and_plugins() {
  // What moving to another disk means: nothing of the three lands in the
  // default place once a location is chosen for it.
  let fixture = Fixture::new();
  let library = fixture.make_library_artifact("VSCO-2-CE");
  fixture.add_library("vsco2", "1.1.0", "VSCO-2-CE", &library);
  let plugin = fixture.make_artifact("Testsynth");
  fixture.add_plugin("testsynth", "1.2.3", &plugin, &[]);

  let disk = fixture.dir.path().join("external");
  for part in ["cache", "libraries", "plugins"] {
    std::fs::create_dir_all(disk.join(part)).unwrap();
    fixture
      .luthier()
      .args(["location", "set", part])
      .arg(disk.join(part))
      .assert()
      .success();
  }

  fixture
    .luthier()
    .args(["install", "vsco2", "testsynth"])
    .assert()
    .success();

  assert!(
    disk
      .join("libraries/VSCO-2-CE/Strings/violin.sfz")
      .is_file()
  );
  assert!(disk.join("plugins/clap/Testsynth.clap").is_file());
  assert!(disk.join("plugins/vst3/Testsynth.vst3").is_dir());
  assert!(disk.join("cache/artifacts").is_dir());
  assert!(
    !fixture
      .root()
      .join("share/luthier/libraries/VSCO-2-CE")
      .exists()
  );
  assert!(!fixture.clap_dir().join("Testsynth.clap").exists());
  assert!(!fixture.root().join("cache/luthier/artifacts").exists());

  // Hosts are told where to look, and LV2 keeps its usual locations
  // because its variable replaces them.
  let exports = fixture
    .luthier()
    .args(["location", "search-path"])
    .output()
    .unwrap();
  let text = stdout_of(&exports);
  assert!(
    text.contains(&format!(
      "CLAP_PATH=\"{}",
      disk.join("plugins/clap").display()
    )),
    "{text}"
  );
  assert!(text.contains(".lv2:/usr/lib/lv2"), "{text}");

  fixture
    .luthier()
    .args(["remove", "vsco2", "testsynth"])
    .assert()
    .success();
  assert!(!disk.join("libraries/VSCO-2-CE").exists());
  assert!(!disk.join("plugins/clap/Testsynth.clap").exists());
}

#[test]
fn a_missing_disk_is_refused_rather_than_written_underneath() {
  // An unmounted disk leaves a path that does not exist, or an empty mount
  // point. Creating it would put the samples on the disk the user was
  // trying to spare, and removing from it would drop a package from state
  // with its files still on the disk.
  let fixture = Fixture::new();
  let library = fixture.make_library_artifact("VSCO-2-CE");
  fixture.add_library("vsco2", "1.1.0", "VSCO-2-CE", &library);

  let disk = fixture.dir.path().join("external");
  std::fs::create_dir_all(&disk).unwrap();
  fixture
    .luthier()
    .args(["location", "set", "libraries"])
    .arg(&disk)
    .assert()
    .success();
  fixture
    .luthier()
    .args(["install", "vsco2"])
    .assert()
    .success();

  let unmounted = fixture.dir.path().join("unplugged");
  std::fs::rename(&disk, &unmounted).unwrap();

  for command in [&["install", "vsco2"][..], &["remove", "vsco2"], &["verify"]] {
    let output = fixture.luthier().args(command).output().unwrap();
    assert!(!output.status.success(), "{command:?} went ahead");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mount the disk"), "{command:?}: {stderr}");
  }
  assert!(!disk.exists(), "the missing location was created");
  // Still recorded, so plugging the disk back in is all it takes.
  let list = fixture.luthier().arg("list").output().unwrap();
  assert!(stdout_of(&list).contains("Vsco2"), "{}", stdout_of(&list));

  // A missing disk says so where the locations are listed.
  let show = fixture.luthier().arg("location").output().unwrap();
  assert!(stdout_of(&show).contains("MISSING"), "{}", stdout_of(&show));

  std::fs::rename(&unmounted, &disk).unwrap();
  fixture
    .luthier()
    .args(["remove", "vsco2"])
    .assert()
    .success();
}

#[test]
fn a_location_holding_installed_packages_cannot_be_moved_away_from() {
  // State records absolute paths and removal deletes only under the current
  // roots, so a package left in the old place could never be removed.
  let fixture = Fixture::new();
  let library = fixture.make_library_artifact("VSCO-2-CE");
  fixture.add_library("vsco2", "1.1.0", "VSCO-2-CE", &library);
  fixture
    .luthier()
    .args(["install", "vsco2"])
    .assert()
    .success();

  let disk = fixture.dir.path().join("external");
  std::fs::create_dir_all(&disk).unwrap();
  let output = fixture
    .luthier()
    .args(["location", "set", "libraries"])
    .arg(&disk)
    .output()
    .unwrap();
  assert!(!output.status.success());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("luthier remove vsco2"), "{stderr}");

  // The cache holds nothing a package records a path into, so it moves.
  std::fs::create_dir_all(disk.join("cache")).unwrap();
  fixture
    .luthier()
    .args(["location", "set", "cache"])
    .arg(disk.join("cache"))
    .assert()
    .success();
}

#[test]
fn a_location_must_be_an_existing_directory_of_its_own() {
  let fixture = Fixture::new();
  let disk = fixture.dir.path().join("external");

  // Not there: most likely a disk that is not mounted.
  fixture
    .luthier()
    .args(["location", "set", "libraries"])
    .arg(&disk)
    .assert()
    .code(2);
  // Relative: there is no meaningful base for it.
  fixture
    .luthier()
    .args(["location", "set", "libraries", "samples"])
    .assert()
    .code(2);

  // Around another location: a package ID could then name the cache.
  std::fs::create_dir_all(disk.join("cache")).unwrap();
  fixture
    .luthier()
    .args(["location", "set", "cache"])
    .arg(disk.join("cache"))
    .assert()
    .success();
  fixture
    .luthier()
    .args(["location", "set", "libraries"])
    .arg(&disk)
    .assert()
    .code(2);

  // And a reset puts it back.
  fixture
    .luthier()
    .args(["location", "reset", "cache"])
    .assert()
    .success();
  let show = fixture.luthier().arg("location").output().unwrap();
  assert!(!stdout_of(&show).contains("chosen"), "{}", stdout_of(&show));
}

#[cfg(unix)]
#[test]
fn a_closed_pipe_ends_the_process_quietly() {
  // `luthier completions zsh | head` used to panic: Rust ignores SIGPIPE, so
  // the write failed with EPIPE and the completion generator `expect`s every
  // write. The reading end is closed before the process starts, so the very
  // first write meets a closed pipe and the outcome does not depend on timing.
  use std::os::unix::process::ExitStatusExt;

  let (reader, writer) = std::io::pipe().unwrap();
  drop(reader);
  let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("luthier"))
    .args(["completions", "zsh"])
    .stdout(writer)
    .stderr(std::process::Stdio::piped())
    .output()
    .unwrap();

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(!stderr.contains("panicked"), "{stderr}");
  assert_eq!(
    output.status.signal(),
    Some(13),
    "{:?}: {stderr}",
    output.status
  );
}

// ------------------------------------------------------------ self-update --

/// A Luthier release served over `file://`: GitHub's description of it, and
/// the tarball the install script installs. Its `luthier` is a shell script
/// that reports `version`, which is all `upgrade --self` asks of it.
struct PublishedRelease {
  dir: tempfile::TempDir,
}

/// What the description says about the tarball's SHA-256.
enum Digest {
  Published,
  Wrong,
  Absent,
}

impl PublishedRelease {
  fn new(version: &str, digest: Digest) -> Self {
    let dir = tempfile::tempdir().expect("tempdir");
    let top = format!("luthier-{version}-{ARCH}-unknown-linux-musl");
    let mut builder = tar::Builder::new(Vec::new());
    let script = format!("#!/bin/sh\necho 'luthier {version}'\n");
    append(
      &mut builder,
      &format!("{top}/luthier"),
      script.as_bytes(),
      0o755,
    );
    append(
      &mut builder,
      &format!("{top}/luthier.1"),
      b"new man page",
      0o644,
    );
    for shell in ["bash", "zsh", "fish"] {
      append(
        &mut builder,
        &format!("{top}/completions/luthier.{shell}"),
        b"new completions",
        0o644,
      );
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&builder.into_inner().unwrap()).unwrap();
    let bytes = encoder.finish().unwrap();

    let name = format!("luthier-{ARCH}-linux.tar.gz");
    let tarball = dir.path().join(&name);
    std::fs::write(&tarball, &bytes).unwrap();
    let digest = match digest {
      Digest::Published => format!(
        r#", "digest": "sha256:{}""#,
        luthier_core::fsutil::hash_file(&tarball).unwrap()
      ),
      Digest::Wrong => format!(r#", "digest": "sha256:{}""#, "0".repeat(64)),
      Digest::Absent => String::new(),
    };
    let description = format!(
      r#"{{ "tag_name": "v{version}", "assets": [ {{ "name": "{name}",
         "browser_download_url": "{}", "size": {}{digest} }} ] }}"#,
      url::Url::from_file_path(&tarball).unwrap(),
      bytes.len()
    );
    std::fs::write(dir.path().join("release.json"), description).unwrap();
    Self { dir }
  }

  fn api(&self) -> String {
    url::Url::from_file_path(self.dir.path().join("release.json"))
      .unwrap()
      .to_string()
  }
}

/// This build of `luthier`, copied to `<prefix>/bin` with an old man page
/// beside it, as the install script would leave it. The copy is what updates
/// itself, so the binary the suite runs everywhere else is never touched.
fn installed_copy(prefix: &Path) -> PathBuf {
  let binary = prefix.join("bin/luthier");
  std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
  std::fs::copy(assert_cmd::cargo::cargo_bin("luthier"), &binary).unwrap();
  let man = prefix.join("share/man/man1/luthier.1");
  std::fs::create_dir_all(man.parent().unwrap()).unwrap();
  std::fs::write(&man, b"old man page").unwrap();
  binary
}

/// Runs a binary this suite has just written. Another test forking while it
/// was open for writing makes it busy for a moment, which exec reports as
/// ETXTBSY, so that is waited out.
fn run_written(command: &mut std::process::Command) -> std::process::Output {
  for _ in 0..50 {
    match command.output() {
      Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
        std::thread::sleep(std::time::Duration::from_millis(20));
      }
      output => return output.unwrap(),
    }
  }
  panic!("{command:?} stayed busy");
}

/// `binary upgrade --self` under `--root prefix`, which the binary is inside,
/// as it has to be.
fn upgrade_self(binary: &Path, prefix: &Path, api: &str) -> std::process::Output {
  run_written(
    std::process::Command::new(binary)
      .arg("--root")
      .arg(prefix)
      .args(["--releases-api", api, "--yes", "upgrade", "--self"]),
  )
}

fn version_of(binary: &Path) -> String {
  let output = run_written(std::process::Command::new(binary).arg("--version"));
  String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn upgrade_self_replaces_a_standalone_binary_and_the_files_beside_it() {
  let release = PublishedRelease::new("99.0.0", Digest::Published);
  let prefix = tempfile::tempdir().unwrap();
  let binary = installed_copy(prefix.path());

  let output = upgrade_self(&binary, prefix.path(), &release.api());
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    output.status.success(),
    "{stdout}{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(stdout.contains("-> 99.0.0"), "{stdout}");
  assert!(stdout.contains("Luthier is now 99.0.0"), "{stdout}");

  assert_eq!(version_of(&binary), "luthier 99.0.0");
  let man = prefix.path().join("share/man/man1/luthier.1");
  assert_eq!(std::fs::read(man).unwrap(), b"new man page");
  // A completion the install script did not put there is not created.
  assert!(
    !prefix
      .path()
      .join("share/bash-completion/completions/luthier")
      .exists()
  );
  // Nor is the download kept once it is in place.
  assert!(!prefix.path().join("cache/luthier/self-update").exists());
}

#[test]
fn upgrade_self_on_the_latest_release_changes_nothing() {
  let release = PublishedRelease::new(env!("CARGO_PKG_VERSION"), Digest::Published);
  let prefix = tempfile::tempdir().unwrap();
  let binary = installed_copy(prefix.path());
  let before = std::fs::read(&binary).unwrap();

  let output = upgrade_self(&binary, prefix.path(), &release.api());
  assert!(output.status.success());
  assert!(String::from_utf8_lossy(&output.stdout).contains("is the latest release"));
  assert_eq!(std::fs::read(&binary).unwrap(), before);
}

#[test]
fn upgrade_self_refuses_a_download_that_is_not_the_published_one() {
  let release = PublishedRelease::new("99.0.0", Digest::Wrong);
  let prefix = tempfile::tempdir().unwrap();
  let binary = installed_copy(prefix.path());
  let before = std::fs::read(&binary).unwrap();

  let output = upgrade_self(&binary, prefix.path(), &release.api());
  assert_eq!(
    output.status.code(),
    Some(4),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(std::fs::read(&binary).unwrap(), before);
}

#[test]
fn upgrade_self_refuses_a_download_with_no_published_digest() {
  let release = PublishedRelease::new("99.0.0", Digest::Absent);
  let prefix = tempfile::tempdir().unwrap();
  let binary = installed_copy(prefix.path());
  let before = std::fs::read(&binary).unwrap();

  let output = upgrade_self(&binary, prefix.path(), &release.api());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(output.status.code(), Some(4), "{stderr}");
  assert!(stderr.contains("publishes no SHA-256"), "{stderr}");
  assert_eq!(std::fs::read(&binary).unwrap(), before);
}

#[test]
fn upgrade_self_under_root_leaves_a_binary_outside_it_alone() {
  let release = PublishedRelease::new("99.0.0", Digest::Published);
  let prefix = tempfile::tempdir().unwrap();
  let binary = installed_copy(prefix.path());
  let before = std::fs::read(&binary).unwrap();
  let elsewhere = tempfile::tempdir().unwrap();

  let output = upgrade_self(&binary, elsewhere.path(), &release.api());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(output.status.code(), Some(2), "{stderr}");
  assert!(stderr.contains("outside --root"), "{stderr}");
  assert_eq!(std::fs::read(&binary).unwrap(), before);
}

#[test]
fn upgrade_self_leaves_nothing_behind_when_the_new_binary_does_not_run() {
  // A release whose binary reports another version than its tag.
  let release = PublishedRelease::new("99.0.0", Digest::Published);
  let description = release.dir.path().join("release.json");
  let text = std::fs::read_to_string(&description).unwrap();
  std::fs::write(&description, text.replace("v99.0.0", "v98.0.0")).unwrap();
  let prefix = tempfile::tempdir().unwrap();
  let binary = installed_copy(prefix.path());
  let before = std::fs::read(&binary).unwrap();

  let output = upgrade_self(&binary, prefix.path(), &release.api());
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(output.status.code(), Some(5), "{stderr}");
  assert!(stderr.contains("does not run here"), "{stderr}");
  assert_eq!(std::fs::read(&binary).unwrap(), before);
  let bin: Vec<_> = std::fs::read_dir(prefix.path().join("bin"))
    .unwrap()
    .map(|e| e.unwrap().file_name())
    .collect();
  assert_eq!(bin, ["luthier"], "a staged copy was left behind");
  assert!(
    !prefix.path().join("cache/luthier/self-update").exists(),
    "the download was left behind"
  );
}

/// `binary <command>` against the fixture, with the binary inside its root as
/// the newer-Luthier line requires under `--root`, and the core's debug log
/// on so a test can see whether the check was made.
fn run_in_root(
  fixture: &Fixture,
  binary: &Path,
  api: &str,
  extra: &[&str],
  command: &str,
) -> std::process::Output {
  run_written(
    std::process::Command::new(binary)
      .env("LUTHIER_LOG", "luthier_core=debug")
      .arg("--root")
      .arg(fixture.root())
      .arg("--registry-path")
      .arg(fixture.registry())
      .args(["--no-system-plugins", "--releases-api", api])
      .args(extra)
      .arg(command),
  )
}

#[test]
fn update_and_outdated_tell_of_a_newer_luthier_but_not_under_json() {
  let fixture = Fixture::new();
  let binary = installed_copy(&fixture.root());
  let release = PublishedRelease::new("99.0.0", Digest::Published);

  for command in ["update", "outdated"] {
    let text = run_in_root(&fixture, &binary, &release.api(), &[], command);
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(text.status.success(), "{command}: {stdout}");
    assert!(
      stdout.contains("Luthier 99.0.0 is out"),
      "{command}: {stdout}"
    );
    assert!(
      stdout.contains("luthier upgrade --self"),
      "{command}: {stdout}"
    );

    // One JSON document on stdout, as everywhere else.
    let json = run_in_root(&fixture, &binary, &release.api(), &["--json"], command);
    let stdout = String::from_utf8_lossy(&json.stdout);
    assert!(!stdout.contains("is out"), "{command}: {stdout}");
    serde_json::from_str::<serde_json::Value>(&stdout).expect("one JSON document");
  }
}

#[test]
fn the_newer_luthier_check_is_not_made_when_its_line_would_be_dropped() {
  let fixture = Fixture::new();
  let binary = installed_copy(&fixture.root());
  // A scheme nothing fetches: a check made fails, and the debug log says so.
  let asked = |extra: &[&str]| {
    let output = run_in_root(
      &fixture,
      &binary,
      "ftp://example.invalid/r.json",
      extra,
      "outdated",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    stderr.contains("could not check for a newer Luthier")
  };
  assert!(asked(&[]), "the check is not made at all");
  assert!(!asked(&["--json"]));
  assert!(!asked(&["--quiet"]));
  assert!(!asked(&["--offline"]));

  // Nor for a binary outside --root, which `upgrade --self` would refuse.
  let outside = run_in_root(
    &fixture,
    &assert_cmd::cargo::cargo_bin("luthier"),
    "ftp://example.invalid/r.json",
    &[],
    "outdated",
  );
  assert!(!String::from_utf8_lossy(&outside.stderr).contains("could not check"));
}

#[test]
fn a_release_check_that_gets_no_answer_does_not_hold_up_outdated() {
  let fixture = Fixture::new();
  let binary = installed_copy(&fixture.root());
  // Takes connections, into the kernel's backlog, and never answers.
  let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let api = format!("http://{}/release.json", silent.local_addr().unwrap());

  let started = std::time::Instant::now();
  let output = run_in_root(&fixture, &binary, &api, &[], "outdated");
  let elapsed = started.elapsed();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  // The transfer's own timeout is a minute; the notice gives up long before.
  assert!(
    elapsed < std::time::Duration::from_secs(20),
    "took {elapsed:?}"
  );
  assert!(!String::from_utf8_lossy(&output.stdout).contains("is out"));
}
