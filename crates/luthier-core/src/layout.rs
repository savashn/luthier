//! Where everything lives on disk.
//!
//! Every path the manager touches is derived from a `Layout` value that is
//! passed in explicitly. Nothing deep in the call graph reaches for
//! `dirs::home_dir()` or reads `$HOME` on its own. That single rule is what
//! makes the test suite hermetic: a test constructs [`Layout::rooted_at`] over
//! a temporary directory and there is then no code path that could reach the
//! real `~/.clap`.

use crate::env::EnvName;
use luthier_manifest::Format;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Something a user may move to another disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocationKind {
  /// Downloaded artifacts and fetched registry snapshots.
  Cache,
  /// Sample libraries and other content.
  Libraries,
  /// Plugins, one directory per format beneath it.
  Plugins,
}

impl LocationKind {
  pub const ALL: [LocationKind; 3] = [Self::Cache, Self::Libraries, Self::Plugins];

  pub fn label(self) -> &'static str {
    match self {
      Self::Cache => "cache",
      Self::Libraries => "libraries",
      Self::Plugins => "plugins",
    }
  }

  pub fn parse(raw: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| kind.label() == raw)
  }
}

impl std::fmt::Display for LocationKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.label())
  }
}

/// Directories the user chose in place of the defaults, typically on another
/// disk. Stored in `config.json`; absent means the default.
///
/// Each is a directory the user made, and this manager never creates one. An
/// external disk that is not mounted leaves its mount point behind as an
/// ordinary empty directory — or leaves nothing, and then creating the path
/// would put gigabytes of samples on the disk the user was trying to spare.
/// So a configured location that is missing is a refusal
/// ([`Layout::unavailable_locations`]), never a `create_dir_all`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locations {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cache: Option<PathBuf>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub libraries: Option<PathBuf>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub plugins: Option<PathBuf>,
}

impl Locations {
  pub fn is_empty(&self) -> bool {
    self.configured().next().is_none()
  }

  pub fn get(&self, kind: LocationKind) -> Option<&Path> {
    match kind {
      LocationKind::Cache => self.cache.as_deref(),
      LocationKind::Libraries => self.libraries.as_deref(),
      LocationKind::Plugins => self.plugins.as_deref(),
    }
  }

  pub fn set(&mut self, kind: LocationKind, path: Option<PathBuf>) {
    let slot = match kind {
      LocationKind::Cache => &mut self.cache,
      LocationKind::Libraries => &mut self.libraries,
      LocationKind::Plugins => &mut self.plugins,
    };
    *slot = path;
  }

  /// Every location that is set, in a stable order.
  pub fn configured(&self) -> impl Iterator<Item = (LocationKind, &Path)> {
    LocationKind::ALL
      .into_iter()
      .filter_map(|kind| self.get(kind).map(|path| (kind, path)))
  }
}

/// Resolved filesystem locations for one installation.
///
/// An *environment* varies the per-installation parts — state, plugin roots,
/// sample libraries — while leaving the shared parts alone. Registry snapshots
/// and the artifact cache stay global on purpose: the cache is keyed by
/// content hash, so sharing it means a second environment installing the same
/// plugin re-extracts rather than re-downloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
  /// Managed data: registry snapshots, environments. `~/.local/share/luthier`.
  data: PathBuf,
  /// Installation state for *this* environment.
  state: PathBuf,
  /// Sample libraries and other non-plugin content for this environment.
  libraries: PathBuf,
  /// Re-fetchable data only. `~/.cache/luthier`.
  cache: PathBuf,
  /// User configuration. `~/.config/luthier`.
  config: PathBuf,
  /// Where each plugin format is installed. Keyed by format so adding one is
  /// a data change rather than a new field.
  plugin_roots: BTreeMap<Format, PathBuf>,
  /// Which environment this layout was redirected into, if any. Recorded so
  /// an export can name its source; nothing resolves a path from it.
  environment: Option<EnvName>,
  /// Read-only locations searched for software this manager does not install:
  /// distribution packages, vendor installers, or whatever a container image
  /// baked in. Never written to, never deleted from, and deliberately absent
  /// from [`Layout::is_managed_location`].
  system_roots: BTreeMap<Format, Vec<PathBuf>>,
  /// The user's choices this layout honours, kept so a command can refuse
  /// when one of them is not there. Only what still applies: an environment
  /// keeps its libraries and plugins inside itself, so redirecting into one
  /// drops those two.
  locations: Locations,
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
  #[error("cannot determine the home directory; set HOME or XDG_DATA_HOME")]
  NoHome,
}

