//! Turning results into output.
//!
//! Every command produces a serialisable result from the core and hands it
//! here, so `--json` is a rendering choice rather than a separate code path
//! (§33). Human output goes to stdout; logs and progress go to stderr, so
//! piping `--json` into a tool stays clean.

use luthier_core::api::{
  CacheCleaned, CacheEntry, ImportOutcome, InstallOutcome, InstallPlan, InstalledSummary,
  KeptEngine, Outdated, PackageInfo, RemovalPlan, RemoveOutcome, SearchResult, SourceSummary,
  StrandedContent, Unavailable, UnavailableReason, VerifyResult,
};
use luthier_core::registry::RefreshOutcome;
use luthier_core::scan::{DetectedPlugin, PluginStatus};
use luthier_core::selfupdate::{NewerLuthier, SelfUpdatePlan};
use serde::Serialize;
use std::io::Write;

pub struct Reporter {
  pub json: bool,
  pub quiet: bool,
}

impl Reporter {
  pub fn new(json: bool, quiet: bool) -> Self {
    Self { json, quiet }
  }

  fn emit<T: Serialize>(&self, value: &T) {
    let text =
      serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    println!("{text}");
  }

  /// A status line that `--json` and `--quiet` suppress.
  /// A caveat about the answer, not part of it: stderr, so piping `--json`
  /// into another program still yields only the answer.
  pub fn warn(&self, message: impl AsRef<str>) {
    if !self.quiet {
      eprintln!("warning: {}", message.as_ref());
    }
  }

  pub fn note(&self, message: impl AsRef<str>) {
    if !self.quiet && !self.json {
      println!("{}", message.as_ref());
    }
  }

  // ---------------------------------------------------------------- search --

  pub fn search(&self, results: &[SearchResult]) {
    if self.json {
      return self.emit(&results);
    }
    if results.is_empty() {
      self.note("No packages matched.");
      return;
    }
    let rows: Vec<Vec<String>> = results
      .iter()
      .map(|r| {
        vec![
          r.id.clone(),
          r.name.clone(),
          r.version.clone().unwrap_or_else(|| "-".into()),
          r.category.clone(),
          if r.tags.is_empty() {
            "-".into()
          } else {
            r.tags.join(", ")
          },
        ]
      })
      .collect();
    table(&["ID", "NAME", "VERSION", "CATEGORY", "TAGS"], &rows);
  }

  // ------------------------------------------------------------------ info --

  pub fn info(&self, info: &PackageInfo) {
    if self.json {
      return self.emit(info);
    }
    println!("{}\n", info.name);
    let mut fields: Vec<(&str, String)> = vec![
      ("ID", info.id.clone()),
      (
        "Version",
        info.latest_version.clone().unwrap_or_else(|| "-".into()),
      ),
      ("Type", info.kind.clone()),
    ];
    fields.push(("Category", info.category.clone()));
    if !info.tags.is_empty() {
      fields.push(("Tags", info.tags.join(", ")));
    }
    fields.push((
      "License",
      format!("{} ({})", info.license, info.license_kind),
    ));
    if !info.formats.is_empty() {
      fields.push(("Formats", info.formats.join(", ")));
    }
    fields.push(("Platform", info.target.clone()));
    fields.push(("Registry", info.registry.clone()));
    if let Some(installed) = &info.installed_version {
      let suffix = info
        .pinned
        .as_ref()
        .map(|p| format!(" (pinned to {p})"))
        .unwrap_or_default();
      fields.push(("Installed", format!("{installed}{suffix}")));
      for (i, path) in info.files.iter().enumerate() {
        fields.push((if i == 0 { "Location" } else { "" }, path.clone()));
      }
    } else if let Some(path) = &info.detected_at {
      fields.push(("Installed", "yes, outside Luthier".into()));
      fields.push(("Found at", path.clone()));
    } else {
      fields.push(("Installed", "no".into()));
    }
    if info.available_versions.len() > 1 {
      fields.push(("Available", info.available_versions.join(", ")));
    }
    if !info.dependencies.is_empty() {
      fields.push(("Depends on", info.dependencies.join(", ")));
    }
    if !info.content.is_empty() {
      fields.push(("Content", info.content.join(", ")));
      fields.push((
        "Played by",
        if info.played_by.is_empty() {
          "no engine any configured registry names".into()
        } else {
          info.played_by.join(", ")
        },
      ));
    }
    if let Some(reviewed) = info.rules_reviewed {
      fields.push((
        "Install rules",
        if reviewed {
          "written and reviewed".into()
        } else {
          "derived from the archive, not reviewed".into()
        },
      ));
    }
    if !info.authors.is_empty() {
      fields.push(("Authors", info.authors.join(", ")));
    }
    if let Some(homepage) = &info.homepage {
      fields.push(("Homepage", homepage.clone()));
    }
    if let Some(repository) = &info.repository {
      fields.push(("Source", repository.clone()));
    }
    if let Some(hint) = &info.provisioning_hint {
      fields.push(("Obtaining", hint.clone()));
    }

    let width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in &fields {
      println!("{key:<width$}  {value}", width = width);
    }
    if let Some(description) = &info.description {
      println!("\n{description}");
    }
    if !info.unknown_fields.is_empty() {
      eprintln!(
        "\nnote: this manifest uses fields this version does not understand: {}",
        info.unknown_fields.join(", ")
      );
    }
  }

