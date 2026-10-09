//! Which packages play which content.
//!
//! A sample library is only useful next to something that can play it, and
//! the package that says what it *is* — `content = ["sfz"]`, or `contains:
//! [sfz]` in the Open Audio Stack registry — cannot also say what plays it
//! without naming one engine and shutting out the rest. A DrumGizmo kit plays
//! in DrumGizmo and in DrumCraker alike; a dependency on either would refuse a
//! user who has the other.
//!
//! So the two facts are kept apart. The content comes from the package; the
//! engines come from this file at the root of a source, and the manager checks
//! the system for any one of them before downloading anything. An entry names
//! an engine by package ID and may carry detect rules, because an engine from
//! a registry with no such field would otherwise only be recognised when this
//! manager installed it.

use crate::id::PackageId;
use crate::manifest::{DetectRule, Extra, SCHEMA_VERSION};
use crate::path::FileName;
use crate::types::{Content, Format};
use serde::{Deserialize, Serialize};

/// `engines.toml` at the root of a registry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnginesFile {
  #[serde(default = "default_schema")]
  pub schema: u32,
  #[serde(default, rename = "engine", skip_serializing_if = "Vec::is_empty")]
  pub entries: Vec<EngineEntry>,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

fn default_schema() -> u32 {
  SCHEMA_VERSION
}

/// One engine, and the content it plays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EngineEntry {
  /// The engine, by ID as this manager resolves it. It may come from any
  /// configured registry.
  pub package: PackageId,
  /// Content this engine can play.
  pub plays: Vec<Content>,
  /// How to recognise a copy this manager did not install, in addition to
  /// any rules the package's own manifest carries.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub detect: Vec<DetectRule>,
  #[serde(flatten)]
  #[schemars(skip)]
  pub extra: Extra,
}

/// The conventional filename, at the root of a registry.
pub const ENGINES_FILE: &str = "engines.toml";

/// The engines every build knows about before a single registry is read.
///
/// This used to be `engines.toml` alone, and that put general knowledge in an
/// optional place: "an SFZ library needs an SFZ engine, and sfizz is one" is
/// true of formats and of well-known software, not of anybody's package list.
/// A user who configures only the Open Audio Stack registry — which has no
/// field for what plays what — otherwise gets told nothing can play a library
/// while sfizz sits installed on their machine.
///
/// A source's `engines.toml` still adds to this: one in `extras/` ships with
/// the next release, as data rather than code. Registry entries come first,
/// so a source refines an engine this list already names rather than colliding
/// with it; nothing here can be *removed* by a source, which is why what goes
/// in is narrow: engines that are the reference implementation for their
/// format, or that extras has already vetted.
///
/// Detect rules are carried only where the installed name is stable. `sfzq`
/// stamps its release date into its filename, and a name that changes on
/// every release would be a detection that silently stops working between
/// releases of this manager — a source is the right place for that one.
pub fn builtin_engines() -> Vec<EngineEntry> {
  fn entry(package: &str, plays: &[Content], detect: &[(Format, &str)]) -> EngineEntry {
    EngineEntry {
      package: PackageId::new(package).expect("built-in engine IDs are valid"),
      plays: plays.to_vec(),
      detect: detect
        .iter()
        .map(|(format, name)| DetectRule {
          format: format.clone(),
          name: FileName::new(*name).expect("built-in detect names are valid"),
          extra: Extra::default(),
        })
        .collect(),
      extra: Extra::default(),
    }
  }

  vec![
    // The reference SFZ engine. Published as source, packaged by every
    // distribution, so detection is the only way it is ever found.
    entry(
      "sfizz",
      &[Content::Sfz],
      &[
        (Format::Vst3, "sfizz.vst3"),
        (Format::Lv2, "sfizz.lv2"),
        (Format::Clap, "sfizz.clap"),
      ],
    ),
    entry(
      "sfizioso-player",
      &[Content::Sfz],
      &[(Format::Vst3, "Sfizioso Player.vst3")],
    ),
    entry("sfzq", &[Content::Sfz], &[]),
    entry(
      "fluida-lv2",
      &[Content::Sf2],
      &[(Format::Lv2, "Fluida.lv2")],
    ),
    // The one SoundFont player either registry can install: fluida-lv2 is
    // only ever found, never offered. It links the system's libfluidsynth,
    // which most distributions install alongside anything audio.
    entry(
      "fluidsynth-clap",
      &[Content::Sf2],
      &[(Format::Clap, "FluidSynth.clap")],
    ),
    // DrumGizmo plays its own kits; DrumCraker plays them too, which is
    // why content names a format and never an engine.
    entry(
      "drumgizmo",
      &[Content::Drumgizmo],
      &[(Format::Lv2, "drumgizmo.lv2")],
    ),
    entry(
      "drumcraker",
      &[Content::Drumgizmo],
      &[(Format::Vst3, "DrumCraker.vst3")],
    ),
  ]
}

