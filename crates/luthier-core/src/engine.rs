//! Whether the content about to be installed has anything to play it.
//!
//! A sample library is the one package that can install perfectly and still
//! do nothing: a directory of `.sfz` and `.wav` files with no engine on the
//! machine is a download nobody can hear. So the check runs after resolution
//! and before the first byte is fetched, and what it produces is a *note on
//! the plan* — the confirmation a user already gives is where they decide.
//!
//! It is not a refusal, and the reason is what the absence of an engine
//! actually means. "No engine is installed" is a fact about the machine;
//! "no registry names an engine for this format" is a fact about this
//! manager's knowledge, and the Open Audio Stack registry has no field for
//! what plays what, so every library read from it lands in the second case.
//! Refusing there would mean treating our own ignorance as evidence — and a
//! user with sfizz installed from their distribution being told they cannot
//! install a kit.
//!
//! What a package holds comes from its manifest (`content`, or `contains` in
//! the Open Audio Stack registry), and that much needs no curation. What
//! *plays* it comes from `engines.toml`, where any one engine is enough —
//! which is exactly what a dependency could not say, since a kit that
//! depended on DrumGizmo would refuse a DrumCraker user. Without it the note
//! still names what the format needs (`Content::played_by`); with it, the
//! note also says which engine is a command away.

use crate::registry::RegistryIndex;
use crate::resolver::{Disposition, Resolution};
use crate::state::State;
use luthier_manifest::{Content, PackageId};
use std::collections::BTreeSet;

/// Content that would arrive with nothing to play it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplayable {
  pub package: PackageId,
  pub content: Content,
  /// What would play it, in the order the registries list them. Empty when
  /// no configured registry names an engine for this content at all — a
  /// thinner note, not a different verdict.
  pub engines: Vec<PackageId>,
}

/// Every piece of content in `resolution` that nothing could play.
///
/// An engine counts when it was detected on the system (`present`), when this
/// manager has it recorded as installed, or when it is part of this same
/// resolution — `luthier install crocellkit drumcraker` must not be refused
/// for the order it names things in. Packages already installed at the
/// selected version are not rechecked; nothing is being downloaded for them.
pub fn unplayable(
  resolution: &Resolution<'_>,
  index: &RegistryIndex,
  state: &State,
  present: &BTreeSet<PackageId>,
) -> Vec<Unplayable> {
  let arriving: BTreeSet<&PackageId> = resolution.order.iter().map(|p| p.id()).collect();
  let playable = |engine: &PackageId| {
    present.contains(engine) || state.is_installed(engine) || arriving.contains(engine)
  };

  let mut out = Vec::new();
  for package in &resolution.order {
    if package.disposition == Disposition::Satisfied {
      continue;
    }
    let mut content: Vec<&Content> = package.entry.manifest.content.iter().collect();
    content.sort();
    content.dedup();
    for wanted in content {
      let engines: Vec<PackageId> = index
        .engines_for(wanted)
        .into_iter()
        .map(|engine| engine.package.clone())
        .collect();
      if !engines.iter().any(playable) {
        out.push(Unplayable {
          package: package.id().clone(),
          content: wanted.clone(),
          engines,
        });
      }
    }
  }
  out
}

/// Installed content that would be left with nothing to play it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stranded {
  /// The library that would be left silent.
  pub package: PackageId,
  pub content: Content,
  /// Engines that would play it, none of which would remain.
  pub engines: Vec<PackageId>,
}

