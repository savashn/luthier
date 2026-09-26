//! Environments: named, self-contained sets of installed software.
//!
//! An environment redirects the per-installation parts of a [`Layout`] —
//! plugin roots, state, sample libraries — under
//! `~/.local/share/luthier/envs/<name>`. Registry snapshots and the
//! artifact cache stay shared, so a second environment installing the same
//! plugin re-extracts from cache rather than downloading again.
//!
//! Selection is by environment variable or flag, never by a "current
//! environment" file. A stateful pointer would mean one terminal could change
//! what another terminal is about to install into. `LUTHIER_ENV` is what
//! [`Activation`] exports, and it behaves the way a virtualenv does.

use crate::error::{Error, Result};
use crate::layout::Layout;
use luthier_manifest::Format;
use std::path::{Path, PathBuf};

/// The variable naming the active environment.
pub const ENV_VAR: &str = "LUTHIER_ENV";

/// A validated environment name.
///
/// This becomes a single path segment under the data directory, so it is
/// checked rather than trusted: `--env ../../etc` must not be able to point
/// the installer outside the layout.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvName(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvNameError {
  #[error("an environment name cannot be empty")]
  Empty,
  #[error("an environment name cannot be longer than {max} characters")]
  TooLong { max: usize },
  #[error("{0:?} is not a valid environment name: use letters, digits, '.', '-' and '_'")]
  InvalidCharacters(String),
  #[error("{0:?} is not a valid environment name: it must not start with a dot")]
  LeadingDot(String),
}

impl EnvName {
  const MAX: usize = 64;

  pub fn new(raw: impl Into<String>) -> std::result::Result<Self, EnvNameError> {
    let raw = raw.into();
    if raw.is_empty() {
      return Err(EnvNameError::Empty);
    }
    if raw.len() > Self::MAX {
      return Err(EnvNameError::TooLong { max: Self::MAX });
    }
    // Rejecting a leading dot also disposes of "." and "..", and keeps
    // environments from hiding themselves in a listing.
    if raw.starts_with('.') {
      return Err(EnvNameError::LeadingDot(raw));
    }
    if !raw
      .chars()
      .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
      return Err(EnvNameError::InvalidCharacters(raw));
    }
    Ok(Self(raw))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }
}

impl std::fmt::Display for EnvName {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::str::FromStr for EnvName {
  type Err = EnvNameError;
  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
    Self::new(s)
  }
}

/// One environment as reported by [`list`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EnvSummary {
  pub name: String,
  pub path: PathBuf,
  /// Packages recorded as installed in it.
  pub packages: usize,
  /// Whether this is the environment the current invocation is using.
  pub active: bool,
}

/// Creates the environment's directories.
///
/// Creating is deliberately explicit: `install --env typo` should say the
/// environment does not exist rather than silently make one.
pub fn create(layout: &Layout, name: &EnvName) -> Result<PathBuf> {
  let env_layout = layout.clone().into_env(name);
  let root = layout.envs_dir().join(name.as_str());
  if root.exists() {
    return Err(Error::InvalidArgument(format!(
      "environment {name} already exists at {}",
      root.display()
    )));
  }
  crate::fsutil::ensure_dir(&env_layout.state_dir())?;
  crate::fsutil::ensure_dir(env_layout.library_root())?;
  for (_, plugin_root) in env_layout.plugin_roots() {
    crate::fsutil::ensure_dir(plugin_root)?;
  }
  Ok(root)
}

/// Whether `name` has been created.
pub fn exists(layout: &Layout, name: &EnvName) -> bool {
  layout.envs_dir().join(name.as_str()).is_dir()
}

/// Every environment, alphabetically.
///
/// A directory whose name is not a valid environment name is skipped rather
/// than reported: it was not put there by this manager.
pub fn list(layout: &Layout, active: Option<&EnvName>) -> Result<Vec<EnvSummary>> {
  let dir = layout.envs_dir();
  if !dir.is_dir() {
    return Ok(Vec::new());
  }

  let mut found = Vec::new();
  let entries = std::fs::read_dir(&dir).map_err(|e| Error::io("list", &dir, e))?;
  for entry in entries {
    let entry = entry.map_err(|e| Error::io("list", &dir, e))?;
    if !entry.path().is_dir() {
      continue;
    }
    let Ok(name) = EnvName::new(entry.file_name().to_string_lossy().into_owned()) else {
      continue;
    };
    let env_layout = layout.clone().into_env(&name);
    // A state file that cannot be read is reported as zero rather than
    // failing the whole listing; `list` is diagnostic.
    let packages = crate::state::load(&env_layout)
      .map(|state| state.packages.len())
      .unwrap_or(0);
    found.push(EnvSummary {
      path: entry.path(),
      active: active == Some(&name),
      name: name.to_string(),
      packages,
    });
  }
  found.sort_by(|a, b| a.name.cmp(&b.name));
  Ok(found)
}