impl Layout {
  /// The standard user-local layout.
  ///
  /// Data, cache and config honour the XDG base directory variables.
  /// Plugin directories deliberately do not: `~/.clap`, `~/.vst3` and
  /// `~/.lv2` are fixed by each format's own convention and are where every
  /// host looks, so they hang off `$HOME` directly.
  pub fn from_env() -> Result<Self, LayoutError> {
    let home = dirs::home_dir().ok_or(LayoutError::NoHome)?;
    let xdg = |var: &str, fallback: &str| -> PathBuf {
      match std::env::var_os(var) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home.join(fallback),
      }
    };
    let data = xdg("XDG_DATA_HOME", ".local/share").join("luthier");
    Ok(Self {
      state: data.join("state"),
      libraries: data.join("libraries"),
      data,
      cache: xdg("XDG_CACHE_HOME", ".cache").join("luthier"),
      config: xdg("XDG_CONFIG_HOME", ".config").join("luthier"),
      plugin_roots: Self::default_plugin_roots(&home),
      environment: None,
      system_roots: Self::default_system_roots(),
      locations: Locations::default(),
    })
  }

  /// Moves the parts `locations` names to where it says.
  ///
  /// Applied to the base layout, before any environment redirect. An
  /// environment is one directory that `env remove` deletes whole, so its
  /// libraries and plugins stay inside it; the cache is shared by every
  /// environment and moves for all of them.
  pub fn with_locations(mut self, locations: &Locations) -> Self {
    if let Some(cache) = &locations.cache {
      self.cache = cache.clone();
    }
    if let Some(libraries) = &locations.libraries {
      self.libraries = libraries.clone();
    }
    if let Some(plugins) = &locations.plugins {
      self.plugin_roots = Self::relocated_plugin_roots(plugins);
    }
    self.locations = locations.clone();
    self
  }

  /// The locations this layout honours.
  pub fn locations(&self) -> &Locations {
    &self.locations
  }

  /// Configured locations that are not a directory right now — most likely
  /// a disk that is not mounted.
  ///
  /// Anything that writes checks this first, so a missing disk costs a
  /// refusal rather than a copy of everything on the disk underneath.
  pub fn unavailable_locations(&self) -> Vec<(LocationKind, PathBuf)> {
    self
      .locations
      .configured()
      .filter(|(_, path)| !path.is_dir())
      .map(|(kind, path)| (kind, path.to_path_buf()))
      .collect()
  }

  /// The standard layout, redirected into the environment named `name`.
  ///
  /// Plugin roots, state and libraries move under the environment; the
  /// registry snapshots and the artifact cache do not.
  pub fn for_env(name: &EnvName) -> Result<Self, LayoutError> {
    let base = Self::from_env()?;
    Ok(base.into_env(name))
  }

  /// Redirects this layout into an environment. Kept separate from
  /// [`Layout::for_env`] so tests can build one over a temporary root.
  pub fn into_env(self, name: &EnvName) -> Self {
    let root = self.data.join("envs").join(name.as_str());
    Self {
      state: root.join("state"),
      libraries: root.join("libraries"),
      plugin_roots: Self::default_plugin_roots(&root),
      environment: Some(name.clone()),
      locations: Locations {
        cache: self.locations.cache.clone(),
        ..Locations::default()
      },
      ..self
    }
  }

  /// The environment this layout points into, if it is not the default one.
  pub fn environment(&self) -> Option<&EnvName> {
    self.environment.as_ref()
  }

  /// Where environments live.
  pub fn envs_dir(&self) -> PathBuf {
    self.data.join("envs")
  }

  /// A complete layout confined to `root`, for tests and `--root`.
  ///
  /// System roots are deliberately empty here. They are the one part of a
  /// layout that points outside the root, so populating them would let a
  /// test's result depend on what the machine running it happens to have
  /// installed. A caller that wants them opts in with
  /// [`Layout::with_system_roots`].
  pub fn rooted_at(root: impl AsRef<Path>) -> Self {
    let root = root.as_ref();
    let data = root.join("share/luthier");
    Self {
      state: data.join("state"),
      libraries: data.join("libraries"),
      data,
      cache: root.join("cache/luthier"),
      config: root.join("config/luthier"),
      plugin_roots: Self::default_plugin_roots(root),
      environment: None,
      system_roots: BTreeMap::new(),
      locations: Locations::default(),
    }
  }

  /// Conventional read-only locations for each format on Linux.
  ///
  /// Each format defines a search-path variable, and they do not mean the
  /// same thing, so this does not treat them the same:
  ///
  /// * `LV2_PATH` **replaces** the default path. That is what lilv does, so
  ///   a host with `LV2_PATH=/opt/lv2` genuinely cannot see `/usr/lib/lv2`,
  ///   and reporting a bundle there as present would be a lie.
  /// * `CLAP_PATH` and `VST3_PATH` **extend** the standard locations, which
  ///   is what those two conventions specify.
  ///
  /// Honouring the variables at all is what lets a container or a `--prefix`
  /// install be seen: an image that puts plugins in `/usr/lib/lv2` and
  /// exports `LV2_PATH` is describing itself, and there is no reason to
  /// ignore it.
  pub fn default_system_roots() -> BTreeMap<Format, Vec<PathBuf>> {
    Self::system_roots_from(|var| std::env::var_os(var))
  }

  /// [`Layout::default_system_roots`] with the environment injected.
  ///
  /// Tests pass a fake lookup rather than calling `set_var`, which is
  /// process-global and would race the rest of the suite.
  pub fn system_roots_from(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
  ) -> BTreeMap<Format, Vec<PathBuf>> {
    /// Whether a format's search-path variable adds to the conventional
    /// locations or stands in for them.
    enum Env {
      Extends,
      Replaces,
    }

    let multiarch = match std::env::consts::ARCH {
      "aarch64" => "aarch64-linux-gnu",
      "x86" => "i386-linux-gnu",
      _ => "x86_64-linux-gnu",
    };

    [
      (Format::Clap, "CLAP_PATH", "clap", Env::Extends),
      (Format::Vst3, "VST3_PATH", "vst3", Env::Extends),
      (Format::Lv2, "LV2_PATH", "lv2", Env::Replaces),
    ]
    .into_iter()
    .map(|(format, var, dir, semantics)| {
      // Colon-separated, like PATH. Relative and empty entries are
      // dropped rather than resolved against the current directory,
      // which is not a meaningful base for a plugin search path.
      let from_env: Vec<PathBuf> = lookup(var)
        .map(|value| {
          std::env::split_paths(&value)
            .filter(|p| p.is_absolute())
            .collect()
        })
        .unwrap_or_default();

      let conventional = [
        PathBuf::from("/usr/lib").join(dir),
        PathBuf::from("/usr/local/lib").join(dir),
        PathBuf::from("/usr/lib").join(multiarch).join(dir),
      ];

      // A variable set to nothing usable falls back to the convention:
      // an empty search path is far more likely to be a broken
      // environment than a deliberate "look nowhere".
      let roots = match semantics {
        Env::Replaces if !from_env.is_empty() => from_env,
        _ => from_env.into_iter().chain(conventional).collect(),
      };

      (format, dedup_preserving_order(roots))
    })
    .collect()
  }

  fn default_plugin_roots(home: &Path) -> BTreeMap<Format, PathBuf> {
    BTreeMap::from([
      (Format::Clap, home.join(".clap")),
      (Format::Vst3, home.join(".vst3")),
      (Format::Lv2, home.join(".lv2")),
    ])
  }

  /// Plugin roots beneath a directory the user chose.
  ///
  /// Named without the leading dot the home-directory convention uses: on a
  /// disk of its own there is nothing to hide them from, and a user looking
  /// for them there should find them.
  fn relocated_plugin_roots(base: &Path) -> BTreeMap<Format, PathBuf> {
    BTreeMap::from([
      (Format::Clap, base.join("clap")),
      (Format::Vst3, base.join("vst3")),
      (Format::Lv2, base.join("lv2")),
    ])
  }

  pub fn data_dir(&self) -> &Path {
    &self.data
  }

  pub fn cache_dir(&self) -> &Path {
    &self.cache
  }

  pub fn config_dir(&self) -> &Path {
    &self.config
  }

  pub fn config_file(&self) -> PathBuf {
    self.config.join("config.json")
  }

  /// Directory holding installation state for this environment.
  pub fn state_dir(&self) -> PathBuf {
    self.state.clone()
  }

  /// Where sample libraries and other non-plugin content are installed.
  ///
  /// Not a plugin root: no host scans it, and nothing here is a plugin. The
  /// library installer reaches it through [`FormatInstaller::root`], which is
  /// the seam that exists so a format need not live under `plugin_roots`.
  ///
  /// [`FormatInstaller::root`]: crate::install::formats::FormatInstaller::root
  pub fn library_root(&self) -> &Path {
    &self.libraries
  }

  /// The single state document.
  pub fn state_file(&self) -> PathBuf {
    self.state_dir().join("state.json")
  }

  /// Previous state, kept so a truncated write is recoverable.
  pub fn state_backup_file(&self) -> PathBuf {
    self.state_dir().join("state.json.bak")
  }

  /// Advisory lock guarding every mutating operation.
  pub fn lock_file(&self) -> PathBuf {
    self.state_dir().join("lock")
  }

  /// Scratch space for in-flight transactions. Under `data`, not `cache`,
  /// because a journal here may be the only record of a partially applied
  /// install and must survive a cache wipe.
  pub fn transactions_dir(&self) -> PathBuf {
    self.state_dir().join("tmp")
  }

  pub fn transaction_dir(&self, id: &str) -> PathBuf {
    self.transactions_dir().join(id)
  }

  /// Extracted registry snapshots, one directory per configured registry.
  pub fn registry_dir(&self, name: &str) -> PathBuf {
    self.data.join("registries").join(name)
  }

  pub fn registries_dir(&self) -> PathBuf {
    self.data.join("registries")
  }

  /// Content-addressed artifact cache. Keying by digest means a cache hit is
  /// self-verifying: the name *is* the expected hash.
  pub fn artifact_cache_dir(&self) -> PathBuf {
    self.cache.join("artifacts")
  }

  /// Install root for `format`, if this build installs it.
  pub fn plugin_root(&self, format: &Format) -> Option<&Path> {
    self.plugin_roots.get(format).map(PathBuf::as_path)
  }

  /// Every known plugin root, in a stable order.
  pub fn plugin_roots(&self) -> impl Iterator<Item = (&Format, &Path)> {
    self.plugin_roots.iter().map(|(f, p)| (f, p.as_path()))
  }

  /// Overrides one plugin root. Used by `--install-root` and by tests.
  pub fn set_plugin_root(&mut self, format: Format, path: PathBuf) {
    self.plugin_roots.insert(format, path);
  }

  /// Read-only system locations searched for `format`, in priority order.
  pub fn system_roots(&self, format: &Format) -> &[PathBuf] {
    self.system_roots.get(format).map_or(&[], Vec::as_slice)
  }

  /// Every system root, in a stable order.
  pub fn all_system_roots(&self) -> impl Iterator<Item = (&Format, &Path)> {
    self
      .system_roots
      .iter()
      .flat_map(|(f, roots)| roots.iter().map(move |p| (f, p.as_path())))
  }

  /// Replaces the system search paths. Used by tests and by callers that
  /// want a rooted layout to still see the machine.
  pub fn with_system_roots(mut self, roots: BTreeMap<Format, Vec<PathBuf>>) -> Self {
    self.system_roots = roots;
    self
  }

  /// Whether `path` lies inside a directory this layout manages.
  ///
  /// The uninstaller consults this before deleting anything, so a corrupted
  /// or hand-edited state file still cannot make it remove `/etc/passwd`.
  ///
  /// System roots are *not* managed locations. Detection may read
  /// `/usr/lib/lv2`; nothing may ever write to or delete from it.
  ///
  /// The path must lie strictly *below* a root, by plain names only. A root
  /// itself is not a location: `~/.clap` holds plugins this manager never
  /// installed, and a delete aimed at it takes them too. And `starts_with`
  /// compares components without resolving them, so `~/.clap/../Documents`
  /// starts with `~/.clap` and names somewhere else entirely.
  /// Roots nest — the library and state roots sit inside the data root — so
  /// "not a root" means not *any* root, not just the one the path is under.
  pub fn is_managed_location(&self, path: &Path) -> bool {
    let roots = || {
      self
        .plugin_roots
        .values()
        .chain([&self.data, &self.state, &self.libraries])
    };
    if roots().any(|root| path == root) {
      return false;
    }
    roots().any(|root| match path.strip_prefix(root) {
      Ok(rest) => {
        rest.components().next().is_some()
          && rest
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
      }
      Err(_) => false,
    })
  }
}

