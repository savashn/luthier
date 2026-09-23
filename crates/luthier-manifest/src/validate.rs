//! Semantic validation of a parsed manifest.
//!
//! Parsing establishes that a document is structurally a manifest and that its
//! security-critical fields (IDs, paths, digests) hold safe values. Validation
//! establishes that it *means* something the manager can act on: that the
//! licence claim is honest, that every artifact can actually be extracted and
//! installed, and that the release set is coherent. The registry's CI (§44)
//! runs this over every file and fails the build on any error.

use crate::engines::EnginesFile;
use crate::manifest::{Artifact, Manifest, SCHEMA_VERSION};
use crate::types::{
  AllowedWarning, ArchiveFormat, ArchiveFormat as Af, Category, Content, Format, PackageKind,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
  Warning,
  Error,
}

impl std::fmt::Display for Severity {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Severity::Warning => "warning",
      Severity::Error => "error",
    })
  }
}

/// One finding about a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
  pub severity: Severity,
  /// Dotted location, e.g. `releases[0].artifacts[1].archive`.
  pub path: String,
  pub message: String,
  pub hint: Option<String>,
}

impl std::fmt::Display for Diagnostic {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}: {}: {}", self.severity, self.path, self.message)
  }
}

/// The findings for one manifest.
#[derive(Debug, Clone, Default)]
pub struct Report {
  pub diagnostics: Vec<Diagnostic>,
}

impl Report {
  pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
    self
      .diagnostics
      .iter()
      .filter(|d| d.severity == Severity::Error)
  }

  pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
    self
      .diagnostics
      .iter()
      .filter(|d| d.severity == Severity::Warning)
  }

  pub fn has_errors(&self) -> bool {
    self.errors().next().is_some()
  }

  fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
    self.diagnostics.push(Diagnostic {
      severity: Severity::Error,
      path: path.into(),
      message: message.into(),
      hint: None,
    });
  }

  fn error_with_hint(
    &mut self,
    path: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
  ) {
    self.diagnostics.push(Diagnostic {
      severity: Severity::Error,
      path: path.into(),
      message: message.into(),
      hint: Some(hint.into()),
    });
  }

  fn warn(&mut self, path: impl Into<String>, message: impl Into<String>) {
    self.diagnostics.push(Diagnostic {
      severity: Severity::Warning,
      path: path.into(),
      message: message.into(),
      hint: None,
    });
  }

  fn warn_with_hint(
    &mut self,
    path: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
  ) {
    self.diagnostics.push(Diagnostic {
      severity: Severity::Warning,
      path: path.into(),
      message: message.into(),
      hint: Some(hint.into()),
    });
  }
}

/// Formats this build can actually install. Anything else is metadata the
/// manager will refuse to act on rather than mis-install.
pub const INSTALLABLE_FORMATS: &[Format] =
  &[Format::Clap, Format::Vst3, Format::Lv2, Format::Library];

/// Archive containers the extractor can open.
pub const SUPPORTED_ARCHIVES: &[ArchiveFormat] =
  &[Af::TarGz, Af::TarXz, Af::Zip, Af::SevenZ, Af::None];

/// Runs every rule over `manifest`.
pub fn validate(manifest: &Manifest) -> Report {
  let mut report = Report::default();
  check_header(manifest, &mut report);
  check_license(manifest, &mut report);
  check_release_set(manifest, &mut report);
  check_kind_shape(manifest, &mut report);
  check_content(manifest, &mut report);
  for (i, release) in manifest.releases.iter().enumerate() {
    let base = format!("releases[{i}]");
    if release.artifacts.is_empty()
      && !matches!(manifest.kind, PackageKind::Pack | PackageKind::External)
    {
      report.error(
        &base,
        "release has no artifacts, so nothing could be installed",
      );
    }
    check_dependencies(manifest, release, &base, &mut report);
    for (j, artifact) in release.artifacts.iter().enumerate() {
      check_artifact(artifact, &format!("{base}.artifacts[{j}]"), &mut report);
    }
  }
  report
}