  // --------------------------------------------------------------- install --

  /// The plan as the answer: the command stops here.
  pub fn install_plan(&self, plan: &InstallPlan) {
    if self.json {
      return self.emit(plan);
    }
    self.plan_body(plan);
  }

  /// The plan as a preview of what is about to happen.
  ///
  /// Silent under `--json`, where the outcome is the answer: emitting both
  /// would put two complete documents on stdout, and a stream of concatenated
  /// documents is not JSON — `jq` and `serde_json` reject it alike.
  pub fn install_preview(&self, plan: &InstallPlan) {
    if self.json {
      return;
    }
    self.plan_body(plan);
  }

  fn plan_body(&self, plan: &InstallPlan) {
    let actionable: Vec<&_> = plan.actionable().collect();
    for id in &plan.marked_requested {
      self.note(format!(
        "{id} is already installed, as a dependency; asking for it by name records it as \
         requested, so `autoremove` keeps it."
      ));
    }
    for id in &plan.marked_dependency {
      self.note(format!(
        "{id} was installed by name; the file lists it as a dependency, which is how it is \
         recorded, so `autoremove` removes it once nothing needs it."
      ));
    }
    if actionable.is_empty() {
      if plan.marked_requested.is_empty() && plan.marked_dependency.is_empty() {
        self.note("Everything requested is already installed.");
      }
    } else {
      println!("The following will be installed:\n");
      let rows: Vec<Vec<String>> = actionable
        .iter()
        .map(|s| {
          vec![
            s.id.clone(),
            s.version.clone(),
            s.action.clone(),
            s.reason.clone(),
            s.download_bytes
              .map(human_bytes)
              .unwrap_or_else(|| "-".into()),
          ]
        })
        .collect();
      table(&["ID", "VERSION", "ACTION", "WHY", "DOWNLOAD"], &rows);

      let total = plan.total_download_bytes();
      if total > 0 {
        println!("\nTotal download: {}", human_bytes(total));
      }
    }

    for external in &plan.missing_externals {
      println!(
        "\n{} ({}) is required but is not installed.",
        external.name, external.id
      );
      if let Some(hint) = &external.provisioning_hint {
        println!("  {hint}");
      }
    }

    for found in &plan.unplayable {
      if self.quiet {
        break;
      }
      eprintln!();
      // A note on the plan, not a refusal: the confirmation below is where
      // the user decides. What the format needs is true whatever any
      // registry knows, so it is said first and always.
      if found.engines.is_empty() {
        self.warn(format!(
          "{} holds {} content, and playing it needs {}. Nothing on this system \
           looks like one, and no configured registry names one either.",
          found.id, found.content, found.played_by
        ));
        continue;
      }
      self.warn(format!(
        "{} holds {} content, and playing it needs {}. Nothing on this system \
         looks like one; any of these would do:",
        found.id, found.content, found.played_by
      ));
      let width = found.engines.iter().map(|e| e.id.len()).max().unwrap_or(0);
      for engine in &found.engines {
        let how = if engine.installable {
          format!("luthier install {}", engine.id)
        } else {
          engine
            .provisioning_hint
            .clone()
            .unwrap_or_else(|| "not available from any configured registry".into())
        };
        eprintln!("  {:<width$}  {how}", engine.id, width = width);
      }
    }
  }

