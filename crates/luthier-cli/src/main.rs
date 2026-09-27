//! The `luthier` command.
//!
//! Deliberately thin (§38): parse arguments, build a [`Session`], call one core
//! operation, render the result. Anything resembling a decision about packages
//! belongs in `luthier-core`, so the GUI planned later behaves identically.

mod args;
mod progress;
mod render;

use args::{BenchCommand, CacheCommand, Cli, Command, EnvCommand, GlobalArgs, LocationCommand};
use clap::Parser;
use luthier_core::api::{Environments, Session, Storage};
use luthier_core::env::EnvName;
use luthier_core::error::{Error, ExitCode, Result};
use luthier_core::layout::LocationKind;
use luthier_core::{Config, Layout};
use luthier_manifest::PackageId;
use progress::BarProgress;
use render::Reporter;
use std::io::{IsTerminal, Write};

fn main() -> std::process::ExitCode {
  let cli = Cli::parse();
  init_logging(&cli.global);

  let reporter = Reporter::new(cli.global.json, cli.global.quiet);

  let runtime = match tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
  {
    Ok(runtime) => runtime,
    Err(e) => {
      eprintln!("error: cannot start the async runtime: {e}");
      return ExitCode::Generic.into();
    }
  };

  match runtime.block_on(run(&cli, &reporter)) {
    Ok(()) => ExitCode::Success.into(),
    Err(error) => {
      report_error(&error);
      error.exit_code().into()
    }
  }
}

/// Errors are printed in full, with the follow-up line that says what to do (§36).
fn report_error(error: &Error) {
  eprintln!("error: {error}");

  let mut source = std::error::Error::source(error);
  while let Some(cause) = source {
    eprintln!("  caused by: {cause}");
    source = cause.source();
  }

  if let Some(hint) = error.hint() {
    eprintln!("\n{hint}");
  }
}

fn init_logging(global: &GlobalArgs) {
  use tracing_subscriber::{EnvFilter, fmt};

  let default = match (global.quiet, global.verbose) {
    (true, _) => "error",
    (_, 0) => "warn",
    (_, 1) => "luthier_core=debug,luthier_cli=debug,luthier_manifest=debug",
    _ => "trace",
  };

  // Logs go to stderr so `--json` on stdout stays parseable.
  let _ = fmt()
    .with_env_filter(
      EnvFilter::try_from_env("LUTHIER_LOG").unwrap_or_else(|_| EnvFilter::new(default)),
    )
    .with_writer(std::io::stderr)
    .with_target(global.verbose > 1)
    .without_time()
    .try_init();
}

/// The layout before any location or environment redirect.
///
/// `--root` confines what is *written*; the system search paths govern what is
/// *seen*. They are orthogonal, so a rooted layout still sees the machine
/// unless `--no-system-plugins` says otherwise. `Layout::rooted_at` itself
/// stays hermetic, which is what keeps the library's own tests independent of
/// the machine running them.
fn home_layout(global: &GlobalArgs) -> Result<Layout> {
  Ok(match &global.root {
    Some(root) => {
      let layout = Layout::rooted_at(root);
      if global.no_system_plugins {
        layout
      } else {
        layout.with_system_roots(Layout::default_system_roots())
      }
    }
    None if global.no_system_plugins => Layout::from_env()?.with_system_roots(Default::default()),
    None => Layout::from_env()?,
  })
}

/// The layout before any environment redirect: the base, with the locations
/// the configuration names applied.
fn build_layout(global: &GlobalArgs) -> Result<Layout> {
  Config::located(home_layout(global)?)
}

/// Which environment this invocation acts on: `--env`, else `LUTHIER_ENV`.
///
/// Selection is never read from a file on disk. A stored "current environment"
/// would let one terminal change what another is about to install into.
fn selected_env(global: &GlobalArgs) -> Result<Option<EnvName>> {
  let raw = match &global.env {
    Some(name) => Some(EnvName::new(name.clone())),
    None => luthier_core::env::from_environment(),
  };
  raw
    .transpose()
    .map_err(|e| Error::InvalidArgument(e.to_string()))
}

fn build_session(global: &GlobalArgs) -> Result<Session> {
  let layout = environments(global)?.selected_layout()?;
  let config = match &global.registry_path {
    Some(path) => luthier_core::config::from_path(path),
    None => Config::load(&layout)?,
  };
  Ok(Session::new(layout, config)?.offline(global.offline))
}

/// The environment surface, over the base layout: `env` acts *on*
/// environments, so it must work when the selected one does not exist yet.
fn environments(global: &GlobalArgs) -> Result<Environments> {
  Ok(Environments::new(
    build_layout(global)?,
    selected_env(global)?,
  ))
}

