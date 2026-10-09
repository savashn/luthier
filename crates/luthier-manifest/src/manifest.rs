//! The v1 package manifest.

use crate::hash::Checksum;
use crate::id::PackageId;
use crate::license::License;
use crate::macros::string_enum;
use crate::path::{ArchivePath, FileName};
use crate::types::{
  AllowedWarning, ArchiveFormat, Category, Content, EntryKind, Format, PackageKind, Target,
};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

/// The schema revision this build writes and fully understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Fields present in the document that this build has no name for.
///
/// Keeping them rather than erroring is what makes §7 forward compatibility
/// real: an `luthier` binary built today keeps working against a registry that
/// has begun emitting v1.1 fields. The registry's own CI runs in strict mode
/// (see [`crate::validate`]) so a contributor's typo is still caught.
pub type Extra = BTreeMap<String, serde_json::Value>;

/// Walks the document collecting the dotted paths of unrecognised fields.
pub trait UnknownFields {
  fn collect_unknown(&self, path: &str, out: &mut Vec<String>);
}

fn note(extra: &Extra, path: &str, out: &mut Vec<String>) {
  for key in extra.keys() {
    out.push(if path.is_empty() {
      key.clone()
    } else {
      format!("{path}.{key}")
    });
  }
}

fn child(path: &str, segment: &str) -> String {
  if path.is_empty() {
    segment.to_owned()
  } else {
    format!("{path}.{segment}")
  }
}

/// A package as described by the registry.
///
/// Static identity and metadata live at the top level; everything that changes
/// between versions lives in [`Release`]. Keeping every release in one document
/// is what lets `luthier pin <pkg> <old-version>` and future lock files resolve
/// to something that still exists (§25, §27, §51).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Manifest {
  /// Manifest schema revision. See [`SCHEMA_VERSION`].
  pub schema: u32,
  /// Immutable identifier. Never contains a version (§5).
  pub id: PackageId,
  /// Display name, e.g. `Surge XT`.
  pub name: String,
  /// What this package fundamentally is.
  pub kind: PackageKind,
  /// The single primary classification. Required: every package is filed
  /// under exactly one, from a closed vocabulary (§5).
  pub category: Category,
  /// Free-form refinements used by `search`, e.g. `synthesizer`, `fm`.
  ///
  /// Unlike [`Manifest::category`] this stays open, because the useful
  /// descriptors are long-tailed and a controlled list would either be
  /// enormous or block contributions.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tags: Vec<String>,
  /// What the package holds that only an engine can play, e.g. `sfz`.
  ///
  /// Only meaningful for [`PackageKind::Library`]. Content nothing on the
  /// system can play is reported before the download rather than refused;
  /// which packages count as engines for each is the built-in engine list
  /// plus any source's `engines.toml`, not this manifest. Named after the
  /// format rather than an engine because a kit that DrumCraker plays as
  /// well as DrumGizmo needs neither by name.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub content: Vec<Content>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  #[schemars(with = "Option<String>")]
  pub homepage: Option<Url>,
  /// Where the source lives. Note this is frequently *not* where the release
  /// artifacts live — Surge XT builds from `surge-synthesizer/surge` but
  /// publishes stable artifacts from `surge-synthesizer/releases-xt`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  #[schemars(with = "Option<String>")]
  pub repository: Option<Url>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  #[schemars(with = "Option<String>")]
  pub documentation: Option<Url>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub authors: Vec<String>,
  pub license: License,
  /// Known releases. Order in the file is not significant; the resolver
  /// sorts by version so results do not depend on how the file was written.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub releases: Vec<Release>,
  /// How to recognise an `external` package that is already present.
  ///
  /// Only meaningful for [`PackageKind::External`]: software with no
  /// redistributable Linux binary, which the manager can depend on and
  /// detect but never downloads or installs.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub detect: Vec<DetectRule>,
  /// Where to obtain an `external` package, shown when it is missing.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub provisioning_hint: Option<String>,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

/// One published version of a package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Release {
  #[schemars(with = "String")]
  pub version: Version,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  #[schemars(with = "Option<String>")]
  pub release_notes: Option<Url>,
  /// Packages that must be installed for this one to work.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub dependencies: Vec<Dependency>,
  /// Packages that enhance this one but are not required.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub optional_dependencies: Vec<Dependency>,
  /// Downloadable artifacts, one or more per target.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub artifacts: Vec<Artifact>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub yanked: Option<String>,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

impl Release {
  /// Artifacts built for `target`, in declaration order.
  pub fn artifacts_for(&self, target: &Target) -> impl Iterator<Item = &Artifact> {
    self.artifacts.iter().filter(move |a| &a.target == target)
  }

  /// Whether this release should be skipped by normal resolution.
  pub fn is_yanked(&self) -> bool {
    self.yanked.is_some()
  }
}

