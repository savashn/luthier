//! Licensing metadata.
//!
//! §8 is emphatic that "free" and "open source" are not the same thing, so the
//! schema records an explicit [`LicenseKind`] alongside the SPDX expression
//! rather than inferring openness from the presence of an identifier. SPDX
//! strings are parsed with the `spdx` crate against the real licence list —
//! identifiers are never invented, and a typo like `GPL3` is caught in CI.

use crate::macros::string_enum;
use serde::{Deserialize, Serialize};

string_enum! {
    /// How a package may be used and redistributed.
    pub enum LicenseKind {
        /// An OSI-approved or FSF-libre licence. Requires a valid `spdx`.
        OpenSource => "open-source",
        /// Free of charge, but not open source.
        Freeware => "freeware",
        /// Paid and/or restrictively licensed.
        Proprietary => "proprietary",
        /// A bespoke licence with no SPDX identifier. Requires `name`.
        Custom => "custom",
    }
}

/// The licence a package is distributed under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct License {
  /// The category, which the manager surfaces to the user.
  pub kind: LicenseKind,
  /// An SPDX licence expression, e.g. `GPL-3.0-or-later` or `MIT OR Apache-2.0`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub spdx: Option<String>,
  /// Human-readable name, for licences with no SPDX identifier.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  /// Where the full licence text can be read.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub url: Option<String>,
}

/// What is wrong with a [`License`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LicenseError {
  #[error("license.spdx is required when license.kind is 'open-source'")]
  MissingSpdx,
  #[error("license.name is required when license.kind is 'custom'")]
  MissingName,
  #[error("license.spdx {0:?} is not a valid SPDX expression: {1}")]
  InvalidExpression(String, String),
  #[error(
    "license.spdx {0:?} names the deprecated SPDX identifier {1:?}; use its current replacement"
  )]
  Deprecated(String, String),
  #[error(
    "license.kind is 'open-source' but {1:?} in {0:?} is neither OSI-approved nor FSF-libre; \
         use kind 'freeware', 'proprietary' or 'custom' instead"
  )]
  NotOpenSource(String, String),
  #[error("license.kind {0:?} is not a value this build understands (known: {1})")]
  UnknownKind(String, String),
}

impl License {
  /// Checks the licence is internally consistent and that any SPDX
  /// expression names real, current identifiers.
  pub fn validate(&self) -> Result<(), LicenseError> {
    if !self.kind.is_known() {
      return Err(LicenseError::UnknownKind(
        self.kind.to_string(),
        LicenseKind::known_values(),
      ));
    }
    if self.kind == LicenseKind::Custom && self.name.is_none() {
      return Err(LicenseError::MissingName);
    }

    let Some(expr_text) = self.spdx.as_deref() else {
      return if self.kind == LicenseKind::OpenSource {
        Err(LicenseError::MissingSpdx)
      } else {
        Ok(())
      };
    };

    let expr = spdx::Expression::parse(expr_text)
      .map_err(|e| LicenseError::InvalidExpression(expr_text.to_owned(), e.to_string()))?;

    for req in expr.requirements() {
      let spdx::LicenseItem::Spdx { id, .. } = req.req.license else {
        // A `LicenseRef-` document reference: legitimate for a bespoke
        // licence, but it carries no approval metadata, so it cannot
        // substantiate an open-source claim.
        if self.kind == LicenseKind::OpenSource {
          return Err(LicenseError::NotOpenSource(
            expr_text.to_owned(),
            req.req.license.to_string(),
          ));
        }
        continue;
      };
      if id.is_deprecated() {
        return Err(LicenseError::Deprecated(
          expr_text.to_owned(),
          id.name.to_owned(),
        ));
      }
      if self.kind == LicenseKind::OpenSource && !(id.is_osi_approved() || id.is_fsf_free_libre()) {
        return Err(LicenseError::NotOpenSource(
          expr_text.to_owned(),
          id.name.to_owned(),
        ));
      }
    }
    Ok(())
  }

  /// A short label for CLI output.
  pub fn display(&self) -> String {
    match (&self.spdx, &self.name) {
      (Some(spdx), _) => spdx.clone(),
      (None, Some(name)) => name.clone(),
      (None, None) => self.kind.to_string(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn lic(kind: LicenseKind, spdx: Option<&str>) -> License {
    License {
      kind,
      spdx: spdx.map(str::to_owned),
      name: None,
      url: None,
    }
  }

  #[test]
  fn accepts_real_open_source_expressions() {
    for e in [
      "GPL-3.0-or-later",
      "MIT",
      "Apache-2.0",
      "BSD-3-Clause",
      "MIT OR Apache-2.0",
    ] {
      lic(LicenseKind::OpenSource, Some(e))
        .validate()
        .unwrap_or_else(|err| panic!("{e} should validate: {err}"));
    }
  }

  #[test]
  fn rejects_invented_identifiers() {
    // The whole point of using the real SPDX list rather than a hand-rolled one.
    let err = lic(LicenseKind::OpenSource, Some("GPL3"))
      .validate()
      .unwrap_err();
    assert!(
      matches!(err, LicenseError::InvalidExpression(..)),
      "{err:?}"
    );
  }

  #[test]
  fn free_of_charge_is_not_open_source() {
    // §8: CC-BY-NC forbids commercial use, so it must not pass as open source.
    let err = lic(LicenseKind::OpenSource, Some("CC-BY-NC-4.0"))
      .validate()
      .unwrap_err();
    assert!(matches!(err, LicenseError::NotOpenSource(..)), "{err:?}");
    // ...but it is perfectly valid metadata under a different kind.
    lic(LicenseKind::Freeware, Some("CC-BY-NC-4.0"))
      .validate()
      .unwrap();
  }

  #[test]
  fn open_source_requires_an_identifier() {
    assert_eq!(
      lic(LicenseKind::OpenSource, None).validate().unwrap_err(),
      LicenseError::MissingSpdx
    );
  }

  #[test]
  fn custom_requires_a_name() {
    assert_eq!(
      lic(LicenseKind::Custom, None).validate().unwrap_err(),
      LicenseError::MissingName
    );
    let mut ok = lic(LicenseKind::Custom, None);
    ok.name = Some("Vendor EULA".into());
    ok.validate().unwrap();
  }

  #[test]
  fn deprecated_identifiers_are_rejected() {
    // `GPL-3.0` was superseded by `GPL-3.0-only`/`GPL-3.0-or-later`. The
    // `spdx` crate refuses deprecated identifiers during parsing, so this
    // surfaces as InvalidExpression rather than reaching our own
    // `is_deprecated` guard; either way the manifest is rejected and the
    // message says why.
    let err = lic(LicenseKind::OpenSource, Some("GPL-3.0"))
      .validate()
      .unwrap_err();
    assert!(
      matches!(
        err,
        LicenseError::Deprecated(..) | LicenseError::InvalidExpression(..)
      ),
      "{err:?}"
    );
    assert!(err.to_string().contains("deprecated"), "{err}");
  }
}
