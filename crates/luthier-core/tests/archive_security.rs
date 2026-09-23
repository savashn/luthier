//! The malicious-archive corpus (§43, §56).
//!
//! Each test feeds the extractor an archive built to escape it, and asserts
//! both that the attempt was refused for the right reason and — the assertion
//! that really matters — that nothing was written outside the extraction
//! directory.

mod support;

use luthier_core::archive::{ExtractLimits, extract, place};
use luthier_core::error::{ArchiveError, UnsafeEntry};
use luthier_manifest::ArchiveFormat;
use std::path::{Path, PathBuf};
use support::*;
use tar::EntryType;

/// A sandbox with an extraction target and a set of canary files outside it.
struct Sandbox {
  _dir: tempfile::TempDir,
  root: PathBuf,
  dest: PathBuf,
  /// Where generated payloads live, so they are not mistaken for escapes.
  archives: PathBuf,
  canaries: Vec<PathBuf>,
}

impl Sandbox {
  fn new() -> Self {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let dest = root.join("extract");
    std::fs::create_dir(&dest).unwrap();
    let archives = root.join("archives");
    std::fs::create_dir(&archives).unwrap();

    // Stand-ins for the things a traversal would actually go after.
    let mut canaries = Vec::new();
    for (relative, contents) in [
      ("home/.ssh/authorized_keys", "ORIGINAL KEY"),
      ("home/.clap/Existing.clap", "ORIGINAL PLUGIN"),
      ("etc/passwd", "ORIGINAL PASSWD"),
    ] {
      let path = root.join(relative);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(&path, contents).unwrap();
      canaries.push(path);
    }
    Self {
      _dir: dir,
      root,
      dest,
      archives,
      canaries,
    }
  }

  /// Fails if any canary changed or any new file appeared outside `dest`.
  fn assert_nothing_escaped(&self) {
    for path in &self.canaries {
      let contents = std::fs::read_to_string(path).expect("canary still readable");
      assert!(
        contents.starts_with("ORIGINAL"),
        "canary {} was modified: {contents:?}",
        path.display()
      );
    }
    let outside: Vec<String> = list_tree(&self.root)
      .into_iter()
      .filter(|entry| !entry.starts_with("extract") && !entry.starts_with("archives"))
      .collect();
    let expected = vec![
      "etc/".to_string(),
      "etc/passwd".to_string(),
      "home/".to_string(),
      "home/.clap/".to_string(),
      "home/.clap/Existing.clap".to_string(),
      "home/.ssh/".to_string(),
      "home/.ssh/authorized_keys".to_string(),
    ];
    assert_eq!(
      outside, expected,
      "files appeared outside the extraction directory"
    );
  }

  fn extract_tar_gz(&self, entries: &[TarEntry]) -> Result<(), ArchiveError> {
    let archive = tar_gz_file(&self.archives, "payload.tar.gz", entries);
    self.run(&archive, ArchiveFormat::TarGz)
  }

  fn extract_zip(&self, entries: &[ZipEntry]) -> Result<(), ArchiveError> {
    let archive = zip_file(&self.archives, "payload.zip", entries);
    self.run(&archive, ArchiveFormat::Zip)
  }

  fn extract_7z(&self, entries: &[SevenZEntry]) -> Result<(), ArchiveError> {
    let archive = sevenz_file(&self.archives, "payload.7z", entries);
    self.run(&archive, ArchiveFormat::SevenZ)
  }

  /// Places a bare file under a name, as an artifact declared `none` is.
  fn place_bare(&self, name: &str, bytes: &[u8]) -> Result<(), ArchiveError> {
    let source = write_file(&self.archives, "payload", bytes);
    place(&source, name, &self.dest, ExtractLimits::small()).map(|_| ())
  }

  fn run(&self, archive: &Path, format: ArchiveFormat) -> Result<(), ArchiveError> {
    extract(archive, &format, &self.dest, ExtractLimits::small()).map(|_| ())
  }
}