/// Removes repeats while keeping first-occurrence order.
fn dedup_preserving_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
  let mut seen = std::collections::BTreeSet::new();
  paths
    .into_iter()
    .filter(|p| seen.insert(p.clone()))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rooted_layout_confines_every_path() {
    let root = Path::new("/tmp/luthier-test-root");
    let layout = Layout::rooted_at(root);
    for path in [
      layout.data_dir().to_path_buf(),
      layout.cache_dir().to_path_buf(),
      layout.config_dir().to_path_buf(),
      layout.state_file(),
      layout.lock_file(),
      layout.artifact_cache_dir(),
      layout.registry_dir("default"),
      layout.transaction_dir("abc"),
    ] {
      assert!(path.starts_with(root), "{path:?} escaped the root");
    }
    for (_, plugin_root) in layout.plugin_roots() {
      assert!(
        plugin_root.starts_with(root),
        "{plugin_root:?} escaped the root"
      );
    }
  }

  #[test]
  fn a_rooted_layout_has_no_system_roots() {
    // Otherwise a test's result would depend on what the machine running
    // it happens to have in /usr/lib.
    let layout = Layout::rooted_at("/tmp/luthier-test-root");
    assert!(layout.all_system_roots().next().is_none());
    assert!(layout.system_roots(&Format::Lv2).is_empty());
  }

  #[test]
  fn system_roots_are_never_managed_locations() {
    // The uninstaller gates on is_managed_location. If a system root ever
    // counted as managed, a bad state file could delete a distribution's
    // plugins out of /usr/lib.
    let layout =
      Layout::rooted_at("/tmp/luthier-test-root").with_system_roots(Layout::default_system_roots());
    assert!(layout.all_system_roots().next().is_some());
    for (_, root) in layout.all_system_roots() {
      assert!(
        !layout.is_managed_location(root),
        "{root:?} counted as managed"
      );
      assert!(
        !layout.is_managed_location(&root.join("Anything.lv2")),
        "a file under {root:?} counted as managed"
      );
    }
  }

  #[test]
  fn system_roots_cover_each_formats_convention() {
    let roots = Layout::default_system_roots();
    for format in [Format::Clap, Format::Vst3, Format::Lv2] {
      let for_format = &roots[&format];
      assert!(
        for_format.iter().all(|p| p.is_absolute()),
        "{format} has a relative system root: {for_format:?}"
      );
      let dir = format.extension().unwrap();
      assert!(
        for_format.contains(&PathBuf::from(format!("/usr/lib/{dir}"))),
        "{format} is missing /usr/lib/{dir}: {for_format:?}"
      );
    }
  }

  /// Builds an environment lookup from pairs, for the search-path tests.
  fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + use<> {
    let pairs: Vec<(String, String)> = pairs
      .iter()
      .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
      .collect();
    move |var| {
      pairs
        .iter()
        .find(|(k, _)| k == var)
        .map(|(_, v)| std::ffi::OsString::from(v))
    }
  }

  #[test]
  fn lv2_path_replaces_the_default_search_path() {
    // lilv uses LV2_PATH instead of the default when it is set, so a host
    // with LV2_PATH=/opt/lv2 genuinely cannot see /usr/lib/lv2. Reporting
    // a bundle there as present would be a lie.
    let roots = Layout::system_roots_from(env(&[("LV2_PATH", "/opt/lv2:/srv/lv2")]));
    assert_eq!(
      roots[&Format::Lv2],
      vec![PathBuf::from("/opt/lv2"), PathBuf::from("/srv/lv2")]
    );
  }

  #[test]
  fn clap_and_vst3_paths_extend_the_default_search_path() {
    // Those two conventions specify additional locations, not a
    // replacement, so the standard ones must survive.
    let roots = Layout::system_roots_from(env(&[
      ("CLAP_PATH", "/opt/clap"),
      ("VST3_PATH", "/opt/vst3"),
    ]));
    assert_eq!(roots[&Format::Clap][0], PathBuf::from("/opt/clap"));
    assert!(roots[&Format::Clap].contains(&PathBuf::from("/usr/lib/clap")));
    assert_eq!(roots[&Format::Vst3][0], PathBuf::from("/opt/vst3"));
    assert!(roots[&Format::Vst3].contains(&PathBuf::from("/usr/lib/vst3")));
  }

  #[test]
  fn relative_and_empty_search_path_entries_are_dropped() {
    // A relative entry has no meaningful base here, and an LV2_PATH that
    // yields nothing usable is far more likely to be a broken environment
    // than a deliberate "look nowhere".
    let roots = Layout::system_roots_from(env(&[("LV2_PATH", "relative/dir::")]));
    assert!(roots[&Format::Lv2].iter().all(|p| p.is_absolute()));
    assert!(roots[&Format::Lv2].contains(&PathBuf::from("/usr/lib/lv2")));
  }

  #[test]
  fn a_search_path_repeating_a_conventional_location_lists_it_once() {
    let roots = Layout::system_roots_from(env(&[("CLAP_PATH", "/usr/lib/clap")]));
    let hits = roots[&Format::Clap]
      .iter()
      .filter(|p| *p == Path::new("/usr/lib/clap"))
      .count();
    assert_eq!(hits, 1, "{:?}", roots[&Format::Clap]);
  }

  #[test]
  fn plugin_roots_follow_each_formats_convention() {
    let layout = Layout::rooted_at("/home/u");
    assert_eq!(
      layout.plugin_root(&Format::Clap).unwrap(),
      Path::new("/home/u/.clap")
    );
    assert_eq!(
      layout.plugin_root(&Format::Vst3).unwrap(),
      Path::new("/home/u/.vst3")
    );
    assert_eq!(
      layout.plugin_root(&Format::Lv2).unwrap(),
      Path::new("/home/u/.lv2")
    );
    assert!(layout.plugin_root(&Format::Other("sf2".into())).is_none());
  }

  #[test]
  fn managed_location_check_rejects_paths_outside_the_layout() {
    let layout = Layout::rooted_at("/home/u");
    assert!(layout.is_managed_location(Path::new("/home/u/.clap/Surge XT.clap")));
    assert!(layout.is_managed_location(Path::new("/home/u/share/luthier/state/state.json")));
    assert!(!layout.is_managed_location(Path::new("/etc/passwd")));
    assert!(!layout.is_managed_location(Path::new("/home/u/.ssh/authorized_keys")));
    assert!(!layout.is_managed_location(Path::new("/home/u/Documents/song.wav")));
  }

  #[test]
  fn a_root_itself_is_not_a_managed_location() {
    // Deleting `~/.clap` would take every plugin in it, installed by this
    // manager or not.
    let layout = Layout::rooted_at("/home/u");
    assert!(!layout.is_managed_location(Path::new("/home/u/.clap")));
    assert!(!layout.is_managed_location(Path::new("/home/u/.clap/")));
    assert!(!layout.is_managed_location(layout.library_root()));
  }

  #[test]
  fn a_parent_component_does_not_pass_as_managed() {
    // `Path::starts_with` is satisfied by these; the paths they name are not
    // under any root.
    let layout = Layout::rooted_at("/home/u");
    assert!(!layout.is_managed_location(Path::new("/home/u/.clap/../Documents")));
    assert!(!layout.is_managed_location(Path::new("/home/u/.vst3/x/../../.ssh/id_ed25519")));
  }

  #[test]
  fn an_environment_keeps_the_chosen_cache_and_nothing_else() {
    // `env remove` deletes an environment's directory whole, so its plugins
    // and libraries must live inside it; the cache is shared by design.
    let locations = Locations {
      cache: Some("/mnt/ext/cache".into()),
      libraries: Some("/mnt/ext/samples".into()),
      plugins: Some("/mnt/ext/plugins".into()),
    };
    let layout = Layout::rooted_at("/home/u").with_locations(&locations);
    assert_eq!(layout.library_root(), Path::new("/mnt/ext/samples"));
    assert_eq!(
      layout.plugin_root(&Format::Clap).unwrap(),
      Path::new("/mnt/ext/plugins/clap")
    );
    assert!(layout.is_managed_location(Path::new("/mnt/ext/samples/vsco2")));

    let env = layout.into_env(&EnvName::new("mixing").unwrap());
    assert_eq!(env.cache_dir(), Path::new("/mnt/ext/cache"));
    assert!(
      env
        .library_root()
        .starts_with("/home/u/share/luthier/envs/mixing")
    );
    assert!(
      env
        .plugin_root(&Format::Clap)
        .unwrap()
        .starts_with("/home/u/share/luthier/envs/mixing")
    );
    // And only what still applies is required to be present.
    let required: Vec<_> = env.locations().configured().map(|(k, _)| k).collect();
    assert_eq!(required, vec![LocationKind::Cache]);
  }

  #[test]
  fn artifact_cache_is_keyed_by_content() {
    // The cache filename is the digest, so reusing an entry cannot
    // resurrect a file whose contents no longer match the manifest.
    let layout = Layout::rooted_at("/home/u");
    assert!(layout.artifact_cache_dir().ends_with("artifacts"));
  }
}
