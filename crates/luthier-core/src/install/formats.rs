//! Per-format installers.
//!
//! A [`FormatInstaller`] contributes only what genuinely differs between plugin
//! formats: where the format lives, whether it is a file or a directory, and
//! what a valid one looks like. All the staging, atomicity and rollback logic
//! lives once in [`super::transaction`]. That is why adding LV2 or a sample
//! library installer later is a small file rather than a refactor.

use crate::error::InstallError;
use crate::layout::Layout;
use luthier_manifest::{EntryKind, Format};
use std::io::Read;
use std::path::Path;

/// Knows how to place one plugin format.
pub trait FormatInstaller: Send + Sync {
  fn format(&self) -> Format;

  /// File or directory.
  fn entry_kind(&self) -> EntryKind;

  /// Checks a staged artifact really is what the manifest claimed.
  ///
  /// This is a real inspection, not an extension check: a mislabelled
  /// artifact would otherwise install cleanly and then fail inside a DAW,
  /// where the cause is far harder to see.
  fn validate(&self, staged: &Path) -> Result<(), InstallError>;

  /// Where this format is installed.
  fn root<'a>(&self, layout: &'a Layout) -> Option<&'a Path> {
    layout.plugin_root(&self.format())
  }
}

/// Every installer this build has.
pub fn installers() -> &'static [&'static dyn FormatInstaller] {
  &[
    &ClapInstaller,
    &Vst3Installer,
    &Lv2Installer,
    &LibraryInstaller,
  ]
}

/// The installer for `format`, if there is one.
pub fn installer_for(format: &Format) -> Option<&'static dyn FormatInstaller> {
  installers().iter().copied().find(|i| &i.format() == format)
}

// -------------------------------------------------------------------- CLAP --

/// CLAP: a single shared object named `*.clap`, installed into `~/.clap`.
pub struct ClapInstaller;

impl FormatInstaller for ClapInstaller {
  fn format(&self) -> Format {
    Format::Clap
  }

  fn entry_kind(&self) -> EntryKind {
    EntryKind::File
  }

  fn validate(&self, staged: &Path) -> Result<(), InstallError> {
    let fail = |reason: String| InstallError::FailedValidation {
      format: Format::Clap,
      path: staged.to_path_buf(),
      reason,
    };

    let meta =
      std::fs::symlink_metadata(staged).map_err(|e| fail(format!("cannot inspect it: {e}")))?;
    if !meta.is_file() {
      return Err(fail("a CLAP plugin is a single file on Linux".into()));
    }
    check_elf_shared_object(staged).map_err(fail)
  }
}

// -------------------------------------------------------------------- VST3 --

/// VST3: a bundle directory named `*.vst3`, installed into `~/.vst3`.
///
/// On Linux the binary lives at `Contents/<arch>-linux/<name>.so`, which is
/// what a host looks for; a bundle without one loads nowhere.
pub struct Vst3Installer;

/// The architecture subdirectory a VST3 bundle uses on Linux.
fn vst3_arch_dir() -> &'static str {
  match std::env::consts::ARCH {
    "aarch64" => "aarch64-linux",
    "x86" => "i386-linux",
    _ => "x86_64-linux",
  }
}

impl FormatInstaller for Vst3Installer {
  fn format(&self) -> Format {
    Format::Vst3
  }

  fn entry_kind(&self) -> EntryKind {
    EntryKind::Bundle
  }

  fn validate(&self, staged: &Path) -> Result<(), InstallError> {
    let fail = |reason: String| InstallError::FailedValidation {
      format: Format::Vst3,
      path: staged.to_path_buf(),
      reason,
    };

    if !staged.is_dir() {
      return Err(fail("a VST3 plugin is a bundle directory".into()));
    }

    let contents = staged.join("Contents");
    if !contents.is_dir() {
      return Err(fail("the bundle has no Contents directory".into()));
    }

    let arch_dir = contents.join(vst3_arch_dir());
    if !arch_dir.is_dir() {
      return Err(fail(format!(
        "the bundle has no Contents/{} directory, so no host on this platform could load it",
        vst3_arch_dir()
      )));
    }

    let mut binaries = std::fs::read_dir(&arch_dir)
      .map_err(|e| fail(format!("cannot read {}: {e}", arch_dir.display())))?
      .flatten()
      .filter(|entry| {
        entry.path().extension().and_then(|e| e.to_str()) == Some("so") && entry.path().is_file()
      })
      .peekable();

    if binaries.peek().is_none() {
      return Err(fail(format!(
        "Contents/{} contains no .so binary",
        vst3_arch_dir()
      )));
    }
    Ok(())
  }
}