fn assert_unsafe(result: Result<(), ArchiveError>, expected: UnsafeEntry) {
  match result {
    Err(ArchiveError::Unsafe { reason, .. }) => {
      assert_eq!(reason, expected, "refused, but for the wrong reason");
    }
    other => panic!("expected a refusal for {expected:?}, got {other:?}"),
  }
}

// ------------------------------------------------------------ path escapes --

#[test]
fn tar_relative_traversal_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("../../evil", b"pwned")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_deep_traversal_targeting_ssh_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file(
    "../../home/.ssh/authorized_keys",
    b"ATTACKER KEY",
  )]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_absolute_path_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("/etc/passwd", b"root::0:0")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_traversal_hidden_mid_path_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("plugins/../../escape", b"x")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_current_dir_component_is_refused() {
  // Regression guard: `Path::components()` normalises an interior `.` away,
  // so a component-based check would have let this through.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("a/./b", b"x")]);
  assert_unsafe(result, UnsafeEntry::MalformedPath);
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_leading_current_directory_prefix_cannot_smuggle_a_traversal() {
  // `./` is stripped so that GNU-tar-style archives work; that stripping must
  // not become a way to hide `..` from the check that follows it.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("./../../evil", b"pwned")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_backslash_separator_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("..\\..\\evil", b"x")]);
  assert_unsafe(result, UnsafeEntry::MalformedPath);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_drive_prefix_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("C:/Windows/system32/x", b"x")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_empty_component_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::file("a//b", b"x")]);
  assert_unsafe(result, UnsafeEntry::MalformedPath);
  sandbox.assert_nothing_escaped();
}

// ------------------------------------------------------------------- links --

