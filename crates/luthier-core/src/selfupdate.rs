//! Luthier updating itself: `luthier update --self`.
//!
//! A release publishes, for each architecture, the tarball the install script
//! and a hand install put in place, and a .deb and an .rpm of the same files.
//! An update goes the way the running binary came: a standalone binary is
//! replaced where it is, with the man page and completions the install script
//! put beside it; a packaged one is handed to its package manager, so the
//! package database stays right; one in the Nix store, or in a directory some
//! other package manager owns, is left to whatever put it there.
//!
//! What is downloaded is checked against the SHA-256 GitHub publishes for each
//! release asset, through the same [`Downloader`] as every artifact, so nothing
//! unverified is unpacked or installed (§28), and the tarball is opened by the
//! one extraction policy like any other.

use crate::archive::{self, ExtractLimits};
use crate::download::{Downloader, Progress};
use crate::error::{Error, Result, SelfUpdateError};
use crate::fsutil;
use luthier_manifest::{ArchiveFormat, Sha256Hash};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use url::Url;

/// Where the latest release is described: GitHub's API, whose answer carries
/// each asset's SHA-256.
pub const RELEASES_API: &str = "https://api.github.com/repos/savashn/luthier/releases/latest";

/// The most GitHub's description of a release may be. A few dozen kilobytes
/// in practice, release notes included.
pub const RELEASE_DESCRIPTION_LIMIT: u64 = 1024 * 1024;

/// How long the new binary gets to print its version before it is taken not
/// to run here.
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The version of this build. Every crate in the workspace carries the same.
pub fn this_version() -> Version {
  Version::parse(env!("CARGO_PKG_VERSION")).expect("the workspace version is semver")
}

/// The running executable as the kernel names it, links resolved, so an
/// update replaces the file itself rather than a link to it.
pub fn running_binary() -> Result<PathBuf> {
  std::env::current_exe().map_err(|e| Error::io("find the running binary", "/proc/self/exe", e))
}

/// The latest published release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
  pub version: Version,
  pub assets: Vec<Asset>,
}

/// One file a release publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Asset {
  pub name: String,
  pub url: Url,
  pub size: u64,
  /// `None` when GitHub publishes no digest for it, which makes it unusable
  /// here: there would be nothing to check the download against.
  pub sha256: Option<Sha256Hash>,
}

#[derive(Deserialize)]
struct ApiRelease {
  tag_name: String,
  assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
  name: String,
  browser_download_url: String,
  size: u64,
  #[serde(default)]
  digest: Option<String>,
}

/// Reads GitHub's description of a release.
pub fn parse_release(json: &[u8]) -> Result<Release> {
  let malformed = |reason: String| Error::SelfUpdate(SelfUpdateError::Malformed(reason));
  let api: ApiRelease = serde_json::from_slice(json).map_err(|e| malformed(e.to_string()))?;
  let raw = api.tag_name.strip_prefix('v').unwrap_or(&api.tag_name);
  let version = Version::parse(raw).map_err(|e| malformed(format!("tag {}: {e}", api.tag_name)))?;
  let assets = api
    .assets
    .into_iter()
    .map(|asset| {
      let url = Url::parse(&asset.browser_download_url)
        .map_err(|e| malformed(format!("{}: {e}", asset.browser_download_url)))?;
      let sha256 = match asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
      {
        Some(hex) => {
          Some(Sha256Hash::parse(hex).map_err(|e| malformed(format!("{}: {e}", asset.name)))?)
        }
        None => None,
      };
      Ok(Asset {
        name: asset.name,
        url,
        size: asset.size,
        sha256,
      })
    })
    .collect::<Result<Vec<_>>>()?;
  Ok(Release { version, assets })
}

/// A release newer than the running build, for the line after `update` and
/// `refresh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewerLuthier {
  pub current: Version,
  pub latest: Version,
  /// Whether `luthier update --self` updates this installation. When it
  /// does not, the way it was installed does.
  pub updates_itself: bool,
}

/// How the running binary was installed, which decides how it is updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Installation {
  /// By the install script or by hand: replaced where it is.
  Standalone { binary: PathBuf },
  /// From the .deb: updated through apt.
  Deb,
  /// From the .rpm: updated through dnf, or zypper on openSUSE.
  Rpm,
  /// In the Nix store, which is read-only: updated through Nix.
  Nix,
  /// In a directory a package manager owns, by one other than dpkg and rpm:
  /// pacman from the AUR, Homebrew, another distribution's. Replacing its
  /// file would leave that package manager's records wrong.
  ForeignPackage { binary: PathBuf },
}