fn check_header(m: &Manifest, r: &mut Report) {
  if m.schema > SCHEMA_VERSION {
    r.error_with_hint(
      "schema",
      format!(
        "manifest declares schema v{} but this build understands up to v{SCHEMA_VERSION}",
        m.schema
      ),
      "Upgrade `luthier` to read this registry.",
    );
  }
  if m.schema == 0 {
    r.error("schema", "schema revision must be at least 1");
  }
  if m.name.trim().is_empty() {
    r.error("name", "display name must not be empty");
  }
  if !m.kind.is_known() {
    r.error_with_hint(
      "kind",
      format!("unknown package kind {:?}", m.kind.to_string()),
      format!("Known kinds: {}.", PackageKind::known_values()),
    );
  }
  if m.id.looks_versioned() {
    r.warn(
      "id",
      format!(
        "package ID {:?} looks like it embeds a version; IDs must stay stable across \
                 releases (§5)",
        m.id.as_str()
      ),
    );
  }
  // The category vocabulary is closed, so an unrecognised value is an error
  // rather than a warning: it is the one field a browsing UI groups on, and
  // three spellings of "synthesizer" would silently become three groups.
  // Parsing stays lenient (§7) so a v1 client can still read a registry that
  // added a category later; refusing it is this validator's job.
  if !m.category.is_known() {
    r.error(
      "category",
      format!(
        "unknown category {:?}; known categories are: {}",
        m.category.as_str(),
        Category::known_values()
      ),
    );
  }
  for (i, tag) in m.tags.iter().enumerate() {
    if tag.chars().any(|c| c.is_ascii_uppercase() || c == ' ') {
      r.warn(
        format!("tags[{i}]"),
        format!("tag {tag:?} should be lowercase and hyphenated"),
      );
    }
  }
  if m.description.as_ref().is_none_or(|d| d.trim().is_empty()) {
    r.warn(
      "description",
      "a description makes `luthier search` far more useful",
    );
  }
}

fn check_license(m: &Manifest, r: &mut Report) {
  if let Err(e) = m.license.validate() {
    r.error_with_hint(
      "license",
      e.to_string(),
      "SPDX identifiers are checked against the official list; see \
             https://spdx.org/licenses/.",
    );
  }
}

fn check_release_set(m: &Manifest, r: &mut Report) {
  let mut seen = BTreeSet::new();
  for (i, release) in m.releases.iter().enumerate() {
    if !seen.insert(release.version.clone()) {
      r.error(
        format!("releases[{i}].version"),
        format!("duplicate release version {}", release.version),
      );
    }
  }
  if m.releases.is_empty() && !matches!(m.kind, PackageKind::External) {
    r.error(
      "releases",
      "no releases declared, so there is nothing to install",
    );
  }
  if m.releases.iter().all(|rel| rel.is_yanked()) && !m.releases.is_empty() {
    r.warn(
      "releases",
      "every release is yanked; this package cannot be installed",
    );
  }
}

fn check_kind_shape(m: &Manifest, r: &mut Report) {
  match m.kind {
    PackageKind::External => {
      // An external package is a promise that we will never download it,
      // so an artifact here would be a contradiction.
      if m.releases.iter().any(|rel| !rel.artifacts.is_empty()) {
        r.error_with_hint(
          "releases",
          "an 'external' package must not declare artifacts",
          "External packages are detected, never downloaded. Use kind 'plugin' or \
                     'library' if a redistributable artifact does exist.",
        );
      }
      if m.detect.is_empty() {
        r.error_with_hint(
          "detect",
          "an 'external' package needs at least one detect rule",
          "Without one the resolver can never tell whether the dependency is satisfied.",
        );
      }
      if m.provisioning_hint.is_none() {
        r.warn(
          "provisioning_hint",
          "tell users how to obtain this package when it is missing",
        );
      }
    }
    PackageKind::Pack => {
      if m.releases.iter().any(|rel| !rel.artifacts.is_empty()) {
        r.error_with_hint(
          "releases",
          "a 'pack' must not declare artifacts",
          "Packs are metadata only and resolve to dependencies (§49).",
        );
      }
      if m.releases.iter().all(|rel| rel.dependencies.is_empty()) {
        r.error("releases", "a 'pack' must declare dependencies");
      }
    }
    _ => {
      if !m.detect.is_empty() {
        r.warn(
          "detect",
          "detect rules are only meaningful for 'external' packages",
        );
      }
    }
  }
}