  /// An import with nothing to install or remove.
  pub fn import_unchanged(&self, plan: &InstallPlan) {
    if plan.marked_requested.is_empty() && plan.marked_dependency.is_empty() {
      self.note("Nothing to do; the installation already matches.");
    } else {
      self.note("Nothing else to do; the installation now matches.");
    }
  }

  pub fn install_outcome(&self, outcome: &InstallOutcome) {
    if self.json {
      return self.emit(outcome);
    }
    // Before the result, not after: a mismatch between what a source
    // claimed and what its archive held is the reason to distrust the
    // line that follows.
    for warning in &outcome.warnings {
      self.warn(warning);
    }
    for package in &outcome.installed {
      if package.formats.is_empty() {
        // A pack installs no files of its own.
        println!("Installed {} {}", package.name, package.version);
      } else {
        println!(
          "Installed {} {} ({})",
          package.name,
          package.version,
          package.formats.join(", ")
        );
      }
    }
    for package in &outcome.skipped {
      self.note(format!(
        "{} {} was already installed",
        package.name, package.version
      ));
    }
    for id in &outcome.no_longer_installed {
      self.note(format!(
        "{id} was removed after the plan was made, so it was not upgraded"
      ));
    }
    if outcome.installed.is_empty()
      && outcome.skipped.is_empty()
      && outcome.no_longer_installed.is_empty()
    {
      self.note("Nothing to do.");
    }
  }

  // ---------------------------------------------------------------- remove --

  /// The removal plan as a preview of what is about to happen; silent under
  /// `--json` for the same reason as [`Reporter::install_preview`].
  pub fn removal_preview(&self, plan: &RemovalPlan) {
    if self.json {
      return;
    }
    self.removal_plan(plan);
  }

  /// An import's result: one document under `--json`, whatever it did.
  pub fn import_outcome(&self, outcome: &ImportOutcome) {
    if self.json {
      return self.emit(outcome);
    }
    self.install_outcome(&outcome.installed);
    for pin in &outcome.reapplied_pins {
      self.note(format!("Reapplied pin: {pin}"));
    }
    for pin in &outcome.skipped_pins {
      self.warn(format!("pin not reapplied: {pin}"));
    }
    if let Some(pruned) = &outcome.pruned {
      self.remove_outcome(pruned);
    }
    for id in &outcome.prune_kept {
      self.warn(format!(
        "kept {id}, which was to be removed: a package that stays still needs it"
      ));
    }
  }

  /// Said with the plan, since an unattended `import --prune` should not
  /// look as though it converged; under `--json` the outcome carries it.
  pub fn prune_withheld(&self, reason: Option<&str>) {
    if let Some(reason) = reason {
      self.warn(format!(
        "not removing anything the file does not name: a source could not be read ({reason}). \
         `luthier update`, then import again."
      ));
    }
  }

  pub fn removal_plan(&self, plan: &RemovalPlan) {
    if self.json {
      return self.emit(plan);
    }
    for package in &plan.packages {
      println!("Package: {} {}\n", package.name, package.version);
      if package.files.is_empty() {
        println!("This package installs no files of its own.");
      } else {
        println!("Files to remove:");
        for file in &package.files {
          println!("  {file}");
        }
      }
      if !package.blocked_by.is_empty() {
        println!(
          "\n{} is still required by {}.",
          package.id,
          package.blocked_by.join(", ")
        );
      }
      println!();
    }
    self.stranded(&plan.stranded, "would be left");
  }