/// Deletes an environment and everything installed in it.
pub fn remove(layout: &Layout, name: &EnvName) -> Result<PathBuf> {
  let root = layout.envs_dir().join(name.as_str());
  if !root.is_dir() {
    return Err(Error::InvalidArgument(format!(
      "no environment named {name}"
    )));
  }
  // The name is validated, but the join is checked anyway: this is a
  // recursive delete.
  if !root.starts_with(layout.envs_dir()) || root == layout.envs_dir() {
    return Err(Error::InvalidArgument(format!(
      "refusing to remove {}",
      root.display()
    )));
  }
  std::fs::remove_dir_all(&root).map_err(|e| Error::io("remove", &root, e))?;
  Ok(root)
}

/// Shell variables that put an environment on a host's search path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Activation {
  /// Variables to set, in a stable order.
  pub set: Vec<(String, String)>,
  /// Variables to unset, for deactivation.
  pub unset: Vec<String>,
}

/// What to export so hosts started from this shell see `name`.
///
/// The three plugin variables do not mean the same thing, and this is where
/// that matters most:
///
/// * `LV2_PATH` **replaces** the default search path, so everything that
///   should stay visible has to be listed — the environment's own bundles and
///   the system directories both.
/// * `CLAP_PATH` and `VST3_PATH` **extend** the standard locations, so naming
///   the environment root is enough and the rest still applies.
///
/// A consequence worth knowing: an environment can fully replace what a host
/// sees for LV2, but for CLAP and VST3 it can only add to it. `~/.clap` stays
/// visible because those conventions provide no way to switch it off.
pub fn activation(layout: &Layout, name: &EnvName) -> Activation {
  // Computed from the conventional locations rather than the current
  // variables, so activating a second environment replaces the first
  // instead of appending to it.
  let conventional = Layout::system_roots_from(|_| None);
  let mut set = vec![(ENV_VAR.to_string(), name.to_string())];

  for format in [Format::Lv2, Format::Clap, Format::Vst3] {
    let Some(root) = layout.plugin_root(&format) else {
      continue;
    };
    let mut paths = vec![root.to_path_buf()];
    if format == Format::Lv2 {
      paths.extend(conventional.get(&format).cloned().unwrap_or_default());
    }
    set.push((search_path_var(&format).to_string(), join_paths(&paths)));
  }

  Activation {
    unset: set.iter().map(|(k, _)| k.clone()).collect(),
    set,
  }
}

/// What to export so hosts see plugins installed under a location the user
/// chose, which no host searches on its own.
///
/// `home` is the layout before any location is applied: its plugin roots are
/// the conventional `~/.clap`, `~/.vst3` and `~/.lv2`. Only LV2 needs it,
/// because `LV2_PATH` replaces the default search path — setting it to the
/// new root alone would hide every bundle in `~/.lv2` and `/usr/lib/lv2`.
/// `CLAP_PATH` and `VST3_PATH` add to the conventional locations, so the new
/// root is all they need.
///
/// Empty when plugins are where hosts already look. Nothing here names an
/// environment: this is the default one, which needs no `LUTHIER_ENV`.
pub fn relocated_search_path(home: &Layout, located: &Layout) -> Activation {
  let conventional = Layout::system_roots_from(|_| None);
  let mut set = Vec::new();

  for format in [Format::Lv2, Format::Clap, Format::Vst3] {
    let Some(root) = located.plugin_root(&format) else {
      continue;
    };
    let usual = home.plugin_root(&format);
    if usual == Some(root) {
      continue;
    }
    let mut paths = vec![root.to_path_buf()];
    if format == Format::Lv2 {
      paths.extend(usual.map(Path::to_path_buf));
      paths.extend(conventional.get(&format).cloned().unwrap_or_default());
    }
    set.push((search_path_var(&format).to_string(), join_paths(&paths)));
  }

  Activation {
    unset: set.iter().map(|(k, _)| k.clone()).collect(),
    set,
  }
}

