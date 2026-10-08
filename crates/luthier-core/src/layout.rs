//! Where everything lives on disk.
//!
//! Every path the manager touches is derived from a `Layout` value that is
//! passed in explicitly. Nothing deep in the call graph reaches for
//! `dirs::home_dir()` or reads `$HOME` on its own. That single rule is what
//! makes the test suite hermetic: a test constructs [`Layout::rooted_at`] over
//! a temporary directory and there is then no code path that could reach the
//! real `~/.clap`.

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
  /// Managed data: registry snapshots and state. `~/.local/share/luthier`.
  data: PathBuf,
  /// Installation state.
  state: PathBuf,
  /// Sample libraries and other non-plugin content.
  libraries: PathBuf,
  /// Re-fetchable data only. `~/.cache/luthier`.
  cache: PathBuf,
  /// User configuration. `~/.config/luthier`.
  config: PathBuf,
  /// Where each plugin format is installed. Keyed by format so adding one is
  /// a data change rather than a new field.
  plugin_roots: BTreeMap<Format, PathBuf>,
  /// Read-only locations searched for software this manager does not install:
  /// distribution packages, vendor installers, or whatever a container image
  /// baked in. Never written to, never deleted from, and deliberately absent
  /// from [`Layout::is_managed_location`].
  system_roots: BTreeMap<Format, Vec<PathBuf>>,
  /// The user's choices this layout honours, kept so a command can refuse
  /// when one of them is not there.
  locations: Locations,
  /// The directory `--root` confines everything to, if any.
  root: Option<PathBuf>,
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
      system_roots: Self::default_system_roots(),
      locations: Locations::default(),
      root: None,
    })
  }

  /// Moves the parts `locations` names to where it says.
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
      system_roots: BTreeMap::new(),
      locations: Locations::default(),
      root: Some(root.to_path_buf()),
    }
  }

  /// The directory `--root` confines everything to, if any.
  pub fn root(&self) -> Option<&Path> {
    self.root.as_deref()
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

      let mut conventional = vec![
        PathBuf::from("/usr/lib").join(dir),
        PathBuf::from("/usr/local/lib").join(dir),
        // Debian and Ubuntu.
        PathBuf::from("/usr/lib").join(multiarch).join(dir),
        // Fedora, openSUSE and the rest of the RPM world, which put 64-bit
        // plugins here and leave /usr/lib to 32-bit ones.
        PathBuf::from("/usr/lib64").join(dir),
        PathBuf::from("/usr/local/lib64").join(dir),
        // NixOS has no /usr/lib: the system profile, and each user's
        // profile — the classic one, the XDG one newer Nix uses, and the
        // per-user one Home Manager's `useUserPackages` fills — are where
        // an engine from nixpkgs lives.
        PathBuf::from("/run/current-system/sw/lib").join(dir),
      ];
      if let Some(home) = lookup("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
      {
        conventional.push(home.join(".nix-profile/lib").join(dir));
        conventional.push(home.join(".local/state/nix/profile/lib").join(dir));
      }
      if let Some(user) = lookup("USER").and_then(|u| u.into_string().ok()) {
        // A name, used as one path segment: anything that could climb out
        // of the directory is ignored rather than joined.
        if !user.is_empty() && !user.contains('/') && user != "." && user != ".." {
          conventional.push(
            PathBuf::from("/etc/profiles/per-user")
              .join(user)
              .join("lib")
              .join(dir),
          );
        }
      }

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

  /// Directory holding installation state.
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

  /// Held while the snapshots are being refreshed, so two refreshes never
  /// write them at once. Apart from [`Layout::lock_file`], so a command that
  /// only reads packages never holds up one that installs them.
  pub fn registries_lock_file(&self) -> PathBuf {
    self.registries_dir().join(".lock")
  }

  /// Content-addressed artifact cache. Keying by digest means a cache hit is
  /// self-verifying: the name *is* the expected hash.
  pub fn artifact_cache_dir(&self) -> PathBuf {
    self.cache.join("artifacts")
  }

  /// Where `luthier update --self` downloads and unpacks a release. Apart
  /// from the artifact cache, which holds what installed packages name, and
  /// removed once the update is done.
  pub fn self_update_dir(&self) -> PathBuf {
    self.cache.join("self-update")
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

/// Shell variables that let hosts find plugins where this manager put them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SearchPath {
  /// Variables to set, in a stable order.
  pub set: Vec<(String, String)>,
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
/// Empty when plugins are where hosts already look.
pub fn search_path(home: &Layout, located: &Layout) -> SearchPath {
  let conventional = Layout::system_roots_from(|_| None);
  let mut set = Vec::new();

  for (format, var) in [
    (Format::Lv2, "LV2_PATH"),
    (Format::Clap, "CLAP_PATH"),
    (Format::Vst3, "VST3_PATH"),
  ] {
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
    let joined = paths
      .iter()
      .map(|p| p.display().to_string())
      .collect::<Vec<_>>()
      .join(":");
    set.push((var.to_owned(), joined));
  }

  SearchPath { set }
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
  fn lib64_is_a_system_root_too() {
    // Fedora installs an LV2 plugin from `dnf` in /usr/lib64/lv2, and
    // detection that did not look there called it absent.
    let roots = Layout::system_roots_from(env(&[]));
    for (format, dir) in [
      (Format::Clap, "clap"),
      (Format::Vst3, "vst3"),
      (Format::Lv2, "lv2"),
    ] {
      for expected in [
        format!("/usr/lib64/{dir}"),
        format!("/usr/local/lib64/{dir}"),
      ] {
        assert!(
          roots[&format].contains(&PathBuf::from(&expected)),
          "{format} is missing {expected}: {:?}",
          roots[&format]
        );
      }
    }
  }

  #[test]
  fn nix_profiles_are_system_roots_too() {
    // On NixOS an engine from nixpkgs is in a profile, never in /usr/lib,
    // and detection that missed it would call sfizz absent on every NixOS
    // machine that has it.
    let roots = Layout::system_roots_from(env(&[("HOME", "/home/u"), ("USER", "u")]));
    for (format, dir) in [
      (Format::Clap, "clap"),
      (Format::Vst3, "vst3"),
      (Format::Lv2, "lv2"),
    ] {
      for expected in [
        format!("/run/current-system/sw/lib/{dir}"),
        format!("/home/u/.nix-profile/lib/{dir}"),
        format!("/home/u/.local/state/nix/profile/lib/{dir}"),
        format!("/etc/profiles/per-user/u/lib/{dir}"),
      ] {
        assert!(
          roots[&format].contains(&PathBuf::from(&expected)),
          "{format} is missing {expected}: {:?}",
          roots[&format]
        );
      }
    }

    // A user name that is not one segment is not joined into a path.
    let roots = Layout::system_roots_from(env(&[("USER", "../../etc")]));
    assert!(
      roots[&Format::Clap]
        .iter()
        .all(|p| !p.starts_with("/etc/profiles")),
      "{:?}",
      roots[&Format::Clap]
    );
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
  fn chosen_locations_move_what_they_name() {
    let locations = Locations {
      cache: Some("/mnt/ext/cache".into()),
      libraries: Some("/mnt/ext/samples".into()),
      plugins: Some("/mnt/ext/plugins".into()),
    };
    let layout = Layout::rooted_at("/home/u").with_locations(&locations);
    assert_eq!(layout.cache_dir(), Path::new("/mnt/ext/cache"));
    assert_eq!(layout.library_root(), Path::new("/mnt/ext/samples"));
    assert_eq!(
      layout.plugin_root(&Format::Clap).unwrap(),
      Path::new("/mnt/ext/plugins/clap")
    );
    assert!(layout.is_managed_location(Path::new("/mnt/ext/samples/vsco2")));
  }

  #[test]
  fn a_relocated_plugin_root_is_put_on_the_search_path() {
    let home = Layout::rooted_at("/home/u");
    assert!(search_path(&home, &home).set.is_empty());

    let located = home.clone().with_locations(&Locations {
      plugins: Some("/mnt/ext/plugins".into()),
      ..Locations::default()
    });
    let set: BTreeMap<String, String> = search_path(&home, &located).set.into_iter().collect();
    assert_eq!(set["CLAP_PATH"], "/mnt/ext/plugins/clap");
    assert_eq!(set["VST3_PATH"], "/mnt/ext/plugins/vst3");
    // LV2_PATH replaces a host's default, so ~/.lv2 is listed again.
    assert!(
      set["LV2_PATH"].starts_with("/mnt/ext/plugins/lv2:/home/u/.lv2:"),
      "{}",
      set["LV2_PATH"]
    );
  }

  #[test]
  fn artifact_cache_is_keyed_by_content() {
    // The cache filename is the digest, so reusing an entry cannot
    // resurrect a file whose contents no longer match the manifest.
    let layout = Layout::rooted_at("/home/u");
    assert!(layout.artifact_cache_dir().ends_with("artifacts"));
  }
}
