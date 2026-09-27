//! The error model.
//!
//! Every failure the manager can produce is described here, in one place, so
//! that the mapping to exit codes (§34) and to user-facing hints (§36) stays
//! coherent. Errors carry the specifics a user needs to act — the URL, the HTTP
//! status, the two digests that differed — rather than collapsing to
//! "installation failed".

use luthier_manifest::{Format, PackageId, Sha256Hash, Target};
use semver::{Version, VersionReq};
use std::path::PathBuf;

/// Process exit statuses. Documented in `docs/EXIT_CODES.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
  Success = 0,
  Generic = 1,
  InvalidArguments = 2,
  PackageNotFound = 3,
  VerificationFailed = 4,
  InstallationFailed = 5,
  DependencyResolutionFailed = 6,
}

impl From<ExitCode> for std::process::ExitCode {
  fn from(code: ExitCode) -> Self {
    std::process::ExitCode::from(code as u8)
  }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything that can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error("{operation} failed for {path}: {source}")]
  Io {
    operation: &'static str,
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },

  #[error(transparent)]
  Layout(#[from] crate::layout::LayoutError),

  // Discovery lives in `luthier-manifest` so validation needs no runtime;
  // its failures are ordinary I/O and are reported as such.
  #[error("{0}")]
  Discovery(#[from] luthier_manifest::DiscoveryError),

  #[error(transparent)]
  Manifest(#[from] luthier_manifest::ParseError),

  #[error(transparent)]
  Registry(#[from] RegistryError),

  #[error(transparent)]
  Download(#[from] DownloadError),

  #[error(transparent)]
  Archive(#[from] ArchiveError),

  #[error(transparent)]
  Install(#[from] InstallError),

  #[error(transparent)]
  Resolve(#[from] ResolveError),

  #[error(transparent)]
  State(#[from] StateError),

  #[error(
    "{failed} of {total} installed package(s) no longer match what was recorded at install time"
  )]
  VerificationFailed { failed: usize, total: usize },

  #[error("{0}")]
  InvalidArgument(String),

  /// A directory the user chose for `kind` is not there. Most likely a disk
  /// that is not mounted, which is why nothing creates it instead.
  #[error("the {kind} location {} does not exist or is not a directory", path.display())]
  LocationUnavailable {
    kind: crate::layout::LocationKind,
    path: PathBuf,
  },

  #[error("operation cancelled")]
  Cancelled,
}

impl Error {
  /// Convenience for attaching path context to an I/O failure.
  pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
    Error::Io {
      operation,
      path: path.into(),
      source,
    }
  }

  pub fn exit_code(&self) -> ExitCode {
    match self {
      Error::Resolve(e) => e.exit_code(),
      Error::Registry(RegistryError::PackageNotFound { .. }) => ExitCode::PackageNotFound,
      Error::Download(DownloadError::ChecksumMismatch { .. }) => ExitCode::VerificationFailed,
      Error::VerificationFailed { .. } => ExitCode::VerificationFailed,
      Error::Archive(_) | Error::Install(_) => ExitCode::InstallationFailed,
      // Refusing to break another package's dependency is a dependency
      // failure, not a generic one, so scripts can tell them apart.
      Error::State(StateError::StillRequired { .. }) => ExitCode::DependencyResolutionFailed,
      Error::InvalidArgument(_) => ExitCode::InvalidArguments,
      _ => ExitCode::Generic,
    }
  }

  /// A follow-up line telling the user what to do about it.
  pub fn hint(&self) -> Option<String> {
    match self {
      Error::Manifest(e) => e.hint(),
      Error::Registry(e) => e.hint(),
      Error::Download(e) => e.hint(),
      Error::Archive(e) => e.hint(),
      Error::Install(e) => e.hint(),
      Error::Resolve(e) => e.hint(),
      Error::State(e) => e.hint(),
      Error::VerificationFailed { .. } => Some(
        "Files may have been changed or removed outside Luthier. Reinstall the \
                 affected packages with `luthier install --force <package>`."
          .into(),
      ),
      Error::LocationUnavailable { kind, .. } => Some(format!(
        "If it is on an external disk, mount the disk and try again. To go back \
         to the default location, run `luthier location reset {kind}`."
      )),
      _ => None,
    }
  }
}

// ---------------------------------------------------------------- registry --

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
  #[error("package {id} is not in the {registry} registry")]
  PackageNotFound { id: String, registry: String },

  #[error("no registry named {0:?} is configured")]
  NoSuchRegistry(String),

  #[error("registry {0:?} has not been fetched yet")]
  NotFetched(String),

  #[error("registry {registry} defines package {id} twice, in {first} and {second}")]
  DuplicatePackage {
    registry: String,
    id: PackageId,
    first: PathBuf,
    second: PathBuf,
  },

  #[error("manifest {path} declares id {declared} but is filed as {expected}")]
  IdFilenameMismatch {
    path: PathBuf,
    declared: PackageId,
    expected: String,
  },

  #[error("registry snapshot at {0} is not a directory")]
  NotADirectory(PathBuf),

  #[error("registry {registry} sent something this build cannot read: {reason}")]
  Malformed { registry: String, reason: String },
}

impl RegistryError {
  fn hint(&self) -> Option<String> {
    match self {
      RegistryError::PackageNotFound { .. } => Some(
        "Run `luthier refresh` to update the registry, or `luthier search <term>` to find \
                 the right package ID."
          .into(),
      ),
      RegistryError::NotFetched(_) => Some("Run `luthier refresh` first.".into()),
      RegistryError::IdFilenameMismatch { .. } => {
        Some("Each manifest must be filed as <id>.toml.".into())
      }
      _ => None,
    }
  }
}

// ---------------------------------------------------------------- download --

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
  #[error("failed to download {url}\n\nReason:\nHTTP {status}")]
  HttpStatus { url: String, status: u16 },

  #[error("failed to download {url}\n\nReason:\n{reason}")]
  Transport { url: String, reason: String },

  #[error("checksum verification failed for {url}\n\nExpected:\n{expected}\n\nReceived:\n{actual}")]
  ChecksumMismatch {
    url: String,
    expected: Sha256Hash,
    actual: Sha256Hash,
  },

  #[error("{url} returned {actual} bytes but the manifest declares {expected}")]
  SizeMismatch {
    url: String,
    expected: u64,
    actual: u64,
  },

  #[error("{url} is larger than the {limit} byte download limit")]
  TooLarge { url: String, limit: u64 },

  #[error("cannot fetch {url} while running in offline mode")]
  Offline { url: String },

  #[error("unsupported URL scheme {scheme:?} in {url}")]
  UnsupportedScheme { url: String, scheme: String },

  #[error("{url} does not name a readable local file")]
  BadFileUrl { url: String },
}

impl DownloadError {
  fn hint(&self) -> Option<String> {
    match self {
      DownloadError::HttpStatus { status: 404, .. } => Some(
        "The package manifest may reference a release that has been moved or withdrawn. \
                 Run `luthier refresh`, and report the package if the problem persists."
          .into(),
      ),
      DownloadError::HttpStatus { status, .. } if *status >= 500 => {
        Some("The server is having trouble. Try again shortly.".into())
      }
      DownloadError::ChecksumMismatch { .. } => Some(
        "Installation aborted; nothing was written. This means the download did not \
                 match what the registry expects, either because the file was corrupted in \
                 transit or because upstream replaced the release."
          .into(),
      ),
      DownloadError::Offline { .. } => {
        Some("Drop --offline, or install from a cached artifact.".into())
      }
      _ => None,
    }
  }
}

// ----------------------------------------------------------------- archive --

/// Why an archive entry was refused.
///
/// Split out from the message so the security tests can assert on the precise
/// reason rather than on wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsafeEntry {
  /// `../` or an absolute path: would write outside the extraction root.
  PathEscape,
  /// A `.` component, `//`, a NUL, a backslash or a drive prefix.
  MalformedPath,
  /// Symlinks are refused outright; a link is the classic way to redirect a
  /// later write outside the root.
  Symlink,
  /// A hard link naming something this archive has not already written.
  /// Such a link could alias a file outside the root; one that names an
  /// earlier entry is copied instead.
  HardLink,
  /// Device nodes, FIFOs and sockets have no business in a plugin archive.
  SpecialFile,
  /// The same name twice: the second write would clobber the first, and
  /// which one wins depends on extraction order.
  Duplicate,
  /// A parent component already exists as a symlink.
  SymlinkedParent,
}

impl std::fmt::Display for UnsafeEntry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      UnsafeEntry::PathEscape => "it points outside the extraction directory",
      UnsafeEntry::MalformedPath => "its path is malformed",
      UnsafeEntry::Symlink => "it is a symbolic link",
      UnsafeEntry::HardLink => "it is a hard link to something this archive has not written",
      UnsafeEntry::SpecialFile => "it is not a regular file or directory",
      UnsafeEntry::Duplicate => "the archive contains that name more than once",
      UnsafeEntry::SymlinkedParent => "one of its parent directories is a symbolic link",
    })
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
  #[error("refusing to extract {name:?} because {reason}")]
  Unsafe { name: String, reason: UnsafeEntry },

  #[error("archive format {0} is not supported by this build")]
  UnsupportedFormat(String),

  #[error("archive declares format {declared} but its contents look like {detected}")]
  FormatMismatch { declared: String, detected: String },

  #[error("archive contains more than {limit} entries")]
  TooManyEntries { limit: usize },

  #[error("archive expands to more than {limit} bytes")]
  TooLarge { limit: u64 },

  #[error("entry {name:?} expands to more than {limit} bytes")]
  EntryTooLarge { name: String, limit: u64 },

  #[error("archive is corrupt: {0}")]
  Corrupt(String),

  #[error("the archive does not contain {path:?}")]
  MissingEntry { path: String },

  #[error("{operation} failed for {path}: {source}")]
  Io {
    operation: &'static str,
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },
}