fn ids(session: &Session, raw: &[String]) -> Result<Vec<PackageId>> {
  raw.iter().map(|name| session.parse_id(name)).collect()
}

/// Asks for confirmation unless `--yes`, and refuses on a non-interactive stdin.
///
/// `--json` is not consent. It says how to render an answer, and a user who
/// asked for machine-readable output has not thereby agreed to have files
/// deleted without being asked — which is the same reasoning that makes a
/// non-interactive stdin a refusal rather than a yes.
fn confirm(global: &GlobalArgs, question: &str) -> Result<bool> {
  if global.yes {
    return Ok(true);
  }
  if !std::io::stdin().is_terminal() {
    // Refusing rather than assuming yes: this is the safe default for a
    // command that writes to a user's plugin directories.
    return Err(Error::InvalidArgument(
      "confirmation required; re-run with --yes to proceed non-interactively".into(),
    ));
  }

  print!("{question} [y/N] ");
  let _ = std::io::stdout().flush();
  let mut answer = String::new();
  std::io::stdin()
    .read_line(&mut answer)
    .map_err(|e| Error::io("read confirmation from", "stdin", e))?;
  Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

async fn run(cli: &Cli, reporter: &Reporter) -> Result<()> {
  let global = &cli.global;

  // Neither reads a registry, a state file or a plugin directory, so neither
  // waits for a session to be built or for a registry warning to be printed.
  match &cli.command {
    Command::Completions { shell } => {
      let mut command = <Cli as clap::CommandFactory>::command();
      let name = command.get_name().to_string();
      clap_complete::generate(*shell, &mut command, name, &mut std::io::stdout());
      return Ok(());
    }
    Command::Man => {
      let command = <Cli as clap::CommandFactory>::command();
      clap_mangen::Man::new(command)
        .render(&mut std::io::stdout())
        .map_err(|e| Error::io("write", "stdout", e))?;
      return Ok(());
    }
    _ => {}
  }

  // Changes where things go, so it must work when a chosen disk is missing
  // — `location reset` is the way out of that.
  if let Command::Location { command } = &cli.command {
    return run_location(global, command.as_ref(), reporter);
  }

  if let Command::Env { command } = &cli.command
    && !matches!(
      command,
      EnvCommand::Export { .. } | EnvCommand::Import { .. }
    )
  {
    return run_env(global, command, reporter);
  }

  let session = build_session(global)?;

  // A registry that could not be read is reported before the command
  // answers, so a half-configured setup never looks like a complete one.
  //
  // Named as the commands that consult a registry rather than as the ones
  // that do not: asking costs a full merge of every bench, and a command
  // that only reads the state file — `list`, `verify`, `cleanup`, `pin`,
  // `unpin`, `env export` — has no reason to pay for one or to warn about
  // benches it never opens. A deny-list had to be extended by hand for each
  // new registry-free command, and was not.
  if matches!(
    cli.command,
    Command::Search { .. }
      | Command::Info { .. }
      | Command::Install { .. }
      | Command::Update { .. }
      | Command::Remove { .. }
      | Command::Env {
        command: EnvCommand::Import { .. }
      }
  ) {
    for problem in session.registry_problems() {
      reporter.warn(format!("registry unavailable: {problem}"));
    }
  }

  match &cli.command {
    // The rest of `env` is handled before `build_session`, because managing
    // environments must work when the selected one does not exist yet.
    // Export and import need a session, so they land here.
    Command::Env { command } => match command {
      EnvCommand::Export { output, loose } => {
        let file = session.export_env(!*loose)?;
        let text = file.to_toml()?;
        match output {
          Some(path) => {
            std::fs::write(path, &text)
              .map_err(|e| Error::io("write environment file", path, e))?;
            reporter.note(format!(
              "Exported {} package(s) to {}",
              file.packages.len(),
              path.display()
            ));
          }
          // Straight to stdout, so it can be piped or redirected.
          None => print!("{text}"),
        }
      }

      EnvCommand::Import { file } => {
        // Reading the file from stdin leaves nothing to ask the confirmation
        // on: stdin is at EOF by the time the question is put, so a pipe is
        // refused as non-interactive and a terminal reads an empty answer as
        // no. Said here, before the file is read, rather than discovered
        // after the work.
        if file.as_os_str() == "-" && !global.yes {
          return Err(Error::InvalidArgument(
            "reading the environment file from stdin leaves nothing to confirm on; \
             pass --yes, or give the file a path"
              .into(),
          ));
        }
        let text = if file.as_os_str() == "-" {
          let mut buffer = String::new();
          std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
            .map_err(|e| Error::io("read environment file", file, e))?;
          buffer
        } else {
          std::fs::read_to_string(file).map_err(|e| Error::io("read environment file", file, e))?
        };
        let env_file =
          luthier_core::envfile::EnvFile::from_toml(&text, &file.display().to_string())?;

        let plan = session.plan_import(&env_file)?;
        if plan.is_blocked() || plan.actionable().next().is_none() {
          reporter.install_plan(&plan);
          if let Some(error) = plan.refusal() {
            return Err(error);
          }
          reporter.note("Nothing to do; the environment already matches.");
          return Ok(());
        }

        reporter.install_preview(&plan);
        if !confirm(global, "\nProceed?")? {
          return Err(Error::Cancelled);
        }

        let show_progress = !global.quiet && !global.json && std::io::stderr().is_terminal();
        let mut bar = BarProgress::new(show_progress);
        let outcome = session.import_env(&env_file, &mut bar).await?;
        reporter.install_outcome(&outcome.installed);
        for pin in &outcome.reapplied_pins {
          reporter.note(format!("Reapplied pin: {pin}"));
        }
        for pin in &outcome.skipped_pins {
          reporter.warn(format!("pin not reapplied: {pin}"));
        }
      }

      _ => unreachable!("dispatched earlier"),
    },

    Command::Refresh => {
      let outcomes = session.refresh().await?;
      reporter.refresh(&outcomes);
    }

    Command::Search { query } => {
      reporter.search(&session.search(query)?);
    }

    Command::Info { package } => {
      let id = session.parse_id(package)?;
      reporter.info(&session.info(&id)?);
    }

    Command::List { unmanaged } => {
      if *unmanaged {
        reporter.scan(&session.scan()?);
      } else {
        reporter.list(&session.list()?);
      }
    }

    Command::Install { packages, force } => {
      let ids = ids(&session, packages)?;
      let plan = session.plan_install(&ids, *force)?;

      // A blocked plan and a plan with nothing to do both end here, so the
      // plan is the answer and is rendered as one.
      if plan.is_blocked() || plan.actionable().next().is_none() {
        reporter.install_plan(&plan);
        // Refused with the plan's own verdict rather than by calling the
        // installer and letting it decide again from state read afresh:
        // asking first would offer a download that cannot happen, and
        // proceeding would install something never confirmed.
        if let Some(error) = plan.refusal() {
          return Err(error);
        }
        return Ok(());
      }

      reporter.install_preview(&plan);
      if !confirm(global, "\nProceed?")? {
        return Err(Error::Cancelled);
      }

      let show_progress = !global.quiet && !global.json && std::io::stderr().is_terminal();
      let mut bar = BarProgress::new(show_progress);
      let outcome = session.install(&ids, *force, &mut bar).await?;
      reporter.install_outcome(&outcome);
    }

    Command::Remove { packages, force } => {
      let ids = ids(&session, packages)?;
      let plan = session.plan_remove(&ids)?;
      reporter.removal_plan(&plan);

      if !confirm(global, "Proceed?")? {
        return Err(Error::Cancelled);
      }
      let outcome = session.remove(&ids, *force)?;
      reporter.remove_outcome(&outcome);
    }

    Command::Update { packages } => {
      if packages.is_empty() {
        // §26: report, never silently update everything.
        reporter.updates(&session.available_updates()?);
        return Ok(());
      }

      let ids = ids(&session, packages)?;
      let plan = session.plan_install(&ids, false)?;
      if plan.is_blocked() || plan.actionable().next().is_none() {
        reporter.install_plan(&plan);
        if let Some(error) = plan.refusal() {
          return Err(error);
        }
        // §27: a pin is a decision to stay put and is reported, not
        // silently obeyed. Without this, `update <pinned>` says
        // "already installed" while `update` with no arguments says a
        // newer release is being held back — the same question, two
        // answers.
        reporter.held_back(&session.held_back(&ids)?);
        return Ok(());
      }
      reporter.install_preview(&plan);
      if !confirm(global, "\nProceed?")? {
        return Err(Error::Cancelled);
      }
      let show_progress = !global.quiet && !global.json && std::io::stderr().is_terminal();
      let mut bar = BarProgress::new(show_progress);
      reporter.install_outcome(&session.install(&ids, false, &mut bar).await?);
    }

    Command::Verify { packages } => {
      let ids = ids(&session, packages)?;
      let results = session.verify(&ids)?;
      reporter.verify(&results);
      let failed = results.iter().filter(|r| !r.ok).count();
      if failed > 0 {
        return Err(Error::VerificationFailed {
          failed,
          total: results.len(),
        });
      }
    }

    Command::Cleanup => {
      reporter.cleanup(&session.cleanup()?);
    }

    Command::Bench { command } => match command {
      BenchCommand::List => {
        if global.registry_path.is_some() {
          reporter.warn("--registry-path replaces the sources below for this command only");
        }
        reporter.benches(&session.benches());
      }
    },

    Command::Cache { command } => match command {
      CacheCommand::List => reporter.cache_list(&session.cache_entries()?),
      CacheCommand::Clean { dry_run } => {
        reporter.cache_cleaned(&session.clean_cache(*dry_run)?);
      }
    },

    Command::Pin { package, version } => {
      let id = session.parse_id(package)?;
      let version = match version {
        Some(raw) => Some(
          raw
            .parse()
            .map_err(|e| Error::InvalidArgument(format!("invalid version {raw:?}: {e}")))?,
        ),
        None => None,
      };
      reporter.pin(&session.pin(&id, version)?);
    }

    Command::Unpin { package } => {
      let id = session.parse_id(package)?;
      reporter.pin(&session.unpin(&id)?);
    }

    // Unreachable: handled before a session is built, since neither needs
    // one. Kept exhaustive so a new command cannot be forgotten here.
    Command::Completions { .. } | Command::Man | Command::Location { .. } => {
      unreachable!("handled before the session")
    }
  }

  Ok(())
}

/// Location management: rendering only. What a valid location is, and when
/// moving one would strand an installed package, is `api::Storage`'s call.
fn run_location(
  global: &GlobalArgs,
  command: Option<&LocationCommand>,
  reporter: &Reporter,
) -> Result<()> {
  let storage = Storage::new(home_layout(global)?);
  let kind = |raw: &str| {
    LocationKind::parse(raw)
      .ok_or_else(|| Error::InvalidArgument(format!("unknown location {raw:?}")))
  };

  let summaries = match command.unwrap_or(&LocationCommand::Show) {
    LocationCommand::Show => storage.show()?,
    LocationCommand::Set { kind: raw, dir } => storage.set(kind(raw)?, dir)?,
    LocationCommand::Reset { kind: raw } => storage.reset(kind(raw)?)?,
    LocationCommand::SearchPath => {
      reporter.activation(&storage.search_path()?);
      return Ok(());
    }
  };
  reporter.locations(&summaries);
  if !storage.search_path()?.set.is_empty() {
    reporter.note(
      "\nHosts do not search the plugin location on their own. Add it to their search path with:\n  \
       eval \"$(luthier location search-path)\"",
    );
  }
  Ok(())
}

/// Environment management: prompts and rendering only. Every decision —
/// what a valid name is, what "not there" means, which environment `path`
/// defaults to — belongs to `api::Environments`.
fn run_env(global: &GlobalArgs, command: &EnvCommand, reporter: &Reporter) -> Result<()> {
  let envs = environments(global)?;

  match command {
    EnvCommand::List => {
      reporter.envs(&envs.list()?);
    }

    EnvCommand::Create { name } => {
      let name = envs.name(name)?;
      let path = envs.create(&name)?;
      reporter.note(format!("Created environment {name} at {}", path.display()));
      reporter.note(format!(
        "Activate it with: eval \"$(luthier env activate {name})\""
      ));
    }

    EnvCommand::Remove { name } => {
      let name = envs.name(name)?;
      // Located first, so a typo is refused before anything is asked.
      let path = envs.locate(&name)?;
      if !confirm(
        global,
        &format!(
          "Delete environment {name} and everything in {}?",
          path.display()
        ),
      )? {
        reporter.note("Cancelled.");
        return Ok(());
      }
      let removed = envs.remove(&name)?;
      reporter.note(format!("Removed {}", removed.display()));
    }

    EnvCommand::Activate { name } => {
      let name = envs.name(name)?;
      reporter.activation(&envs.activation(&name)?);
    }

    EnvCommand::Deactivate => {
      reporter.deactivation(&envs.deactivation());
    }

    EnvCommand::Path { name } => {
      let name = name.as_deref().map(|raw| envs.name(raw)).transpose()?;
      reporter.path(&envs.path_of(name.as_ref())?);
    }

    // Both need a session, so `run` handles them before reaching here.
    EnvCommand::Export { .. } | EnvCommand::Import { .. } => {
      unreachable!("dispatched in run()")
    }

    EnvCommand::Show => match envs.active() {
      Some((name, path)) => reporter.env_show(Some(name.as_str()), Some(&path)),
      None => reporter.env_show(None, None),
    },
  }

  Ok(())
}