impl Installation {
  /// Works out how `binary`, the running executable, was installed: from
  /// where it is, and whether dpkg or rpm lists it as one of their files.
  pub fn of(binary: &Path) -> Self {
    let deb_listed = std::fs::read_to_string("/var/lib/dpkg/info/luthier.list")
      .is_ok_and(|list| list.lines().any(|line| Path::new(line) == binary));
    // Only a binary under /usr can be the .rpm's; asking rpm costs a process.
    // By name: a distribution's own package of it is not ours to replace.
    let rpm_owned = binary.starts_with("/usr")
      && std::process::Command::new("rpm")
        .args(["-qf", "--queryformat", "%{NAME}\n"])
        .arg(binary)
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| {
          output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "luthier"
        });
    classify(binary, deb_listed, rpm_owned)
  }

  /// Whether `luthier update --self` updates an installation like this one.
  pub fn updates_itself(&self) -> bool {
    matches!(
      self,
      Installation::Standalone { .. } | Installation::Deb | Installation::Rpm
    )
  }
}

fn classify(binary: &Path, deb_listed: bool, rpm_owned: bool) -> Installation {
  if binary.starts_with("/nix/store") {
    Installation::Nix
  } else if deb_listed {
    Installation::Deb
  } else if rpm_owned {
    Installation::Rpm
  } else if owned_by_a_package_manager(binary) {
    Installation::ForeignPackage {
      binary: binary.to_path_buf(),
    }
  } else {
    Installation::Standalone {
      binary: binary.to_path_buf(),
    }
  }
}

/// `/usr` and the `/bin` and `/sbin` merged into it belong to the system's
/// package manager; `/usr/local` is the administrator's, and where the install
/// script puts a system-wide Luthier. Guix keeps its packages in a read-only
/// store as Nix does, and Homebrew in a `Cellar`, which the running binary's
/// resolved path goes through.
fn owned_by_a_package_manager(binary: &Path) -> bool {
  let system = ["/usr", "/bin", "/sbin", "/gnu/store"]
    .iter()
    .any(|dir| binary.starts_with(dir))
    && !binary.starts_with("/usr/local");
  system || binary.components().any(|part| part.as_os_str() == "Cellar")
}

/// A file a standalone update replaces: `from` inside the release tarball's
/// top directory, written over `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Replacement {
  pub from: String,
  pub path: PathBuf,
}

/// The machine the plan is made for, beyond the binary's place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
  /// As `std::env::consts::ARCH` and the release assets spell it.
  pub arch: String,
  /// Whether the process runs as root, which the package managers need.
  pub is_root: bool,
  /// Whether `dnf` is on PATH; an .rpm goes to zypper otherwise.
  pub has_dnf: bool,
  /// Whether `sudo` is on PATH, which a package install needs unless root.
  pub has_sudo: bool,
}

impl Host {
  pub fn this_machine() -> Self {
    Self {
      arch: std::env::consts::ARCH.to_string(),
      is_root: rustix::process::geteuid().is_root(),
      has_dnf: on_path("dnf"),
      has_sudo: on_path("sudo"),
    }
  }
}

fn on_path(program: &str) -> bool {
  std::env::var_os("PATH")
    .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// How a downloaded package is installed: by the package manager, as root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageInstall {
  /// Through sudo, because the process is not root.
  pub sudo: bool,
  /// The package manager's command; the package's file goes after it.
  pub manager: Vec<String>,
}

impl PackageInstall {
  /// As someone would type it, for showing.
  pub fn command_line(&self, file: &str) -> String {
    match self.sudo {
      true => format!("sudo {}", self.manager_line(file)),
      false => self.manager_line(file),
    }
  }

  /// The package manager's part of [`PackageInstall::command_line`], which
  /// root runs on its own copy of `file`.
  pub fn manager_line(&self, file: &str) -> String {
    self
      .manager
      .iter()
      .map(String::as_str)
      .chain([file])
      .collect::<Vec<_>>()
      .join(" ")
  }
}

