//! The full package lifecycle, end to end (§61).
//!
//! Every test builds its own registry and its own artifacts, served over
//! `file://`, so the suite never depends on a live GitHub release (§55) and can
//! run offline. `--root` confines all state and plugin directories to a
//! temporary directory, so nothing here can reach the real `~/.clap`.

use assert_cmd::Command;
use std::io::Write;
use std::path::{Path, PathBuf};

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

  fn env_dir(&self, name: &str) -> PathBuf {
    self.root().join("share/luthier/envs").join(name)
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
             target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
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
             target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
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
      &format!("{name}.vst3/Contents/x86_64-linux/{name}.so"),
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
             target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
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
             target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
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
  let machine: u16 = if std::env::consts::ARCH == "aarch64" {
    0xb7
  } else {
    0x3e
  };
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
      .join("Testsynth.vst3/Contents/x86_64-linux/Testsynth.so")
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
  let cleanup = stdout_of(&fixture.luthier().arg("cleanup").output().unwrap());
  assert!(cleanup.contains("engine"), "{cleanup}");
  assert!(
    fixture.clap_dir().join("Engine.clap").is_file(),
    "cleanup must report orphans, never delete them (§24)"
  );
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
    vec!["cleanup"],
    vec!["update"],
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

#[test]
fn environments_hold_separate_sets_of_packages() {
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Dexed");
  fixture.add_plugin("dexed", "1.0.1", &artifact, &[]);

  fixture
    .luthier()
    .args(["env", "create", "mixing"])
    .assert()
    .success();

  // Installing into the environment must not touch the default roots.
  fixture
    .luthier()
    .args(["--env", "mixing", "install", "dexed"])
    .assert()
    .success();

  assert!(fixture.env_dir("mixing").join(".clap/Dexed.clap").is_file());
  assert!(!fixture.clap_dir().join("Dexed.clap").exists());

  // And the default environment still reports nothing installed.
  let listed = fixture.luthier().arg("list").assert().success();
  assert!(stdout_of(listed.get_output()).contains("No packages installed"));

  let in_env = fixture
    .luthier()
    .args(["--env", "mixing", "list"])
    .assert()
    .success();
  assert!(stdout_of(in_env.get_output()).contains("Dexed"));
}

#[test]
fn installing_into_an_environment_that_does_not_exist_is_refused() {
  // A typo must not silently create a second environment and install there.
  let fixture = Fixture::new();
  let artifact = fixture.make_artifact("Dexed");
  fixture.add_plugin("dexed", "1.0.1", &artifact, &[]);

  let output = fixture
    .luthier()
    .args(["--env", "mixnig", "install", "dexed"])
    .assert()
    .failure();
  let text = String::from_utf8_lossy(&output.get_output().stderr).to_string();
  assert!(text.contains("no environment named mixnig"), "{text}");
}

#[test]
fn an_environment_name_cannot_point_outside_the_layout() {
  let fixture = Fixture::new();
  for hostile in ["../escape", "..", "/etc"] {
    fixture
      .luthier()
      .args(["env", "create", hostile])
      .assert()
      .failure();
  }
  assert!(!fixture.root().parent().unwrap().join("escape").exists());
}

#[test]
fn activation_exports_the_environments_search_paths() {
  let fixture = Fixture::new();
  fixture
    .luthier()
    .args(["env", "create", "mixing"])
    .assert()
    .success();

  let output = fixture
    .luthier()
    .args(["env", "activate", "mixing"])
    .assert()
    .success();
  let script = stdout_of(output.get_output());

  // Every line has to be evaluable: a stray status line would be executed.
  for line in script.lines().filter(|l| !l.trim().is_empty()) {
    assert!(line.starts_with("export "), "not evaluable: {line:?}");
  }
  assert!(script.contains("export LUTHIER_ENV=\"mixing\""), "{script}");
  assert!(script.contains("envs/mixing/.lv2"), "{script}");
  assert!(script.contains("envs/mixing/.clap"), "{script}");
  // LV2_PATH replaces the default path, so the system directories must be
  // named explicitly or they stop being visible.
  assert!(script.contains("/usr/lib/lv2"), "{script}");
}

/// §51: an exported environment reproduces an installation somewhere else,
/// which is the whole reason the file records versions rather than names.
#[test]
fn an_exported_environment_reproduces_the_same_set_elsewhere() {
  let source = Fixture::new();
  let one = source.make_artifact("One");
  let two = source.make_artifact("Two");
  // `two` arrives only as a dependency, so the file has to carry it even
  // though the import will not ask for it by name.
  source.add_plugin("two", "1.0.0", &two, &[]);
  source.add_plugin("one", "1.0.0", &one, &["two"]);
  source.luthier().args(["install", "one"]).assert().success();

  let exported = stdout_of(&source.luthier().args(["env", "export"]).output().unwrap());
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
    .args(["env", "import"])
    .arg(&file)
    .assert()
    .success();

  assert!(target.clap_dir().join("One.clap").is_file());
  assert!(target.clap_dir().join("Two.clap").is_file());

  // The dependency stays a dependency, so `cleanup` still owns it (§24).
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
      .args(["env", "export", "--loose"])
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
    .args(["env", "import"])
    .arg(&file)
    .output()
    .unwrap();
  assert!(!output.status.success());
  assert!(!fixture.clap_dir().join("One.clap").exists());
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
fn the_sources_are_built_in_and_cannot_be_added_to() {
  // Luthier reads the Open Audio Stack registry and its own bench, in that
  // precedence, and nothing else.
  let fixture = Fixture::new();

  let listed = stdout_of(&fixture.luthier().args(["bench", "list"]).output().unwrap());
  let bench = listed.find("luthier-extras").expect(&listed);
  let oas = listed.find("oas").expect(&listed);
  assert!(bench < oas, "{listed}");

  for command in [
    vec!["bench", "add", "second", "/tmp"],
    vec!["bench", "remove", "luthier-extras"],
    vec!["bench", "trust", "luthier-extras", "RWS"],
    vec!["refresh", "--allow-unsigned"],
  ] {
    let output = fixture.luthier().args(&command).output().unwrap();
    assert!(!output.status.success(), "{command:?}");
  }
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
    assert!(text.contains("bench"), "{shell}");
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
  // not a harmless note: `update` would report a package held above the
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
fn an_update_does_not_delete_what_the_user_changed() {
  // Replacing a package moves the old files aside and deletes them, so an
  // update used to throw away exactly what `remove` keeps.
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
    .args(["update", "vsco2"])
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
             target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
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
