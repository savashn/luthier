//! Finding the manifests in a registry tree.
//!
//! This lives here rather than in `luthier-core` for one reason: the registry's
//! CI validates a pull request on every push, and validation should not need an
//! async runtime, an HTTP stack or an archive decoder to list some files. Every
//! consumer that walks a registry — the manager's index builder and the
//! registry tool's validator alike — shares this one implementation, so the
//! rules below cannot drift apart between them.
//!
//! It is deliberately *discovery*, not filesystem policy: nothing here decides
//! where anything is installed or what the manager owns. That stays in
//! `luthier-core::layout`.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{operation} failed for {path}: {source}")]
pub struct DiscoveryError {
  pub operation: &'static str,
  pub path: PathBuf,
  #[source]
  pub source: std::io::Error,
}

/// Every `.toml` file under `root`, sorted, skipping hidden and non-manifest
/// directories.
///
/// Sorting is not cosmetic: it makes an index deterministic regardless of the
/// order the filesystem hands entries back, which is what lets two runs on two
/// machines produce the same result.
pub fn manifest_files(root: &Path) -> Result<Vec<PathBuf>, DiscoveryError> {
  fn io(operation: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> DiscoveryError {
    let path = path.to_path_buf();
    move |source| DiscoveryError {
      operation,
      path,
      source,
    }
  }

  fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), DiscoveryError> {
    let entries = std::fs::read_dir(dir).map_err(io("list", dir))?;
    for entry in entries {
      let entry = entry.map_err(io("list", dir))?;
      let path = entry.path();
      let name = entry.file_name().to_string_lossy().into_owned();
      if name.starts_with('.') {
        continue;
      }
      let meta = std::fs::symlink_metadata(&path).map_err(io("inspect", &path))?;
      if meta.file_type().is_symlink() {
        // A registry snapshot is untrusted input like any other.
        continue;
      }
      if meta.is_dir() {
        // `schemas/` holds the JSON Schema, not manifests.
        if name == "schemas" {
          continue;
        }
        walk(&path, out)?;
      } else if name == crate::engines::ENGINES_FILE {
        // Registry-level data about packages, not a package.
        continue;
      } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        out.push(path);
      }
    }
    Ok(())
  }

  let mut out = Vec::new();
  walk(root, &mut out)?;
  out.sort();
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn touch(path: PathBuf) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "schema = 1\n").unwrap();
  }

  #[test]
  fn walks_the_whole_tree_and_sorts() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path().join("plugins/surge-xt.toml"));
    touch(dir.path().join("libraries/vsco2.toml"));
    touch(dir.path().join("packs/foss-studio.toml"));

    let found = manifest_files(dir.path()).unwrap();
    assert_eq!(found.len(), 3);
    let mut sorted = found.clone();
    sorted.sort();
    assert_eq!(found, sorted, "order must not depend on the filesystem");
  }

  #[test]
  fn skips_the_schema_directory_and_hidden_files() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path().join("plugins/dexed.toml"));
    touch(dir.path().join("schemas/package-v1.toml"));
    touch(dir.path().join(".git/config.toml"));
    touch(dir.path().join(".hidden.toml"));

    let found = manifest_files(dir.path()).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("dexed.toml"));
  }

  #[test]
  fn ignores_files_that_are_not_toml() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path().join("plugins/dexed.toml"));
    std::fs::write(dir.path().join("README.md"), "# hi").unwrap();
    std::fs::write(dir.path().join("plugins/notes.txt"), "x").unwrap();

    assert_eq!(manifest_files(dir.path()).unwrap().len(), 1);
  }

  #[test]
  fn a_symlink_is_skipped_because_a_snapshot_is_untrusted() {
    // A downloaded registry snapshot is attacker-controlled input; a link
    // in it must not make the walker read outside the tree.
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path().join("plugins/dexed.toml"));
    std::os::unix::fs::symlink("/etc", dir.path().join("plugins/escape")).unwrap();
    std::os::unix::fs::symlink(
      dir.path().join("plugins/dexed.toml"),
      dir.path().join("plugins/alias.toml"),
    )
    .unwrap();

    let found = manifest_files(dir.path()).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with("dexed.toml"));
  }

  #[test]
  fn a_missing_root_names_the_path() {
    let err = manifest_files(Path::new("/nonexistent-luthier-extras")).unwrap_err();
    assert!(err.to_string().contains("nonexistent-luthier-extras"));
  }
}
