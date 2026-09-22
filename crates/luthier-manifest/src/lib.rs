//! Package manifest schema, parsing and validation for Luthier.
//!
//! This crate is deliberately free of networking, async and filesystem policy.
//! It is shared by the package manager and by the registry's CI validator, and
//! the validator must be able to check a pull request without pulling in an
//! HTTP stack. Everything here is pure data plus rules about that data.
//!
//! The two entry points are [`parse::from_yaml`], which turns YAML into a
//! [`Manifest`], and [`validate::validate`], which reports what is wrong with
//! one.

#![forbid(unsafe_code)]

mod macros;

pub mod discover;
pub mod engines;
pub mod hash;
pub mod id;
pub mod license;
pub mod manifest;
pub mod parse;
pub mod path;
pub mod types;
pub mod validate;

pub use discover::{DiscoveryError, manifest_files};
pub use engines::{ENGINES_FILE, EngineEntry, EnginesFile, builtin_content, builtin_engines};
pub use hash::{Checksum, HashError, Sha256Hash};
pub use id::{IdError, PackageId};
pub use license::{License, LicenseError, LicenseKind};
pub use manifest::{
  Artifact, Dependency, DetectRule, Extra, InstallRule, Manifest, Release, SCHEMA_VERSION, Source,
  SourceType, UnknownFields,
};
pub use parse::{ParseError, ParseMode, Parsed, from_toml, to_toml};
pub use path::{ArchivePath, FileName, PathError};
pub use types::{
  AllowedWarning, Arch, ArchiveFormat, Category, Content, EntryKind, Format, Os, PackageKind,
  Target,
};
pub use validate::{Diagnostic, Report, Severity, validate, validate_engines};

/// The JSON Schema for a v1 manifest, generated from the Rust types.
///
/// The registry commits this file so editors can offer completion and so
/// contributors get feedback before CI runs. Generating it rather than
/// maintaining it by hand is what stops the published schema and the parser
/// from drifting apart; a test asserts the committed copy matches.
pub fn json_schema() -> serde_json::Value {
  let schema = schemars::schema_for!(Manifest);
  let mut value = serde_json::to_value(schema).expect("schema serialises");
  if let Some(obj) = value.as_object_mut() {
    obj.insert(
      "$id".into(),
      serde_json::Value::String("https://luthier.dev/schemas/package-v1.json".into()),
    );
    obj.insert(
      "title".into(),
      serde_json::Value::String("Luthier package manifest v1".into()),
    );
  }
  value
}

/// The generated schema as pretty JSON with a trailing newline.
pub fn json_schema_text() -> String {
  let mut text = serde_json::to_string_pretty(&json_schema()).expect("schema serialises");
  text.push('\n');
  text
}

#[cfg(test)]
mod tests {
  #[test]
  fn schema_generation_describes_the_manifest() {
    let schema = super::json_schema();
    let props = schema
      .get("properties")
      .and_then(|p| p.as_object())
      .expect("manifest is an object schema");
    for required in ["schema", "id", "name", "kind", "license", "releases"] {
      assert!(
        props.contains_key(required),
        "schema should describe {required}"
      );
    }
  }
}
