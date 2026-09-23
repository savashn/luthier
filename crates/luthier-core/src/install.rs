//! Installing and removing files.
//!
//! `rename(2)` is atomic for one path, not for a whole package, so a multi-file
//! install cannot be made atomic by renaming alone (§18). Instead every step is
//! journalled before it happens: each item is staged *in its final directory*
//! (so the rename is same-filesystem and therefore atomic), the journal is
//! flushed, and only then is the rename performed. A failure part-way replays
//! the journal backwards, and a journal left behind by a killed process is
//! replayed on the next run — so the outcome is always either the complete new
//! state or the complete previous one.

pub mod derive;
pub mod formats;

pub use derive::{ContentSource, Derived, Listed};
pub use formats::{ClapInstaller, FormatInstaller, Vst3Installer, installer_for, installers};

use crate::error::{Error, InstallError, Result};
use crate::fsutil;
use crate::layout::Layout;
use crate::state::{BundleFile, InstalledEntry, State};
use luthier_manifest::{Artifact, EntryKind, Format, InstallRule, PackageId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Whether this build could install `artifact` at all, judged before fetching
/// a byte of it.
///
/// An artifact that declares its rules is installable by definition: the
/// validator has already refused any rule naming a format with no installer.
/// One whose rules must be derived is a different matter — derivation reads
/// only what [`derive::DERIVABLE_FORMATS`] lists, so a release that holds
/// nothing on that list has nothing to install, and the only way to discover
/// that after the fact is to download it first.
///
/// That is what this exists to avoid. The Open Audio Stack registry carries
/// standalone programs and VST2 builds alongside plugins; before this, both
/// were fetched in full — up to gigabytes for a sample library — and then
/// refused by the installer with a message about the archive. The artifact
/// said what it held all along.
///
/// It takes upstream's claim at its word in the refusing direction, which is
/// a real cost: a release whose metadata under-reports what it ships is now
/// unreachable rather than merely wasteful. The alternative is to download
/// everything on the chance that a claim is wrong, and a claim that is wrong
/// is a thing to fix where it is published.
pub fn installable(artifact: &Artifact) -> bool {
  if !artifact.install.is_empty() {
    return true;
  }
  artifact.derive_install
    && artifact
      .provides
      .iter()
      .any(|format| derive::DERIVABLE_FORMATS.contains(format))
}

/// One thing to install, with its source and destination resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedItem {
  pub format: Format,
  /// Where it currently is, inside the extracted archive.
  pub source: PathBuf,
  /// Where it will end up.
  pub destination: PathBuf,
  pub kind: EntryKind,
}

/// Resolves install rules against an extracted tree and the current state.
///
/// Note the destination is *derived* here from the format's root plus the leaf
/// name — it is never read from the manifest. A registry contributor therefore
/// cannot direct a write anywhere outside the plugin directory for that format.
pub fn plan(
  layout: &Layout,
  extract_root: &Path,
  rules: &[InstallRule],
  package: &PackageId,
  state: &State,
) -> Result<Vec<PlannedItem>> {
  let mut items = Vec::new();

  for rule in rules {
    let installer = installer_for(&rule.format)
      .ok_or_else(|| Error::Install(InstallError::NoInstaller(rule.format.clone())))?;

    let root = installer
      .root(layout)
      .ok_or_else(|| Error::Install(InstallError::NoInstaller(rule.format.clone())))?;

    let source = rule.source.resolve_under(extract_root);
    if !source.exists() {
      return Err(Error::Archive(crate::error::ArchiveError::MissingEntry {
        path: rule.source.to_string(),
      }));
    }

    let destination = root.join(rule.installed_name());

    // Belt and braces: the destination is derived, but assert it anyway.
    if !destination.starts_with(root) || !layout.is_managed_location(&destination) {
      return Err(Error::Install(InstallError::OutsideManagedRoot {
        path: destination,
      }));
    }

    check_conflict(&destination, package, state)?;

    items.push(PlannedItem {
      format: rule.format.clone(),
      source,
      destination,
      kind: rule.kind.clone(),
    });
  }

  Ok(items)
}

/// Plans the one item a package of content installs.
///
/// The destination is derived exactly as a plugin's is — a root from the
/// [`Layout`], a leaf this function chooses — except that the leaf is the
/// package ID rather than a name out of the archive. That is deliberate. The
/// archives this exists for unpack to `BillieDrum-48fadc01…`, a directory
/// named after a commit and renamed on every release, and half of them unpack
/// with no directory at all. Installing under the ID gives one predictable
/// path in both cases, and the ID is already a validated path component —
/// lowercase, digits and hyphens, nothing else — which is what makes it safe
/// to join.
pub fn plan_content(
  layout: &Layout,
  extract_root: &Path,
  content: &ContentSource,
  package: &PackageId,
  state: &State,
) -> Result<PlannedItem> {
  let installer = installer_for(&Format::Library)
    .ok_or_else(|| Error::Install(InstallError::NoInstaller(Format::Library)))?;
  let root = installer
    .root(layout)
    .ok_or_else(|| Error::Install(InstallError::NoInstaller(Format::Library)))?;

  let source = match content {
    ContentSource::Directory(path) => path.resolve_under(extract_root),
    ContentSource::Root => extract_root.to_path_buf(),
  };
  if !source.is_dir() {
    return Err(Error::Archive(crate::error::ArchiveError::MissingEntry {
      path: match content {
        ContentSource::Directory(path) => path.to_string(),
        ContentSource::Root => ".".to_owned(),
      },
    }));
  }

  let destination = root.join(package.as_str());
  if !destination.starts_with(root) || !layout.is_managed_location(&destination) {
    return Err(Error::Install(InstallError::OutsideManagedRoot {
      path: destination,
    }));
  }
  check_conflict(&destination, package, state)?;

  Ok(PlannedItem {
    format: Format::Library,
    source,
    destination,
    kind: EntryKind::Bundle,
  })
}