impl ArchiveError {
  fn hint(&self) -> Option<String> {
    match self {
      ArchiveError::Unsafe { .. } => Some(
        "Downloaded archives are treated as untrusted. Nothing was written outside the \
                 temporary directory. Please report this package to the registry."
          .into(),
      ),
      ArchiveError::UnsupportedFormat(f) if f == "7z" => Some(
        "7z archives cannot be extracted yet, so packages that publish only 7z Linux \
                 binaries are not listed in the registry."
          .into(),
      ),
      ArchiveError::MissingEntry { .. } => Some(
        "The manifest's install rules do not match the archive's layout; the package \
                 needs updating in the registry."
          .into(),
      ),
      _ => None,
    }
  }
}

// ----------------------------------------------------------------- install --

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
  #[error("no installer for format {0}")]
  NoInstaller(Format),

  #[error("{path} is not a valid {format} plugin: {reason}")]
  FailedValidation {
    format: Format,
    path: PathBuf,
    reason: String,
  },

  #[error("{path} already exists and was not installed by Luthier")]
  UnmanagedConflict { path: PathBuf, package: PackageId },

  #[error("{id} {version} is already installed")]
  AlreadyInstalled { id: PackageId, version: Version },

  /// The archive verified and extracted, and held nothing this build can
  /// install. Recording the package as installed with no files would hide
  /// that, so it is an error — the archive's `contains` and its contents
  /// disagree, which is upstream data to fix rather than something to
  /// work around here.
  #[error("{id} {version}: the archive contains no CLAP, VST3 or LV2 plugin to install")]
  NothingToInstall { id: PackageId, version: Version },

  #[error("refusing to write to {path}, which is outside every managed directory")]
  OutsideManagedRoot { path: PathBuf },

  #[error("{operation} failed for {path}: {source}")]
  Io {
    operation: &'static str,
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },

  #[error(
    "not enough room on the filesystem holding {path}: {required} bytes needed, {available} free"
  )]
  NotEnoughSpace {
    path: PathBuf,
    required: u64,
    available: u64,
  },

  /// Replacing the package would delete what someone changed or added in
  /// its files since it was installed.
  #[error("{id} has changed since it was installed: {}", changes.join("; "))]
  LocalChanges { id: PackageId, changes: Vec<String> },

  #[error("rolled back after a failure: {0}")]
  RolledBack(String),
}

