//! A registry that is simply a directory on disk.
//!
//! This is what the MVP uses for development and for the whole test suite: the
//! registry repository is a git checkout, and `luthier --registry-path` points at
//! it. Because the index builder is shared, a local checkout and a fetched
//! snapshot are read by exactly the same code.

use super::{RefreshOutcome, RegistryIndex, RegistryProvider, build_index};
use crate::error::Result;
use luthier_manifest::ParseMode;
use std::path::{Path, PathBuf};

pub struct LocalRegistry {
  name: String,
  root: PathBuf,
  mode: ParseMode,
}

impl LocalRegistry {
  pub fn new(name: impl Into<String>, root: impl Into<PathBuf>) -> Self {
    Self {
      name: name.into(),
      root: root.into(),
      mode: ParseMode::Lenient,
    }
  }

  /// Treats unrecognised fields as errors. Used by the registry validator.
  pub fn strict(mut self) -> Self {
    self.mode = ParseMode::Strict;
    self
  }

  pub fn root(&self) -> &Path {
    &self.root
  }
}

#[async_trait::async_trait]
impl RegistryProvider for LocalRegistry {
  fn name(&self) -> &str {
    &self.name
  }

  /// A local directory is whatever it currently is; there is nothing to fetch.
  async fn refresh(&self) -> Result<RefreshOutcome> {
    let index = self.load_index()?;
    Ok(RefreshOutcome {
      registry: self.name.clone(),
      packages: index.len(),
      updated: false,
    })
  }

  fn load_index(&self) -> Result<RegistryIndex> {
    build_index(&self.name, &self.root, self.mode)
  }
}