/// Refuses to write over anything we do not already own (§30).
fn check_conflict(destination: &Path, package: &PackageId, state: &State) -> Result<()> {
  if !destination.exists() {
    return Ok(());
  }
  match state.owner_of(destination) {
    // Ours already: an upgrade or a repair, which is allowed.
    Some(owner) if &owner.id == package => Ok(()),
    Some(owner) => Err(Error::Install(InstallError::UnmanagedConflict {
      path: destination.to_path_buf(),
      package: owner.id.clone(),
    })),
    // Present but unrecorded: installed by hand, by a distro package, or by
    // an installer script. Never silently replaced.
    None => Err(Error::Install(InstallError::UnmanagedConflict {
      path: destination.to_path_buf(),
      package: package.clone(),
    })),
  }
}

/// One completed placement, recorded so it can be undone.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Placement {
  destination: PathBuf,
  /// Where the previous occupant was moved, if there was one.
  backup: Option<PathBuf>,
}

/// The on-disk record of an in-flight transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
  id: String,
  package: String,
  placements: Vec<Placement>,
  /// Staging paths written but not yet renamed into place.
  pending: Vec<PathBuf>,
}

/// An in-progress installation.
///
/// Dropping without [`InstallTransaction::commit`] rolls everything back, so a
/// `?` on any intermediate step leaves the previous installation intact.
pub struct InstallTransaction {
  dir: PathBuf,
  journal_path: PathBuf,
  journal: Journal,
  committed: bool,
}

impl InstallTransaction {
  pub fn begin(layout: &Layout, package: &PackageId) -> Result<Self> {
    let id = transaction_id();
    let dir = layout.transaction_dir(&id);
    fsutil::ensure_dir(&dir)?;
    let journal_path = dir.join("journal.json");
    let transaction = Self {
      dir,
      journal_path,
      journal: Journal {
        id,
        package: package.to_string(),
        placements: Vec::new(),
        pending: Vec::new(),
      },
      committed: false,
    };
    transaction.flush_journal()?;
    Ok(transaction)
  }

  /// Scratch space for this transaction, e.g. the extraction directory.
  pub fn workspace(&self) -> &Path {
    &self.dir
  }

  fn flush_journal(&self) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&self.journal).map_err(|e| {
      Error::io(
        "serialise journal",
        &self.journal_path,
        std::io::Error::other(e),
      )
    })?;
    fsutil::write_atomic(&self.journal_path, &bytes)
  }

  /// Stages, validates and atomically moves one item into place.
  pub fn place(&mut self, item: &PlannedItem) -> Result<InstalledEntry> {
    let installer = installer_for(&item.format)
      .ok_or_else(|| Error::Install(InstallError::NoInstaller(item.format.clone())))?;

    let parent = item.destination.parent().ok_or_else(|| {
      Error::Install(InstallError::OutsideManagedRoot {
        path: item.destination.clone(),
      })
    })?;
    fsutil::ensure_dir(parent)?;

    let leaf = item
      .destination
      .file_name()
      .unwrap_or_default()
      .to_string_lossy()
      .into_owned();
    // Staged as a sibling of the destination so the later rename stays
    // within one filesystem and is therefore atomic.
    let staging = parent.join(format!(".luthier-stage-{}-{leaf}", self.journal.id));

    self.journal.pending.push(staging.clone());
    self.flush_journal()?;

    fsutil::remove_any(&staging)?;
    fsutil::copy_tree(&item.source, &staging)?;

    // Validate the staged copy, not the source: this is the last moment
    // before it becomes visible to a host.
    if let Err(e) = installer.validate(&staging) {
      let _ = fsutil::remove_any(&staging);
      return Err(Error::Install(e));
    }

    let backup = if item.destination.exists() {
      let backup = parent.join(format!(".luthier-backup-{}-{leaf}", self.journal.id));
      fsutil::remove_any(&backup)?;
      std::fs::rename(&item.destination, &backup)
        .map_err(|e| Error::io("move aside previous version of", &item.destination, e))?;
      Some(backup)
    } else {
      None
    };

    self.journal.placements.push(Placement {
      destination: item.destination.clone(),
      backup: backup.clone(),
    });
    self.flush_journal()?;

    if let Err(e) = std::fs::rename(&staging, &item.destination) {
      // Put the previous version back before reporting.
      if let Some(backup) = &backup {
        let _ = std::fs::rename(backup, &item.destination);
      }
      self.journal.placements.pop();
      let _ = fsutil::remove_any(&staging);
      return Err(Error::io("install", &item.destination, e));
    }

    self.journal.pending.retain(|p| p != &staging);
    self.flush_journal()?;

    record_entry(&item.destination, &item.kind)
  }

  /// Makes the transaction permanent and discards its scratch space.
  pub fn commit(mut self) -> Result<()> {
    self.committed = true;
    for placement in &self.journal.placements {
      if let Some(backup) = &placement.backup {
        fsutil::remove_any(backup)?;
      }
    }
    fsutil::remove_any(&self.dir)?;
    Ok(())
  }

  /// Reverses everything done so far.
  fn undo(&mut self) -> Result<()> {
    for placement in self.journal.placements.iter().rev() {
      fsutil::remove_any(&placement.destination)?;
      if let Some(backup) = &placement.backup
        && backup.exists()
      {
        std::fs::rename(backup, &placement.destination)
          .map_err(|e| Error::io("restore", &placement.destination, e))?;
      }
    }
    for staging in &self.journal.pending {
      fsutil::remove_any(staging)?;
    }
    self.journal.placements.clear();
    self.journal.pending.clear();
    fsutil::remove_any(&self.dir)?;
    Ok(())
  }
}

