//! Deriving install rules from an extracted archive.
//!
//! A registry entry that carries its own rules says *which* entry to install
//! and *what* it is. A source that carries none — the Open Audio Stack registry
//! is one — says only what formats an archive holds. Sorting entries into
//! per-format directories needs the missing half, and the archive itself is the
//! only place it exists.
//!
//! This is not the guessing §17 refuses. §17 is about a *manifest* that leaves
//! the manager to invent what to copy; the rules here are read from bytes whose
//! checksum has already been verified, by the same code that writes a
//! contributor's rules in `luthier-registry inspect`. One implementation, so
//! what the authoring tool shows and what the installer does cannot drift.
//!
//! Recognition is deliberately narrow: a format's conventional extension *and*
//! the shape that format requires on disk. `ProM.clap` ships as a directory in
//! DPF-Plugins, so it is not a CLAP file and no rule is produced for it —
//! installing it would put something in `~/.clap` no host is guaranteed to
//! load. Anything unrecognised is simply not installed.
//!
//! Content is read differently, because it announces itself differently. A
//! plugin is a file or a bundle with a conventional extension; a sample
//! library is a folder of `.sfz` and `.wav` that looks like any other folder.
//! So what is read for content is the *shape of the archive* — one wrapper
//! directory, or none — and whether that shape is content at all is the
//! manifest's answer, not this module's: [`ContentSource`] is reported for
//! every tree and used only where an artifact says it holds a `library`.

use crate::error::{Error, Result};
use luthier_manifest::{ArchivePath, EntryKind, Format, InstallRule};
use std::path::Path;

/// The formats a rule can be *derived* for.
///
/// Read before downloading, by [`crate::install::installable`]. A release that
/// declares nothing on this list is refused at that point rather than after
/// its bytes are on disk.
///
/// The plugin formats are read from a tree by [`recognise`], on an extension
/// and a shape. `Library` is not: it is read from the archive's shape by
/// [`content_of`], and only where the artifact says it holds one.
pub const DERIVABLE_FORMATS: &[Format] =
  &[Format::Clap, Format::Vst3, Format::Lv2, Format::Library];

/// Where a package's content sits in an extracted archive.
///
/// Both shapes are common, and both were counted in the Open Audio Stack
/// registry's sample libraries before this existed: thirteen unpack to one
/// directory named after a tag or a commit, fourteen unpack flat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSource {
  /// The single directory the archive unpacks to. Its name carries a commit
  /// or a tag more often than not, which is why nothing installs under it.
  Directory(ArchivePath),
  /// Everything extracted. The archive has no wrapper directory, so there is
  /// no name in it to use and nothing to descend into.
  Root,
}

/// One entry seen while walking an extracted archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
  /// Path relative to the extraction root.
  pub path: String,
  pub is_dir: bool,
  /// Set when this entry is a recognised plugin, and a rule was produced.
  pub format: Option<Format>,
}

/// What an extracted archive contains, and what of it can be installed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Derived {
  /// Every entry, sorted. Recognised bundles are listed but not descended
  /// into: a `.vst3` carrying a nested `.clap` installs whole.
  pub listing: Vec<Listed>,
  /// Rules for the recognised entries, sorted with the listing.
  pub rules: Vec<InstallRule>,
  /// What would be installed if this archive holds content. Reported for
  /// every non-empty tree and acted on only where the artifact declares a
  /// `library`, so a plugin archive that happens to hold no plugin is never
  /// installed as a folder of samples.
  pub content: Option<ContentSource>,
}

