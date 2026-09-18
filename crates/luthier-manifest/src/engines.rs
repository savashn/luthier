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
//! engines come from this file at the root of a bench, and the manager checks
//! the system for any one of them before downloading anything. An entry names
//! an engine by package ID and may carry detect rules, because an engine from
//! a registry with no such field would otherwise only be recognised when this
//! manager installed it.

use crate::id::PackageId;
use crate::manifest::{DetectRule, Extra, SCHEMA_VERSION};
use crate::types::Content;
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