/// A single downloadable file and the rules for what to take out of it.
///
/// A release carries a *list* of artifacts rather than a map keyed by format,
/// because upstream reality does not line up one-archive-per-format: Surge XT
/// ships a single tarball containing both its CLAP and its VST3, and also
/// publishes a much larger tarball with the full content set. The manifest
/// picks which one to use; the client never guesses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Artifact {
  pub target: Target,
  pub source: Source,
  /// Container format. Also sniffed from the downloaded bytes; a mismatch is
  /// an error rather than something we silently work around.
  pub archive: ArchiveFormat,
  /// Expected size in bytes, used for progress reporting and as an early
  /// mismatch signal. Advisory: the checksum is what decides.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub size: Option<u64>,
  pub checksum: Checksum,
  /// Formats this artifact can deliver.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub provides: Vec<Format>,
  /// What to extract and install. Declarative only — there is deliberately
  /// no way to express "run this script" (§42).
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub install: Vec<InstallRule>,
  /// Read the install rules from the archive instead of from here.
  ///
  /// Set by a registry provider whose source carries no rules — the Open
  /// Audio Stack registry says which *formats* an archive holds, never which
  /// *entry* is which. A manifest may not request it: `validate` refuses the
  /// field, because extras earns its precedence by carrying rules a
  /// person looked at.
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub derive_install: bool,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

impl Artifact {
  /// Whether this artifact says how to install itself — either by declaring
  /// rules, or by deferring them to the archive its checksum covers.
  ///
  /// An artifact that says neither is not a release for the target: §17
  /// stops at the manifest, so there is nothing left to act on.
  pub fn is_installable(&self) -> bool {
    self.derive_install || !self.install.is_empty()
  }
}

string_enum! {
    /// How an artifact is fetched.
    pub enum SourceType {
        /// Plain HTTP(S) download. Covers GitHub and GitLab release assets,
        /// which are ordinary URLs, as well as vendor download pages.
        Http => "http",
        /// A `file://` URL. Used by tests and by local registries so the
        /// integration suite never depends on a live release (§55).
        File => "file",
    }
}

/// Where an artifact comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Source {
  #[serde(rename = "type")]
  pub kind: SourceType,
  #[schemars(with = "String")]
  pub url: Url,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

/// One thing to take out of an artifact and install.
///
/// Deliberately absent: a `destination` field. §17's example writes an absolute
/// destination into the manifest, which would hand any registry contributor an
/// arbitrary-write primitive against the user's home directory. The
/// destination is derived by the installer from `format` plus the resolved
/// [`Layout`](../../luthier_core/layout/struct.Layout.html); `rename` can change the
/// leaf name and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InstallRule {
  /// Which plugin format this produces, selecting the destination root.
  pub format: Format,
  /// Path inside the extracted archive.
  pub source: ArchivePath,
  /// Whether `source` is a file or a bundle directory.
  pub kind: EntryKind,
  /// Optional replacement leaf name. A single filename, never a path.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub rename: Option<FileName>,
  /// Warnings this rule deliberately accepts, so that `--strict` stays usable.
  ///
  /// Never a safety escape: the values name naming conventions, and there is
  /// no value that relaxes a check on where a file may be written.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub allow: Vec<AllowedWarning>,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

impl InstallRule {
  /// The leaf name this rule installs as.
  pub fn installed_name(&self) -> &str {
    self
      .rename
      .as_ref()
      .map(FileName::as_str)
      .unwrap_or_else(|| self.source.file_name())
  }
}

/// A dependency on another package.
///
/// Accepts both the shorthand `- sfizz` and the full `- {id: sfizz, version:
/// ">=1.2, <2.0"}` (§22). Version requirements are optional in v1; when absent
/// any release satisfies the edge.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Dependency {
  pub id: PackageId,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  #[schemars(with = "Option<String>")]
  pub version: Option<VersionReq>,
  #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
  #[schemars(skip)]
  pub extra: Extra,
}

impl Dependency {
  pub fn new(id: PackageId) -> Self {
    Self {
      id,
      version: None,
      extra: Extra::new(),
    }
  }

  /// Whether `version` satisfies this dependency.
  pub fn matches(&self, version: &Version) -> bool {
    self.version.as_ref().is_none_or(|req| req.matches(version))
  }
}

impl<'de> Deserialize<'de> for Dependency {
  fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
    #[derive(Deserialize)]
    struct Full {
      id: PackageId,
      #[serde(default)]
      version: Option<VersionReq>,
      #[serde(flatten)]
      extra: Extra,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
      Shorthand(PackageId),
      Full(Full),
    }

    Ok(match Either::deserialize(de)? {
      Either::Shorthand(id) => Dependency::new(id),
      Either::Full(f) => Dependency {
        id: f.id,
        version: f.version,
        extra: f.extra,
      },
    })
  }
}