  /// Content this removal leaves with no engine.
  ///
  /// Not a refusal: the engine may be arriving from the user's distribution,
  /// or the library may be going next. Silence with no explanation is what
  /// this exists to prevent.
  fn stranded(&self, stranded: &[StrandedContent], tense: &str) {
    for found in stranded {
      println!(
        "warning: {} {tense} with nothing to play its {} content.",
        found.id, found.content
      );
      if !found.engines.is_empty() {
        println!("  Any one of these would: {}", found.engines.join(", "));
      }
    }
  }

  pub fn remove_outcome(&self, outcome: &RemoveOutcome) {
    if self.json {
      return self.emit(outcome);
    }
    for package in &outcome.removed {
      println!("Removed {} {}", package.name, package.version);
    }
    for kept in &outcome.kept_files {
      println!("Kept {}: {}", kept.path, kept.reason);
    }
    if !outcome.kept_files.is_empty() {
      println!(
        "Luthier no longer tracks what it kept; delete it yourself when you are done with it."
      );
    }
    self.stranded(&outcome.stranded, "is left");
    for id in &outcome.no_longer_installed {
      self.note(format!("{id} was removed meanwhile by another command"));
    }
  }

  // ---------------------------------------------------------- search path --

  /// Shell commands, printed bare so they can be evaluated.
  ///
  /// Nothing else may reach stdout here: the caller pipes this into `eval`,
  /// so a stray status line would be executed.
  pub fn search_path(&self, search_path: &luthier_core::layout::SearchPath) {
    if self.json {
      return self.emit(search_path);
    }
    for (key, value) in &search_path.set {
      println!("export {key}=\"{value}\"");
    }
  }

  // ------------------------------------------------------------------ list --

  pub fn list(&self, packages: &[InstalledSummary]) {
    if self.json {
      return self.emit(&packages);
    }
    if packages.is_empty() {
      self.note("No packages installed.");
      return;
    }
    let rows: Vec<Vec<String>> = packages
      .iter()
      .map(|p| {
        let mut version = p.version.clone();
        if let Some(pin) = &p.pinned {
          version = format!("{version} (pinned {pin})");
        }
        vec![
          p.name.clone(),
          version,
          p.formats.join(","),
          p.reason.clone(),
        ]
      })
      .collect();
    table(&["NAME", "VERSION", "FORMATS", "WHY"], &rows);
  }

  /// The paths of named packages, one per line, like `brew list <formula>`.
  pub fn files(&self, packages: &[InstalledSummary]) {
    if self.json {
      return self.emit(&packages);
    }
    for path in packages.iter().flat_map(|p| &p.files) {
      println!("{path}");
    }
  }

  pub fn scan(&self, plugins: &[DetectedPlugin]) {
    #[derive(Serialize)]
    struct Row {
      path: String,
      format: String,
      name: String,
      status: String,
      package: Option<String>,
    }
    let rows: Vec<Row> = plugins
      .iter()
      .map(|p| Row {
        path: p.path.display().to_string(),
        format: p.format.to_string(),
        name: p.name.clone(),
        status: if p.is_managed() {
          "managed"
        } else {
          "unmanaged"
        }
        .into(),
        package: match &p.status {
          PluginStatus::Managed { package, .. } => Some(package.to_string()),
          PluginStatus::Unmanaged => None,
        },
      })
      .collect();

    if self.json {
      return self.emit(&rows);
    }
    if rows.is_empty() {
      self.note("No plugins found.");
      return;
    }
    let table_rows: Vec<Vec<String>> = rows
      .iter()
      .map(|r| {
        vec![
          r.name.clone(),
          r.format.clone(),
          match &r.package {
            Some(id) => format!("Installed by Luthier ({id})"),
            None => "Installed outside Luthier".into(),
          },
        ]
      })
      .collect();
    table(&["NAME", "FORMAT", "STATUS"], &table_rows);
  }

  // ---------------------------------------------------------------- update --

  /// An `upgrade` with nothing to do, or refused: the plan is the answer.
  pub fn upgrade_plan(&self, plan: &InstallPlan) {
    if self.json {
      return self.emit(plan);
    }
    if !plan.is_blocked() && plan.actionable().next().is_none() {
      // Not "up to date" when the notes that follow say something was
      // left behind.
      if plan.held_back.is_empty() {
        self.note("Everything is up to date.");
      } else {
        self.note("Nothing to upgrade.");
      }
      return;
    }
    self.plan_body(plan);
  }