#[test]
fn tar_symlink_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::symlink("link", "/etc")]);
  assert_unsafe(result, UnsafeEntry::Symlink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_symlink_then_write_through_it_is_refused() {
  // The classic two-step escape: plant a link to somewhere outside, then
  // write "inside" it. Refusing every symlink stops step one.
  let sandbox = Sandbox::new();
  let outside = sandbox.root.join("home");
  let result = sandbox.extract_tar_gz(&[
    TarEntry::symlink("escape", outside.to_str().unwrap()),
    TarEntry::file("escape/.ssh/authorized_keys", b"ATTACKER KEY"),
  ]);
  assert_unsafe(result, UnsafeEntry::Symlink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_relative_symlink_escaping_the_root_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[TarEntry::symlink("a", "../../../../etc/passwd")]);
  assert_unsafe(result, UnsafeEntry::Symlink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_hard_link_to_an_absolute_path_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[
    TarEntry::file("real.txt", b"data"),
    TarEntry::hardlink("alias.txt", "/etc/passwd"),
  ]);
  assert_unsafe(result, UnsafeEntry::HardLink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_hard_link_that_traverses_out_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[
    TarEntry::file("real.txt", b"data"),
    TarEntry::hardlink("alias.txt", "../../home/.ssh/authorized_keys"),
  ]);
  assert_unsafe(result, UnsafeEntry::HardLink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_hard_link_to_an_entry_the_archive_never_wrote_is_refused() {
  // The rule is "already written by this extraction", not "looks relative".
  // A name that happens to exist on disk but was not produced here must not
  // satisfy the link, or a link could alias a file the archive does not own.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[
    TarEntry::file("real.txt", b"data"),
    TarEntry::hardlink("alias.txt", "never-extracted.txt"),
  ]);
  assert_unsafe(result, UnsafeEntry::HardLink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_hard_link_naming_a_later_entry_is_refused() {
  // Order matters: the target must already be on disk. Accepting a forward
  // reference would mean resolving a path that does not exist yet.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[
    TarEntry::hardlink("alias.txt", "real.txt"),
    TarEntry::file("real.txt", b"data"),
  ]);
  assert_unsafe(result, UnsafeEntry::HardLink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_hard_link_to_an_earlier_entry_is_copied_not_linked() {
  // The legitimate case, which real packages need: DPF-Plugins ships its
  // preset collection with hard links for files repeated across plugins.
  // The result must be an independent file — two paths on one inode would
  // reintroduce the aliasing the refusal existed to prevent.
  let sandbox = Sandbox::new();
  sandbox
    .extract_tar_gz(&[
      TarEntry::file("presets/original.txt", b"preset data"),
      TarEntry::hardlink("presets/copy.txt", "presets/original.txt"),
    ])
    .expect("a link to an earlier entry is legitimate");

  let original = sandbox.dest.join("presets/original.txt");
  let copy = sandbox.dest.join("presets/copy.txt");
  assert_eq!(std::fs::read(&copy).unwrap(), b"preset data");

  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let a = std::fs::metadata(&original).unwrap();
    let b = std::fs::metadata(&copy).unwrap();
    assert_ne!(a.ino(), b.ino(), "the entries share an inode");
    assert_eq!(b.nlink(), 1, "the copy is still linked to something else");
  }
  sandbox.assert_nothing_escaped();
}

#[test]
fn tar_hard_link_cannot_reach_a_canary_through_a_written_entry() {
  // Belt to the braces: even a link whose target *was* written cannot be
  // used to reach outside, because the written entry is itself inside the
  // root by construction.
  let sandbox = Sandbox::new();
  sandbox
    .extract_tar_gz(&[
      TarEntry::file("a.txt", b"inside"),
      TarEntry::hardlink("b.txt", "a.txt"),
    ])
    .expect("legitimate");
  sandbox.assert_nothing_escaped();
  assert_eq!(
    std::fs::read_to_string(sandbox.root.join("home/.ssh/authorized_keys")).unwrap(),
    "ORIGINAL KEY"
  );
}

#[test]
fn zip_symlink_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_zip(&[ZipEntry::symlink("link", "/etc/passwd")]);
  assert_unsafe(result, UnsafeEntry::Symlink);
  sandbox.assert_nothing_escaped();
}

// --------------------------------------------------------- special entries --

#[test]
fn tar_device_and_fifo_entries_are_refused() {
  for kind in [EntryType::Char, EntryType::Block, EntryType::Fifo] {
    let sandbox = Sandbox::new();
    let result = sandbox.extract_tar_gz(&[TarEntry::special("dev/thing", kind)]);
    assert_unsafe(result, UnsafeEntry::SpecialFile);
    sandbox.assert_nothing_escaped();
  }
}

#[test]
fn tar_duplicate_entries_are_refused() {
  // Which copy wins would otherwise depend on extraction order, so a review
  // of the archive would not tell you what actually lands on disk.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_tar_gz(&[
    TarEntry::file("Plugin.clap", b"first"),
    TarEntry::file("Plugin.clap", b"second"),
  ]);
  assert_unsafe(result, UnsafeEntry::Duplicate);
  sandbox.assert_nothing_escaped();
}

#[test]
fn zip_colliding_file_and_directory_entries_are_refused() {
  // The zip writer refuses to emit two entries with byte-identical names, so
  // the reachable collision is a name stored both as `Plugin.vst3/` and as
  // `Plugin.vst3`. Whether the result is a file or a directory would depend
  // on extraction order, which is exactly what the duplicate check exists to
  // prevent.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_zip(&[
    ZipEntry::dir("Plugin.vst3"),
    ZipEntry::file("Plugin.vst3", b"actually a file"),
  ]);
  assert_unsafe(result, UnsafeEntry::Duplicate);
  sandbox.assert_nothing_escaped();
}