fn check_content(m: &Manifest, r: &mut Report) {
  let mut seen = BTreeSet::new();
  for (i, content) in m.content.iter().enumerate() {
    let path = format!("content[{i}]");
    // Closed for the same reason as the category: an unknown value is one
    // `engines.toml` has no entry for, so the package could never install.
    if !content.is_known() {
      r.error_with_hint(
        &path,
        format!("unknown content {:?}", content.as_str()),
        format!("Known content: {}.", Content::known_values()),
      );
    }
    if !seen.insert(content) {
      r.error(&path, format!("{content} is listed twice"));
    }
  }
  // A plugin that ships its own instruments plays them itself; demanding a
  // second engine for them would refuse an install that works.
  if !m.content.is_empty() && m.kind != PackageKind::Library {
    r.error_with_hint(
      "content",
      format!(
        "only a 'library' declares content, and this is a '{}'",
        m.kind
      ),
      "`content` is for packages that need an engine to be heard.",
    );
  }
}

/// Runs every rule over an `engines.toml`.
pub fn validate_engines(file: &EnginesFile) -> Report {
  let mut r = Report::default();
  if file.schema > SCHEMA_VERSION {
    r.error(
      "schema",
      format!(
        "declares schema v{} but this build understands up to v{SCHEMA_VERSION}",
        file.schema
      ),
    );
  }
  let mut packages = BTreeSet::new();
  for (i, entry) in file.entries.iter().enumerate() {
    let base = format!("engine[{i}]");
    if !packages.insert(&entry.package) {
      r.error(
        &base,
        format!(
          "{} has two entries; list everything it plays in one",
          entry.package
        ),
      );
    }
    if entry.plays.is_empty() {
      r.error(format!("{base}.plays"), "an engine must play something");
    }
    for (j, content) in entry.plays.iter().enumerate() {
      if !content.is_known() {
        r.error_with_hint(
          format!("{base}.plays[{j}]"),
          format!("unknown content {:?}", content.as_str()),
          format!("Known content: {}.", Content::known_values()),
        );
      }
    }
    for (j, rule) in entry.detect.iter().enumerate() {
      // Detection looks in a plugin directory, and only a format with an
      // extension has one to look in.
      if rule.format.extension().is_none() {
        r.error(
          format!("{base}.detect[{j}].format"),
          format!("{} has no plugin directory to look in", rule.format),
        );
      }
    }
  }
  r
}

fn check_dependencies(
  m: &Manifest,
  release: &crate::manifest::Release,
  base: &str,
  r: &mut Report,
) {
  let mut seen = BTreeSet::new();
  for (label, deps) in [
    ("dependencies", &release.dependencies),
    ("optional_dependencies", &release.optional_dependencies),
  ] {
    for (i, dep) in deps.iter().enumerate() {
      let path = format!("{base}.{label}[{i}]");
      if dep.id == m.id {
        r.error(&path, "a package cannot depend on itself");
      }
      if !seen.insert((label, dep.id.clone())) {
        r.error(&path, format!("duplicate dependency on {}", dep.id));
      }
    }
  }
}