  /// What `upgrade` leaves alone: updates a pin is holding, and packages
  /// nothing can upgrade any more.
  ///
  /// Notes rather than a document: the plan is the answer this command
  /// already emitted, and under `--json` they are in it.
  pub fn held_back(&self, held: &Outdated) {
    for update in &held.updates {
      let Some(pin) = &update.pinned else {
        continue;
      };
      self.note(format!(
        "{} {} is available; {} is pinned to {pin}. Lift it with: luthier unpin {}",
        update.name, update.available, update.id, update.id
      ));
    }
    for package in &held.unavailable {
      self.note(format!(
        "{} stays at {}: {}.",
        package.name,
        package.installed,
        unavailable_reason(package)
      ));
    }
  }

  pub fn updates(&self, outdated: &Outdated) {
    if self.json {
      return self.emit(outdated);
    }
    let Outdated {
      updates,
      unavailable,
    } = outdated;
    if outdated.is_empty() {
      self.note("Everything is up to date.");
      return;
    }
    if !updates.is_empty() {
      println!("Updates available:\n");
      let width = updates.iter().map(|u| u.name.len()).max().unwrap_or(0);
      for update in updates {
        let pinned = update
          .pinned
          .as_ref()
          .map(|p| format!("  (pinned to {p}; skipped)"))
          .unwrap_or_default();
        println!(
          "{:<width$}  {} -> {}{pinned}",
          update.name,
          update.installed,
          update.available,
          width = width
        );
      }
      if updates.iter().any(|u| u.pinned.is_none()) {
        println!(
          "\nRun `luthier upgrade` to upgrade them, or `luthier upgrade <package>` for one."
        );
      }
    }
    if !unavailable.is_empty() {
      if !updates.is_empty() {
        println!();
      }
      println!("Staying at the version installed:\n");
      let versions: Vec<String> = unavailable
        .iter()
        .map(|u| match &u.available {
          Some(available) => format!("{} ({available} is out)", u.installed),
          None => u.installed.clone(),
        })
        .collect();
      let width = unavailable.iter().map(|u| u.name.len()).max().unwrap_or(0);
      let version_width = versions.iter().map(String::len).max().unwrap_or(0);
      for (package, version) in unavailable.iter().zip(&versions) {
        println!(
          "{:<width$}  {:<version_width$}  {}",
          package.name,
          version,
          unavailable_reason(package),
          width = width,
          version_width = version_width
        );
      }
    }
  }