// --------------------------------------------------------------------- LV2 --

/// LV2: a bundle directory named `*.lv2`, installed into `~/.lv2`.
///
/// The defining artifact is `manifest.ttl` at the top of the bundle: the LV2
/// specification requires it, and a host that cannot read it will not load the
/// bundle at all. Checked against the 255 bundles Debian ships — every one has
/// it, which is what makes it a safe hard requirement.
///
/// The binary is deliberately *not* required. Most bundles carry one, but a
/// preset or data-only bundle is equally valid LV2 and rejecting it would be
/// wrong. Where a binary does exist it is validated, which is what catches a
/// macOS or Windows bundle mislabelled as a Linux one.
pub struct Lv2Installer;

/// How far into a bundle to look for binaries.
///
/// Depth 1 covers almost everything; sfizz keeps its binaries at
/// `Contents/Binary/*.so`, which is depth 3. The bound stops a pathological
/// bundle turning validation into a full filesystem walk.
const LV2_MAX_DEPTH: usize = 6;

impl FormatInstaller for Lv2Installer {
  fn format(&self) -> Format {
    Format::Lv2
  }

  fn entry_kind(&self) -> EntryKind {
    EntryKind::Bundle
  }

  fn validate(&self, staged: &Path) -> Result<(), InstallError> {
    let fail = |reason: String| InstallError::FailedValidation {
      format: Format::Lv2,
      path: staged.to_path_buf(),
      reason,
    };

    if !staged.is_dir() {
      return Err(fail("an LV2 plugin is a bundle directory".into()));
    }
    if !staged.join("manifest.ttl").is_file() {
      return Err(fail(
        "the bundle has no manifest.ttl, which every LV2 bundle must have".into(),
      ));
    }

    for binary in collect_shared_objects(staged, LV2_MAX_DEPTH).map_err(&fail)? {
      check_elf_shared_object(&binary).map_err(|reason| {
        fail(format!(
          "{}: {reason}",
          binary.strip_prefix(staged).unwrap_or(&binary).display()
        ))
      })?;
    }
    Ok(())
  }
}

/// Every `*.so` inside `root`, to a bounded depth.
fn collect_shared_objects(
  root: &Path,
  max_depth: usize,
) -> Result<Vec<std::path::PathBuf>, String> {
  let mut found = Vec::new();
  let mut queue = vec![(root.to_path_buf(), 0usize)];

  while let Some((dir, depth)) = queue.pop() {
    if depth >= max_depth {
      continue;
    }
    let entries =
      std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
      let path = entry.path();
      // Symlinks are not followed: a staged bundle could otherwise point
      // validation at a file outside the transaction workspace.
      let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(_) => continue,
      };
      if meta.is_dir() {
        queue.push((path, depth + 1));
      } else if meta.is_file() && path.extension().and_then(|e| e.to_str()) == Some("so") {
        found.push(path);
      }
    }
  }
  found.sort();
  Ok(found)
}

// ----------------------------------------------------------------- library --

/// Sample libraries, preset packs and soundfonts.
///
/// The one installer that does not target a plugin root. Content has no format
/// root to derive a destination from, which was the open question this answers:
/// it uses [`Layout::library_root`] instead, reached through the trait's own
/// `root` hook. The manifest still never names a destination — the leaf comes
/// from the archive entry or a validated `rename`, exactly as for a plugin.
///
/// Validation is deliberately thin. Content is data: there is no header to
/// check and no shape that is universally wrong, so the useful checks are that
/// the entry exists, is not empty, and is what the rule said it was. A stricter
/// rule would reject legitimate libraries for no gain in safety — the extractor
/// has already refused traversal and the destination is already derived.
pub struct LibraryInstaller;

impl FormatInstaller for LibraryInstaller {
  fn format(&self) -> Format {
    Format::Library
  }

  fn entry_kind(&self) -> EntryKind {
    EntryKind::Bundle
  }

  fn root<'a>(&self, layout: &'a Layout) -> Option<&'a Path> {
    Some(layout.library_root())
  }

  fn validate(&self, staged: &Path) -> Result<(), InstallError> {
    let fail = |reason: String| InstallError::FailedValidation {
      format: Format::Library,
      path: staged.to_path_buf(),
      reason,
    };

    let meta =
      std::fs::symlink_metadata(staged).map_err(|e| fail(format!("cannot inspect it: {e}")))?;

    if meta.is_dir() {
      let mut entries = std::fs::read_dir(staged)
        .map_err(|e| fail(format!("cannot read it: {e}")))?
        .flatten()
        .peekable();
      if entries.peek().is_none() {
        return Err(fail("the directory is empty".into()));
      }
      Ok(())
    } else if meta.is_file() {
      if meta.len() == 0 {
        return Err(fail("the file is empty".into()));
      }
      Ok(())
    } else {
      Err(fail("library content must be a file or a directory".into()))
    }
  }
}