fn check_artifact(a: &Artifact, base: &str, r: &mut Report) {
  if !a.target.os.is_known() {
    r.error(
      format!("{base}.target.os"),
      format!("unknown os {:?}", a.target.os.to_string()),
    );
  }
  if !a.target.arch.is_known() {
    r.error(
      format!("{base}.target.arch"),
      format!("unknown architecture {:?}", a.target.arch.to_string()),
    );
  }

  check_source(a, base, r);
  check_archive(a, base, r);

  // §17 refuses a *manifest* that leaves the manager to invent what to copy.
  // It does not refuse rules read from an archive whose checksum has already
  // been verified — but that is a provider's answer to a source that carries
  // none, decided in memory, never something a contributor can ask for.
  if a.derive_install {
    r.error_with_hint(
      format!("{base}.derive_install"),
      "a manifest cannot ask for derived install rules",
      "Only a registry provider sets this, for a source that carries no rules of its own. \
             Declare them here instead — `luthier-registry inspect` writes them for you.",
    );
  } else if a.install.is_empty() {
    r.error_with_hint(
      format!("{base}.install"),
      "artifact declares no install rules",
      "Without rules the manager would have to guess what to copy, which it will not do \
             (§17).",
    );
  }

  let mut declared: BTreeSet<Format> = a.provides.iter().cloned().collect();
  let mut leaves = BTreeSet::new();
  for (k, rule) in a.install.iter().enumerate() {
    let path = format!("{base}.install[{k}]");
    if !rule.format.is_known() {
      r.error_with_hint(
        format!("{path}.format"),
        format!("unknown plugin format {:?}", rule.format.to_string()),
        format!("Known formats: {}.", Format::known_values()),
      );
    } else if !INSTALLABLE_FORMATS.contains(&rule.format) {
      r.error_with_hint(
        format!("{path}.format"),
        format!("format {} has no installer in this build", rule.format),
        "Installable formats are CLAP, VST3, LV2 and library content.",
      );
    }
    // A CLAP declared as a bundle (or a VST3 as a file) would sail past
    // extraction and only fail when a DAW tried to load it.
    if let Some(expected) = rule.format.entry_kind()
      && rule.kind != expected
    {
      r.error(
        format!("{path}.kind"),
        format!(
          "{} is a {} on this platform, but the rule declares {}",
          rule.format, expected, rule.kind
        ),
      );
    }
    if !leaves.insert((rule.format.clone(), rule.installed_name().to_owned())) {
      r.error(
        &path,
        format!(
          "two rules install {:?} into the {} directory",
          rule.installed_name(),
          rule.format
        ),
      );
    }
    // An allowance a contributor spelled wrong would silence nothing and say
    // nothing, so an unknown value is an error like any other unknown value
    // from a closed list (§7).
    for allowed in &rule.allow {
      if !allowed.is_known() {
        r.error_with_hint(
          format!("{path}.allow"),
          format!("unknown warning {allowed:?}"),
          format!("Known values: {}.", AllowedWarning::known_values()),
        );
      }
    }

    let allows_extension = rule.allow.contains(&AllowedWarning::FileExtension);
    if let Some(expected_ext) = rule.format.extension() {
      let name = rule.installed_name();
      let conventional = name
        .to_ascii_lowercase()
        .ends_with(&format!(".{expected_ext}"));
      if !conventional && !allows_extension {
        r.warn_with_hint(
          &path,
          format!("{name:?} does not end in .{expected_ext}; hosts may not find it"),
          format!(
            "If that is deliberate — a library the plugin loads at runtime, say — \
             write `allow = [\"{}\"]` on this rule.",
            AllowedWarning::FileExtension
          ),
        );
      }
      // An allowance that no longer silences anything is stale, and stale
      // allowances are how a real warning gets accepted years later without
      // anyone deciding to.
      if conventional && allows_extension {
        r.warn(
          format!("{path}.allow"),
          format!("{name:?} ends in .{expected_ext}; the file-extension allowance does nothing"),
        );
      }
    } else if allows_extension {
      r.warn(
        format!("{path}.allow"),
        format!(
          "{} has no conventional extension; the file-extension allowance does nothing",
          rule.format
        ),
      );
    }
    declared.remove(&rule.format);
  }

  for format in declared {
    r.warn(
      format!("{base}.provides"),
      format!("declares {format} but no install rule produces it"),
    );
  }

  let produced: BTreeSet<Format> = a.install.iter().map(|rule| rule.format.clone()).collect();
  for format in produced {
    if !a.provides.contains(&format) {
      r.warn(
        format!("{base}.provides"),
        format!("an install rule produces {format} but `provides` omits it"),
      );
    }
  }
}

fn check_source(a: &Artifact, base: &str, r: &mut Report) {
  let scheme = a.source.url.scheme();
  match a.source.kind {
    crate::manifest::SourceType::Http => {
      if !matches!(scheme, "http" | "https") {
        r.error(
          format!("{base}.source.url"),
          format!("source type 'http' requires an http(s) URL, got scheme {scheme:?}"),
        );
      } else if scheme == "http" {
        r.warn(
          format!("{base}.source.url"),
          "plain http is unencrypted; prefer https for the download URL",
        );
      }
    }
    crate::manifest::SourceType::File => {
      if scheme != "file" {
        r.error(
          format!("{base}.source.url"),
          format!("source type 'file' requires a file:// URL, got scheme {scheme:?}"),
        );
      }
    }
    crate::manifest::SourceType::Other(ref other) => {
      r.error_with_hint(
        format!("{base}.source.type"),
        format!("unknown artifact source type {other:?}"),
        format!(
          "Known source types: {}.",
          crate::manifest::SourceType::known_values()
        ),
      );
    }
  }
}