/// Walks an extracted archive and derives what to install from it.
///
/// `root` is the extraction directory. Paths that cannot be represented in a
/// manifest — invalid UTF-8, or anything `ArchivePath` refuses — produce no
/// rule and are listed as ordinary entries, which leaves them uninstalled.
/// That is the safe direction, and the extractor has already refused traversal
/// before anything reaches here.
pub fn from_tree(root: &Path) -> Result<Derived> {
  let mut derived = Derived::default();
  if root.is_dir() {
    walk(root, root, &mut derived)?;
    derived.content = content_of(root)?;
  }
  derived.listing.sort_by(|a, b| a.path.cmp(&b.path));
  derived
    .rules
    .sort_by(|a, b| (&a.format, a.source.as_str()).cmp(&(&b.format, b.source.as_str())));
  Ok(derived)
}

/// Reads the shape of an extracted archive, for a package that holds content.
///
/// One directory and nothing beside it is a wrapper, exactly as a forge's
/// branch tarball is; anything else is taken whole. The same rule as
/// `HttpSnapshotRegistry::unwrap_single_root`, for the same reason: what
/// upstream wrapped its files in is not part of what it published.
///
/// `None` only for an empty tree, which is an archive with nothing in it.
pub fn content_of(root: &Path) -> Result<Option<ContentSource>> {
  let mut entries = std::fs::read_dir(root)
    .map_err(|e| Error::io("list", root, e))?
    .collect::<std::result::Result<Vec<_>, _>>()
    .map_err(|e| Error::io("list", root, e))?;
  entries.sort_by_key(|entry| entry.file_name());

  match entries.as_slice() {
    [] => Ok(None),
    [only] if only.path().is_dir() => {
      let name = only.file_name();
      match name.to_str().map(ArchivePath::new) {
        // A name a manifest cannot express — invalid UTF-8, or anything
        // `ArchivePath` refuses — is not descended into. The whole tree
        // installs instead, which loses nothing and invents nothing.
        Some(Ok(path)) => Ok(Some(ContentSource::Directory(path))),
        _ => Ok(Some(ContentSource::Root)),
      }
    }
    _ => Ok(Some(ContentSource::Root)),
  }
}

/// The format an entry is, judged by its extension *and* its shape on disk.
fn recognise(path: &Path, is_dir: bool) -> Option<Format> {
  let format = match path.extension().and_then(|e| e.to_str())? {
    "clap" => Format::Clap,
    "vst3" => Format::Vst3,
    "lv2" => Format::Lv2,
    _ => return None,
  };
  // A CLAP is a file and a VST3 or LV2 is a directory. An entry with the
  // right name in the wrong shape is not that format.
  let expected = format.entry_kind()?;
  let actual = if is_dir {
    EntryKind::Bundle
  } else {
    EntryKind::File
  };
  (expected == actual).then_some(format)
}