/// What `luthier update --self` will do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelfUpdatePlan {
  pub current: Version,
  pub latest: Version,
  pub installation: Installation,
  /// The file it downloads; `None` when this is already the latest release.
  pub asset: Option<Asset>,
  /// For a standalone binary, every file it replaces, the binary first: the
  /// man page and completions the install script put beside it, where they
  /// are.
  pub replaces: Vec<Replacement>,
  /// For a packaged binary, how the download is installed.
  pub package: Option<PackageInstall>,
}

impl SelfUpdatePlan {
  pub fn is_current(&self) -> bool {
    self.latest <= self.current
  }
}

/// Plans an update from `current` to `release` for a binary installed the way
/// `installation` says, on `host`. Refuses up front what could not be carried
/// out, before anything is downloaded.
pub fn plan(
  release: &Release,
  current: &Version,
  installation: Installation,
  host: &Host,
) -> Result<SelfUpdatePlan> {
  let mut plan = SelfUpdatePlan {
    current: current.clone(),
    latest: release.version.clone(),
    installation,
    asset: None,
    replaces: Vec::new(),
    package: None,
  };
  if plan.is_current() {
    return Ok(plan);
  }

  let name = match &plan.installation {
    Installation::Nix => {
      return Err(Error::SelfUpdate(SelfUpdateError::InstalledByNix {
        latest: release.version.to_string(),
      }));
    }
    Installation::ForeignPackage { binary } => {
      return Err(Error::SelfUpdate(
        SelfUpdateError::InstalledByPackageManager {
          binary: binary.clone(),
        },
      ));
    }
    Installation::Standalone { .. } => format!("luthier-{}-linux.tar.gz", host.arch),
    Installation::Deb => format!("luthier-{}-linux.deb", host.arch),
    Installation::Rpm => format!("luthier-{}-linux.rpm", host.arch),
  };
  let asset = release
    .assets
    .iter()
    .find(|asset| asset.name == name)
    .ok_or_else(|| {
      Error::SelfUpdate(SelfUpdateError::NoAsset {
        name: name.clone(),
        version: release.version.to_string(),
      })
    })?;
  if asset.sha256.is_none() {
    return Err(Error::SelfUpdate(SelfUpdateError::NoDigest { name }));
  }

  match &plan.installation {
    Installation::Standalone { binary } => {
      plan.replaces = replacements(binary);
      // Each file is renamed into place, which takes its directory.
      for replacement in &plan.replaces {
        let dir = replacement.path.parent().unwrap_or(Path::new("/"));
        if rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_err() {
          return Err(Error::SelfUpdate(SelfUpdateError::NotWritable {
            dir: dir.to_path_buf(),
          }));
        }
      }
    }
    Installation::Deb => {
      plan.package = Some(PackageInstall {
        sudo: !host.is_root,
        manager: vec!["apt-get".into(), "install".into(), "-y".into()],
      });
    }
    Installation::Rpm => {
      plan.package = Some(PackageInstall {
        sudo: !host.is_root,
        manager: if host.has_dnf {
          vec!["dnf".into(), "install".into(), "-y".into()]
        } else {
          // The .rpm carries no signature; what vouches for it is the
          // SHA-256 it is checked against before this runs.
          vec![
            "zypper".into(),
            "--non-interactive".into(),
            "install".into(),
            "--allow-unsigned-rpm".into(),
          ]
        },
      });
    }
    Installation::Nix | Installation::ForeignPackage { .. } => unreachable!("refused above"),
  }
  if plan.package.as_ref().is_some_and(|p| p.sudo) && !host.has_sudo {
    return Err(Error::SelfUpdate(SelfUpdateError::CouldNotRun {
      program: "sudo".into(),
      reason: "it is not on PATH".into(),
    }));
  }
  plan.asset = Some(asset.clone());
  Ok(plan)
}

/// The binary, then the man page and completions where the install script
/// puts them under the binary's prefix — each only if it is there, so a
/// binary put somewhere by hand is replaced alone.
fn replacements(binary: &Path) -> Vec<Replacement> {
  let mut replaces = vec![Replacement {
    from: "luthier".into(),
    path: binary.to_path_buf(),
  }];
  let prefix = binary
    .parent()
    .filter(|dir| dir.file_name().is_some_and(|name| name == "bin"))
    .and_then(Path::parent);
  if let Some(prefix) = prefix {
    for (from, path) in [
      ("luthier.1", "share/man/man1/luthier.1"),
      (
        "completions/luthier.bash",
        "share/bash-completion/completions/luthier",
      ),
      (
        "completions/luthier.zsh",
        "share/zsh/site-functions/_luthier",
      ),
      (
        "completions/luthier.fish",
        "share/fish/vendor_completions.d/luthier.fish",
      ),
    ] {
      let path = prefix.join(path);
      if path.is_file() {
        replaces.push(Replacement {
          from: from.into(),
          path,
        });
      }
    }
  }
  replaces
}