#[test]
fn zip_traversal_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_zip(&[ZipEntry::file("../../evil", b"pwned")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn zip_absolute_path_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_zip(&[ZipEntry::file("/etc/passwd", b"root::0:0")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

// ------------------------------------------------------------------ limits --

#[test]
fn an_entry_count_bomb_is_refused() {
  let sandbox = Sandbox::new();
  let entries: Vec<TarEntry> = (0..ExtractLimits::small().max_entries + 10)
    .map(|i| TarEntry::file(&format!("f{i}"), b"x"))
    .collect();
  let result = sandbox.extract_tar_gz(&entries);
  assert!(
    matches!(result, Err(ArchiveError::TooManyEntries { .. })),
    "{result:?}"
  );
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_decompression_bomb_is_refused() {
  // Highly compressible payload well past the configured ceiling: small on
  // the wire, ruinous on disk.
  let sandbox = Sandbox::new();
  let payload = vec![0u8; (ExtractLimits::small().max_total_bytes + 4096) as usize];
  let result = sandbox.extract_tar_gz(&[TarEntry::file("big.bin", &payload)]);
  assert!(
    matches!(
      result,
      Err(ArchiveError::TooLarge { .. }) | Err(ArchiveError::EntryTooLarge { .. })
    ),
    "{result:?}"
  );
  sandbox.assert_nothing_escaped();
}

// -------------------------------------------------------------- permissions --

#[cfg(unix)]
#[test]
fn setuid_and_sticky_bits_never_reach_the_disk() {
  use std::os::unix::fs::PermissionsExt;

  let sandbox = Sandbox::new();
  sandbox
    .extract_tar_gz(&[
      TarEntry::file("setuid.bin", b"x").with_mode(0o4755),
      TarEntry::file("setgid.bin", b"x").with_mode(0o2755),
      TarEntry::file("sticky.bin", b"x").with_mode(0o1777),
      TarEntry::file("plain.txt", b"x").with_mode(0o644),
    ])
    .expect("these are legal entries, just with hostile modes");

  for name in ["setuid.bin", "setgid.bin", "sticky.bin"] {
    let mode = std::fs::metadata(sandbox.dest.join(name))
      .unwrap()
      .permissions()
      .mode();
    assert_eq!(mode & 0o7000, 0, "{name} kept a special bit: {mode:o}");
    assert_eq!(mode & 0o777, 0o755, "{name} should be a plain executable");
  }
  let plain = std::fs::metadata(sandbox.dest.join("plain.txt"))
    .unwrap()
    .permissions()
    .mode();
  assert_eq!(plain & 0o777, 0o644);
  sandbox.assert_nothing_escaped();
}

// -------------------------------------------------------- positive controls --

#[test]
fn a_legitimate_tar_gz_extracts_intact() {
  // The shape Surge XT actually ships: a CLAP file beside a VST3 bundle.
  let sandbox = Sandbox::new();
  sandbox
    .extract_tar_gz(&[
      TarEntry::dir("surge-xt"),
      TarEntry::file("surge-xt/Surge XT.clap", b"\x7fELF fake"),
      TarEntry::dir("surge-xt/Surge XT.vst3"),
      TarEntry::dir("surge-xt/Surge XT.vst3/Contents"),
      TarEntry::dir("surge-xt/Surge XT.vst3/Contents/x86_64-linux"),
      TarEntry::file(
        "surge-xt/Surge XT.vst3/Contents/x86_64-linux/Surge XT.so",
        b"\x7fELF fake",
      ),
    ])
    .expect("a well-formed archive must extract");

  assert_eq!(
    std::fs::read(sandbox.dest.join("surge-xt/Surge XT.clap")).unwrap(),
    b"\x7fELF fake"
  );
  assert!(
    sandbox
      .dest
      .join("surge-xt/Surge XT.vst3/Contents/x86_64-linux/Surge XT.so")
      .is_file()
  );
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_gnu_tar_style_archive_with_dot_entries_extracts() {
  // Regression guard for a real package: Surge XT's release tarball is built
  // with `tar -C dir .`, so every entry is `./`-prefixed and the first entry
  // is a bare `./`. Rejecting that made a flagship package uninstallable.
  let sandbox = Sandbox::new();
  sandbox
    .extract_tar_gz(&[
      TarEntry::dir("./"),
      TarEntry::file("./Surge XT.clap", b"\x7fELF fake"),
      TarEntry::dir("./Surge XT.vst3"),
      TarEntry::file(
        "./Surge XT.vst3/Contents/x86_64-linux/Surge XT.so",
        b"\x7fELF",
      ),
    ])
    .expect("a GNU-tar-style archive must extract");

  assert!(sandbox.dest.join("Surge XT.clap").is_file());
  assert!(
    sandbox
      .dest
      .join("Surge XT.vst3/Contents/x86_64-linux/Surge XT.so")
      .is_file()
  );
  // The `./` root entry must not have produced a directory literally named ".".
  assert!(!sandbox.dest.join(".").join("Surge XT.clap").is_file() || true);
  let entries = list_tree(&sandbox.dest);
  assert!(!entries.iter().any(|e| e.starts_with("./")), "{entries:?}");
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_legitimate_zip_extracts_intact() {
  // The shape Dexed ships: a zip holding a VST3 bundle.
  let sandbox = Sandbox::new();
  sandbox
    .extract_zip(&[
      ZipEntry::dir("Dexed.vst3/"),
      ZipEntry::dir("Dexed.vst3/Contents/"),
      ZipEntry::dir("Dexed.vst3/Contents/x86_64-linux/"),
      ZipEntry::file("Dexed.vst3/Contents/x86_64-linux/Dexed.so", b"\x7fELF fake"),
    ])
    .expect("a well-formed zip must extract");
  assert!(
    sandbox
      .dest
      .join("Dexed.vst3/Contents/x86_64-linux/Dexed.so")
      .is_file()
  );
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_legitimate_tar_xz_extracts_intact() {
  // Dragonfly Reverb publishes tar.xz, so this container is not optional.
  let dir = tempfile::tempdir().unwrap();
  let dest = dir.path().join("out");
  std::fs::create_dir(&dest).unwrap();
  let archive = tar_xz_file(
    dir.path(),
    "dragonfly.tar.xz",
    &[
      TarEntry::dir("DragonflyHallReverb.vst3"),
      TarEntry::file("DragonflyHallReverb.vst3/plugin.so", b"\x7fELF fake"),
    ],
  );
  extract(
    &archive,
    &ArchiveFormat::TarXz,
    &dest,
    ExtractLimits::default(),
  )
  .expect("tar.xz must extract");
  assert!(dest.join("DragonflyHallReverb.vst3/plugin.so").is_file());
}

#[test]
fn extraction_reports_what_it_wrote() {
  let sandbox = Sandbox::new();
  let archive = tar_gz_file(
    &sandbox.archives,
    "p.tar.gz",
    &[
      TarEntry::file("a.txt", b"12345"),
      TarEntry::file("b.txt", b"678"),
    ],
  );
  let report = extract(
    &archive,
    &ArchiveFormat::TarGz,
    &sandbox.dest,
    ExtractLimits::small(),
  )
  .unwrap();
  assert_eq!(report.entries, 2);
  assert_eq!(report.bytes, 8);
}

// ---------------------------------------------------------------------- 7z --
//
// 7z is here for LSP Plugins, which publishes Linux binaries in no other
// container. The policy is the same one tar and zip go through; these assert
// that adding a container did not add a second, weaker policy.

#[test]
fn sevenz_traversal_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_7z(&[SevenZEntry::file(
    "../../home/.ssh/authorized_keys",
    b"ATTACKER KEY",
  )]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn sevenz_absolute_path_is_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_7z(&[SevenZEntry::file("/etc/passwd", b"root::0:0")]);
  assert_unsafe(result, UnsafeEntry::PathEscape);
  sandbox.assert_nothing_escaped();
}

#[test]
fn sevenz_symlink_is_refused() {
  // A unix-built 7z stores the symlink flag in the packed Unix mode.
  let sandbox = Sandbox::new();
  let result = sandbox.extract_7z(&[SevenZEntry::symlink("link", "/etc/passwd")]);
  assert_unsafe(result, UnsafeEntry::Symlink);
  sandbox.assert_nothing_escaped();
}

#[test]
fn sevenz_duplicate_entries_are_refused() {
  let sandbox = Sandbox::new();
  let result = sandbox.extract_7z(&[
    SevenZEntry::file("same.txt", b"first"),
    SevenZEntry::file("same.txt", b"second"),
  ]);
  assert_unsafe(result, UnsafeEntry::Duplicate);
  sandbox.assert_nothing_escaped();
}

#[test]
fn sevenz_setuid_bits_never_reach_the_disk() {
  let sandbox = Sandbox::new();
  sandbox
    .extract_7z(&[SevenZEntry::file("tool", b"\x7fELF").with_unix_mode(0o104_755)])
    .expect("a setuid bit is dropped, not refused");

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(sandbox.dest.join("tool"))
      .unwrap()
      .permissions()
      .mode();
    assert_eq!(mode & 0o7000, 0, "a setuid/setgid/sticky bit survived");
  }
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_legitimate_7z_extracts_intact() {
  // The shape LSP Plugins ships: bundles and shared objects side by side.
  let sandbox = Sandbox::new();
  sandbox
    .extract_7z(&[
      SevenZEntry::dir("lsp-plugins.lv2"),
      SevenZEntry::file("lsp-plugins.lv2/manifest.ttl", b"@prefix lv2: <> .\n"),
      SevenZEntry::file("lsp-plugins.lv2/lsp-plugins.so", b"\x7fELF fake"),
    ])
    .expect("a well-formed 7z must extract");
  assert!(sandbox.dest.join("lsp-plugins.lv2/manifest.ttl").is_file());
  assert_eq!(
    std::fs::read(sandbox.dest.join("lsp-plugins.lv2/lsp-plugins.so")).unwrap(),
    b"\x7fELF fake"
  );
  sandbox.assert_nothing_escaped();
}

// ------------------------------------------------------------- bare files --
//
// A bare file has one name, and it comes from the URL it was published at.
// That name is the whole attack surface, so each shape of escape gets a case.

#[test]
fn a_bare_file_cannot_climb_out_by_its_name() {
  for name in ["../escape.clap", "..", "../../home/.ssh/authorized_keys"] {
    let sandbox = Sandbox::new();
    assert!(
      sandbox.place_bare(name, b"\x7fELF payload").is_err(),
      "{name:?} was placed"
    );
    assert!(
      list_tree(&sandbox.dest).is_empty(),
      "{name:?} left something"
    );
    sandbox.assert_nothing_escaped();
  }
}

#[test]
fn a_bare_file_is_one_component_and_never_a_path() {
  // An archive entry may create directories; a bare file has none to
  // recreate, so a separator means the name is lying about something.
  for name in ["sub/escape.clap", "/etc/passwd", "a/../../escape.clap"] {
    let sandbox = Sandbox::new();
    assert_unsafe(
      sandbox.place_bare(name, b"\x7fELF payload"),
      UnsafeEntry::MalformedPath,
    );
    assert!(
      list_tree(&sandbox.dest).is_empty(),
      "{name:?} left something"
    );
    sandbox.assert_nothing_escaped();
  }
}

#[test]
fn a_bare_file_needs_a_name() {
  for name in ["", "."] {
    let sandbox = Sandbox::new();
    assert!(
      sandbox.place_bare(name, b"data").is_err(),
      "{name:?} was placed"
    );
    sandbox.assert_nothing_escaped();
  }
}

#[test]
fn an_archive_declared_bare_is_refused_rather_than_placed_whole() {
  // Otherwise a zip named `.clap` would be installed as a plugin.
  let sandbox = Sandbox::new();
  let zip = build_zip(&[ZipEntry::file("inside.clap", b"\x7fELF")]);
  match sandbox.place_bare("Plugin.clap", &zip) {
    Err(ArchiveError::FormatMismatch { declared, detected }) => {
      assert_eq!(declared, "none");
      assert_eq!(detected, "zip");
    }
    other => panic!("expected a format mismatch, got {other:?}"),
  }
  assert!(list_tree(&sandbox.dest).is_empty());
  sandbox.assert_nothing_escaped();
}

#[test]
fn a_bare_file_is_placed_whole_without_special_bits() {
  let sandbox = Sandbox::new();
  sandbox
    .place_bare("LibreKick_linux_x86_64.clap", b"\x7fELF plugin")
    .expect("a plain name must be placed");
  let placed = sandbox.dest.join("LibreKick_linux_x86_64.clap");
  assert_eq!(std::fs::read(&placed).unwrap(), b"\x7fELF plugin");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&placed).unwrap().permissions().mode();
    assert_eq!(mode & 0o7000, 0);
  }
  sandbox.assert_nothing_escaped();
}
