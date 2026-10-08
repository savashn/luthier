//! `extras`: the manifests Luthier carries in the binary.
//!
//! They are what the Open Audio Stack registry cannot say — an engine to
//! install from the distribution, content it does not carry, a correction to
//! one of its packages — kept in `bench/` in this repository and built in by
//! `build.rs`. Built in rather than fetched: they change only when Luthier is
//! released, so a binary reads exactly the ones it was released with, never a
//! later set written for a manifest format it does not know, and nothing has
//! to be downloaded before the first command can find them.
//!
//! Releases still publish them as `bench.tar.gz`, for the 0.2–0.4 clients
//! that fetch it.

use super::{RefreshOutcome, RegistryIndex, RegistryProvider, index_of};
use crate::error::Result;
use luthier_manifest::ParseMode;
use std::path::{Path, PathBuf};

mod generated {
  include!(concat!(env!("OUT_DIR"), "/extras.rs"));
}

/// The name the built-in manifests go by.
pub const NAME: &str = "extras";

pub struct BuiltinRegistry {
  name: String,
}

impl BuiltinRegistry {
  pub fn new(name: impl Into<String>) -> Self {
    Self { name: name.into() }
  }
}

#[async_trait::async_trait]
impl RegistryProvider for BuiltinRegistry {
  fn name(&self) -> &str {
    &self.name
  }

  /// Nothing to fetch: it came with the binary.
  async fn refresh(&self) -> Result<RefreshOutcome> {
    Ok(RefreshOutcome {
      registry: self.name.clone(),
      packages: self.load_index()?.len(),
      updated: false,
      failure: None,
    })
  }

  fn is_built_in(&self) -> bool {
    true
  }

  fn load_index(&self) -> Result<RegistryIndex> {
    index_of(
      &self.name,
      Path::new(""),
      generated::MANIFESTS
        .iter()
        .map(|(path, text)| (PathBuf::from(path), text.as_bytes())),
      generated::ENGINES,
      ParseMode::Lenient,
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_manifest_in_bench_is_built_in() {
    let bench = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../../bench")
      .canonicalize()
      .unwrap();
    let on_disk: Vec<PathBuf> = luthier_manifest::manifest_files(&bench)
      .unwrap()
      .into_iter()
      .map(|path| path.strip_prefix(&bench).unwrap().to_path_buf())
      .collect();
    let built_in: Vec<PathBuf> = generated::MANIFESTS
      .iter()
      .map(|(path, _)| PathBuf::from(path))
      .collect();
    assert_eq!(built_in, on_disk, "the same files, in the same order");
    for (path, text) in generated::MANIFESTS {
      assert_eq!(
        std::fs::read_to_string(bench.join(path)).unwrap(),
        *text,
        "{path}"
      );
    }
    assert_eq!(
      generated::ENGINES.is_some(),
      bench.join(luthier_manifest::ENGINES_FILE).is_file()
    );

    let index = BuiltinRegistry::new(NAME).load_index().unwrap();
    assert_eq!(index.len(), on_disk.len());
    assert!(!index.is_empty());
    // Read strictly too: what ships in the binary is held to what the
    // bench's own CI holds it to.
    index_of(
      NAME,
      Path::new(""),
      generated::MANIFESTS
        .iter()
        .map(|(path, text)| (PathBuf::from(path), text.as_bytes())),
      generated::ENGINES,
      ParseMode::Strict,
    )
    .unwrap();
    for entry in index.packages.values() {
      assert_eq!(entry.registry, NAME);
    }
  }
}
