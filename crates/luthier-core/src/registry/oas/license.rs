//! Mapping Open Audio Stack licence keys onto SPDX.
//!
//! OAS records a licence as one value from a closed, lower-cased list — the
//! same vocabulary GitHub's licence detection uses. Most entries map onto one
//! SPDX identifier and nothing is lost. The GPL family does not: `gpl-3.0`
//! stands for both `GPL-3.0-only` and `GPL-3.0-or-later`, and SPDX retired the
//! bare identifier precisely because the two are different grants.
//!
//! Those map to the `-only` form and are reported. `-only` is the conservative
//! reading: it never claims a permission the licensor did not give, so a user
//! deciding whether they may combine two packages is told "no" when the answer
//! is uncertain rather than "yes". The note says the value was assumed, so the
//! guess is visible rather than silently authoritative.

use luthier_manifest::{License, LicenseKind};

/// What a licence key became, and whether anyone should check.
pub struct Mapped {
  pub license: License,
  /// Set when the value was inferred rather than read.
  pub note: Option<String>,
}

/// SPDX identifiers whose OAS key is unambiguous.
const EXACT: &[(&str, &str)] = &[
  ("0bsd", "0BSD"),
  ("afl-3.0", "AFL-3.0"),
  ("apache-2.0", "Apache-2.0"),
  ("artistic-2.0", "Artistic-2.0"),
  ("blueoak-1.0.0", "BlueOak-1.0.0"),
  ("bsd-2-clause", "BSD-2-Clause"),
  ("bsd-2-clause-patent", "BSD-2-Clause-Patent"),
  ("bsd-3-clause", "BSD-3-Clause"),
  ("bsd-3-clause-clear", "BSD-3-Clause-Clear"),
  ("bsd-4-clause", "BSD-4-Clause"),
  ("bsl-1.0", "BSL-1.0"),
  ("cc-by-4.0", "CC-BY-4.0"),
  ("cc-by-sa-4.0", "CC-BY-SA-4.0"),
  ("cc0-1.0", "CC0-1.0"),
  ("cecill-2.1", "CECILL-2.1"),
  ("cern-ohl-p-2.0", "CERN-OHL-P-2.0"),
  ("cern-ohl-s-2.0", "CERN-OHL-S-2.0"),
  ("cern-ohl-w-2.0", "CERN-OHL-W-2.0"),
  ("ecl-2.0", "ECL-2.0"),
  ("epl-1.0", "EPL-1.0"),
  ("epl-2.0", "EPL-2.0"),
  ("eupl-1.1", "EUPL-1.1"),
  ("eupl-1.2", "EUPL-1.2"),
  ("isc", "ISC"),
  ("lppl-1.3c", "LPPL-1.3c"),
  ("mit", "MIT"),
  ("mit-0", "MIT-0"),
  ("mpl-2.0", "MPL-2.0"),
  ("ms-pl", "MS-PL"),
  ("ms-rl", "MS-RL"),
  ("mulanpsl-2.0", "MulanPSL-2.0"),
  ("ncsa", "NCSA"),
  ("odbl-1.0", "ODbL-1.0"),
  ("ofl-1.1", "OFL-1.1"),
  ("osl-3.0", "OSL-3.0"),
  ("postgresql", "PostgreSQL"),
  ("unlicense", "Unlicense"),
  ("upl-1.0", "UPL-1.0"),
  ("vim", "Vim"),
  ("wtfpl", "WTFPL"),
  ("zlib", "Zlib"),
];

/// Keys that stand for two different grants. The value is the `-only` form.
const AMBIGUOUS: &[(&str, &str, &str)] = &[
  ("agpl-3.0", "AGPL-3.0-only", "AGPL-3.0-or-later"),
  ("gfdl-1.3", "GFDL-1.3-only", "GFDL-1.3-or-later"),
  ("gpl-2.0", "GPL-2.0-only", "GPL-2.0-or-later"),
  ("gpl-3.0", "GPL-3.0-only", "GPL-3.0-or-later"),
  ("lgpl-2.1", "LGPL-2.1-only", "LGPL-2.1-or-later"),
  ("lgpl-3.0", "LGPL-3.0-only", "LGPL-3.0-or-later"),
];

pub fn map(key: &str) -> Mapped {
  let key = key.trim().to_ascii_lowercase();

  if let Some((_, spdx)) = EXACT.iter().find(|(k, _)| *k == key) {
    return Mapped {
      license: open_source(spdx),
      note: None,
    };
  }

  if let Some((_, only, or_later)) = AMBIGUOUS.iter().find(|(k, _, _)| *k == key) {
    return Mapped {
      license: open_source(only),
      note: Some(format!(
        "licence recorded upstream as {key:?}, which is either {only} or {or_later}; \
                 {only} assumed"
      )),
    };
  }

  // `other` carries no information at all, and an unknown key means the
  // upstream vocabulary grew. Neither can be turned into an SPDX expression,
  // and inventing one would be worse than saying so.
  Mapped {
    license: License {
      kind: LicenseKind::Custom,
      spdx: None,
      name: Some(format!("Unspecified (upstream says {key:?})")),
      url: None,
    },
    note: Some(format!("no SPDX identifier for upstream licence {key:?}")),
  }
}

/// Builds an open-source licence, falling back when the identifier is valid
/// SPDX but neither OSI-approved nor FSF-libre — several Creative Commons
/// licences in this vocabulary are exactly that.
fn open_source(spdx: &str) -> License {
  let candidate = License {
    kind: LicenseKind::OpenSource,
    spdx: Some(spdx.to_owned()),
    name: None,
    url: None,
  };
  if candidate.validate().is_ok() {
    return candidate;
  }
  License {
    kind: LicenseKind::Custom,
    spdx: Some(spdx.to_owned()),
    name: Some(spdx.to_owned()),
    url: None,
  }
}