impl Drop for InstallTransaction {
  fn drop(&mut self) {
    if self.committed {
      return;
    }
    // An error on the failure path has nowhere useful to go, but silence
    // would leave staging files behind with no explanation.
    if let Err(e) = self.undo() {
      tracing::error!(
          transaction = %self.journal.id,
          package = %self.journal.package,
          "failed to roll back cleanly: {e}"
      );
    } else {
      tracing::debug!(transaction = %self.journal.id, "rolled back");
    }
  }
}

/// Replays and discards journals left behind by an interrupted run.
///
/// Called before any mutating operation, while the state lock is held. An
/// interrupted install is rolled back rather than completed: we cannot know
/// whether the remaining steps would have succeeded, and the previous
/// installation is the state the user last consented to.
pub fn recover_interrupted(layout: &Layout) -> Result<Vec<String>> {
  let root = layout.transactions_dir();
  if !root.is_dir() {
    return Ok(Vec::new());
  }

  let mut recovered = Vec::new();
  let entries = std::fs::read_dir(&root).map_err(|e| Error::io("list", &root, e))?;
  for entry in entries {
    let entry = entry.map_err(|e| Error::io("list", &root, e))?;
    let dir = entry.path();
    if !dir.is_dir() {
      continue;
    }
    let journal_path = dir.join("journal.json");
    let Ok(bytes) = std::fs::read(&journal_path) else {
      // No journal: nothing was placed, so the directory is just litter.
      fsutil::remove_any(&dir)?;
      continue;
    };
    let journal: Journal = match serde_json::from_slice(&bytes) {
      Ok(journal) => journal,
      Err(e) => {
        tracing::warn!(path = %journal_path.display(), "unreadable journal: {e}");
        continue;
      }
    };
    let id = journal.id.clone();
    let mut transaction = InstallTransaction {
      dir,
      journal_path,
      journal,
      committed: false,
    };
    transaction.undo()?;
    transaction.committed = true; // undone already; do not repeat on drop
    recovered.push(id);
  }
  recovered.sort();
  Ok(recovered)
}

/// Records what was installed, so it can later be verified and removed exactly.
fn record_entry(path: &Path, kind: &EntryKind) -> Result<InstalledEntry> {
  match kind {
    EntryKind::File => Ok(InstalledEntry::File {
      path: path.to_path_buf(),
      sha256: fsutil::hash_file(path)?,
      mode: fsutil::file_mode(path)?,
    }),
    EntryKind::Bundle => {
      let mut contents = Vec::new();
      for relative in fsutil::walk_files(path)? {
        let full = path.join(&relative);
        contents.push(BundleFile {
          path: relative.to_string_lossy().into_owned(),
          sha256: fsutil::hash_file(&full)?,
          mode: fsutil::file_mode(&full)?,
        });
      }
      Ok(InstalledEntry::Bundle {
        path: path.to_path_buf(),
        contents,
      })
    }
    EntryKind::Other(other) => Err(Error::Install(InstallError::NoInstaller(Format::Other(
      other.clone(),
    )))),
  }
}

/// Result of checking an installed entry against what was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryStatus {
  Intact,
  Missing,
  Modified { detail: String },
}

/// Compares what is on disk with what was recorded at install time (§31 `verify`).
pub fn verify_entry(entry: &InstalledEntry) -> Result<EntryStatus> {
  match entry {
    InstalledEntry::File { path, sha256, .. } => {
      if !path.is_file() {
        return Ok(EntryStatus::Missing);
      }
      let actual = fsutil::hash_file(path)?;
      Ok(if &actual == sha256 {
        EntryStatus::Intact
      } else {
        EntryStatus::Modified {
          detail: format!("contents changed (now {})", actual.short(12)),
        }
      })
    }
    InstalledEntry::Bundle { path, contents } => {
      if !path.is_dir() {
        return Ok(EntryStatus::Missing);
      }
      let mut changed = Vec::new();
      for file in contents {
        let full = path.join(&file.path);
        if !full.is_file() {
          changed.push(format!("{} is missing", file.path));
          continue;
        }
        if fsutil::hash_file(&full)? != file.sha256 {
          changed.push(format!("{} changed", file.path));
        }
      }
      let present = fsutil::walk_files(path)?;
      for extra in &present {
        let name = extra.to_string_lossy();
        if !contents.iter().any(|f| f.path == name) {
          changed.push(format!("{name} was added"));
        }
      }
      Ok(if changed.is_empty() {
        EntryStatus::Intact
      } else {
        EntryStatus::Modified {
          detail: changed.join(", "),
        }
      })
    }
    InstalledEntry::Dir { path } => Ok(if path.is_dir() {
      EntryStatus::Intact
    } else {
      EntryStatus::Missing
    }),
  }
}