impl InstallError {
  fn hint(&self) -> Option<String> {
    match self {
      InstallError::UnmanagedConflict { path, package } => Some(format!(
        "Luthier will not overwrite an installation it did not create.\n\
                 Remove or rename {} yourself and run `luthier install {package}` again.",
        path.display()
      )),
      InstallError::AlreadyInstalled { id, .. } => Some(format!(
        "Use `luthier remove {id}` first, or `luthier update {id}`."
      )),
      InstallError::FailedValidation { .. } => Some(
        "The downloaded artifact does not contain what the manifest promised; the \
                 package needs updating in the registry."
          .into(),
      ),
      InstallError::LocalChanges { id, .. } => Some(format!(
        "Replacing it would delete those changes. Copy anything you want to keep \
                 somewhere else, then run `luthier install --force {id}`."
      )),
      InstallError::NotEnoughSpace { .. } => Some(
        "The estimate allows for the archive, the extracted copy and the installed \
                 files existing at once. Free some space, or run `luthier cache clean`."
          .into(),
      ),
      _ => None,
    }
  }
}

// ---------------------------------------------------------------- resolver --

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
  #[error("package {0} not found in any configured registry")]
  NotFound(PackageId),

  #[error("no release of {id} satisfies {req}")]
  NoMatchingVersion { id: PackageId, req: VersionReq },

  #[error("{id} has no release for {target}")]
  NoReleaseForTarget { id: PackageId, target: Target },

  #[error("{id} {version} has no artifact for {target}")]
  NoArtifactForTarget {
    id: PackageId,
    version: Version,
    target: Target,
  },

  /// The release publishes something for this target, and none of it is
  /// anything this build installs. Raised before the download, which is the
  /// point: the alternative is fetching a standalone program or a VST2 build
  /// in full and refusing it afterwards.
  #[error("{id} publishes nothing for {target} that this build can install")]
  NothingInstallable {
    id: PackageId,
    target: Target,
    /// What the artifact says it holds. Empty means it says nothing.
    declared: Vec<Format>,
  },

  #[error("dependency cycle: {}", .0.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" -> "))]
  Cycle(Vec<PackageId>),

  #[error(
        "cannot satisfy {id}: {} require incompatible versions",
        .requirers.iter().map(|(who, req)| format!("{who} needs {req}")).collect::<Vec<_>>().join(", ")
    )]
  Conflict {
    id: PackageId,
    requirers: Vec<(PackageId, VersionReq)>,
  },

  #[error("{id} is required but is not installed")]
  ExternalMissing {
    id: PackageId,
    provisioning_hint: Option<String>,
  },

  /// An imported environment file named a version the registry cannot
  /// supply. Falling back to another version would defeat the point of a
  /// pinned export, so this is fatal (§51).
  #[error("{id} {version} is not in the registry, so the environment cannot be reproduced")]
  RequiredVersionMissing { id: PackageId, version: Version },

  #[error("{id} is pinned to {pinned} but {requirer} needs {req}")]
  PinConflict {
    id: PackageId,
    pinned: Version,
    requirer: PackageId,
    req: VersionReq,
  },
}

