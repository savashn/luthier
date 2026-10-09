//! Turning an Open Audio Stack index into manifests.
//!
//! The two schemas describe the same world with different vocabularies, and
//! every difference is decided here rather than scattered through the
//! resolver. What OAS does not carry at all — install rules and dependency
//! edges — is not invented: rules are marked for derivation from the archive
//! (see [`crate::install::derive`]), and edges stay absent. Sample content
//! needs no edge: its `contains` says what it is, and which engines play that
//! is the built-in engine list, which a source's `engines.toml` adds to.
//!
//! Anything that cannot be represented is dropped with a reason rather than
//! guessed at, because a package that resolves and then fails to install is
//! worse than one that never appeared.

use super::license;
use luthier_manifest::{
  Arch, ArchiveFormat, Artifact, Category, Checksum, Content, Format, License, Manifest, Os,
  PackageId, PackageKind, Release, Sha256Hash, Source, SourceType, Target,
};
use semver::Version;
use serde::Deserialize;
use std::collections::BTreeMap;
use url::Url;

/// One package in the published index, keyed by `org/name`.
#[derive(Debug, Clone, Deserialize)]
pub struct OasPackage {
  pub slug: String,
  pub version: String,
  #[serde(default)]
  pub versions: BTreeMap<String, OasVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OasVersion {
  pub name: String,
  #[serde(default)]
  pub author: String,
  #[serde(default)]
  pub description: String,
  #[serde(default)]
  pub license: String,
  #[serde(rename = "type", default)]
  pub kind: String,
  #[serde(default)]
  pub tags: Vec<String>,
  #[serde(default)]
  pub url: String,
  #[serde(default)]
  pub files: Vec<OasFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OasFile {
  #[serde(default)]
  pub systems: Vec<OasSystem>,
  #[serde(default)]
  pub architectures: Vec<String>,
  /// Which formats upstream says the archive holds. Never an install
  /// instruction — it does not say which entry is which — so it is kept to
  /// check the derivation against later, and to say what content a library
  /// needs an engine for.
  #[serde(default)]
  pub contains: Vec<String>,
  #[serde(rename = "type", default)]
  pub kind: String,
  #[serde(default)]
  pub size: u64,
  #[serde(default)]
  pub sha256: String,
  #[serde(default)]
  pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OasSystem {
  #[serde(rename = "type", default)]
  pub kind: String,
}

/// A translated package, with whatever had to be interpreted to get there.
///
/// What upstream claims each artifact holds is not carried separately: it is
/// each artifact's `provides`, which is what the install-time check reads. One
/// copy, so the claim `info` prints is the claim that gets checked.
#[derive(Debug)]
pub struct Translated {
  pub manifest: Manifest,
  pub notes: Vec<String>,
}

/// Turns one index entry into a manifest, or explains why it cannot be one.
///
/// Every version upstream still publishes becomes a release. Reading only the
/// one `version` names would make the index a moving target: `env export`
/// pins the version it installed, and the moment upstream published a newer
/// one the pinned version would vanish and the import fail. Thirty-two of the
/// published index's packages carry more than one.
pub fn package(entry: &OasPackage) -> Result<Translated, String> {
  let id = id_from_slug(&entry.slug)?;
  let mut notes = Vec::new();

  // A version that cannot be translated costs that version, not the package.
  // An old release whose files have gone missing upstream is common; it is
  // not a reason to withhold the ones that are still there.
  let mut built: Vec<(Release, &OasVersion)> = Vec::new();
  let mut refusals: Vec<String> = Vec::new();
  for (key, version) in &entry.versions {
    match release_for(key, version) {
      Ok((release, version_notes)) => {
        notes.extend(version_notes.into_iter().map(|n| format!("{key}: {n}")));
        built.push((release, version));
      }
      Err(reason) => refusals.push(format!("{key}: {reason}")),
    }
  }

  if built.is_empty() {
    return Err(match refusals.len() {
      0 => "index carries no versions".to_owned(),
      1 => format!("no file this build can install ({})", refusals[0]),
      n => format!(
        "no file this build can install in any of {n} versions ({})",
        refusals.join("; ")
      ),
    });
  }
  if !refusals.is_empty() {
    notes.push(format!("versions not translated: {}", refusals.join("; ")));
  }
  built.sort_by(|a, b| a.0.version.cmp(&b.0.version));

  // Package-level facts — name, licence, what kind of thing this is — belong
  // to the package rather than to a release, so one version has to supply
  // them. It is the one the index names as current, or the newest that
  // survived translation if that one did not.
  let primary = Version::parse(&entry.version)
    .ok()
    .and_then(|named| built.iter().position(|(r, _)| r.version == named))
    .unwrap_or(built.len() - 1);
  let version = built[primary].1;

  let mapped = license::map(&version.license);
  if let Some(note) = mapped.note {
    notes.push(note);
  }

  let (kind, category) = classify(&version.kind, &version.files);
  // Only a library needs an engine for its content. A package that also
  // ships a plugin plays what it carries, and `airwindows`, an effect suite
  // that upstream files as containing SFZ, would otherwise be refused for
  // want of an engine it never needed.
  let content = if kind == PackageKind::Library {
    content_of(&version.files)
  } else {
    Vec::new()
  };

  // Kind and content sit on the package, not on the release, so a package
  // that changed shape between versions can only be described by one of
  // them. Saying so is better than silently describing the others wrongly.
  for (release, other) in &built {
    if classify(&other.kind, &other.files) != (kind.clone(), category.clone()) {
      notes.push(format!(
        "version {} is a different kind of package upstream; described as the {} one",
        release.version, built[primary].0.version
      ));
    }
  }

  let homepage = Url::parse(&version.url).ok();
  let manifest = Manifest {
    schema: luthier_manifest::SCHEMA_VERSION,
    id,
    name: version.name.clone(),
    kind,
    category,
    tags: version
      .tags
      .iter()
      .map(|t| t.trim().to_ascii_lowercase())
      .filter(|t| !t.is_empty())
      .collect(),
    content,
    description: (!version.description.is_empty()).then(|| version.description.clone()),
    homepage: homepage.clone(),
    repository: homepage,
    documentation: None,
    authors: if version.author.is_empty() {
      Vec::new()
    } else {
      vec![version.author.clone()]
    },
    license: mapped.license,
    releases: built.into_iter().map(|(release, _)| release).collect(),
    detect: Vec::new(),
    provisioning_hint: None,
    extra: Default::default(),
  };

  Ok(Translated { manifest, notes })
}

/// One version of one package, with whatever had to be skipped to build it.
///
/// A version that contributes no artifact is an error rather than an empty
/// release: the resolver reads a release as something it can select, and one
/// with nothing to fetch would be selected and then fail.
fn release_for(key: &str, version: &OasVersion) -> Result<(Release, Vec<String>), String> {
  let semver = Version::parse(key).map_err(|e| format!("version {key:?} is not semver: {e}"))?;

  let mut notes = Vec::new();
  let mut artifacts = Vec::new();
  for file in &version.files {
    match artifacts_for_file(file) {
      Ok(built) => artifacts.extend(built),
      Err(reason) => notes.push(format!("skipped a file: {reason}")),
    }
  }
  if artifacts.is_empty() {
    return Err("no file this build can install".to_owned());
  }

  Ok((
    Release {
      version: semver,
      release_notes: None,
      dependencies: Vec::new(),
      optional_dependencies: Vec::new(),
      artifacts,
      yanked: None,
      extra: Default::default(),
    },
    notes,
  ))
}

/// `org/name` becomes `name`, which is what a user types.
///
/// Collisions are not resolved here: the caller sees every candidate and can
/// fall back to `org-name` for the handful that clash. Today that is one name
/// across 559 packages.
pub fn id_from_slug(slug: &str) -> Result<PackageId, String> {
  let last = slug.rsplit('/').next().unwrap_or(slug);
  PackageId::new(normalise(last)).map_err(|e| format!("slug {slug:?} is not a usable id: {e}"))
}

/// The `org-name` form, for a name two organisations both publish under.
pub fn qualified_id(slug: &str) -> Result<PackageId, String> {
  PackageId::new(normalise(&slug.replace('/', "-")))
    .map_err(|e| format!("slug {slug:?} is not a usable id: {e}"))
}

/// Lower-cases and turns separators into hyphens, which is the whole gap
/// between the two vocabularies — four slugs in the published index need it.
fn normalise(raw: &str) -> String {
  let mapped: String = raw
    .to_ascii_lowercase()
    .chars()
    .map(|c| {
      if c.is_ascii_lowercase() || c.is_ascii_digit() {
        c
      } else {
        '-'
      }
    })
    .collect();
  let collapsed = mapped
    .split('-')
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("-");
  collapsed.chars().take(PackageId::MAX_LEN).collect()
}

/// OAS files everything under "plugins" and splits by what a package *does*;
/// this schema splits by what a package *is* and then by what it does.
///
/// The two disagree on `sampler`, which covers both a sampler plugin and a
/// library of samples for one. What the archive holds settles it: content with
/// no plugin binary is a library.
fn classify(kind: &str, files: &[OasFile]) -> (PackageKind, Category) {
  let contains: Vec<&str> = files
    .iter()
    .flat_map(|f| f.contains.iter().map(String::as_str))
    .collect();
  let has_plugin = contains
    .iter()
    .any(|c| matches!(*c, "clap" | "vst3" | "lv2" | "vst" | "dll" | "component"));
  let has_content = contains.iter().any(|c| content_value(c).is_some());

  match kind {
    "sampler" if has_content && !has_plugin => (PackageKind::Library, Category::SampleLibrary),
    "sampler" | "instrument" | "generator" => (PackageKind::Plugin, Category::Instrument),
    "effect" => (PackageKind::Plugin, Category::Effect),
    _ => (PackageKind::Plugin, Category::Utility),
  }
}

/// What upstream's `contains` names that only an engine can play.
///
/// Across every file, like [`classify`]: content is the same thing on every
/// platform, and a value upstream has not adopted yet parses harmlessly to
/// nothing.
fn content_of(files: &[OasFile]) -> Vec<Content> {
  let mut content: Vec<Content> = files
    .iter()
    .flat_map(|f| f.contains.iter())
    .filter_map(|c| content_value(c))
    .collect();
  content.sort();
  content.dedup();
  content
}

/// The content a `contains` value names, if it names content at all.
fn content_value(raw: &str) -> Option<Content> {
  let content: Content = raw.parse().expect("parsing is infallible");
  content.is_known().then_some(content)
}

/// Every target one downloadable file serves.
///
/// A file lists systems and architectures as two lists, and means their
/// product: platform-neutral content is filed as all three systems and all
/// four architectures because the one archive genuinely serves every
/// combination. Taking the first of each would pick a target at the mercy of
/// upstream's ordering — `[arm32, arm64, x32, x64]` would resolve to arm64 and
/// the package would vanish from an x86_64 machine.
///
/// An empty result means the file is for platforms this build has no target
/// for. `Err` means it is for one of ours and still unusable.
fn artifacts_for_file(file: &OasFile) -> Result<Vec<Artifact>, String> {
  let systems: Vec<Os> = file
    .systems
    .iter()
    .filter_map(|s| match s.kind.as_str() {
      "linux" => Some(Os::Linux),
      "mac" => Some(Os::Macos),
      "win" => Some(Os::Windows),
      _ => None,
    })
    .collect();
  // 32-bit and arm64ec have no target in this schema.
  let arches: Vec<Arch> = file
    .architectures
    .iter()
    .filter_map(|a| match a.as_str() {
      "x64" => Some(Arch::X86_64),
      "arm64" => Some(Arch::Aarch64),
      _ => None,
    })
    .collect();
  if systems.is_empty() || arches.is_empty() || file.kind != "archive" {
    return Ok(Vec::new());
  }

  let url = Url::parse(&file.url).map_err(|e| format!("{}: bad url: {e}", file.url))?;
  // The declared type says "archive" for things that are not archives, so
  // the extension decides and the downloaded bytes are sniffed again later.
  let archive = ArchiveFormat::from_filename(url.path())
    .ok_or_else(|| format!("{}: not an archive format this build reads", file.url))?;
  let sha256 =
    Sha256Hash::parse(&file.sha256).map_err(|e| format!("{}: unusable checksum: {e}", file.url))?;

  let mut out = Vec::new();
  for os in &systems {
    for arch in &arches {
      out.push(Artifact {
        target: Target {
          os: os.clone(),
          arch: arch.clone(),
        },
        source: Source {
          kind: SourceType::Http,
          url: url.clone(),
          extra: Default::default(),
        },
        archive: archive.clone(),
        size: (file.size > 0).then_some(file.size),
        checksum: Checksum { sha256 },
        // Upstream's claim, not a verified fact: it says which
        // formats the archive holds, and it is sometimes wrong.
        // Recorded so `info` can answer without downloading, and
        // checked against the derivation before anything is
        // installed.
        provides: declared_formats(&file.contains),
        install: Vec::new(),
        derive_install: true,
        extra: Default::default(),
      });
    }
  }
  Ok(out)
}

/// The subset of upstream's `contains` this schema has a format for.
///
/// `so`, `dll`, `elf` and `component` name things with no installer here, so
/// they are dropped rather than translated into something that would resolve
/// and then fail.
fn declared_formats(contains: &[String]) -> Vec<Format> {
  let mut formats: Vec<Format> = contains
    .iter()
    .filter_map(|c| match c.as_str() {
      "clap" => Some(Format::Clap),
      "vst3" => Some(Format::Vst3),
      "lv2" => Some(Format::Lv2),
      other if content_value(other).is_some() => Some(Format::Library),
      _ => None,
    })
    .collect();
  formats.sort();
  formats.dedup();
  formats
}

/// Reports a licence that could not be expressed, for callers that only want
/// the mapping.
pub fn license_for(key: &str) -> License {
  license::map(key).license
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(json: &str) -> OasPackage {
    serde_json::from_str(json).expect("fixture parses")
  }

  const WSTD_EQ: &str = r#"{
      "slug": "wasted-audio/wstd-eq",
      "version": "1.1.1",
      "versions": { "1.1.1": {
        "name": "WSTD EQ", "author": "Wasted Audio",
        "description": "Simple nasty EQ plugin.",
        "license": "gpl-3.0", "type": "effect", "tags": ["EQ", "Equalizer"],
        "url": "https://github.com/Wasted-Audio/wstd-eq",
        "files": [
          { "systems": [{"type":"linux"}], "architectures": ["x64"],
            "contains": ["so","vst3","clap","lv2"], "type": "archive",
            "size": 989692,
            "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
            "url": "https://example.invalid/wstd-eq-v1.1.1-linux-x86_64.tar.xz" },
          { "systems": [{"type":"mac"}], "architectures": ["x64"],
            "contains": ["vst3"], "type": "installer",
            "size": 3123950,
            "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
            "url": "https://example.invalid/wstd-eq-v1.1.1-macos-intel.pkg" }
        ]
      }}
    }"#;

  #[test]
  fn an_artifact_carries_no_rules_and_asks_for_them_to_be_derived() {
    let translated = package(&entry(WSTD_EQ)).unwrap();
    let artifact = &translated.manifest.releases[0].artifacts[0];

    // OAS says which formats the archive holds, never which entry is
    // which, so the rules come from the archive at install time.
    assert!(artifact.install.is_empty());
    assert!(artifact.derive_install);
    assert!(artifact.is_installable());
  }

  #[test]
  fn an_installer_is_not_an_artifact() {
    // A `.pkg` runs someone else's installer. Nothing here can do that,
    // and pretending otherwise would resolve and then fail.
    let translated = package(&entry(WSTD_EQ)).unwrap();
    assert_eq!(translated.manifest.releases[0].artifacts.len(), 1);
  }

  #[test]
  fn upstreams_claim_is_recorded_but_only_for_formats_this_build_installs() {
    let translated = package(&entry(WSTD_EQ)).unwrap();

    // "so" names a VST2 shared object, which has no installer here.
    // Carrying it would make `info` promise a format the manager cannot
    // deliver, and make the install-time check complain about an absence
    // that was never a surprise.
    let provides = &translated.manifest.releases[0].artifacts[0].provides;
    assert_eq!(
      provides.iter().map(ToString::to_string).collect::<Vec<_>>(),
      vec!["clap", "vst3", "lv2"]
    );
  }

  /// The same entry with a second, older version alongside the current one.
  fn two_versions() -> String {
    WSTD_EQ.replace(
      r#""versions": { "1.1.1": {"#,
      r#""versions": {
        "1.0.0": {
          "name": "WSTD EQ", "author": "Wasted Audio", "license": "gpl-3.0",
          "type": "effect", "url": "https://github.com/Wasted-Audio/wstd-eq",
          "files": [
            { "systems": [{"type":"linux"}], "architectures": ["x64"],
              "contains": ["clap"], "type": "archive", "size": 900000,
              "sha256": "8c79675a4379125e894416430cde82ec81734512b8b0e55c8745c939e24ddd25",
              "url": "https://example.invalid/wstd-eq-v1.0.0-linux-x86_64.tar.xz" }
          ]
        },
        "1.1.1": {"#,
    )
  }

  #[test]
  fn every_version_upstream_publishes_becomes_a_release() {
    // Without this, `env export` pins a version that leaves the index as
    // soon as upstream ships another, and the import cannot resolve it.
    let translated = package(&entry(&two_versions())).unwrap();
    let versions: Vec<String> = translated
      .manifest
      .releases
      .iter()
      .map(|r| r.version.to_string())
      .collect();
    assert_eq!(versions, vec!["1.0.0", "1.1.1"]);
  }

  #[test]
  fn package_level_facts_come_from_the_version_the_index_names() {
    let json = two_versions().replace(r#""version": "1.1.1""#, r#""version": "1.0.0""#);
    let translated = package(&entry(&json)).unwrap();
    // Both releases are still there; the description is 1.0.0's, which
    // carries none.
    assert_eq!(translated.manifest.releases.len(), 2);
    assert_eq!(translated.manifest.description, None);
  }

  #[test]
  fn a_version_that_cannot_be_translated_costs_that_version_only() {
    let json = two_versions().replace("wstd-eq-v1.0.0-linux-x86_64.tar.xz", "wstd-eq-v1.0.0.pkg");
    let translated = package(&entry(&json)).unwrap();

    let versions: Vec<String> = translated
      .manifest
      .releases
      .iter()
      .map(|r| r.version.to_string())
      .collect();
    assert_eq!(versions, vec!["1.1.1"]);
    assert!(
      translated.notes.iter().any(|n| n.contains("1.0.0")),
      "{:?}",
      translated.notes
    );
  }

  #[test]
  fn a_version_key_that_is_not_semver_is_skipped_not_fatal() {
    let json = two_versions().replace(r#""1.0.0": {"#, r#""nightly": {"#);
    let translated = package(&entry(&json)).unwrap();
    assert_eq!(translated.manifest.releases.len(), 1);
    assert!(
      translated.notes.iter().any(|n| n.contains("not semver")),
      "{:?}",
      translated.notes
    );
  }

  #[test]
  fn an_ambiguous_licence_is_resolved_conservatively_and_reported() {
    let translated = package(&entry(WSTD_EQ)).unwrap();

    assert_eq!(
      translated.manifest.license.spdx.as_deref(),
      Some("GPL-3.0-only")
    );
    assert!(
      translated
        .notes
        .iter()
        .any(|n| n.contains("GPL-3.0-only assumed")),
      "{:?}",
      translated.notes
    );
  }

  #[test]
  fn sample_content_with_no_plugin_is_a_library() {
    // OAS files a sampler plugin and a library of samples under one type.
    // What the archive holds is what separates them.
    // Every file, not just this platform's: a package is one thing
    // across platforms, so a plugin shipped only for macOS still makes it
    // a plugin here.
    let json = WSTD_EQ
      .replace(r#""type": "effect""#, r#""type": "sampler""#)
      .replace(
        r#""contains": ["so","vst3","clap","lv2"]"#,
        r#""contains": ["sfz"]"#,
      )
      .replace(r#""contains": ["vst3"]"#, r#""contains": ["sfz"]"#);
    let translated = package(&entry(&json)).unwrap();

    assert_eq!(translated.manifest.kind, PackageKind::Library);
    assert_eq!(translated.manifest.category, Category::SampleLibrary);
    // What an engine must play is read from the same claim, so nothing
    // outside the package has to name one.
    assert_eq!(translated.manifest.content, vec![Content::Sfz]);
  }

  #[test]
  fn a_drumgizmo_kit_is_content_too() {
    let json = WSTD_EQ
      .replace(r#""type": "effect""#, r#""type": "sampler""#)
      .replace(
        r#""contains": ["so","vst3","clap","lv2"]"#,
        r#""contains": ["drumgizmo"]"#,
      )
      .replace(r#""contains": ["vst3"]"#, r#""contains": ["drumgizmo"]"#);
    let translated = package(&entry(&json)).unwrap();

    assert_eq!(translated.manifest.kind, PackageKind::Library);
    assert_eq!(translated.manifest.content, vec![Content::Drumgizmo]);
    let provides = &translated.manifest.releases[0].artifacts[0].provides;
    assert_eq!(provides, &vec![Format::Library]);
  }

  #[test]
  fn a_package_that_ships_a_plugin_needs_no_engine_for_its_content() {
    // `airwindows/airwindows`: an effect suite upstream files as holding
    // SFZ. Asking for an SFZ engine would refuse a plugin that needs none.
    let json = WSTD_EQ.replace(
      r#""contains": ["so","vst3","clap","lv2"]"#,
      r#""contains": ["sfz","vst3","clap","lv2"]"#,
    );
    let translated = package(&entry(&json)).unwrap();

    assert_eq!(translated.manifest.kind, PackageKind::Plugin);
    assert!(translated.manifest.content.is_empty());
  }

  #[test]
  fn a_sampler_that_ships_a_plugin_stays_a_plugin() {
    let json = WSTD_EQ.replace(r#""type": "effect""#, r#""type": "sampler""#);
    let translated = package(&entry(&json)).unwrap();

    assert_eq!(translated.manifest.kind, PackageKind::Plugin);
    assert_eq!(translated.manifest.category, Category::Instrument);
  }

  #[test]
  fn a_package_with_nothing_installable_is_refused_rather_than_half_translated() {
    let json = WSTD_EQ.replace(r#""type": "archive""#, r#""type": "installer""#);
    assert!(package(&entry(&json)).is_err());
  }

  #[test]
  fn a_bare_clap_is_a_file_to_place_not_an_archive_to_open() {
    // Six Linux packages in the published index ship this way — LibreKick,
    // FreqChain, Delax — filed as `type: archive` all the same.
    let json = WSTD_EQ.replace(
      "wstd-eq-v1.1.1-linux-x86_64.tar.xz",
      "LibreKick_linux_x86_64.clap",
    );
    let translated = package(&entry(&json)).unwrap();
    let artifact = &translated.manifest.releases[0].artifacts[0];
    assert_eq!(artifact.archive, ArchiveFormat::None);
    assert!(artifact.derive_install);
  }

  #[test]
  fn a_single_file_vst3_is_not_taken_for_a_bundle() {
    // A VST3 is a directory on Linux. One file by that name is a zip
    // mislabelled or a build for another platform.
    let json = WSTD_EQ.replace("wstd-eq-v1.1.1-linux-x86_64.tar.xz", "Plugin.vst3");
    assert!(package(&entry(&json)).is_err());
  }

  #[test]
  fn a_file_that_is_not_an_archive_this_build_reads_is_reported() {
    // sfizz's entry points at a `.tar.bz2`-shaped case: declared an
    // archive, in a format with no decoder here.
    let json = WSTD_EQ.replace("linux-x86_64.tar.xz", "linux-x86_64.tar.bz2");
    let err = package(&entry(&json)).unwrap_err();
    assert!(err.contains("no file this build can install"), "{err}");
  }

  #[test]
  fn ids_come_from_the_name_not_the_organisation() {
    assert_eq!(id_from_slug("sfztools/sfizz").unwrap().as_str(), "sfizz");
    assert_eq!(
      qualified_id("distrho/mverb").unwrap().as_str(),
      "distrho-mverb"
    );
  }

  #[test]
  fn separators_upstream_allows_and_this_schema_does_not_are_normalised() {
    // Four slugs in the published index need this and no more.
    for (slug, expected) in [
      ("sfzinstruments/virtuosity_drums", "virtuosity-drums"),
      ("cannerycoders/fluidsynth.clap", "fluidsynth-clap"),
      ("lluisestape-upc/synth1.0", "synth1-0"),
    ] {
      assert_eq!(id_from_slug(slug).unwrap().as_str(), expected);
    }
  }

  #[test]
  fn a_32_bit_file_has_no_target_here_and_is_skipped() {
    let json = WSTD_EQ.replace(r#""architectures": ["x64"]"#, r#""architectures": ["x32"]"#);
    assert!(package(&entry(&json)).is_err());
  }
}