// ------------------------------------------------------------------- shared --

/// Confirms a file is an ELF shared object for this architecture.
fn check_elf_shared_object(path: &Path) -> Result<(), String> {
  const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
  const CLASS_64: u8 = 2;
  const TYPE_DYN: u16 = 3;
  const EM_X86_64: u16 = 0x3e;
  const EM_AARCH64: u16 = 0xb7;

  let mut file = std::fs::File::open(path).map_err(|e| format!("cannot open it: {e}"))?;
  let mut header = [0u8; 20];
  let mut filled = 0;
  while filled < header.len() {
    match file.read(&mut header[filled..]) {
      Ok(0) => break,
      Ok(n) => filled += n,
      Err(e) => return Err(format!("cannot read it: {e}")),
    }
  }
  if filled < header.len() || &header[..4] != ELF_MAGIC {
    return Err("it is not an ELF binary".into());
  }
  if header[4] != CLASS_64 {
    return Err("it is a 32-bit binary; this build installs 64-bit plugins".into());
  }
  let little_endian = header[5] == 1;
  let read_u16 = |bytes: [u8; 2]| {
    if little_endian {
      u16::from_le_bytes(bytes)
    } else {
      u16::from_be_bytes(bytes)
    }
  };

  let e_type = read_u16([header[16], header[17]]);
  if e_type != TYPE_DYN {
    return Err("it is not a shared object; a plugin must be loadable by a host".into());
  }

  let e_machine = read_u16([header[18], header[19]]);
  let expected = match std::env::consts::ARCH {
    "aarch64" => EM_AARCH64,
    _ => EM_X86_64,
  };
  if e_machine != expected {
    return Err(format!(
      "it is built for machine type {e_machine:#x}, not this machine's {expected:#x}"
    ));
  }
  Ok(())
}

#[cfg(test)]
pub(crate) use tests::{elf_shared_object, lv2_bundle, vst3_bundle};

#[cfg(test)]
mod tests {
  use super::*;

