//! Reading manifests from TOML.

use crate::manifest::{Manifest, UnknownFields};

/// How strictly to treat fields this build does not recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseMode {
  /// Keep unknown fields and report them. Used by the client, so that an
  /// `luthier` binary built today keeps working against a newer registry (§7).
  #[default]
  Lenient,
  /// Treat unknown fields as errors. Used by the registry's own CI, so that
  /// a contributor's `licence:` typo fails the pull request instead of being
  /// silently ignored.
  Strict,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
  #[error("{source_name}: {message}")]
  Toml {
    source_name: String,
    message: String,
    /// Byte offset of the offending span, when the parser reports one.
    offset: Option<usize>,
  },
  #[error(
    "{source_name}: unrecognised field{plural} not part of manifest schema v{schema}: {fields}"
  )]
  UnknownFields {
    source_name: String,
    schema: u32,
    fields: String,
    plural: &'static str,
  },
}

impl ParseError {
  /// A hint for the CLI to print underneath the error (§36).
  pub fn hint(&self) -> Option<String> {
    match self {
      ParseError::UnknownFields { .. } => Some(
        "Check the spelling against docs/MANIFEST.md, or raise the manifest's `schema:` \
                 if the field belongs to a newer revision."
          .into(),
      ),
      ParseError::Toml { .. } => None,
    }
  }
}

/// A manifest plus anything about it this build did not understand.
#[derive(Debug, Clone)]
pub struct Parsed {
  pub manifest: Manifest,
  /// Dotted paths of fields not in schema v1, e.g. `releases[0].signature`.
  pub unknown_fields: Vec<String>,
}

impl Parsed {
  pub fn into_manifest(self) -> Manifest {
    self.manifest
  }
}

/// Parses a manifest from TOML.
///
/// `source_name` appears in error messages and should be the file path when
/// one is available.
pub fn from_toml(text: &str, source_name: &str, mode: ParseMode) -> Result<Parsed, ParseError> {
  let manifest: Manifest = toml::from_str(text).map_err(|e| ParseError::Toml {
    source_name: source_name.to_owned(),
    message: e.to_string(),
    offset: e.span().map(|s| s.start),
  })?;

  let mut unknown_fields = Vec::new();
  manifest.collect_unknown("", &mut unknown_fields);
  unknown_fields.sort();

  if mode == ParseMode::Strict && !unknown_fields.is_empty() {
    return Err(ParseError::UnknownFields {
      source_name: source_name.to_owned(),
      schema: crate::manifest::SCHEMA_VERSION,
      plural: if unknown_fields.len() == 1 { "" } else { "s" },
      fields: unknown_fields.join(", "),
    });
  }

  Ok(Parsed {
    manifest,
    unknown_fields,
  })
}

/// Serialises a manifest back to TOML. Used by the registry authoring helpers.
pub fn to_toml(manifest: &Manifest) -> Result<String, toml::ser::Error> {
  toml::to_string_pretty(manifest)
}

#[cfg(test)]
mod tests {
  use super::*;

  const MINIMAL: &str = r#"
schema = 1
id = "dexed"
name = "Dexed"
kind = "plugin"
category = "instrument"
tags = ["synthesizer", "fm"]

[license]
kind = "open-source"
spdx = "GPL-3.0-or-later"

[[releases]]
version = "1.0.1"

[[releases.artifacts]]
target = { os = "linux", arch = "x86_64" }
source = { type = "http", url = "https://example.invalid/dexed.zip" }
archive = "zip"
checksum = { sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" }
provides = ["vst3"]
install = [{ format = "vst3", source = "Dexed.vst3", kind = "bundle" }]
"#;

  #[test]
  fn parses_a_realistic_manifest() {
    let parsed = from_toml(MINIMAL, "dexed.toml", ParseMode::Strict).unwrap();
    let m = parsed.manifest;
    assert_eq!(m.id.as_str(), "dexed");
    assert_eq!(m.category, crate::types::Category::Instrument);
    assert_eq!(m.tags, vec!["synthesizer", "fm"]);
    assert_eq!(m.releases.len(), 1);
    let artifact = &m.releases[0].artifacts[0];
    assert_eq!(artifact.install[0].installed_name(), "Dexed.vst3");
    assert!(parsed.unknown_fields.is_empty());
  }

  #[test]
  fn lenient_mode_keeps_working_against_a_newer_registry() {
    // The whole point of §7: a field added in a later schema revision must
    // not stop today's client from installing the package.
    let text = MINIMAL.replace(
      r#"kind = "plugin""#,
      "kind = \"plugin\"\nfuture_flag = true\nsignature = { ed25519 = \"deadbeef\" }",
    );
    let parsed = from_toml(&text, "dexed.toml", ParseMode::Lenient).unwrap();
    assert_eq!(parsed.unknown_fields, vec!["future_flag", "signature"]);
    assert_eq!(parsed.manifest.id.as_str(), "dexed");
  }

  #[test]
  fn strict_mode_catches_a_typo() {
    let text = MINIMAL.replace(
      "[license]",
      "licence = { kind = \"open-source\" }\n\n[license]",
    );
    let err = from_toml(&text, "dexed.toml", ParseMode::Strict).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("licence"), "{text}");
    assert!(err.hint().is_some());
  }

  #[test]
  fn nested_unknown_fields_are_reported_with_a_path() {
    let text = MINIMAL.replace(r#"archive = "zip""#, "archive = \"zip\"\nmirror = \"x\"");
    let parsed = from_toml(&text, "dexed.toml", ParseMode::Lenient).unwrap();
    assert_eq!(
      parsed.unknown_fields,
      vec!["releases[0].artifacts[0].mirror"]
    );
  }

  #[test]
  fn toml_errors_carry_a_location() {
    let err = from_toml("schema = 1\nid = [unclosed", "bad.toml", ParseMode::Lenient).unwrap_err();
    match err {
      ParseError::Toml { offset, .. } => assert!(offset.is_some()),
      other => panic!("expected a TOML error, got {other}"),
    }
  }

  #[test]
  fn a_missing_category_is_refused_at_parse_time() {
    // `category` is required: a manifest without one cannot be filed.
    let text = MINIMAL.replace("category = \"instrument\"\n", "");
    let err = from_toml(&text, "dexed.toml", ParseMode::Lenient).unwrap_err();
    assert!(err.to_string().contains("category"), "{err}");
  }

  #[test]
  fn hostile_install_paths_are_rejected_at_parse_time() {
    let text = MINIMAL.replace(
      r#"source = "Dexed.vst3""#,
      r#"source = "../../../.ssh/authorized_keys""#,
    );
    let err = from_toml(&text, "dexed.toml", ParseMode::Lenient).unwrap_err();
    assert!(
      err.to_string().contains("escapes the archive root"),
      "{err}"
    );
  }
}