/// The variables activation touches, and therefore the ones deactivation
/// clears. Fixed, so undoing an activation needs no environment name.
pub fn deactivation() -> Activation {
  Activation {
    set: Vec::new(),
    unset: [ENV_VAR, "LV2_PATH", "CLAP_PATH", "VST3_PATH"]
      .iter()
      .map(|s| (*s).to_string())
      .collect(),
  }
}

/// The search-path variable a format uses.
fn search_path_var(format: &Format) -> &'static str {
  match format {
    Format::Clap => "CLAP_PATH",
    Format::Vst3 => "VST3_PATH",
    _ => "LV2_PATH",
  }
}

fn join_paths(paths: &[PathBuf]) -> String {
  paths
    .iter()
    .map(|p| p.display().to_string())
    .collect::<Vec<_>>()
    .join(":")
}

/// Where an environment's prefix is, without creating it.
pub fn path(layout: &Layout, name: &EnvName) -> PathBuf {
  layout.envs_dir().join(name.as_str())
}

/// Reads the environment named by [`ENV_VAR`], if any.
pub fn from_environment() -> Option<std::result::Result<EnvName, EnvNameError>> {
  match std::env::var(ENV_VAR) {
    Ok(value) if !value.is_empty() => Some(EnvName::new(value)),
    _ => None,
  }
}