  /// A minimal but genuine ELF64 shared-object header.
  pub(crate) fn elf_shared_object() -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2; // 64-bit
    bytes[5] = 1; // little endian
    bytes[6] = 1; // version
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    let machine: u16 = if std::env::consts::ARCH == "aarch64" {
      0xb7
    } else {
      0x3e
    };
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes
  }

  #[test]
  fn a_real_shared_object_passes_clap_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Surge XT.clap");
    std::fs::write(&path, elf_shared_object()).unwrap();
    ClapInstaller.validate(&path).unwrap();
  }

  #[test]
  fn a_text_file_named_clap_is_rejected() {
    // The failure this prevents: installing cleanly, then silently not
    // appearing in any DAW.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Fake.clap");
    std::fs::write(&path, b"#!/bin/sh\necho not a plugin\n").unwrap();
    let err = ClapInstaller.validate(&path).unwrap_err();
    assert!(err.to_string().contains("not an ELF binary"), "{err}");
  }

  #[test]
  fn an_executable_rather_than_a_shared_object_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Tool.clap");
    let mut bytes = elf_shared_object();
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    std::fs::write(&path, bytes).unwrap();
    let err = ClapInstaller.validate(&path).unwrap_err();
    assert!(err.to_string().contains("not a shared object"), "{err}");
  }

  #[test]
  fn a_binary_for_another_architecture_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Other.clap");
    let mut bytes = elf_shared_object();
    bytes[18..20].copy_from_slice(&0x28u16.to_le_bytes()); // EM_ARM
    std::fs::write(&path, bytes).unwrap();
    let err = ClapInstaller.validate(&path).unwrap_err();
    assert!(err.to_string().contains("machine type"), "{err}");
  }

  #[test]
  fn a_directory_is_not_a_clap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Bundle.clap");
    std::fs::create_dir(&path).unwrap();
    let err = ClapInstaller.validate(&path).unwrap_err();
    assert!(err.to_string().contains("single file"), "{err}");
  }

  /// Builds the bundle layout confirmed against a real installed plugin.
  pub(crate) fn vst3_bundle(root: &Path, name: &str) -> std::path::PathBuf {
    let bundle = root.join(format!("{name}.vst3"));
    let arch = bundle.join("Contents").join(vst3_arch_dir());
    std::fs::create_dir_all(&arch).unwrap();
    std::fs::write(arch.join(format!("{name}.so")), elf_shared_object()).unwrap();
    std::fs::create_dir_all(bundle.join("Contents/Resources")).unwrap();
    std::fs::write(bundle.join("Contents/Resources/moduleinfo.json"), b"{}").unwrap();
    bundle
  }

  #[test]
  fn a_well_formed_bundle_passes_vst3_validation() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = vst3_bundle(dir.path(), "DecentSampler");
    Vst3Installer.validate(&bundle).unwrap();
  }

  #[test]
  fn a_bundle_without_a_linux_binary_is_rejected() {
    // A macOS-only bundle would otherwise install and never load.
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("MacOnly.vst3");
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
    std::fs::write(bundle.join("Contents/MacOS/MacOnly"), b"mach-o").unwrap();
    let err = Vst3Installer.validate(&bundle).unwrap_err();
    assert!(err.to_string().contains(vst3_arch_dir()), "{err}");
  }

  #[test]
  fn an_empty_arch_directory_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("Empty.vst3");
    std::fs::create_dir_all(bundle.join("Contents").join(vst3_arch_dir())).unwrap();
    let err = Vst3Installer.validate(&bundle).unwrap_err();
    assert!(err.to_string().contains("no .so binary"), "{err}");
  }

  #[test]
  fn a_file_is_not_a_vst3_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Flat.vst3");
    std::fs::write(&path, elf_shared_object()).unwrap();
    let err = Vst3Installer.validate(&path).unwrap_err();
    assert!(err.to_string().contains("bundle directory"), "{err}");
  }

  /// The common shape: binaries directly inside the bundle. 462 of the 464
  /// binaries across Debian's 255 LV2 bundles sit here.
  pub(crate) fn lv2_bundle(root: &Path, name: &str) -> std::path::PathBuf {
    let bundle = root.join(format!("{name}.lv2"));
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("manifest.ttl"), b"@prefix lv2: <> .\n").unwrap();
    std::fs::write(bundle.join(format!("{name}.ttl")), b"# plugin\n").unwrap();
    std::fs::write(bundle.join(format!("{name}.so")), elf_shared_object()).unwrap();
    bundle
  }

  #[test]
  fn a_conventional_bundle_passes_lv2_validation() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = lv2_bundle(dir.path(), "Calf");
    Lv2Installer.validate(&bundle).unwrap();
  }

  #[test]
  fn a_bundle_with_nested_binaries_passes() {
    // sfizz keeps its binaries at Contents/Binary/*.so. A top-level-only
    // check would reject the one plugin the registry most wants to see.
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("sfizz.lv2");
    let binary = bundle.join("Contents/Binary");
    std::fs::create_dir_all(&binary).unwrap();
    std::fs::write(bundle.join("manifest.ttl"), b"@prefix lv2: <> .\n").unwrap();
    std::fs::write(binary.join("sfizz.so"), elf_shared_object()).unwrap();
    std::fs::write(binary.join("sfizz_ui.so"), elf_shared_object()).unwrap();
    Lv2Installer.validate(&bundle).unwrap();
  }

  #[test]
  fn a_bundle_without_a_manifest_ttl_is_rejected() {
    // Every one of Debian's 255 bundles has one; the specification requires
    // it, and a host that cannot read it will not load the bundle at all.
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("NoManifest.lv2");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("NoManifest.so"), elf_shared_object()).unwrap();
    let err = Lv2Installer.validate(&bundle).unwrap_err();
    assert!(err.to_string().contains("manifest.ttl"), "{err}");
  }

  #[test]
  fn a_data_only_bundle_is_accepted() {
    // Preset and data bundles carry no binary and are still valid LV2.
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("Presets.lv2");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(bundle.join("manifest.ttl"), b"@prefix lv2: <> .\n").unwrap();
    std::fs::write(bundle.join("presets.ttl"), b"# presets\n").unwrap();
    Lv2Installer.validate(&bundle).unwrap();
  }

  #[test]
  fn a_bundle_whose_binary_is_for_another_platform_is_rejected() {
    // A macOS build mislabelled as Linux would install cleanly and then
    // load nowhere.
    let dir = tempfile::tempdir().unwrap();
    let bundle = lv2_bundle(dir.path(), "Foreign");
    let mut bytes = elf_shared_object();
    bytes[18..20].copy_from_slice(&0x28u16.to_le_bytes()); // EM_ARM
    std::fs::write(bundle.join("Foreign.so"), bytes).unwrap();
    let err = Lv2Installer.validate(&bundle).unwrap_err();
    assert!(err.to_string().contains("machine type"), "{err}");
    assert!(err.to_string().contains("Foreign.so"), "{err}");
  }

  #[test]
  fn a_file_is_not_an_lv2_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Flat.lv2");
    std::fs::write(&path, elf_shared_object()).unwrap();
    let err = Lv2Installer.validate(&path).unwrap_err();
    assert!(err.to_string().contains("bundle directory"), "{err}");
  }

  #[test]
  fn bundle_traversal_does_not_follow_symlinks() {
    // A staged bundle must not be able to point validation at a file
    // outside the transaction workspace.
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("evil.so"), b"not an elf at all").unwrap();

    let bundle = lv2_bundle(dir.path(), "Linked");
    std::os::unix::fs::symlink(&outside, bundle.join("link")).unwrap();

    // The symlinked directory is not descended into, so evil.so is never
    // read and validation passes on the bundle's own contents.
    Lv2Installer.validate(&bundle).unwrap();
  }

  #[test]
  fn a_directory_of_content_passes_library_validation() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("VSCO-2-CE");
    std::fs::create_dir_all(library.join("Strings")).unwrap();
    std::fs::write(library.join("Strings/violin.sfz"), b"<region>").unwrap();
    LibraryInstaller.validate(&library).unwrap();
  }

  #[test]
  fn a_single_file_of_content_passes_library_validation() {
    // A soundfont is one file; a sample library is a directory. Neither
    // shape is wrong, so the installer accepts both.
    let dir = tempfile::tempdir().unwrap();
    let sf2 = dir.path().join("FluidR3_GM.sf2");
    std::fs::write(&sf2, b"RIFF....sfbk").unwrap();
    LibraryInstaller.validate(&sf2).unwrap();
  }

  #[test]
  fn empty_content_is_rejected() {
    // An empty result almost always means the install rule's source path
    // was wrong, which is worth saying now rather than after it installs.
    let dir = tempfile::tempdir().unwrap();
    let empty_dir = dir.path().join("Nothing");
    std::fs::create_dir_all(&empty_dir).unwrap();
    assert!(
      LibraryInstaller
        .validate(&empty_dir)
        .unwrap_err()
        .to_string()
        .contains("empty")
    );

    let empty_file = dir.path().join("Nothing.sf2");
    std::fs::write(&empty_file, b"").unwrap();
    assert!(
      LibraryInstaller
        .validate(&empty_file)
        .unwrap_err()
        .to_string()
        .contains("empty")
    );
  }

  #[test]
  fn library_content_installs_outside_every_plugin_root() {
    // The question `kind: library` existed to answer: content has no
    // format root to derive a destination from. It uses the library root,
    // reached through the trait's own `root` hook.
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::rooted_at(dir.path());
    let root = LibraryInstaller.root(&layout).unwrap();

    assert_eq!(root, layout.library_root());
    for (_, plugin_root) in layout.plugin_roots() {
      assert_ne!(root, plugin_root);
      assert!(!root.starts_with(plugin_root));
    }
    // Still managed, so the uninstaller is allowed to remove it.
    assert!(layout.is_managed_location(&root.join("VSCO2")));
  }

  #[test]
  fn the_validator_and_the_installers_agree() {
    // "Formats this build installs" is stated twice: as a constant in
    // luthier-manifest, which the registry validator uses and which cannot
    // depend on this crate, and as the installer table here. If the two
    // drift, a manifest either validates and then fails at install time,
    // or is rejected for a format that would have worked.
    use std::collections::BTreeSet;
    let declared: BTreeSet<Format> = luthier_manifest::validate::INSTALLABLE_FORMATS
      .iter()
      .cloned()
      .collect();
    let implemented: BTreeSet<Format> = installers().iter().map(|i| i.format()).collect();
    assert_eq!(declared, implemented);
  }

  #[test]
  fn installers_are_registered_for_the_formats_we_support() {
    assert!(installer_for(&Format::Clap).is_some());
    assert!(installer_for(&Format::Vst3).is_some());
    assert!(installer_for(&Format::Lv2).is_some());
    assert!(installer_for(&Format::Library).is_some());
    assert!(installer_for(&Format::Other("sf2".into())).is_none());
  }
}