impl ResolveError {
  fn exit_code(&self) -> ExitCode {
    match self {
      ResolveError::NotFound(_) => ExitCode::PackageNotFound,
      _ => ExitCode::DependencyResolutionFailed,
    }
  }

  fn hint(&self) -> Option<String> {
    match self {
      ResolveError::NotFound(id) => Some(format!(
        "Try `luthier search {id}` to find the right package ID."
      )),
      ResolveError::ExternalMissing {
        provisioning_hint, ..
      } => provisioning_hint.clone().or(Some(
        "This package has no redistributable Linux binary, so Luthier can detect \
                     it but cannot install it."
          .into(),
      )),
      ResolveError::NothingInstallable { declared, .. } => Some(match declared.as_slice() {
        [] => "Its metadata does not say what the archive holds, so the install rules \
               would have to be read out of it — and a release that says nothing is \
               usually a standalone program or a VST2 build. Neither is something this \
               manager installs; your distribution's package manager is the answer for \
               the first, and there is no second answer for VST2."
          .into(),
        other => format!(
          "It declares {}, and rules read out of an archive can only cover {}.",
          other
            .iter()
            .map(Format::to_string)
            .collect::<Vec<_>>()
            .join(", "),
          crate::install::derive::DERIVABLE_FORMATS
            .iter()
            .map(Format::to_string)
            .collect::<Vec<_>>()
            .join(", ")
        ),
      }),
      ResolveError::Cycle(_) => {
        Some("This is a registry bug; please report the packages involved.".into())
      }
      ResolveError::PinConflict { id, .. } => {
        Some(format!("Use `luthier unpin {id}` to lift the pin."))
      }
      ResolveError::RequiredVersionMissing { .. } => Some(
        "Run `luthier refresh`; if the version is genuinely gone, re-export from a \
                 machine that still has it, or import with a `--loose` file."
          .into(),
      ),
      _ => None,
    }
  }
}

// ------------------------------------------------------------------- state --

#[derive(Debug, thiserror::Error)]
pub enum StateError {
  #[error("installation state at {path} is unreadable: {reason}")]
  Corrupt { path: PathBuf, reason: String },

  #[error("another Luthier process is already running")]
  Locked,

  /// An exported environment file could not be read or used.
  #[error("{source_name}: {reason}")]
  EnvFile { source_name: String, reason: String },

  #[error("{id} is not installed")]
  NotInstalled { id: PackageId },