/// Something removal left on disk, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
  pub path: PathBuf,
  pub reason: String,
}

/// Whether a path recorded inside a bundle stays inside it: relative, and
/// made of plain names only.
///
/// State is read from disk, so a bundle's recorded contents are as
/// untrusted as its root. Removal joins these onto the root and deletes the
/// result; `../../.ssh/id_ed25519` would otherwise be a way out.
pub fn is_plain_relative(path: &str) -> bool {
  let path = Path::new(path);
  path.components().next().is_some()
    && path
      .components()
      .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Deletes what this manager installed for `entry` and nothing else (§20).
///
/// Anything changed or added after installation is the user's, so it stays
/// and is reported — but only that. A bundle is decided file by file: one
/// edited preset keeps one file, not the whole bundle. Keeping the whole
/// bundle left a plugin a host would still load, belonging to a package the
/// manager had just reported removed, and a sample library no command could
/// reach again, because the package was already gone from state.
///
/// No symbolic link is followed. `remove_dir_all` never followed one, and
/// deleting file by file must not start: a directory inside a bundle
/// replaced by a link to `~/Music` would otherwise have `~/Music` emptied.
/// A link found where something was recorded is left alone and reported.
pub fn remove_entry(entry: &InstalledEntry) -> Result<Vec<Kept>> {
  let mut kept = Vec::new();
  match entry {
    InstalledEntry::File { path, sha256, .. } => {
      let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(kept);
      };
      if !meta.is_file() {
        kept.push(Kept {
          path: path.clone(),
          reason: "replaced by something that is not a regular file".into(),
        });
      } else if &fsutil::hash_file(path)? == sha256 {
        fsutil::remove_any(path)?;
      } else {
        kept.push(Kept {
          path: path.clone(),
          reason: "contents changed after installation".into(),
        });
      }
    }
    InstalledEntry::Bundle { path, contents } => {
      let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(kept);
      };
      if !meta.is_dir() {
        kept.push(Kept {
          path: path.clone(),
          reason: "replaced by something that is not a directory".into(),
        });
        return Ok(kept);
      }
      for file in contents {
        if !is_plain_relative(&file.path) {
          return Err(Error::Install(InstallError::OutsideManagedRoot {
            path: path.join(&file.path),
          }));
        }
        let full = path.join(&file.path);
        if !reached_without_links(path, Path::new(&file.path)) {
          continue; // Reported below with everything else left behind.
        }
        match std::fs::symlink_metadata(&full) {
          Ok(meta) if meta.is_file() => {
            if fsutil::hash_file(&full)? == file.sha256 {
              fsutil::remove_any(&full)?;
            } else {
              kept.push(Kept {
                path: full,
                reason: "contents changed after installation".into(),
              });
            }
          }
          _ => {}
        }
      }
      for left in remaining_entries(path)? {
        let full = path.join(&left);
        if kept.iter().any(|k| k.path == full) {
          continue;
        }
        let is_link = std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_symlink());
        kept.push(Kept {
          path: full,
          reason: if is_link {
            "a symbolic link, which removal does not follow".into()
          } else {
            "added after installation".into()
          },
        });
      }
      prune_empty_dirs(path)?;
    }
    InstalledEntry::Dir { path } => {
      // "May remove if it ends up empty" — never its contents.
      if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.is_dir() && dir_is_empty(path)? {
          std::fs::remove_dir(path).map_err(|e| Error::io("remove directory", path, e))?;
        } else {
          kept.push(Kept {
            path: path.clone(),
            reason: "not empty".into(),
          });
        }
      }
    }
  }
  Ok(kept)
}

/// Whether every directory between `root` and `root/relative` is a real
/// directory rather than a link to one.
fn reached_without_links(root: &Path, relative: &Path) -> bool {
  let mut current = root.to_path_buf();
  let mut parts = relative.components().peekable();
  while let Some(part) = parts.next() {
    if parts.peek().is_none() {
      return true;
    }
    current.push(part);
    match std::fs::symlink_metadata(&current) {
      Ok(meta) if meta.is_dir() => {}
      _ => return false,
    }
  }
  true
}

/// Everything under `root` that is not a directory — regular files and
/// links alike — as paths relative to it, without following links.
fn remaining_entries(root: &Path) -> Result<Vec<PathBuf>> {
  fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))?;
    for entry in entries {
      let entry = entry.map_err(|e| Error::io("list", dir, e))?;
      let path = entry.path();
      let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io("inspect", &path, e))?;
      if meta.is_dir() {
        walk(base, &path, out)?;
      } else {
        out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf());
      }
    }
    Ok(())
  }
  let mut out = Vec::new();
  walk(root, root, &mut out)?;
  out.sort();
  Ok(out)
}

/// Removes every directory under `root`, and `root` itself, that holds
/// nothing. Deepest first, so a chain of emptied directories goes entirely.
fn prune_empty_dirs(root: &Path) -> Result<()> {
  fn prune(dir: &Path) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))?;
    for entry in entries {
      let entry = entry.map_err(|e| Error::io("list", dir, e))?;
      let path = entry.path();
      let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io("inspect", &path, e))?;
      if meta.is_dir() {
        prune(&path)?;
      }
    }
    if dir_is_empty(dir)? {
      std::fs::remove_dir(dir).map_err(|e| Error::io("remove directory", dir, e))?;
    }
    Ok(())
  }
  prune(root)
}