/// How to recognise an already-present `external` package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DetectRule {
  /// Which format root to look in.
  pub format: Format,
  /// The entry to look for, e.g. `sfizz.vst3`.
  pub name: FileName,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

impl UnknownFields for Manifest {
  fn collect_unknown(&self, path: &str, out: &mut Vec<String>) {
    note(&self.extra, path, out);
    for (i, release) in self.releases.iter().enumerate() {
      release.collect_unknown(&child(path, &format!("releases[{i}]")), out);
    }
    for (i, rule) in self.detect.iter().enumerate() {
      note(&rule.extra, &child(path, &format!("detect[{i}]")), out);
    }
  }
}

impl UnknownFields for Release {
  fn collect_unknown(&self, path: &str, out: &mut Vec<String>) {
    note(&self.extra, path, out);
    for (i, artifact) in self.artifacts.iter().enumerate() {
      artifact.collect_unknown(&child(path, &format!("artifacts[{i}]")), out);
    }
    for (label, deps) in [
      ("dependencies", &self.dependencies),
      ("optional_dependencies", &self.optional_dependencies),
    ] {
      for (i, dep) in deps.iter().enumerate() {
        note(&dep.extra, &child(path, &format!("{label}[{i}]")), out);
      }
    }
  }
}

impl UnknownFields for Artifact {
  fn collect_unknown(&self, path: &str, out: &mut Vec<String>) {
    note(&self.extra, path, out);
    note(&self.source.extra, &child(path, "source"), out);
    for (i, rule) in self.install.iter().enumerate() {
      note(&rule.extra, &child(path, &format!("install[{i}]")), out);
    }
  }
}

impl Manifest {
  /// The highest non-yanked release, if any.
  pub fn latest_release(&self) -> Option<&Release> {
    self
      .releases
      .iter()
      .filter(|r| !r.is_yanked())
      .max_by(|a, b| a.version.cmp(&b.version))
  }

  /// A specific release by exact version.
  pub fn release(&self, version: &Version) -> Option<&Release> {
    self.releases.iter().find(|r| &r.version == version)
  }

  /// Releases sorted newest-first, ignoring yanked ones.
  ///
  /// Sorting here rather than trusting file order is what makes resolution
  /// deterministic regardless of how a contributor ordered the YAML (§57).
  pub fn releases_newest_first(&self) -> Vec<&Release> {
    let mut all: Vec<&Release> = self.releases.iter().filter(|r| !r.is_yanked()).collect();
    all.sort_by(|a, b| b.version.cmp(&a.version));
    all
  }

  /// Every format any release offers for `target`.
  pub fn formats_for(&self, target: &Target) -> Vec<Format> {
    let mut formats: Vec<Format> = self
      .releases
      .iter()
      .flat_map(|r| r.artifacts_for(target))
      .flat_map(|a| a.provides.iter().cloned())
      .collect();
    formats.sort();
    formats.dedup();
    formats
  }

  /// Whether this package is detected rather than installed.
  pub fn is_external(&self) -> bool {
    self.kind == PackageKind::External
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn dependency_accepts_both_shorthand_and_full_forms() {
    #[derive(serde::Deserialize)]
    struct Wrapper {
      deps: Vec<Dependency>,
    }
    let shorthand: Vec<Dependency> =
      toml::from_str::<Wrapper>("deps = [\"sfizz\", \"lsp-plugins\"]\n")
        .unwrap()
        .deps;
    assert_eq!(shorthand.len(), 2);
    assert_eq!(shorthand[0].id.as_str(), "sfizz");
    assert!(shorthand[0].version.is_none());

    let full: Vec<Dependency> =
      toml::from_str::<Wrapper>("deps = [{ id = \"sfizz\", version = \">=1.2, <2.0\" }]\n")
        .unwrap()
        .deps;
    assert_eq!(full[0].id.as_str(), "sfizz");
    assert!(full[0].matches(&Version::new(1, 5, 0)));
    assert!(!full[0].matches(&Version::new(2, 0, 0)));
  }

  #[test]
  fn dependency_without_a_requirement_matches_anything() {
    let dep = Dependency::new(PackageId::new("sfizz").unwrap());
    assert!(dep.matches(&Version::new(0, 1, 0)));
    assert!(dep.matches(&Version::new(9, 9, 9)));
  }

  #[test]
  fn install_rule_leaf_name_honours_rename() {
    let rule = InstallRule {
      format: Format::Clap,
      source: ArchivePath::new("lib/Surge XT.clap").unwrap(),
      kind: EntryKind::File,
      rename: None,
      allow: Vec::new(),
      extra: Extra::new(),
    };
    assert_eq!(rule.installed_name(), "Surge XT.clap");

    let renamed = InstallRule {
      rename: Some(FileName::new("Surge.clap").unwrap()),
      ..rule
    };
    assert_eq!(renamed.installed_name(), "Surge.clap");
  }
}