fn walk(base: &Path, dir: &Path, out: &mut Derived) -> Result<()> {
  let entries = std::fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))?;
  for entry in entries {
    let entry = entry.map_err(|e| Error::io("list", dir, e))?;
    let path = entry.path();
    let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io("inspect", &path, e))?;
    let is_dir = meta.is_dir();

    let relative = path.strip_prefix(base).unwrap_or(&path);
    // A non-UTF-8 name cannot be written into a manifest, so it can be
    // reported but never installed.
    let Some(relative) = relative.to_str() else {
      out.listing.push(Listed {
        path: relative.display().to_string(),
        is_dir,
        format: None,
      });
      continue;
    };

    let rule = recognise(&path, is_dir).and_then(|format| {
      let source = ArchivePath::new(relative).ok()?;
      let kind = format.entry_kind()?;
      Some(InstallRule {
        format,
        source,
        kind,
        rename: None,
        allow: Vec::new(),
        extra: Default::default(),
      })
    });

    if let Some(rule) = rule {
      out.listing.push(Listed {
        path: relative.to_owned(),
        is_dir,
        format: Some(rule.format.clone()),
      });
      out.rules.push(rule);
      // Recognised bundles install whole; their contents are not
      // separate packages.
      continue;
    }

    out.listing.push(Listed {
      path: relative.to_owned(),
      is_dir,
      format: None,
    });
    if is_dir {
      walk(base, &path, out)?;
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  /// The layout a DPF-style release ships: a version-named top directory
  /// holding one entry per format, plus the VST2 shared object that has no
  /// installer here.
  fn version_nested(base: &Path) {
    let root = base.join("wstd-eq-v1.1.1");
    fs::create_dir_all(root.join("WSTD_EQ.vst3/Contents")).unwrap();
    fs::create_dir_all(root.join("WSTD_EQ.lv2")).unwrap();
    fs::write(root.join("WSTD_EQ.lv2/manifest.ttl"), b"").unwrap();
    fs::write(root.join("WSTD_EQ.clap"), b"").unwrap();
    fs::write(root.join("WSTD_EQ-vst.so"), b"").unwrap();
  }

  fn rules(root: &Path) -> Vec<(String, String, String)> {
    from_tree(root)
      .unwrap()
      .rules
      .into_iter()
      .map(|r| {
        (
          r.format.to_string(),
          r.source.as_str().to_owned(),
          r.kind.to_string(),
        )
      })
      .collect()
  }

  #[test]
  fn the_derivable_list_and_the_recogniser_agree() {
    // The list is what `installable` reads before a download and the
    // recogniser is what runs after one. If they drift, a release is
    // either refused for something that would have worked or downloaded
    // for something that never could.
    for format in DERIVABLE_FORMATS {
      // Content is the exception: it is read from the shape of the
      // archive rather than from an extension, because a folder of
      // samples looks like any other folder.
      if format == &Format::Library {
        continue;
      }
      let is_dir = format.entry_kind() == Some(EntryKind::Bundle);
      let path = std::path::PathBuf::from(format!("Thing.{format}"));
      assert_eq!(
        recognise(&path, is_dir).as_ref(),
        Some(format),
        "{format} is listed as derivable and is not recognised"
      );
    }
    // And the other half of that exception: nothing in a tree is
    // recognised as content by name.
    assert_eq!(
      recognise(std::path::Path::new("Samples.library"), true),
      None
    );
  }

  #[test]
  fn one_wrapper_directory_is_the_content_and_anything_else_is_the_whole_tree() {
    // The two shapes the registry's sample libraries actually ship in: a
    // GitHub source archive wrapped in `<name>-<commit>/`, and a release
    // asset unpacked flat.
    let dir = tempfile::tempdir().unwrap();
    let wrapped = dir.path().join("wrapped");
    fs::create_dir_all(wrapped.join("BillieDrum-48fadc0/Samples")).unwrap();
    fs::write(wrapped.join("BillieDrum-48fadc0/BillieDrum.sfz"), b"").unwrap();
    assert_eq!(
      from_tree(&wrapped).unwrap().content,
      Some(ContentSource::Directory(
        ArchivePath::new("BillieDrum-48fadc0").unwrap()
      ))
    );

    let flat = dir.path().join("flat");
    fs::create_dir_all(flat.join("samples")).unwrap();
    fs::write(flat.join("AVL_Drumkits_Percussion-1.0.sfz"), b"").unwrap();
    fs::write(flat.join("LICENSE"), b"").unwrap();
    assert_eq!(from_tree(&flat).unwrap().content, Some(ContentSource::Root));

    // One *file* at the root is not a wrapper to descend into.
    let single_file = dir.path().join("single");
    fs::create_dir_all(&single_file).unwrap();
    fs::write(single_file.join("kit.sfz"), b"").unwrap();
    assert_eq!(
      from_tree(&single_file).unwrap().content,
      Some(ContentSource::Root)
    );

    // An empty archive holds no content, and says so.
    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    assert_eq!(from_tree(&empty).unwrap().content, None);
  }

  #[test]
  fn content_is_reported_for_a_plugin_archive_too_and_the_caller_decides() {
    // Shape is all this reads. Whether a tree *is* content is the
    // manifest's answer — installing a plugin release that happened to
    // hold no plugin as a folder of samples would be a guess.
    let dir = tempfile::tempdir().unwrap();
    version_nested(dir.path());
    let derived = from_tree(dir.path()).unwrap();
    assert!(!derived.rules.is_empty());
    assert_eq!(
      derived.content,
      Some(ContentSource::Directory(
        ArchivePath::new("wstd-eq-v1.1.1").unwrap()
      ))
    );
  }

  #[test]
  fn every_installable_format_in_one_archive_is_derived_once() {
    let dir = tempfile::tempdir().unwrap();
    version_nested(dir.path());

    // The regression this guards: LV2 produced no rule at all, so a
    // manifest written from `inspect` silently dropped the format most
    // Linux plugins ship.
    assert_eq!(
      rules(dir.path()),
      vec![
        (
          "clap".to_owned(),
          "wstd-eq-v1.1.1/WSTD_EQ.clap".to_owned(),
          "file".to_owned()
        ),
        (
          "vst3".to_owned(),
          "wstd-eq-v1.1.1/WSTD_EQ.vst3".to_owned(),
          "bundle".to_owned()
        ),
        (
          "lv2".to_owned(),
          "wstd-eq-v1.1.1/WSTD_EQ.lv2".to_owned(),
          "bundle".to_owned()
        ),
      ]
    );
  }

  #[test]
  fn a_vst2_shared_object_is_not_derived() {
    let dir = tempfile::tempdir().unwrap();
    version_nested(dir.path());

    // There is no VST2 installer, and `.so` is ambiguous anyway: as likely
    // a helper library as a plugin. §17 says do not guess.
    assert!(!rules(dir.path()).iter().any(|(_, s, _)| s.ends_with(".so")));
  }

  #[test]
  fn a_bundle_is_not_descended_into() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("pkg");
    fs::create_dir_all(root.join("Thing.lv2")).unwrap();
    // A bundle carrying something that looks installable still installs
    // whole, rather than being mined for its contents.
    fs::write(root.join("Thing.lv2/Nested.clap"), b"").unwrap();

    let derived = rules(dir.path());

    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].0, "lv2");
  }

  #[test]
  fn a_clap_shipped_as_a_directory_is_not_a_clap() {
    let dir = tempfile::tempdir().unwrap();
    // DPF-Plugins ships `ProM.clap` as a directory. Installing it would
    // put something in `~/.clap` no host is guaranteed to load, so the
    // extension alone must not decide.
    fs::create_dir_all(dir.path().join("ProM.clap")).unwrap();
    fs::write(dir.path().join("ProM.clap/ProM.clap"), b"").unwrap();

    let derived = rules(dir.path());

    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].1, "ProM.clap/ProM.clap");
  }

  #[test]
  fn a_bundle_shipped_as_a_file_is_not_a_bundle() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Thing.vst3"), b"").unwrap();
    fs::write(dir.path().join("Thing.lv2"), b"").unwrap();

    assert!(rules(dir.path()).is_empty());
  }

  #[test]
  fn an_archive_with_no_plugins_derives_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    fs::write(dir.path().join("docs/README.md"), b"").unwrap();

    let derived = from_tree(dir.path()).unwrap();

    assert!(derived.rules.is_empty());
    assert_eq!(derived.listing.len(), 2);
  }

  #[test]
  fn the_listing_reports_shape_and_format() {
    let dir = tempfile::tempdir().unwrap();
    version_nested(dir.path());

    let listing = from_tree(dir.path()).unwrap().listing;
    let find = |p: &str| listing.iter().find(|e| e.path == p).cloned().unwrap();

    let bundle = find("wstd-eq-v1.1.1/WSTD_EQ.lv2");
    assert!(bundle.is_dir);
    assert_eq!(bundle.format, Some(Format::Lv2));

    let plain = find("wstd-eq-v1.1.1/WSTD_EQ-vst.so");
    assert!(!plain.is_dir);
    assert_eq!(plain.format, None);
  }
}