fn dir_is_empty(dir: &Path) -> Result<bool> {
  Ok(
    std::fs::read_dir(dir)
      .map_err(|e| Error::io("list", dir, e))?
      .next()
      .is_none(),
  )
}

/// A transaction identifier that is unique within a machine.
fn transaction_id() -> String {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_nanos())
    .unwrap_or(0);
  format!("{}-{nanos:x}", std::process::id())
}

#[cfg(test)]
mod tests {
  /// An artifact as a source that carries no rules of its own produces one.
  fn derived(provides: &[Format]) -> luthier_manifest::Artifact {
    let text = format!(
      "schema = 1\nid = \"x\"\nname = \"x\"\nkind = \"plugin\"\ncategory = \"effect\"\n\
       license = {{ kind = \"open-source\", spdx = \"MIT\" }}\n\n[[releases]]\n\
       version = \"1.0.0\"\n\n[[releases.artifacts]]\n\
       target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
       source = {{ type = \"http\", url = \"https://e.invalid/x.zip\" }}\narchive = \"zip\"\n\
       checksum = {{ sha256 = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\" }}\n\
       provides = [{}]\nderive_install = true\n",
      provides
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ")
    );
    luthier_manifest::from_toml(&text, "x", luthier_manifest::ParseMode::Strict)
      .unwrap()
      .manifest
      .releases
      .remove(0)
      .artifacts
      .remove(0)
  }

  #[test]
  fn derived_rules_can_only_promise_what_a_tree_can_be_read_for() {
    // A claim this build can act on.
    assert!(installable(&derived(&[Format::Clap])));
    assert!(installable(&derived(&[Format::Vst3, Format::Lv2])));

    // A VST2 build or a standalone program: the source says the archive
    // holds something, and names nothing this build installs.
    assert!(!installable(&derived(&[])));

    // Sample content: the archive's shape says where it is, and the
    // manifest's `library` says that is what it holds.
    assert!(installable(&derived(&[Format::Library])));
    assert!(installable(&derived(&[Format::Library, Format::Clap])));
  }

  #[test]
  fn declared_rules_answer_for_themselves() {
    // What a manifest declares has already been through the validator,
    // which refuses a rule naming a format with no installer. `provides` is
    // upstream's summary and does not get a veto over it.
    let mut artifact = derived(&[]);
    artifact.derive_install = false;
    artifact.install = vec![InstallRule {
      format: Format::Library,
      source: luthier_manifest::ArchivePath::new("Kit").unwrap(),
      kind: EntryKind::Bundle,
      rename: None,
      allow: Vec::new(),
      extra: Default::default(),
    }];
    assert!(installable(&artifact));
  }

  use super::*;
  use crate::install::formats::{elf_shared_object, lv2_bundle, vst3_bundle};
  use crate::state::State;
  use luthier_manifest::{ArchivePath, Extra};

  struct Fixture {
    _dir: tempfile::TempDir,
    layout: Layout,
    extract: PathBuf,
  }

  impl Fixture {
    fn new() -> Self {
      let dir = tempfile::tempdir().unwrap();
      let layout = Layout::rooted_at(dir.path());
      let extract = dir.path().join("extract");
      std::fs::create_dir_all(&extract).unwrap();
      Self {
        _dir: dir,
        layout,
        extract,
      }
    }

    fn with_clap(&self, name: &str) -> PathBuf {
      let path = self.extract.join(format!("{name}.clap"));
      std::fs::write(&path, elf_shared_object()).unwrap();
      path
    }
  }

  fn rule(format: Format, source: &str, kind: EntryKind) -> InstallRule {
    InstallRule {
      format,
      source: ArchivePath::new(source).unwrap(),
      kind,
      rename: None,
      allow: Vec::new(),
      extra: Extra::new(),
    }
  }

  fn package_id(id: &str) -> PackageId {
    PackageId::new(id).unwrap()
  }

  #[test]
  fn planning_derives_the_destination_from_the_format_root() {
    let fixture = Fixture::new();
    fixture.with_clap("Surge XT");
    let rules = vec![rule(Format::Clap, "Surge XT.clap", EntryKind::File)];
    let items = plan(
      &fixture.layout,
      &fixture.extract,
      &rules,
      &package_id("surge-xt"),
      &State::default(),
    )
    .unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(
      items[0].destination,
      fixture
        .layout
        .plugin_root(&Format::Clap)
        .unwrap()
        .join("Surge XT.clap")
    );
  }

  #[test]
  fn planning_fails_when_the_archive_lacks_the_declared_source() {
    let fixture = Fixture::new();
    let rules = vec![rule(Format::Clap, "Missing.clap", EntryKind::File)];
    let err = plan(
      &fixture.layout,
      &fixture.extract,
      &rules,
      &package_id("surge-xt"),
      &State::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not contain"), "{err}");
  }

  #[test]
  fn an_existing_unmanaged_plugin_blocks_the_install() {
    // The live case on a real machine: Surge XT already sits in ~/.clap,
    // installed by hand. It must not be silently replaced (§30).
    let fixture = Fixture::new();
    fixture.with_clap("Surge XT");
    let clap_root = fixture.layout.plugin_root(&Format::Clap).unwrap();
    std::fs::create_dir_all(clap_root).unwrap();
    let existing = clap_root.join("Surge XT.clap");
    std::fs::write(&existing, b"INSTALLED BY HAND").unwrap();

    let rules = vec![rule(Format::Clap, "Surge XT.clap", EntryKind::File)];
    let err = plan(
      &fixture.layout,
      &fixture.extract,
      &rules,
      &package_id("surge-xt"),
      &State::default(),
    )
    .unwrap_err();

    assert!(
      matches!(err, Error::Install(InstallError::UnmanagedConflict { .. })),
      "{err}"
    );
    assert_eq!(std::fs::read(&existing).unwrap(), b"INSTALLED BY HAND");
  }

  #[test]
  fn placing_a_clap_installs_it_and_records_its_digest() {
    let fixture = Fixture::new();
    let source = fixture.with_clap("Surge XT");
    let destination = fixture
      .layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .join("Surge XT.clap");
    let item = PlannedItem {
      format: Format::Clap,
      source,
      destination: destination.clone(),
      kind: EntryKind::File,
    };

    let mut transaction =
      InstallTransaction::begin(&fixture.layout, &package_id("surge-xt")).unwrap();
    let entry = transaction.place(&item).unwrap();
    transaction.commit().unwrap();

    assert!(destination.is_file());
    match entry {
      InstalledEntry::File { path, sha256, .. } => {
        assert_eq!(path, destination);
        assert_eq!(sha256, fsutil::hash_file(&destination).unwrap());
      }
      other => panic!("expected a file entry, got {other:?}"),
    }
    assert!(!fixture.layout.transactions_dir().join("x").exists());
  }

  #[test]
  fn an_lv2_bundle_installs_through_the_same_transaction() {
    // validate() passing is not the same as the format working end to end:
    // this drives the real staging, rename and recording path.
    let fixture = Fixture::new();
    let source = lv2_bundle(&fixture.extract, "Calf");
    let destination = fixture
      .layout
      .plugin_root(&Format::Lv2)
      .unwrap()
      .join("Calf.lv2");
    let item = PlannedItem {
      format: Format::Lv2,
      source,
      destination: destination.clone(),
      kind: EntryKind::Bundle,
    };

    let mut transaction = InstallTransaction::begin(&fixture.layout, &package_id("calf")).unwrap();
    let entry = transaction.place(&item).unwrap();
    transaction.commit().unwrap();

    assert!(destination.join("manifest.ttl").is_file());
    assert!(destination.join("Calf.so").is_file());
    match &entry {
      InstalledEntry::Bundle { contents, .. } => {
        assert_eq!(contents.len(), 3);
        assert!(contents.iter().any(|f| f.path.ends_with("manifest.ttl")));
        assert!(contents.iter().any(|f| f.path.ends_with("Calf.so")));
      }
      other => panic!("expected a bundle entry, got {other:?}"),
    }
  }

  #[test]
  fn placing_a_bundle_records_every_file_inside_it() {
    let fixture = Fixture::new();
    let source = vst3_bundle(&fixture.extract, "Dexed");
    let destination = fixture
      .layout
      .plugin_root(&Format::Vst3)
      .unwrap()
      .join("Dexed.vst3");
    let item = PlannedItem {
      format: Format::Vst3,
      source,
      destination: destination.clone(),
      kind: EntryKind::Bundle,
    };

    let mut transaction = InstallTransaction::begin(&fixture.layout, &package_id("dexed")).unwrap();
    let entry = transaction.place(&item).unwrap();
    transaction.commit().unwrap();

    match &entry {
      InstalledEntry::Bundle { contents, .. } => {
        // Recording contents, not just the root, is what makes verify
        // and exact removal possible.
        assert_eq!(contents.len(), 2);
        assert!(contents.iter().any(|f| f.path.ends_with("Dexed.so")));
        assert!(contents.iter().any(|f| f.path.ends_with("moduleinfo.json")));
      }
      other => panic!("expected a bundle entry, got {other:?}"),
    }
    assert_eq!(verify_entry(&entry).unwrap(), EntryStatus::Intact);
  }

  #[test]
  fn a_failure_part_way_leaves_nothing_behind() {
    let fixture = Fixture::new();
    let good = fixture.with_clap("Good");
    // A second item that will fail validation: not an ELF binary.
    let bad = fixture.extract.join("Bad.clap");
    std::fs::write(&bad, b"not a plugin").unwrap();

    let clap_root = fixture
      .layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .to_path_buf();
    let items = vec![
      PlannedItem {
        format: Format::Clap,
        source: good,
        destination: clap_root.join("Good.clap"),
        kind: EntryKind::File,
      },
      PlannedItem {
        format: Format::Clap,
        source: bad,
        destination: clap_root.join("Bad.clap"),
        kind: EntryKind::File,
      },
    ];

    let result = (|| -> Result<()> {
      let mut transaction = InstallTransaction::begin(&fixture.layout, &package_id("mixed"))?;
      for item in &items {
        transaction.place(item)?;
      }
      transaction.commit()
    })();

    assert!(
      result.is_err(),
      "the second item should have failed validation"
    );
    // The first item was rolled back by the Drop guard.
    assert!(
      !clap_root.join("Good.clap").exists(),
      "a partial install was left behind"
    );
    assert!(!clap_root.join("Bad.clap").exists());
    let leftovers: Vec<_> = std::fs::read_dir(&clap_root)
      .map(|d| d.flatten().map(|e| e.file_name()).collect())
      .unwrap_or_default();
    assert!(
      leftovers.is_empty(),
      "staging files left behind: {leftovers:?}"
    );
  }

  #[test]
  fn upgrading_replaces_our_own_file_and_drops_the_backup() {
    let fixture = Fixture::new();
    let clap_root = fixture
      .layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .to_path_buf();
    std::fs::create_dir_all(&clap_root).unwrap();
    let destination = clap_root.join("Surge XT.clap");
    std::fs::write(&destination, b"old version").unwrap();

    let mut new_bytes = elf_shared_object();
    new_bytes.extend_from_slice(b"new version");
    let source = fixture.extract.join("Surge XT.clap");
    std::fs::write(&source, &new_bytes).unwrap();

    let item = PlannedItem {
      format: Format::Clap,
      source,
      destination: destination.clone(),
      kind: EntryKind::File,
    };
    let mut transaction =
      InstallTransaction::begin(&fixture.layout, &package_id("surge-xt")).unwrap();
    transaction.place(&item).unwrap();
    transaction.commit().unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), new_bytes);
    let leftovers: Vec<String> = std::fs::read_dir(&clap_root)
      .unwrap()
      .flatten()
      .map(|e| e.file_name().to_string_lossy().into_owned())
      .filter(|n| n.starts_with(".luthier-"))
      .collect();
    assert!(leftovers.is_empty(), "backup left behind: {leftovers:?}");
  }

  #[test]
  fn a_failed_upgrade_restores_the_previous_version() {
    let fixture = Fixture::new();
    let clap_root = fixture
      .layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .to_path_buf();
    std::fs::create_dir_all(&clap_root).unwrap();
    let destination = clap_root.join("Plugin.clap");
    std::fs::write(&destination, b"WORKING VERSION").unwrap();

    // The replacement is invalid, so validation fails after staging.
    let source = fixture.extract.join("Plugin.clap");
    std::fs::write(&source, b"corrupt").unwrap();

    let item = PlannedItem {
      format: Format::Clap,
      source,
      destination: destination.clone(),
      kind: EntryKind::File,
    };
    {
      let mut transaction =
        InstallTransaction::begin(&fixture.layout, &package_id("plugin")).unwrap();
      assert!(transaction.place(&item).is_err());
    }
    assert_eq!(
      std::fs::read(&destination).unwrap(),
      b"WORKING VERSION",
      "the previous install must survive a failed upgrade"
    );
  }

  #[test]
  fn an_interrupted_transaction_is_rolled_back_on_the_next_run() {
    let fixture = Fixture::new();
    let clap_root = fixture
      .layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .to_path_buf();
    std::fs::create_dir_all(&clap_root).unwrap();
    let source = fixture.with_clap("Ghost");
    let destination = clap_root.join("Ghost.clap");

    // Simulate a process killed mid-install: place the item, then leak the
    // transaction so its Drop guard never runs.
    let mut transaction = InstallTransaction::begin(&fixture.layout, &package_id("ghost")).unwrap();
    transaction
      .place(&PlannedItem {
        format: Format::Clap,
        source,
        destination: destination.clone(),
        kind: EntryKind::File,
      })
      .unwrap();
    std::mem::forget(transaction);

    assert!(destination.exists(), "precondition: the file was placed");

    let recovered = recover_interrupted(&fixture.layout).unwrap();
    assert_eq!(recovered.len(), 1);
    assert!(
      !destination.exists(),
      "the interrupted install should be undone"
    );
    assert!(
      std::fs::read_dir(fixture.layout.transactions_dir())
        .unwrap()
        .flatten()
        .next()
        .is_none()
    );
  }

  #[test]
  fn verify_notices_a_modified_file() {
    let fixture = Fixture::new();
    let source = fixture.with_clap("Plugin");
    let destination = fixture
      .layout
      .plugin_root(&Format::Clap)
      .unwrap()
      .join("Plugin.clap");
    let mut transaction =
      InstallTransaction::begin(&fixture.layout, &package_id("plugin")).unwrap();
    let entry = transaction
      .place(&PlannedItem {
        format: Format::Clap,
        source,
        destination: destination.clone(),
        kind: EntryKind::File,
      })
      .unwrap();
    transaction.commit().unwrap();

    assert_eq!(verify_entry(&entry).unwrap(), EntryStatus::Intact);

    std::fs::write(&destination, b"tampered").unwrap();
    assert!(matches!(
      verify_entry(&entry).unwrap(),
      EntryStatus::Modified { .. }
    ));

    std::fs::remove_file(&destination).unwrap();
    assert_eq!(verify_entry(&entry).unwrap(), EntryStatus::Missing);
  }

  #[test]
  fn verify_notices_a_file_added_to_a_bundle() {
    let fixture = Fixture::new();
    let source = vst3_bundle(&fixture.extract, "Bundle");
    let destination = fixture
      .layout
      .plugin_root(&Format::Vst3)
      .unwrap()
      .join("Bundle.vst3");
    let mut transaction =
      InstallTransaction::begin(&fixture.layout, &package_id("bundle")).unwrap();
    let entry = transaction
      .place(&PlannedItem {
        format: Format::Vst3,
        source,
        destination: destination.clone(),
        kind: EntryKind::Bundle,
      })
      .unwrap();
    transaction.commit().unwrap();

    std::fs::write(destination.join("Contents/extra.txt"), b"snuck in").unwrap();
    match verify_entry(&entry).unwrap() {
      EntryStatus::Modified { detail } => assert!(detail.contains("was added"), "{detail}"),
      other => panic!("expected a modification, got {other:?}"),
    }
  }

  /// A VST3 bundle installed into the fixture's layout, and its entry.
  fn installed_bundle(fixture: &Fixture) -> (PathBuf, InstalledEntry) {
    let source = vst3_bundle(&fixture.extract, "Bundle");
    let destination = fixture
      .layout
      .plugin_root(&Format::Vst3)
      .unwrap()
      .join("Bundle.vst3");
    let mut transaction =
      InstallTransaction::begin(&fixture.layout, &package_id("bundle")).unwrap();
    let entry = transaction
      .place(&PlannedItem {
        format: Format::Vst3,
        source,
        destination: destination.clone(),
        kind: EntryKind::Bundle,
      })
      .unwrap();
    transaction.commit().unwrap();
    (destination, entry)
  }

  #[test]
  fn removing_an_untouched_bundle_leaves_nothing() {
    let fixture = Fixture::new();
    let (bundle, entry) = installed_bundle(&fixture);
    assert!(remove_entry(&entry).unwrap().is_empty());
    assert!(!bundle.exists());
  }

  #[test]
  fn removal_keeps_only_the_edited_file_of_a_bundle() {
    // Keeping the whole bundle left a plugin a host still loads, owned by
    // nothing, because the package was already gone from state.
    let fixture = Fixture::new();
    let (bundle, entry) = installed_bundle(&fixture);
    let edited = bundle.join("Contents/Resources/moduleinfo.json");
    std::fs::write(&edited, b"{\"mine\": true}").unwrap();

    let kept = remove_entry(&entry).unwrap();

    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].path, edited);
    assert_eq!(std::fs::read(&edited).unwrap(), b"{\"mine\": true}");
    assert_eq!(
      remaining_entries(&bundle).unwrap(),
      vec![PathBuf::from("Contents/Resources/moduleinfo.json")],
      "the plugin binary and its emptied directory should be gone"
    );
  }

  #[test]
  fn removal_keeps_a_file_added_to_a_bundle() {
    let fixture = Fixture::new();
    let (bundle, entry) = installed_bundle(&fixture);
    let added = bundle.join("Contents/Resources/notes.txt");
    std::fs::write(&added, b"mine").unwrap();

    let kept = remove_entry(&entry).unwrap();

    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].path, added);
    assert!(kept[0].reason.contains("added"), "{}", kept[0].reason);
    assert_eq!(std::fs::read(&added).unwrap(), b"mine");
  }

  #[test]
  fn removal_does_not_follow_a_directory_replaced_by_a_link() {
    // `remove_dir_all` never followed links; deleting file by file must not
    // start. The files behind the link have the recorded hashes, which is
    // exactly the case a hash check cannot catch.
    let fixture = Fixture::new();
    let (bundle, entry) = installed_bundle(&fixture);
    let outside = fixture.extract.join("elsewhere");
    std::fs::rename(bundle.join("Contents/Resources"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, bundle.join("Contents/Resources")).unwrap();

    let kept = remove_entry(&entry).unwrap();

    assert!(
      outside.join("moduleinfo.json").is_file(),
      "followed the link"
    );
    assert!(
      kept
        .iter()
        .any(|k| k.path == bundle.join("Contents/Resources")),
      "{kept:?}"
    );
  }

  #[test]
  fn a_recorded_path_that_climbs_out_of_its_bundle_is_refused() {
    let fixture = Fixture::new();
    let (bundle, entry) = installed_bundle(&fixture);
    let victim = fixture.extract.join("victim");
    std::fs::write(&victim, b"keep me").unwrap();
    let InstalledEntry::Bundle { path, mut contents } = entry else {
      unreachable!()
    };
    contents.push(BundleFile {
      path: "../../extract/victim".into(),
      sha256: fsutil::hash_file(&victim).unwrap(),
      mode: 0o644,
    });

    assert!(remove_entry(&InstalledEntry::Bundle { path, contents }).is_err());
    assert!(victim.is_file());
    assert!(bundle.exists());
  }

  #[test]
  fn a_dir_entry_is_removed_only_when_empty() {
    let fixture = Fixture::new();
    let dir = fixture.layout.library_root().join("made");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("song.wav"), b"mine").unwrap();
    let entry = InstalledEntry::Dir { path: dir.clone() };

    let kept = remove_entry(&entry).unwrap();
    assert_eq!(kept.len(), 1);
    assert!(dir.join("song.wav").is_file());

    std::fs::remove_file(dir.join("song.wav")).unwrap();
    assert!(remove_entry(&entry).unwrap().is_empty());
    assert!(!dir.exists());
  }

  #[test]
  fn plain_relative_paths() {
    assert!(is_plain_relative("Contents/x86_64-linux/a.so"));
    assert!(!is_plain_relative(""));
    assert!(!is_plain_relative("/etc/passwd"));
    assert!(!is_plain_relative("a/../../b"));
  }
}