/// Carries `plan` out: downloads and verifies its asset into `downloader`'s
/// directory, then either puts the files it names in place or has the
/// package manager install it. `work_dir` is scratch space for unpacking.
pub async fn apply(
  plan: &SelfUpdatePlan,
  downloader: &Downloader,
  work_dir: &Path,
  progress: &mut dyn Progress,
) -> Result<()> {
  let Some(asset) = &plan.asset else {
    return Ok(());
  };
  let sha256 = asset
    .sha256
    .as_ref()
    .expect("a plan only holds an asset with a digest");
  let fetched = downloader
    .fetch(&asset.url, sha256, Some(asset.size), progress)
    .await?;

  if let Some(package) = &plan.package {
    return install_package(package, &fetched.path, sha256, &asset.name);
  }

  let unpacked = work_dir.join("unpacked");
  fsutil::remove_any(&unpacked)?;
  fsutil::ensure_dir(&unpacked)?;
  archive::unpack(
    &fetched.path,
    &ArchiveFormat::TarGz,
    &asset.name,
    &unpacked,
    ExtractLimits::for_download(asset.size),
  )?;
  let top = single_directory(&unpacked)
    .ok_or_else(|| Error::SelfUpdate(SelfUpdateError::BadTarball(asset.name.clone())))?;
  for replacement in &plan.replaces {
    if !top.join(&replacement.from).is_file() {
      return Err(Error::SelfUpdate(SelfUpdateError::BadTarball(
        asset.name.clone(),
      )));
    }
  }
  put_in_place(plan, &top)?;
  fsutil::remove_any(&unpacked)
}

/// The one directory a release tarball unpacks to.
fn single_directory(dir: &Path) -> Option<PathBuf> {
  let mut entries = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok());
  let first = entries.next()?.path();
  (entries.next().is_none() && first.is_dir()).then_some(first)
}

/// Copies each file from `top` beside the one it replaces, tries the new
/// binary there, then renames them all over the old ones.
///
/// Beside, so each rename stays within one filesystem and is atomic: a reader
/// sees the old file or the new one, and a running binary replaced this way
/// keeps running. And the new binary is tried where it will run, not in the
/// cache, which may be on a disk mounted `noexec`.
fn put_in_place(plan: &SelfUpdatePlan, top: &Path) -> Result<()> {
  let mut staged = Staged(Vec::new());
  for replacement in &plan.replaces {
    let mode = if replacement.from == "luthier" {
      0o755
    } else {
      0o644
    };
    let path = stage(&top.join(&replacement.from), &replacement.path, mode)?;
    staged.0.push((path, replacement.path.clone()));
  }
  let (binary, _) = staged.0.first().expect("a plan replaces the binary first");
  check_runs(binary, &plan.latest, VERSION_CHECK_TIMEOUT)?;

  // The binary last: if a rename fails part-way, what is left is the old
  // Luthier with some new documentation, rather than a new one that says it
  // failed to update.
  for (from, to) in staged.0.iter().rev() {
    std::fs::rename(from, to).map_err(|e| Error::io("replace", to, e))?;
    if let Some(dir) = to.parent()
      && let Ok(dir) = std::fs::File::open(dir)
    {
      // Best effort: not every filesystem supports syncing a directory.
      let _ = dir.sync_all();
    }
  }
  staged.0.clear();
  Ok(())
}

/// Staged copies not renamed into place yet, removed if the update stops
/// before they are.
struct Staged(Vec<(PathBuf, PathBuf)>);

impl Drop for Staged {
  fn drop(&mut self) {
    for (staged, _) in &self.0 {
      let _ = std::fs::remove_file(staged);
    }
  }
}

