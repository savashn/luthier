//! Turning results into output.
//!
//! Every command produces a serialisable result from the core and hands it
//! here, so `--json` is a rendering choice rather than a separate code path
//! (§33). Human output goes to stdout; logs and progress go to stderr, so
//! piping `--json` into a tool stays clean.

use luthier_core::api::{
  AvailableUpdate, BenchSummary, CacheCleaned, CacheEntry, InstallOutcome, InstallPlan,
  InstalledSummary, PackageInfo, RemovalPlan, RemoveOutcome, SearchResult, StrandedContent,
  VerifyResult,
};
use luthier_core::registry::RefreshOutcome;
use luthier_core::scan::{DetectedPlugin, PluginStatus};
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
    if actionable.is_empty() {
      self.note("Everything requested is already installed.");
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
    if outcome.installed.is_empty() && outcome.skipped.is_empty() {
      self.note("Nothing to do.");
    }
  }

  // ---------------------------------------------------------------- remove --

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
  }

  // ------------------------------------------------------------------- env --

  pub fn envs(&self, envs: &[luthier_core::env::EnvSummary]) {
    if self.json {
      return self.emit(&envs);
    }
    if envs.is_empty() {
      self.note("No environments. Create one with: luthier env create <name>");
      return;
    }
    let rows: Vec<Vec<String>> = envs
      .iter()
      .map(|e| {
        vec![
          if e.active {
            format!("* {}", e.name)
          } else {
            format!("  {}", e.name)
          },
          e.packages.to_string(),
          e.path.display().to_string(),
        ]
      })
      .collect();
    table(&["NAME", "PACKAGES", "PATH"], &rows);
  }

  /// Shell commands, printed bare so they can be evaluated.
  ///
  /// Nothing else may reach stdout here: the caller pipes this into `eval`,
  /// so a stray status line would be executed.
  pub fn activation(&self, activation: &luthier_core::env::Activation) {
    if self.json {
      return self.emit(activation);
    }
    for (key, value) in &activation.set {
      println!("export {key}=\"{value}\"");
    }
  }

  pub fn deactivation(&self, activation: &luthier_core::env::Activation) {
    if self.json {
      return self.emit(activation);
    }
    for key in &activation.unset {
      println!("unset {key}");
    }
  }

  pub fn env_show(&self, active: Option<&str>, path: Option<&std::path::Path>) {
    if self.json {
      return self.emit(&serde_json::json!({
          "active": active,
          "path": path.map(|p| p.display().to_string()),
      }));
    }
    match (active, path) {
      (Some(name), Some(path)) => {
        println!("{name}");
        println!("{}", path.display());
      }
      _ => self.note("No environment active; using the default locations."),
    }
  }

  /// A bare path, for use in shell substitution.
  pub fn path(&self, path: &std::path::Path) {
    if self.json {
      return self.emit(&path.display().to_string());
    }
    println!("{}", path.display());
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

  /// Why a targeted `update` did nothing: a pin is holding it.
  ///
  /// A note rather than a document: the plan is the answer this command
  /// already emitted, and under `--json` the pin is in it.
  pub fn held_back(&self, updates: &[AvailableUpdate]) {
    for update in updates {
      let Some(pin) = &update.pinned else {
        continue;
      };
      self.note(format!(
        "{} {} is available; {} is pinned to {pin}. Lift it with: luthier unpin {}",
        update.name, update.available, update.id, update.id
      ));
    }
  }

  pub fn updates(&self, updates: &[AvailableUpdate]) {
    if self.json {
      return self.emit(&updates);
    }
    if updates.is_empty() {
      self.note("Everything is up to date.");
      return;
    }
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
    let updatable: Vec<&str> = updates
      .iter()
      .filter(|u| u.pinned.is_none())
      .map(|u| u.id.as_str())
      .collect();
    if let Some(first) = updatable.first() {
      println!("\nRun:\naudio update {first}");
    }
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

  // --------------------------------------------------------------- cleanup --

  pub fn cleanup(&self, orphans: &[InstalledSummary]) {
    if self.json {
      return self.emit(&orphans);
    }
    if orphans.is_empty() {
      self.note("No unused packages.");
      return;
    }
    println!("Unused packages:\n");
    for package in orphans {
      println!("{}", package.id);
    }
    println!("\nThese packages are not currently required.\n\nUse:\nluthier remove <package>");
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

  // ----------------------------------------------------------------- bench --

  pub fn benches(&self, benches: &[BenchSummary]) {
    if self.json {
      return self.emit(&benches);
    }
    if benches.is_empty() {
      self.note("No sources; nothing can be installed.");
      return;
    }
    println!("{:<4}  {:<20}  {:<9}  LOCATION", "#", "NAME", "TYPE");
    for bench in benches {
      println!(
        "{:<4}  {:<20}  {:<9}  {}",
        bench.priority, bench.name, bench.kind, bench.location
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
        None => println!("{}: {} packages", row.registry, row.packages),
      }
    }
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