/// Whether `path` is inside `root`. Used by tests and callers checking
/// confinement.
pub fn is_within(path: &Path, root: &Path) -> bool {
  path.starts_with(root)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn layout(dir: &Path) -> Layout {
    Layout::rooted_at(dir)
  }

  #[test]
  fn an_environment_name_cannot_escape_the_data_directory() {
    // The name becomes a path segment, so this is the check that stops
    // `--env ../../etc` pointing the installer outside the layout.
    for hostile in [
      "..",
      ".",
      "../etc",
      "a/b",
      "a\\b",
      "/absolute",
      ".hidden",
      "with space",
      "semi;colon",
      "null\0byte",
    ] {
      assert!(
        EnvName::new(hostile).is_err(),
        "{hostile:?} was accepted as an environment name"
      );
    }
  }

  #[test]
  fn ordinary_environment_names_are_accepted() {
    for good in ["mixing", "band-2026", "test_env", "v1.2", "A1"] {
      assert!(EnvName::new(good).is_ok(), "{good:?} was rejected");
    }
    assert!(EnvName::new("").is_err());
    assert!(EnvName::new("x".repeat(65)).is_err());
  }

  #[test]
  fn an_environment_confines_state_and_plugins_but_shares_the_cache() {
    // Sharing the cache is the point: a second environment installing the
    // same plugin re-extracts rather than downloading again.
    let dir = tempfile::tempdir().unwrap();
    let base = layout(dir.path());
    let name = EnvName::new("mixing").unwrap();
    let env = base.clone().into_env(&name);

    let root = base.envs_dir().join("mixing");
    assert!(env.state_dir().starts_with(&root));
    assert!(env.library_root().starts_with(&root));
    for (_, plugin_root) in env.plugin_roots() {
      assert!(plugin_root.starts_with(&root), "{plugin_root:?} escaped");
    }

    assert_eq!(env.cache_dir(), base.cache_dir());
    assert_eq!(env.config_dir(), base.config_dir());
    assert_eq!(env.registry_dir("default"), base.registry_dir("default"));
  }

  #[test]
  fn two_environments_do_not_share_state_or_plugin_roots() {
    let dir = tempfile::tempdir().unwrap();
    let base = layout(dir.path());
    let a = base.clone().into_env(&EnvName::new("a").unwrap());
    let b = base.into_env(&EnvName::new("b").unwrap());
    assert_ne!(a.state_dir(), b.state_dir());
    assert_ne!(a.library_root(), b.library_root());
    assert_ne!(
      a.plugin_root(&Format::Clap).unwrap(),
      b.plugin_root(&Format::Clap).unwrap()
    );
  }

  #[test]
  fn create_then_list_then_remove() {
    let dir = tempfile::tempdir().unwrap();
    let base = layout(dir.path());
    let name = EnvName::new("mixing").unwrap();

    assert!(list(&base, None).unwrap().is_empty());
    assert!(!exists(&base, &name));

    create(&base, &name).unwrap();
    assert!(exists(&base, &name));

    let listed = list(&base, Some(&name)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "mixing");
    assert_eq!(listed[0].packages, 0);
    assert!(listed[0].active);

    // Creating twice is an error rather than a silent no-op.
    assert!(create(&base, &name).is_err());

    remove(&base, &name).unwrap();
    assert!(!exists(&base, &name));
    assert!(remove(&base, &name).is_err());
  }

  #[test]
  fn listing_ignores_directories_that_are_not_environments() {
    let dir = tempfile::tempdir().unwrap();
    let base = layout(dir.path());
    std::fs::create_dir_all(base.envs_dir().join(".scratch")).unwrap();
    std::fs::create_dir_all(base.envs_dir().join("real")).unwrap();
    std::fs::write(base.envs_dir().join("a-file"), b"x").unwrap();

    let listed = list(&base, None).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "real");
  }

  #[test]
  fn activation_lists_the_system_path_for_lv2_but_not_for_clap_or_vst3() {
    // LV2_PATH replaces the default search path, so anything that should
    // stay visible has to be named. CLAP_PATH and VST3_PATH extend it, so
    // naming the environment root is enough.
    let dir = tempfile::tempdir().unwrap();
    let name = EnvName::new("mixing").unwrap();
    let env = layout(dir.path()).into_env(&name);
    let act = activation(&env, &name);

    let value = |key: &str| {
      act
        .set
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("{key} not exported"))
    };

    assert_eq!(value(ENV_VAR), "mixing");

    let lv2 = value("LV2_PATH");
    assert!(lv2.starts_with(env.plugin_root(&Format::Lv2).unwrap().to_str().unwrap()));
    assert!(lv2.contains("/usr/lib/lv2"), "{lv2}");

    let clap = value("CLAP_PATH");
    assert_eq!(
      clap,
      env.plugin_root(&Format::Clap).unwrap().to_str().unwrap()
    );
    assert!(!clap.contains("/usr/lib/clap"), "{clap}");
  }

  #[test]
  fn activation_is_idempotent_across_environments() {
    // Activation is computed from the conventional locations, not from
    // whatever the variables currently hold, so activating b after a does
    // not leave a's bundles on the path. Simulated by giving the layout
    // the system roots a's own activation would have produced.
    use std::collections::BTreeMap;
    let dir = tempfile::tempdir().unwrap();
    let base = layout(dir.path());
    let a = EnvName::new("a").unwrap();
    let b = EnvName::new("b").unwrap();

    let a_lv2 = base
      .clone()
      .into_env(&a)
      .plugin_root(&Format::Lv2)
      .unwrap()
      .to_path_buf();

    let after_a = base
      .clone()
      .with_system_roots(BTreeMap::from([(
        Format::Lv2,
        vec![a_lv2.clone(), PathBuf::from("/usr/lib/lv2")],
      )]))
      .into_env(&b);

    let act = activation(&after_a, &b);
    let lv2 = act
      .set
      .iter()
      .find(|(k, _)| k == "LV2_PATH")
      .map(|(_, v)| v.clone())
      .unwrap();

    assert!(
      !lv2.contains(a_lv2.to_str().unwrap()),
      "a's root survived into b's activation: {lv2}"
    );
    assert!(lv2.contains("/envs/b/"), "{lv2}");
    assert!(lv2.contains("/usr/lib/lv2"), "{lv2}");
  }

  #[test]
  fn deactivation_unsets_everything_activation_sets() {
    // The two are written separately -- deactivation needs no environment
    // name -- so a variable added to one and not the other would leak past
    // `env deactivate`.
    let dir = tempfile::tempdir().unwrap();
    let name = EnvName::new("mixing").unwrap();
    let act = activation(&layout(dir.path()).into_env(&name), &name);

    let mut activated: Vec<String> = act.set.iter().map(|(k, _)| k.clone()).collect();
    let mut cleared = deactivation().unset;
    activated.sort();
    cleared.sort();
    assert_eq!(activated, cleared);
    assert!(cleared.contains(&ENV_VAR.to_string()));
    assert!(deactivation().set.is_empty());
  }
}