/// Copies `source` to a hidden name beside `destination`, with `mode`,
/// flushed to disk.
fn stage(source: &Path, destination: &Path, mode: u32) -> Result<PathBuf> {
  use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
  let name = destination
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default();
  let staged = destination.with_file_name(format!(".{name}.luthier-update"));
  // Left by an update that was interrupted.
  let _ = std::fs::remove_file(&staged);

  let mut from = std::fs::File::open(source).map_err(|e| Error::io("open", source, e))?;
  let mut to = std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .mode(mode)
    .open(&staged)
    .map_err(|e| Error::io("create", &staged, e))?;
  let copied = std::io::copy(&mut from, &mut to)
    .and_then(|_| to.set_permissions(std::fs::Permissions::from_mode(mode)))
    .and_then(|()| to.sync_all());
  if let Err(e) = copied {
    let _ = std::fs::remove_file(&staged);
    return Err(Error::io("write", &staged, e));
  }
  Ok(staged)
}

/// Runs the new binary once before it replaces this one: a build for another
/// architecture, one that reports another version, or one that does not
/// answer in `timeout` goes no further.
fn check_runs(binary: &Path, expected: &Version, timeout: Duration) -> Result<()> {
  use std::io::Read;
  let not_runnable = |reason: String| Error::SelfUpdate(SelfUpdateError::NotRunnable(reason));
  let spawn = || {
    std::process::Command::new(binary)
      .arg("--version")
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::null())
      .spawn()
  };
  // A file just written can be busy for a moment: a process forked while it
  // was open for writing holds it until that process execs.
  let mut attempts = 0;
  let mut child = loop {
    match spawn() {
      Err(e)
        if e.raw_os_error() == Some(rustix::io::Errno::TXTBSY.raw_os_error()) && attempts < 50 =>
      {
        attempts += 1;
        std::thread::sleep(Duration::from_millis(20));
      }
      spawned => break spawned.map_err(|e| not_runnable(e.to_string()))?,
    }
  };

  // Read on a thread of its own: anything the binary leaves running can
  // hold the pipe open after it exits, and must not hold this up.
  let (sender, receiver) = std::sync::mpsc::channel();
  if let Some(mut stdout) = child.stdout.take() {
    std::thread::spawn(move || {
      let mut reported = String::new();
      let _ = stdout.read_to_string(&mut reported);
      let _ = sender.send(reported);
    });
  }

  let deadline = Instant::now() + timeout;
  let status = loop {
    match child.try_wait() {
      Ok(Some(status)) => break status,
      Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
      Ok(None) => {
        let _ = child.kill();
        let _ = child.wait();
        return Err(not_runnable(format!(
          "`luthier --version` did not answer within {} seconds",
          timeout.as_secs_f32()
        )));
      }
      Err(e) => return Err(not_runnable(e.to_string())),
    }
  };
  let reported = receiver
    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    .unwrap_or_default();
  if !status.success() || reported.trim() != format!("luthier {expected}") {
    return Err(not_runnable(format!(
      "`luthier --version` said {:?}, not \"luthier {expected}\"",
      reported.trim()
    )));
  }
  Ok(())
}

/// Run as root: copies the package where only root can write, checks it
/// against its SHA-256 again there, and installs that copy. The download sat
/// in a directory this user can write to, and root is about to trust it.
///
/// Arguments: the download, its SHA-256, the package's file name (apt and
/// dnf go by the extension), then the package manager's command. The
/// directory is readable by all so apt's unprivileged `_apt` user can read
/// the package, as it prefers to. A copy that fails the check exits with
/// [`CHANGED_BEFORE_INSTALL`]; the signal traps make an interrupted run
/// still remove the directory, which dash's EXIT trap alone would not.
const INSTALL_AS_ROOT: &str = r#"set -eu
download=$1 sha256=$2 name=$3
shift 3
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
chmod 755 "$dir"
cp -- "$download" "$dir/$name"
printf '%s  %s\n' "$sha256" "$dir/$name" | sha256sum --check --quiet --strict - || exit 87
"$@" "$dir/$name"
"#;

/// The status [`INSTALL_AS_ROOT`] exits with when the copy root made is not
/// what was verified. None of sudo, apt-get, dnf or zypper uses it.
const CHANGED_BEFORE_INSTALL: i32 = 87;