fn check_archive(a: &Artifact, base: &str, r: &mut Report) {
  if !a.archive.is_known() {
    r.error_with_hint(
      format!("{base}.archive"),
      format!("unknown archive format {:?}", a.archive.to_string()),
      format!("Known formats: {}.", ArchiveFormat::known_values()),
    );
    return;
  }
  if !SUPPORTED_ARCHIVES.contains(&a.archive) {
    // 7z is the live case: LSP Plugins publishes Linux binaries only as
    // 7z, so this rule is what keeps an uninstallable package out of the
    // registry rather than letting users hit the failure at install time.
    r.error_with_hint(
      format!("{base}.archive"),
      format!(
        "archive format {} cannot be extracted by this build",
        a.archive
      ),
      "Supported containers are tar.gz, tar.xz and zip. Adding 7z support is tracked \
             separately; until then such a package cannot be listed.",
    );
    return;
  }

  // A container that disagrees with its own filename is usually a copy-paste
  // slip in the manifest, and it would fail late during extraction.
  if let Some(last) = a
    .source
    .url
    .path_segments()
    .and_then(|mut s| s.rfind(|p| !p.is_empty()))
    && let Some(inferred) = ArchiveFormat::from_filename(last)
    && inferred != a.archive
  {
    r.warn(
      format!("{base}.archive"),
      format!(
        "declared {} but the URL ends in a {inferred} filename",
        a.archive
      ),
    );
  }

  if a.archive == ArchiveFormat::None && a.install.len() > 1 {
    r.error(
      format!("{base}.install"),
      "a non-archive artifact is a single file and can have only one install rule",
    );
  }

  // A bare file is placed under the name it was published as, so that is
  // the only source its rule can name. Anything else passes review and
  // then fails after the download, with the file already fetched.
  if a.archive == ArchiveFormat::None
    && let [rule] = a.install.as_slice()
  {
    let published = a
      .source
      .url
      .path_segments()
      .and_then(|mut s| s.rfind(|p| !p.is_empty()))
      .map(|last| {
        percent_encoding::percent_decode_str(last)
          .decode_utf8_lossy()
          .into_owned()
      })
      .unwrap_or_default();
    if rule.source.as_str() != published {
      r.error(
        format!("{base}.install[0].source"),
        format!(
          "a non-archive artifact is installed under its published name, {published:?}; \
           the rule names {:?}",
          rule.source.as_str()
        ),
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::parse::{ParseMode, from_toml};

  fn manifest(text: &str) -> Manifest {
    from_toml(text, "test.toml", ParseMode::Lenient)
      .unwrap()
      .manifest
  }

  const GOOD: &str = r#"
schema = 1
id = "dexed"
name = "Dexed"
kind = "plugin"
description = "A DX7 emulation."
category = "instrument"
tags = ["synthesizer"]
license = { kind = "open-source", spdx = "GPL-3.0-or-later" }

[[releases]]
version = "1.0.1"

[[releases.artifacts]]
target = { os = "linux", arch = "x86_64" }
source = { type = "http", url = "https://example.invalid/Dexed-1.0.1-lnx.zip" }
archive = "zip"
checksum = { sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" }
provides = ["vst3"]
install = [
  { format = "vst3", source = "Dexed.vst3", kind = "bundle" },
]
"#;

  #[test]
  fn a_good_manifest_is_clean() {
    let report = validate(&manifest(GOOD));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
    assert_eq!(report.warnings().count(), 0, "{:?}", report.diagnostics);
  }

  fn errors_for(yaml: &str) -> Vec<String> {
    validate(&manifest(yaml))
      .errors()
      .map(|d| d.to_string())
      .collect()
  }

  #[test]
  fn accepts_7z_now_that_the_extractor_opens_it() {
    // LSP Plugins' real situation: Linux binaries ship only as 7z. This was
    // the rule's example of an archive that could not be opened until the
    // extractor learned the format.
    let yaml = GOOD.replace("archive = \"zip\"", "archive = \"7z\"");
    assert!(errors_for(&yaml).is_empty(), "{:?}", errors_for(&yaml));
  }

  #[test]
  fn every_known_archive_format_can_be_extracted() {
    // Pins the reason the "cannot be extracted" rule below is currently
    // unreachable for a known value. When a format is added without an
    // extractor this fails, which is the moment that rule matters again.
    for format in [Af::TarGz, Af::TarXz, Af::Zip, Af::SevenZ, Af::None] {
      assert!(
        SUPPORTED_ARCHIVES.contains(&format),
        "{format} has no extractor"
      );
    }
  }

  /// A CLAP published as the file itself, as six Linux packages in the Open
  /// Audio Stack registry are.
  fn bare(url_name: &str, source: &str) -> String {
    GOOD
      .replace("Dexed-1.0.1-lnx.zip", url_name)
      .replace("archive = \"zip\"", "archive = \"none\"")
      .replace("provides = [\"vst3\"]", "provides = [\"clap\"]")
      .replace(
        r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
        &format!(r#"{{ format = "clap", source = "{source}", kind = "file" }}"#),
      )
  }

  #[test]
  fn a_bare_file_installed_under_its_own_name_is_clean() {
    let report = validate(&manifest(&bare("Dexed.clap", "Dexed.clap")));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
    assert_eq!(report.warnings().count(), 0, "{:?}", report.diagnostics);
  }

  #[test]
  fn a_bare_file_is_named_by_its_url_once_decoded() {
    let yaml = bare("Dexed%20Synth.clap", "Dexed Synth.clap");
    assert!(errors_for(&yaml).is_empty(), "{:?}", errors_for(&yaml));
  }

  #[test]
  fn a_bare_files_rule_cannot_name_something_else() {
    // It would pass review and fail after the download: the file is placed
    // under the name it was published as, and nothing else exists.
    let errors = errors_for(&bare("Dexed.clap", "Other.clap"));
    assert!(
      errors.iter().any(|e| e.contains("published name")),
      "{errors:?}"
    );
  }

  #[test]
  fn an_archive_declared_bare_is_flagged() {
    let yaml = GOOD.replace("archive = \"zip\"", "archive = \"none\"");
    let warnings: Vec<String> = validate(&manifest(&yaml))
      .warnings()
      .map(|d| d.to_string())
      .collect();
    assert!(
      warnings.iter().any(|w| w.contains("declared none")),
      "{warnings:?}"
    );
  }

  #[test]
  fn rejects_a_format_whose_shape_is_wrong() {
    // A CLAP is a file; declaring it a bundle would only fail in a DAW.
    let yaml = GOOD
      .replace("provides = [\"vst3\"]", "provides = [\"clap\"]")
      .replace(
        r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
        r#"{ format = "clap", source = "Dexed.clap", kind = "bundle" }"#,
      );
    let errors = errors_for(&yaml);
    assert!(
      errors
        .iter()
        .any(|e| e.contains("is a file on this platform")),
      "{errors:?}"
    );
  }

  #[test]
  fn accepts_lv2_now_that_it_has_an_installer() {
    // LV2 was this rule's example of an uninstallable format until the
    // installer landed. Keeping the case as a positive test means the
    // constant and the validator cannot quietly disagree again.
    let yaml = GOOD
      .replace("provides = [\"vst3\"]", "provides = [\"lv2\"]")
      .replace(
        r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
        r#"{ format = "lv2", source = "Dexed.lv2", kind = "bundle" }"#,
      );
    assert!(errors_for(&yaml).is_empty(), "{:?}", errors_for(&yaml));
  }

  #[test]
  fn rejects_a_format_no_installer_handles() {
    // Every *known* format is installable today, so this rule can only be
    // reached through an unrecognised one. It still earns its place: it is
    // what stops a manifest naming a format this build would silently do
    // nothing with.
    let yaml = GOOD.replace(
      r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
      r#"{ format = "ladspa", source = "Dexed.so", kind = "file" }"#,
    );
    let errors = errors_for(&yaml);
    assert!(
      errors.iter().any(|e| e.contains("unknown plugin format")),
      "{errors:?}"
    );
  }

  #[test]
  fn rejects_dishonest_open_source_claims() {
    let yaml = GOOD.replace("spdx = \"GPL-3.0-or-later\"", "spdx = \"CC-BY-NC-4.0\"");
    let errors = errors_for(&yaml);
    assert!(
      errors
        .iter()
        .any(|e| e.contains("neither OSI-approved nor FSF-libre")),
      "{errors:?}"
    );
  }

  #[test]
  fn rejects_duplicate_release_versions() {
    let yaml = format!("{GOOD}\n[[releases]]\nversion = \"1.0.1\"\nartifacts = []\n");
    let errors = errors_for(&yaml);
    assert!(
      errors
        .iter()
        .any(|e| e.contains("duplicate release version")),
      "{errors:?}"
    );
  }

  #[test]
  fn rejects_two_rules_writing_the_same_destination() {
    let yaml = GOOD.replace(
      r#"  { format = "vst3", source = "Dexed.vst3", kind = "bundle" },"#,
      concat!(
        r#"  { format = "vst3", source = "Dexed.vst3", kind = "bundle" },"#,
        "\n",
        r#"  { format = "vst3", source = "other/Dexed.vst3", kind = "bundle" },"#,
      ),
    );
    let errors = errors_for(&yaml);
    assert!(
      errors.iter().any(|e| e.contains("two rules install")),
      "{errors:?}"
    );
  }

  #[test]
  fn rejects_self_dependency() {
    let yaml = GOOD.replace(
      "version = \"1.0.1\"",
      "version = \"1.0.1\"\ndependencies = [\"dexed\"]",
    );
    let errors = errors_for(&yaml);
    assert!(
      errors.iter().any(|e| e.contains("cannot depend on itself")),
      "{errors:?}"
    );
  }

  #[test]
  fn external_packages_must_be_detectable_and_carry_no_artifacts() {
    let yaml = r#"
schema = 1
id = "sfizz"
name = "sfizz"
kind = "external"
category = "instrument"
license = { kind = "open-source", spdx = "BSD-2-Clause" }
description = "SFZ playback engine."
"#;
    let errors = errors_for(yaml);
    assert!(
      errors
        .iter()
        .any(|e| e.contains("needs at least one detect rule")),
      "{errors:?}"
    );

    let with_detect = format!(
      "{yaml}provisioning_hint = \"Install sfizz from your distribution.\"\n\n\
             [[detect]]\nformat = \"vst3\"\nname = \"sfizz.vst3\"\n"
    );
    let report = validate(&manifest(&with_detect));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
  }

  #[test]
  fn packs_carry_dependencies_not_artifacts() {
    let yaml = r#"
schema = 1
id = "foss-studio"
name = "FOSS Studio Pack"
kind = "pack"
category = "pack"
description = "A curated starting set."
license = { kind = "open-source", spdx = "CC0-1.0" }

[[releases]]
version = "1.0.0"
dependencies = ["surge-xt", "dexed"]
"#;
    let report = validate(&manifest(yaml));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
  }

  /// LSP Plugins' real rule: a shared object the CLAP loads at runtime, which
  /// has to sit in the CLAP directory without being a CLAP.
  const SIDECAR: &str = r#"
  { format = "clap", source = "lsp-plugins.clap", kind = "file" },
  { format = "clap", source = "liblsp-r3d-glx-lib.so", kind = "file" },
"#;

  #[test]
  fn a_name_without_its_formats_extension_warns() {
    let yaml = GOOD
      .replace("provides = [\"vst3\"]", "provides = [\"clap\"]")
      .replace(
        r#"  { format = "vst3", source = "Dexed.vst3", kind = "bundle" },"#,
        SIDECAR,
      );
    let report = validate(&manifest(&yaml));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
    assert!(
      report
        .warnings()
        .any(|w| w.message.contains("does not end in .clap")),
      "{:?}",
      report.diagnostics
    );
  }

  #[test]
  fn a_rule_may_accept_that_warning_deliberately() {
    // Without this the bench's CI, which runs --strict, is red on a
    // manifest that is correct.
    let yaml = GOOD
      .replace("provides = [\"vst3\"]", "provides = [\"clap\"]")
      .replace(
        r#"  { format = "vst3", source = "Dexed.vst3", kind = "bundle" },"#,
        &SIDECAR.replace(
          r#"source = "liblsp-r3d-glx-lib.so", kind = "file" }"#,
          r#"source = "liblsp-r3d-glx-lib.so", kind = "file", allow = ["file-extension"] }"#,
        ),
      );
    let report = validate(&manifest(&yaml));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
    assert_eq!(report.warnings().count(), 0, "{:?}", report.diagnostics);
  }

  #[test]
  fn an_allowance_that_silences_nothing_is_reported() {
    // Otherwise an allowance outlives the thing it was written for and
    // quietly accepts a real warning years later.
    let yaml = GOOD.replace(
      r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
      r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle", allow = ["file-extension"] }"#,
    );
    let report = validate(&manifest(&yaml));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
    assert!(
      report
        .warnings()
        .any(|w| w.message.contains("allowance does nothing")),
      "{:?}",
      report.diagnostics
    );
  }

  #[test]
  fn an_unknown_allowance_is_an_error_not_a_silent_no_op() {
    let yaml = GOOD.replace(
      r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
      r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle", allow = ["file-extensions"] }"#,
    );
    assert!(
      errors_for(&yaml)
        .iter()
        .any(|e| e.contains("unknown warning")),
      "{:?}",
      errors_for(&yaml)
    );
  }

  #[test]
  fn warns_when_provides_and_install_rules_disagree() {
    let yaml = GOOD.replace("provides = [\"vst3\"]", "provides = [\"vst3\", \"clap\"]");
    let report = validate(&manifest(&yaml));
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
    assert!(
      report
        .warnings()
        .any(|w| w.message.contains("no install rule produces it")),
      "{:?}",
      report.diagnostics
    );
  }

  #[test]
  fn warns_when_the_url_disagrees_with_the_declared_container() {
    let yaml = GOOD.replace("archive = \"zip\"", "archive = \"tar.gz\"");
    let report = validate(&manifest(&yaml));
    assert!(
      report
        .warnings()
        .any(|w| w.message.contains("URL ends in a zip filename")),
      "{:?}",
      report.diagnostics
    );
  }
  #[test]
  fn a_manifest_cannot_ask_for_derived_install_rules() {
    // The bench wins a collision with OAS because its rules were looked
    // at. A manifest that could ask for derivation would hand that back.
    let text = GOOD.replace(
      r#"install = ["#,
      "derive_install = true
install = [",
    );
    let report = validate(&manifest(&text));

    assert!(
      report
        .errors()
        .any(|e| e.message.contains("cannot ask for derived install rules")),
      "{:#?}",
      report.diagnostics
    );
  }

  #[test]
  fn an_artifact_with_no_rules_at_all_is_still_an_error() {
    let text = GOOD.replace(
      r#"install = [
  { format = "vst3", source = "Dexed.vst3", kind = "bundle" },
]"#,
      "install = []",
    );
    let report = validate(&manifest(&text));

    assert!(
      report
        .errors()
        .any(|e| e.message.contains("declares no install rules")),
      "{:#?}",
      report.diagnostics
    );
  }

  /// `GOOD` as a DrumGizmo kit: a library carrying content.
  fn kit() -> String {
    GOOD
      .replace(
        "kind = \"plugin\"",
        "kind = \"library\"\ncontent = [\"drumgizmo\"]",
      )
      .replace("category = \"instrument\"", "category = \"sample-library\"")
      .replace("provides = [\"vst3\"]", "provides = [\"library\"]")
      .replace(
        r#"{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }"#,
        r#"{ format = "library", source = "Kit", kind = "bundle" }"#,
      )
  }

  #[test]
  fn a_library_declares_the_content_an_engine_must_play() {
    let report = validate(&manifest(&kit()));
    assert!(!report.has_errors(), "{:#?}", report.diagnostics);
  }

  #[test]
  fn a_plugin_cannot_declare_content() {
    // A sampler that ships its own instruments plays them itself; asking
    // for a second engine would refuse an install that works.
    let errors = errors_for(&GOOD.replace(
      "kind = \"plugin\"",
      "kind = \"plugin\"\ncontent = [\"sfz\"]",
    ));
    assert!(
      errors.iter().any(|e| e.contains("only a 'library'")),
      "{errors:?}"
    );
  }

  #[test]
  fn unknown_content_is_refused() {
    let errors = errors_for(&kit().replace("[\"drumgizmo\"]", "[\"hydrogen\"]"));
    assert!(
      errors.iter().any(|e| e.contains("unknown content")),
      "{errors:?}"
    );
  }

  fn engine_errors(text: &str) -> Vec<String> {
    validate_engines(&EnginesFile::parse(text).unwrap())
      .errors()
      .map(|d| d.to_string())
      .collect()
  }

  #[test]
  fn a_well_formed_engines_file_is_clean() {
    let errors = engine_errors(
      r#"
[[engine]]
package = "sfizz"
plays = ["sfz"]

[[engine]]
package = "drumcraker"
plays = ["drumgizmo"]
detect = [{ format = "vst3", name = "DrumCraker.vst3" }]
"#,
    );
    assert!(errors.is_empty(), "{errors:?}");
  }

  #[test]
  fn an_engines_file_refuses_what_could_never_match() {
    let errors = engine_errors(
      r#"
[[engine]]
package = "sfizz"
plays = ["sfz"]

[[engine]]
package = "sfizz"
plays = ["sf3"]
detect = [{ format = "library", name = "sfizz" }]

[[engine]]
package = "silent"
plays = []
"#,
    );
    for expected in [
      "two entries",
      "unknown content",
      "no plugin directory",
      "must play something",
    ] {
      assert!(
        errors.iter().any(|e| e.contains(expected)),
        "{expected}: {errors:?}"
      );
    }
  }
}