  /// The line after `update`, as apt prints one after its own: how many of
  /// the installed packages the lists just fetched have something newer
  /// for, how many a pin holds back, and how many nothing can upgrade.
  pub fn upgradable(&self, outdated: &Outdated) {
    let pinned = outdated
      .updates
      .iter()
      .filter(|u| u.pinned.is_some())
      .count();
    let count = outdated.updates.len() - pinned;
    let reasons = |wanted: &[UnavailableReason]| {
      outdated
        .unavailable
        .iter()
        .filter(|u| wanted.contains(&u.reason))
        .count()
    };
    // Each said as what it is: an orphan can be upgraded by name, and a
    // package from a source that could not be read may be fine after the
    // next `update`.
    let stuck = reasons(&[
      UnavailableReason::NotCarried,
      UnavailableReason::NoReleaseForTarget,
      UnavailableReason::NeededByUnavailable,
    ]);
    let unread = reasons(&[UnavailableReason::SourceUnreadable]);
    let unresolvable = reasons(&[
      UnavailableReason::Unresolvable,
      UnavailableReason::NeededByHeldBack,
    ]);
    let orphaned = reasons(&[UnavailableReason::Orphaned]);
    let said: Vec<String> = [
      (
        count,
        "package can be upgraded.",
        "packages can be upgraded.",
      ),
      (
        pinned,
        "update is held back by a pin.",
        "updates are held back by a pin.",
      ),
      (
        stuck,
        "installed package can no longer be upgraded.",
        "installed packages can no longer be upgraded.",
      ),
      (
        unread,
        "package could not be checked, as its source could not be read.",
        "packages could not be checked, as their source could not be read.",
      ),
      (
        unresolvable,
        "package is held back by a conflict.",
        "packages are held back by a conflict.",
      ),
      (
        orphaned,
        "update is for a dependency nothing needs now.",
        "updates are for dependencies nothing needs now.",
      ),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
    .collect();
    if said.is_empty() {
      return self.note("\nEverything is up to date.");
    }
    let total = count + pinned + stuck + unread + unresolvable + orphaned;
    let them = if total == 1 { "it" } else { "them" };
    self.note(format!(
      "\n{} Run `luthier outdated` to see {them}.",
      said.join(" ")
    ));
  }

  // ---------------------------------------------------------------- verify --

  pub fn verify(&self, results: &[VerifyResult]) {
    if self.json {
      return self.emit(&results);
    }
    if results.is_empty() {
      self.note("Nothing to verify.");
      return;
    }
    for result in results {
      if result.ok {
        println!("{} {}: ok", result.name, result.version);
      } else {
        println!(
          "{} {}: {} problem(s)",
          result.name,
          result.version,
          result.problems.len()
        );
        for problem in &result.problems {
          println!("  {}: {}", problem.path, problem.problem);
        }
      }
    }
  }

  // ------------------------------------------------------------ autoremove --

  /// `autoremove --dry-run`, or one with nothing to remove: the plan is
  /// the answer.
  pub fn autoremove_plan(&self, plan: &RemovalPlan) {
    if self.json {
      return self.emit(plan);
    }
    self.autoremove_body(plan);
  }

  /// What `autoremove` is about to remove; silent under `--json` for the
  /// same reason as [`Reporter::install_preview`].
  pub fn autoremove_preview(&self, plan: &RemovalPlan) {
    if self.json {
      return;
    }
    self.autoremove_body(plan);
  }

  /// Orphans `autoremove` leaves, because content still installed needs
  /// them. A note: nothing is wrong, and `remove` takes one if the user
  /// wants it gone.
  pub fn kept_engines(&self, kept: &[KeptEngine]) {
    for engine in kept {
      self.note(format!(
        "Keeping {}: it plays the {} content in {}. `luthier remove {}` removes it anyway.",
        engine.id, engine.plays, engine.for_package, engine.id
      ));
    }
    if !kept.is_empty() {
      self.note("");
    }
  }

  fn autoremove_body(&self, plan: &RemovalPlan) {
    if plan.packages.is_empty() {
      self.note("Nothing to remove; every dependency is still needed.");
      return;
    }
    println!("Installed as dependencies, and needed by nothing installed now:\n");
    self.removal_plan(plan);
  }

  // -------------------------------------------------------------- location --

  pub fn locations(&self, locations: &[luthier_core::api::LocationSummary]) {
    if self.json {
      return self.emit(&locations);
    }
    println!("{:<10}  {:<9}  PATH", "PART", "SOURCE");
    for location in locations {
      let source = match (&location.configured, location.available) {
        (None, _) => "default",
        (Some(_), true) => "chosen",
        (Some(_), false) => "MISSING",
      };
      for (i, path) in location.paths.iter().enumerate() {
        let (part, source) = if i == 0 {
          (location.kind.label(), source)
        } else {
          ("", "")
        };
        println!("{part:<10}  {source:<9}  {}", path.display());
      }
    }
    if locations.iter().any(|l| !l.available) {
      println!("\nA MISSING location is on a disk that is not mounted, or was deleted.");
    }
  }

  // --------------------------------------------------------------- sources --

  pub fn sources(&self, sources: &[SourceSummary]) {
    if self.json {
      return self.emit(&sources);
    }
    if sources.is_empty() {
      self.note("No sources; nothing can be installed.");
      return;
    }
    println!("{:<4}  {:<20}  {:<9}  LOCATION", "#", "NAME", "TYPE");
    for source in sources {
      println!(
        "{:<4}  {:<20}  {:<9}  {}",
        source.priority, source.name, source.kind, source.location
      );
    }
    println!("\nConsulted in this order; the first to carry a package ID keeps it.");
  }

  // ----------------------------------------------------------------- cache --

  pub fn cache_list(&self, entries: &[CacheEntry]) {
    if self.json {
      return self.emit(&entries);
    }
    if entries.is_empty() {
      self.note("The artifact cache is empty.");
      return;
    }

    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    let reclaimable: u64 = entries
      .iter()
      .filter(|e| e.used_by.is_empty())
      .map(|e| e.bytes)
      .sum();

    println!("{:<12}  {:<10}  USED BY", "SIZE", "STATE");
    for entry in entries {
      let state = if entry.partial {
        "partial"
      } else if entry.used_by.is_empty() {
        "unused"
      } else {
        "in use"
      };
      let used_by = if entry.used_by.is_empty() {
        "—".to_owned()
      } else {
        entry.used_by.join(", ")
      };
      println!(
        "{:<12}  {:<10}  {}",
        human_bytes(entry.bytes),
        state,
        used_by
      );
    }
    println!(
      "\n{} in {} file(s); {} reclaimable with `luthier cache clean`.",
      human_bytes(total),
      entries.len(),
      human_bytes(reclaimable)
    );
  }

  pub fn cache_cleaned(&self, cleaned: &CacheCleaned) {
    if self.json {
      return self.emit(&cleaned);
    }
    if cleaned.removed.is_empty() {
      self.note("Nothing in the cache to reclaim.");
      return;
    }
    if cleaned.dry_run {
      println!(
        "Would remove {} file(s), reclaiming {}.",
        cleaned.removed.len(),
        human_bytes(cleaned.bytes)
      );
      return;
    }
    println!(
      "Removed {} file(s), reclaiming {}.",
      cleaned.removed.len(),
      human_bytes(cleaned.bytes)
    );
  }

  // --------------------------------------------------------------- refresh --

  pub fn refresh(&self, outcomes: &[RefreshOutcome]) {
    #[derive(Serialize)]
    struct Row {
      registry: String,
      packages: usize,
      updated: bool,
      #[serde(skip_serializing_if = "Option::is_none")]
      failed: Option<String>,
    }
    let rows: Vec<Row> = outcomes
      .iter()
      .map(|o| Row {
        registry: o.registry.clone(),
        packages: o.packages,
        updated: o.updated,
        failed: o.failure.clone(),
      })
      .collect();
    if self.json {
      return self.emit(&rows);
    }
    for row in &rows {
      match &row.failed {
        // On stderr and as a warning: the command did refresh something,
        // and a piped stdout should carry that rather than the failure.
        Some(reason) => self.warn(format!("{}: not refreshed: {reason}", row.registry)),
        None if !row.updated => {
          println!("{}: {} packages, unchanged", row.registry, row.packages)
        }
        None => println!("{}: {} packages", row.registry, row.packages),
      }
    }
  }

  /// A refresh a command made on its own before reading the package list.
  /// Said only when it failed: the command goes on with whatever list it
  /// has — and if it has none, the warning that follows says so — and its
  /// own answer is on stdout.
  pub fn automatic_refresh(&self, outcome: &RefreshOutcome) {
    if let Some(reason) = &outcome.failure {
      self.warn(format!(
        "{}: could not check for a newer package list: {reason}",
        outcome.registry
      ));
    }
  }

  // ----------------------------------------------------------- self-update --

  /// After `update` and `outdated`. A status line, so `--json` keeps its
  /// one document and `--quiet` its silence.
  pub fn newer_luthier(&self, newer: &NewerLuthier) {
    let how = if newer.updates_itself {
      "Run `luthier upgrade --self` to upgrade."
    } else {
      "Update it with the package manager that installed it."
    };
    self.note(format!(
      "\nLuthier {} is out (this is {}). {how}",
      newer.latest, newer.current
    ));
  }

  pub fn luthier_is_current(&self, plan: &SelfUpdatePlan) {
    if self.json {
      return self.emit(plan);
    }
    self.note(format!("Luthier {} is the latest release.", plan.current));
  }

  /// What `upgrade --self` will do, before it asks. Under `--json` the one
  /// document is the outcome, written by [`Reporter::self_update_done`].
  pub fn self_update_preview(&self, plan: &SelfUpdatePlan) {
    if self.json {
      return;
    }
    let Some(asset) = &plan.asset else { return };
    println!("Luthier {} -> {}\n", plan.current, plan.latest);
    println!(
      "Downloads {} ({}), checked against the SHA-256 GitHub publishes for it.",
      asset.name,
      human_bytes(asset.size)
    );
    match &plan.package {
      None => {
        println!("Tries the new binary, then replaces:");
        for replacement in &plan.replaces {
          println!("  {}", replacement.path.display());
        }
      }
      Some(package) => {
        let through = if package.sudo {
          " through `sudo /bin/sh`"
        } else {
          ""
        };
        println!(
          "Then, as root{through}, copies it where only root can write, checks it again, and runs:"
        );
        println!("  {}", package.manager_line(&asset.name));
      }
    }
  }

  pub fn self_update_done(&self, plan: &SelfUpdatePlan) {
    if self.json {
      return self.emit(plan);
    }
    self.note(format!("Luthier is now {}.", plan.latest));
  }

  pub fn pin(&self, package: &InstalledSummary) {
    if self.json {
      return self.emit(package);
    }
    match &package.pinned {
      Some(version) => println!("{} is pinned to {version}", package.id),
      None => println!("{} is no longer pinned", package.id),
    }
  }
}

fn unavailable_reason(package: &Unavailable) -> String {
  match package.reason {
    UnavailableReason::NotCarried => "no source carries it any more".into(),
    UnavailableReason::SourceUnreadable => {
      "its source could not be read; `luthier update` fetches it again".into()
    }
    UnavailableReason::NoReleaseForTarget => "no release is built for this machine any more".into(),
    UnavailableReason::NeededByUnavailable => format!(
      "{} needs it, and can no longer be upgraded",
      package.because_of.as_deref().unwrap_or("a package")
    ),
    UnavailableReason::Unresolvable => format!(
      "its upgrade cannot be resolved: {}",
      package
        .detail
        .as_deref()
        .unwrap_or("a requirement conflicts")
    ),
    UnavailableReason::NeededByHeldBack => format!(
      "{} needs it, and is held back",
      package.because_of.as_deref().unwrap_or("a package")
    ),
    UnavailableReason::Orphaned => {
      "nothing installed needs it; `luthier autoremove` removes it".into()
    }
  }
}

/// Renders left-aligned columns with a two-space gutter.
fn table(headers: &[&str], rows: &[Vec<String>]) {
  let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
  for row in rows {
    for (i, cell) in row.iter().enumerate() {
      if i < widths.len() {
        widths[i] = widths[i].max(cell.chars().count());
      }
    }
  }

  let mut out = std::io::stdout().lock();
  let line = |cells: &[String], widths: &[usize]| -> String {
    cells
      .iter()
      .enumerate()
      .map(|(i, cell)| {
        let pad = widths[i].saturating_sub(cell.chars().count());
        if i == cells.len() - 1 {
          cell.clone()
        } else {
          format!("{cell}{}", " ".repeat(pad))
        }
      })
      .collect::<Vec<_>>()
      .join("  ")
  };

  let header: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
  let _ = writeln!(out, "{}", line(&header, &widths));
  for row in rows {
    let _ = writeln!(out, "{}", line(row, &widths));
  }
}

/// Sizes in the units a person reads.
pub fn human_bytes(bytes: u64) -> String {
  const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
  let mut value = bytes as f64;
  let mut unit = 0;
  while value >= 1024.0 && unit < UNITS.len() - 1 {
    value /= 1024.0;
    unit += 1;
  }
  if unit == 0 {
    format!("{bytes} B")
  } else {
    format!("{value:.1} {}", UNITS[unit])
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn byte_sizes_read_naturally() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(999), "999 B");
    assert_eq!(human_bytes(7 * 1024 * 1024), "7.0 MiB");
    // The real Surge XT plugins-only tarball.
    assert_eq!(human_bytes(96_468_992), "92.0 MiB");
  }
}