  #[error(
        "{id} is required by {}",
        .dependents.iter().map(|d| d.as_str()).collect::<Vec<_>>().join(", ")
    )]
  StillRequired {
    id: PackageId,
    dependents: Vec<PackageId>,
  },

  #[error("{operation} failed for {path}: {source}")]
  Io {
    operation: &'static str,
    path: PathBuf,
    #[source]
    source: std::io::Error,
  },
}

impl StateError {
  fn hint(&self) -> Option<String> {
    match self {
      StateError::Corrupt { path, .. } => Some(format!(
        "A previous copy may be available at {}.bak.",
        path.display()
      )),
      StateError::Locked => {
        Some("Wait for it to finish, or remove the lock file if no process is running.".into())
      }
      StateError::StillRequired { dependents, .. } => Some(format!(
        "Remove {} first, or keep this package.",
        dependents
          .iter()
          .map(|d| d.as_str())
          .collect::<Vec<_>>()
          .join(" and ")
      )),
      StateError::NotInstalled { .. } => {
        Some("Run `luthier list` to see what is installed.".into())
      }
      StateError::EnvFile { .. } => Some(
        "Environment files are written by `luthier env export`; see docs/ENVIRONMENTS.md \
                 for the format."
          .into(),
      ),
      StateError::Io { .. } => None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn exit_codes_match_the_documented_table() {
    let id = PackageId::new("surge-xt").unwrap();
    let cases: Vec<(Error, ExitCode)> = vec![
      (
        RegistryError::PackageNotFound {
          id: "x".into(),
          registry: "r".into(),
        }
        .into(),
        ExitCode::PackageNotFound,
      ),
      (
        ResolveError::NotFound(id.clone()).into(),
        ExitCode::PackageNotFound,
      ),
      (
        DownloadError::ChecksumMismatch {
          url: "u".into(),
          expected: Sha256Hash::from_bytes([0; 32]),
          actual: Sha256Hash::from_bytes([1; 32]),
        }
        .into(),
        ExitCode::VerificationFailed,
      ),
      (
        ArchiveError::Unsafe {
          name: "../x".into(),
          reason: UnsafeEntry::PathEscape,
        }
        .into(),
        ExitCode::InstallationFailed,
      ),
      (
        InstallError::NoInstaller(Format::Lv2).into(),
        ExitCode::InstallationFailed,
      ),
      (
        ResolveError::Cycle(vec![id]).into(),
        ExitCode::DependencyResolutionFailed,
      ),
      (
        Error::InvalidArgument("bad".into()),
        ExitCode::InvalidArguments,
      ),
      (
        Error::VerificationFailed {
          failed: 1,
          total: 2,
        },
        ExitCode::VerificationFailed,
      ),
      (
        StateError::StillRequired {
          id: PackageId::new("sfizz").unwrap(),
          dependents: vec![PackageId::new("vsco2").unwrap()],
        }
        .into(),
        ExitCode::DependencyResolutionFailed,
      ),
    ];
    for (error, expected) in cases {
      assert_eq!(error.exit_code(), expected, "wrong code for {error}");
    }
  }

  #[test]
  fn a_404_explains_what_it_probably_means() {
    let error: Error = DownloadError::HttpStatus {
      url: "https://x.invalid/a.tar.gz".into(),
      status: 404,
    }
    .into();
    let text = error.to_string();
    assert!(text.contains("https://x.invalid/a.tar.gz"), "{text}");
    assert!(text.contains("HTTP 404"), "{text}");
    assert!(error.hint().unwrap().contains("moved or withdrawn"));
  }

  #[test]
  fn a_checksum_mismatch_shows_both_digests() {
    let expected = Sha256Hash::from_bytes([0xab; 32]);
    let actual = Sha256Hash::from_bytes([0xcd; 32]);
    let error: Error = DownloadError::ChecksumMismatch {
      url: "u".into(),
      expected,
      actual,
    }
    .into();
    let text = error.to_string();
    assert!(text.contains(&expected.to_string()), "{text}");
    assert!(text.contains(&actual.to_string()), "{text}");
    assert!(error.hint().unwrap().contains("nothing was written"));
  }

  #[test]
  fn refusing_to_clobber_an_unmanaged_plugin_says_what_to_do() {
    let error: Error = InstallError::UnmanagedConflict {
      path: "/home/u/.clap/Surge XT.clap".into(),
      package: PackageId::new("surge-xt").unwrap(),
    }
    .into();
    let hint = error.hint().unwrap();
    assert!(hint.contains("will not overwrite"), "{hint}");
    assert!(hint.contains("luthier install surge-xt"), "{hint}");
  }
}