/// Content at least one built-in engine plays.
pub fn builtin_content() -> std::collections::BTreeSet<Content> {
  builtin_engines()
    .into_iter()
    .flat_map(|entry| entry.plays)
    .collect()
}

impl EnginesFile {
  pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
    toml::from_str(text)
  }

  /// Fields this build has no name for, as dotted paths.
  pub fn unknown_fields(&self) -> Vec<String> {
    let mut out: Vec<String> = self.extra.keys().cloned().collect();
    for (i, entry) in self.entries.iter().enumerate() {
      out.extend(entry.extra.keys().map(|key| format!("engine[{i}].{key}")));
      for (j, rule) in entry.detect.iter().enumerate() {
        out.extend(
          rule
            .extra
            .keys()
            .map(|key| format!("engine[{i}].detect[{j}].{key}")),
        );
      }
    }
    out
  }
}

#[cfg(test)]
mod tests {
  /// Every content type this build knows must have something that plays it.
  ///
  /// The drift this catches is the one `Content` invites: adding a variant is
  /// a two-line change, and a build that knows a format but nothing that
  /// opens it tells every user of that content the same unhelpful thing.
  #[test]
  fn every_known_content_has_a_built_in_engine() {
    let played = builtin_content();
    for content in [Content::Sfz, Content::Sf2, Content::Drumgizmo] {
      assert!(
        played.contains(&content),
        "nothing built in plays {content}"
      );
    }
  }

  #[test]
  fn the_built_in_engines_would_pass_the_validator() {
    // They are data of the same kind a source writes, so they answer to the
    // same rules: one entry per package, something to play, and detect
    // rules only for formats with a directory to look in.
    let file = EnginesFile {
      schema: SCHEMA_VERSION,
      entries: builtin_engines(),
      extra: Default::default(),
    };
    let report = crate::validate_engines(&file);
    assert!(!report.has_errors(), "{:?}", report.diagnostics);
  }

  use super::*;

  #[test]
  fn an_entry_names_an_engine_what_it_plays_and_how_to_find_it() {
    let file = EnginesFile::parse(
      r#"
schema = 1

[[engine]]
package = "drumgizmo"
plays = ["drumgizmo"]

[[engine]]
package = "drumcraker"
plays = ["drumgizmo"]
detect = [{ format = "vst3", name = "DrumCraker.vst3" }]
"#,
    )
    .unwrap();

    assert_eq!(file.entries.len(), 2);
    assert_eq!(file.entries[0].plays, vec![Content::Drumgizmo]);
    assert!(file.entries[0].detect.is_empty());
    assert_eq!(file.entries[1].detect[0].name.as_str(), "DrumCraker.vst3");
    assert!(file.unknown_fields().is_empty());
  }

  #[test]
  fn a_misspelt_field_is_kept_and_reported() {
    let file =
      EnginesFile::parse("[[engine]]\npackage = \"sfizz\"\nplays = [\"sfz\"]\nplay = [\"sf2\"]\n")
        .unwrap();
    assert_eq!(file.unknown_fields(), vec!["engine[0].play"]);
  }
}