fn install_package(
  package: &PackageInstall,
  download: &Path,
  sha256: &Sha256Hash,
  name: &str,
) -> Result<()> {
  let program = if package.sudo { "sudo" } else { "/bin/sh" };
  let mut command = std::process::Command::new(program);
  if package.sudo {
    command.arg("/bin/sh");
  }
  command
    .arg("-c")
    .arg(INSTALL_AS_ROOT)
    .arg("luthier-update")
    .arg(download)
    .arg(sha256.to_string())
    .arg(name)
    .args(&package.manager)
    // The package manager reports its own progress, on stderr: stdout is
    // the one document `--json` promises. sudo asks for the password on
    // the terminal itself.
    .stdout(std::io::stderr());
  let status = command.status().map_err(|e| {
    Error::SelfUpdate(SelfUpdateError::CouldNotRun {
      program: program.into(),
      reason: e.to_string(),
    })
  })?;
  match status.code() {
    _ if status.success() => Ok(()),
    Some(CHANGED_BEFORE_INSTALL) => Err(Error::SelfUpdate(SelfUpdateError::ChangedBeforeInstall {
      path: download.to_path_buf(),
    })),
    code => Err(Error::SelfUpdate(SelfUpdateError::CommandFailed {
      command: package.command_line(name),
      code,
    })),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

  fn release_json(tag: &str, digest: Option<&str>) -> Vec<u8> {
    let digest = digest.map_or(String::new(), |d| format!(r#", "digest": "{d}""#));
    format!(
      r#"{{ "tag_name": "{tag}", "assets": [
        {{ "name": "luthier-x86_64-linux.tar.gz",
           "browser_download_url": "https://example.invalid/luthier-x86_64-linux.tar.gz",
           "size": 10{digest} }},
        {{ "name": "luthier-x86_64-linux.deb",
           "browser_download_url": "https://example.invalid/luthier-x86_64-linux.deb",
           "size": 20{digest} }},
        {{ "name": "luthier-x86_64-linux.rpm",
           "browser_download_url": "https://example.invalid/luthier-x86_64-linux.rpm",
           "size": 30{digest} }}
      ] }}"#
    )
    .into_bytes()
  }

  fn release(tag: &str) -> Release {
    parse_release(&release_json(tag, Some(&format!("sha256:{DIGEST}")))).unwrap()
  }

  fn host(is_root: bool, has_dnf: bool) -> Host {
    Host {
      arch: "x86_64".into(),
      is_root,
      has_dnf,
      has_sudo: true,
    }
  }

  fn v(raw: &str) -> Version {
    Version::parse(raw).unwrap()
  }

  fn is_root() -> bool {
    rustix::process::geteuid().is_root()
  }

  #[test]
  fn a_release_is_read_with_its_version_and_digests() {
    let release = release("v0.5.0");
    assert_eq!(release.version, v("0.5.0"));
    assert_eq!(release.assets.len(), 3);
    assert_eq!(
      release.assets[0].sha256.as_ref().unwrap().to_string(),
      DIGEST
    );
  }

  #[test]
  fn an_asset_without_a_digest_is_kept_but_unusable() {
    let release = parse_release(&release_json("v0.5.0", None)).unwrap();
    assert!(release.assets.iter().all(|a| a.sha256.is_none()));
    let err = plan(&release, &v("0.4.0"), Installation::Deb, &host(true, false)).unwrap_err();
    assert!(
      matches!(err, Error::SelfUpdate(SelfUpdateError::NoDigest { .. })),
      "{err}"
    );
  }

  #[test]
  fn a_malformed_description_is_an_error() {
    assert!(parse_release(b"{}").is_err());
    assert!(parse_release(&release_json("vnext", None)).is_err());
  }

  #[test]
  fn the_install_is_told_by_place_and_package_database() {
    let bin = Path::new("/usr/bin/luthier");
    assert_eq!(classify(bin, true, false), Installation::Deb);
    assert_eq!(classify(bin, false, true), Installation::Rpm);
    assert_eq!(
      classify(
        Path::new("/nix/store/abc-luthier-0.4.0/bin/luthier"),
        false,
        false
      ),
      Installation::Nix
    );
    for standalone in ["/home/u/.local/bin/luthier", "/usr/local/bin/luthier"] {
      let path = Path::new(standalone);
      assert_eq!(
        classify(path, false, false),
        Installation::Standalone {
          binary: path.to_path_buf()
        }
      );
    }
  }

  #[test]
  fn a_binary_another_package_manager_owns_is_left_to_it() {
    for owned in [
      "/usr/bin/luthier",
      "/bin/luthier",
      "/gnu/store/abc-luthier-0.4.0/bin/luthier",
      "/home/linuxbrew/.linuxbrew/Cellar/luthier/0.4.0/bin/luthier",
    ] {
      let binary = PathBuf::from(owned);
      let installation = classify(&binary, false, false);
      assert_eq!(
        installation,
        Installation::ForeignPackage {
          binary: binary.clone()
        }
      );
      assert!(!installation.updates_itself());
      let err = plan(
        &release("v0.5.0"),
        &v("0.4.0"),
        installation,
        &host(false, false),
      )
      .unwrap_err();
      assert!(
        matches!(
          err,
          Error::SelfUpdate(SelfUpdateError::InstalledByPackageManager { .. })
        ),
        "{err}"
      );
    }
  }

  #[test]
  fn the_latest_release_needs_nothing() {
    let plan = plan(
      &release("v0.4.0"),
      &v("0.4.0"),
      Installation::Nix,
      &host(false, false),
    )
    .unwrap();
    assert!(plan.is_current());
    assert!(plan.asset.is_none());
  }

  #[test]
  fn nix_is_left_to_nix() {
    assert!(!Installation::Nix.updates_itself());
    let err = plan(
      &release("v0.5.0"),
      &v("0.4.0"),
      Installation::Nix,
      &host(false, false),
    )
    .unwrap_err();
    assert!(
      matches!(
        err,
        Error::SelfUpdate(SelfUpdateError::InstalledByNix { .. })
      ),
      "{err}"
    );
  }

  #[test]
  fn a_package_goes_to_its_package_manager_through_sudo_unless_root() {
    let release = release("v0.5.0");
    let current = v("0.4.0");

    let deb = plan(&release, &current, Installation::Deb, &host(false, false)).unwrap();
    let name = deb.asset.as_ref().unwrap().name.clone();
    assert_eq!(name, "luthier-x86_64-linux.deb");
    assert_eq!(
      deb.package.unwrap().command_line(&name),
      "sudo apt-get install -y luthier-x86_64-linux.deb"
    );

    let rpm = plan(&release, &current, Installation::Rpm, &host(true, true)).unwrap();
    assert_eq!(
      rpm.package.unwrap().command_line("x.rpm"),
      "dnf install -y x.rpm"
    );

    let zypper = plan(&release, &current, Installation::Rpm, &host(true, false)).unwrap();
    let zypper = zypper.package.unwrap();
    assert_eq!(zypper.manager[0], "zypper");
    assert!(zypper.manager.contains(&"--allow-unsigned-rpm".to_string()));
  }

  #[test]
  fn a_package_install_without_sudo_is_refused_before_the_download() {
    let mut no_sudo = host(false, false);
    no_sudo.has_sudo = false;
    let err = plan(&release("v0.5.0"), &v("0.4.0"), Installation::Deb, &no_sudo).unwrap_err();
    assert!(
      matches!(err, Error::SelfUpdate(SelfUpdateError::CouldNotRun { ref program, .. }) if program == "sudo"),
      "{err}"
    );
    // Root needs no sudo.
    no_sudo.is_root = true;
    plan(&release("v0.5.0"), &v("0.4.0"), Installation::Deb, &no_sudo).unwrap();
  }

  #[test]
  fn a_standalone_binary_takes_the_files_beside_it_that_exist() {
    let prefix = tempfile::tempdir().unwrap();
    let binary = prefix.path().join("bin/luthier");
    let man = prefix.path().join("share/man/man1/luthier.1");
    for file in [&binary, &man] {
      std::fs::create_dir_all(file.parent().unwrap()).unwrap();
      std::fs::write(file, b"old").unwrap();
    }

    let plan = plan(
      &release("v0.5.0"),
      &v("0.4.0"),
      Installation::Standalone {
        binary: binary.clone(),
      },
      &host(false, false),
    )
    .unwrap();
    assert_eq!(plan.asset.unwrap().name, "luthier-x86_64-linux.tar.gz");
    let paths: Vec<_> = plan.replaces.iter().map(|r| r.path.clone()).collect();
    assert_eq!(paths, [binary, man]);
    assert!(plan.package.is_none());
  }

  #[test]
  fn a_standalone_binary_in_a_directory_it_cannot_write_is_refused_up_front() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
      return; // root writes anywhere
    }
    let prefix = tempfile::tempdir().unwrap();
    let bin = prefix.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("luthier"), b"old").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();

    let err = plan(
      &release("v0.5.0"),
      &v("0.4.0"),
      Installation::Standalone {
        binary: bin.join("luthier"),
      },
      &host(false, false),
    )
    .unwrap_err();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
      matches!(err, Error::SelfUpdate(SelfUpdateError::NotWritable { ref dir }) if *dir == bin),
      "{err}"
    );
  }

  #[test]
  fn a_release_without_this_machines_file_says_so() {
    let mut other = host(false, false);
    other.arch = "riscv64".into();
    let err = plan(&release("v0.5.0"), &v("0.4.0"), Installation::Deb, &other).unwrap_err();
    assert!(
      matches!(err, Error::SelfUpdate(SelfUpdateError::NoAsset { .. })),
      "{err}"
    );
  }

  fn script(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("luthier");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
  }

  #[test]
  fn the_new_binary_must_name_exactly_the_expected_version() {
    let dir = tempfile::tempdir().unwrap();
    let timeout = Duration::from_secs(10);
    let good = script(dir.path(), "echo 'luthier 0.5.0'");
    check_runs(&good, &v("0.5.0"), timeout).unwrap();
    // A release candidate is not the release, though it contains its number.
    let candidate = script(dir.path(), "echo 'luthier 0.5.0-rc.1'");
    assert!(check_runs(&candidate, &v("0.5.0"), timeout).is_err());
    let failing = script(dir.path(), "echo 'luthier 0.5.0'; exit 1");
    assert!(check_runs(&failing, &v("0.5.0"), timeout).is_err());
  }

  #[test]
  fn a_new_binary_that_does_not_answer_is_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let hanging = script(dir.path(), "exec sleep 30");
    let started = Instant::now();
    let err = check_runs(&hanging, &v("0.5.0"), Duration::from_millis(200)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(10), "{err}");
    assert!(err.to_string().contains("did not answer"), "{err}");
  }

  /// A package manager stand-in that copies the package it is given to
  /// `out`, and writes where it was given it to `out.from`, so the test sees
  /// what root would have installed and from where.
  fn copying_manager(out: &Path) -> PackageInstall {
    PackageInstall {
      sudo: false,
      manager: vec![
        "/bin/sh".into(),
        "-c".into(),
        r#"cp -- "$1" "$0" && printf %s "$1" > "$0.from""#.into(),
        out.to_string_lossy().into_owned(),
      ],
    }
  }

  #[test]
  fn the_package_installed_is_a_copy_checked_again_after_it_is_out_of_reach() {
    let dir = tempfile::tempdir().unwrap();
    let download = dir.path().join("download");
    std::fs::write(&download, b"package bytes").unwrap();
    let sha256 = fsutil::hash_file(&download).unwrap();
    let out = dir.path().join("installed.deb");

    install_package(
      &copying_manager(&out),
      &download,
      &sha256,
      "luthier-x86_64-linux.deb",
    )
    .unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"package bytes");
    // Given a copy under its own name, in a directory removed afterwards;
    // never the download itself.
    let given =
      PathBuf::from(std::fs::read_to_string(dir.path().join("installed.deb.from")).unwrap());
    assert_ne!(given, download);
    assert_eq!(given.file_name().unwrap(), "luthier-x86_64-linux.deb");
    assert!(!given.parent().unwrap().exists());

    // Changed after it was verified: the copy fails the second check and
    // the package manager never runs.
    std::fs::write(&download, b"something else").unwrap();
    std::fs::remove_file(&out).unwrap();
    let err = install_package(
      &copying_manager(&out),
      &download,
      &sha256,
      "luthier-x86_64-linux.deb",
    )
    .unwrap_err();
    assert!(
      matches!(
        err,
        Error::SelfUpdate(SelfUpdateError::ChangedBeforeInstall { .. })
      ),
      "{err}"
    );
    assert_eq!(err.exit_code(), crate::error::ExitCode::VerificationFailed);
    assert!(!out.exists());
  }

  #[test]
  fn a_package_manager_that_refuses_fails_the_update_with_its_status() {
    let dir = tempfile::tempdir().unwrap();
    let download = dir.path().join("download");
    std::fs::write(&download, b"package bytes").unwrap();
    let sha256 = fsutil::hash_file(&download).unwrap();
    let refusing = PackageInstall {
      sudo: false,
      manager: vec!["/bin/sh".into(), "-c".into(), "exit 100".into()],
    };
    let err = install_package(&refusing, &download, &sha256, "x.deb").unwrap_err();
    assert!(
      matches!(
        err,
        Error::SelfUpdate(SelfUpdateError::CommandFailed {
          code: Some(100),
          ..
        })
      ),
      "{err}"
    );
  }
}