/// Content already installed that removing `removing` would strand.
///
/// The mirror of [`unplayable`], and deliberately not enforced the same way.
/// Refusing to remove an engine would be wrong: the user may have just
/// installed one from their distribution, may be about to, or may simply not
/// want the kit any more. What they should not get is silence with no
/// explanation, so this warns and removal proceeds.
///
/// A package whose manifest has left the registry is skipped rather than
/// guessed at: what it holds is not recorded in state, and an advisory warning
/// is not worth inventing.
pub fn stranded_by_removal(
  index: &RegistryIndex,
  state: &State,
  removing: &BTreeSet<PackageId>,
  present: &BTreeSet<PackageId>,
) -> Vec<Stranded> {
  let survives = |engine: &PackageId| {
    // A detected engine is one the system provides, which removing a package
    // here does not take away.
    present.contains(engine) || (state.is_installed(engine) && !removing.contains(engine))
  };

  let mut out = Vec::new();
  for installed in state.packages.values() {
    if removing.contains(&installed.id) {
      continue;
    }
    let Ok(entry) = index.get(&installed.id) else {
      continue;
    };
    let mut content: Vec<&Content> = entry.manifest.content.iter().collect();
    content.sort();
    content.dedup();
    for wanted in content {
      let engines: Vec<PackageId> = index
        .engines_for(wanted)
        .into_iter()
        .map(|engine| engine.package.clone())
        .collect();
      // Only worth saying when this removal is what breaks it: content that
      // already had no engine was not stranded by anything happening now.
      let played_before = engines
        .iter()
        .any(|e| present.contains(e) || state.is_installed(e));
      if played_before && !engines.iter().any(survives) {
        out.push(Stranded {
          package: installed.id.clone(),
          content: wanted.clone(),
          engines,
        });
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::registry::IndexEntry;
  use crate::resolver::{ResolveRequest, resolve};
  use luthier_manifest::{Arch, EnginesFile, Os, ParseMode, Sha256Hash, Target};
  use std::collections::BTreeMap;
  use std::path::PathBuf;

  const ENGINES: &str = r#"
[[engine]]
package = "drumgizmo"
plays = ["drumgizmo"]

[[engine]]
package = "drumcraker"
plays = ["drumgizmo"]
"#;

  fn artifact(id: &str, format: &str, source: &str) -> String {
    format!(
      "\n[[releases.artifacts]]\n\
       target = {{ os = \"linux\", arch = \"x86_64\" }}\n\
       source = {{ type = \"http\", url = \"https://e.invalid/{id}.zip\" }}\n\
       archive = \"zip\"\n\
       checksum = {{ sha256 = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\" }}\n\
       provides = [\"{format}\"]\n\
       install = [{{ format = \"{format}\", source = \"{source}\", kind = \"bundle\" }}]\n"
    )
  }

  fn index(extra: &[(&str, String)]) -> RegistryIndex {
    let kit = format!(
      "schema = 1\nid = \"crocellkit\"\nname = \"CrocellKit\"\nkind = \"library\"\n\
       category = \"sample-library\"\ncontent = [\"drumgizmo\"]\n\
       license = {{ kind = \"open-source\", spdx = \"CC-BY-4.0\" }}\n\
       [[releases]]\nversion = \"1.1.0\"\n{}",
      artifact("crocellkit", "library", "CrocellKit")
    );
    let drumcraker = format!(
      "schema = 1\nid = \"drumcraker\"\nname = \"DrumCraker\"\nkind = \"plugin\"\n\
       category = \"instrument\"\nlicense = {{ kind = \"open-source\", spdx = \"MIT\" }}\n\
       [[releases]]\nversion = \"1.3.4\"\n{}",
      artifact("drumcraker", "vst3", "DrumCraker.vst3")
    );
    let mut packages = BTreeMap::new();
    for (id, text) in [("crocellkit", kit), ("drumcraker", drumcraker)]
      .into_iter()
      .chain(extra.iter().map(|(id, text)| (*id, text.clone())))
    {
      let manifest = luthier_manifest::from_toml(&text, id, ParseMode::Strict)
        .unwrap_or_else(|e| panic!("{id}: {e}"))
        .manifest;
      packages.insert(
        manifest.id.clone(),
        IndexEntry {
          manifest,
          path: PathBuf::from(format!("{id}.toml")),
          digest: Sha256Hash::from_bytes([0; 32]),
          unknown_fields: Vec::new(),
          registry: "test".into(),
          notes: Vec::new(),
        },
      );
    }
    RegistryIndex {
      name: "test".into(),
      packages,
      problems: Vec::new(),
      engines: EnginesFile::parse(ENGINES).unwrap().entries,
    }
  }

  fn id(raw: &str) -> PackageId {
    PackageId::new(raw).unwrap()
  }

  fn check(
    index: &RegistryIndex,
    roots: &[&str],
    state: &State,
    present: &[&str],
  ) -> Vec<Unplayable> {
    let roots: Vec<PackageId> = roots.iter().map(|r| id(r)).collect();
    let resolution = resolve(
      index,
      &ResolveRequest {
        roots: &roots,
        target: &Target::new(Os::Linux, Arch::X86_64),
        state,
        detected_externals: &BTreeSet::new(),
        force: false,
        required_versions: &BTreeMap::new(),
      },
    )
    .unwrap();
    let present: BTreeSet<PackageId> = present.iter().map(|p| id(p)).collect();
    unplayable(&resolution, index, state, &present)
  }

  #[test]
  fn a_kit_with_no_engine_anywhere_is_unplayable_and_names_every_option() {
    let found = check(&index(&[]), &["crocellkit"], &State::default(), &[]);
    assert_eq!(
      found,
      vec![Unplayable {
        package: id("crocellkit"),
        content: Content::Drumgizmo,
        engines: vec![id("drumgizmo"), id("drumcraker")],
      }]
    );
  }

  #[test]
  fn any_one_engine_on_the_system_is_enough() {
    let index = index(&[]);
    assert!(check(&index, &["crocellkit"], &State::default(), &["drumcraker"]).is_empty());
    assert!(check(&index, &["crocellkit"], &State::default(), &["drumgizmo"]).is_empty());
  }

  #[test]
  fn an_engine_installed_in_the_same_run_counts() {
    let found = check(
      &index(&[]),
      &["crocellkit", "drumcraker"],
      &State::default(),
      &[],
    );
    assert!(found.is_empty(), "{found:?}");
  }

  #[test]
  fn content_no_registry_names_an_engine_for_is_refused_too() {
    // Nothing can vouch that an engine is present, so the answer is the
    // same as for a known engine that is missing.
    let library = format!(
      "schema = 1\nid = \"piano\"\nname = \"Piano\"\nkind = \"library\"\n\
       category = \"sample-library\"\ncontent = [\"sf2\"]\n\
       license = {{ kind = \"open-source\", spdx = \"CC0-1.0\" }}\n\
       [[releases]]\nversion = \"1.0.0\"\n{}",
      artifact("piano", "library", "Piano")
    );
    let found = check(
      &index(&[("piano", library)]),
      &["piano"],
      &State::default(),
      &[],
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].content, Content::Sf2);
    assert!(found[0].engines.is_empty());
  }

  #[test]
  fn a_package_with_no_content_is_never_checked() {
    let found = check(&index(&[]), &["drumcraker"], &State::default(), &[]);
    assert!(found.is_empty());
  }
}
