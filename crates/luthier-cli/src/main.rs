//! The `luthier` command.
//!
//! Deliberately thin (§38): parse arguments, build a [`Session`], call one core
//! operation, render the result. Anything resembling a decision about packages
//! belongs in `luthier-core`, so the GUI planned later behaves identically.

mod args;
mod progress;
mod render;

use args::{CacheCommand, Cli, Command, GlobalArgs, LocationCommand};
use clap::Parser;
use luthier_core::api::{Session, Storage};
use luthier_core::error::{Error, ExitCode, Result};
use luthier_core::layout::LocationKind;
use luthier_core::{Config, Layout};
use luthier_manifest::PackageId;
use progress::{BarProgress, Display};
use render::Reporter;
use std::io::{IsTerminal, Write};

/// Lets a closed pipe end the process, as it does every other Unix tool.
///
/// Rust ignores SIGPIPE, so `luthier search | head` used to panic on the
/// first write after `head` exited — and `completions`, whose generator
/// `expect`s every write, panicked outright. With the default disposition
/// the kernel ends the process quietly and the shell sees 141, which is what
/// a pipeline expects.
fn restore_sigpipe() {
  #[cfg(unix)]
  // SAFETY: called first thing in `main`, before any other thread exists,
  // and SIG_DFL installs no handler that could run Rust code.
  unsafe {
    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
  }
}

fn main() -> std::process::ExitCode {
  restore_sigpipe();
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

  let code = match runtime.block_on(run(&cli, &reporter)) {
    Ok(()) => ExitCode::Success,
    Err(error) => {
      report_error(&error);
      error.exit_code()
    }
  };
  // Everything the command does is awaited by now. What may still be running
  // is a name lookup the newer-Luthier check gave up on, which a resolver
  // that cannot reach its server holds for many seconds; dropping the
  // runtime would wait for it.
  runtime.shutdown_background();
  code.into()
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

/// The layout before any location is applied.
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

/// The layout with the locations the configuration names applied.
fn build_layout(global: &GlobalArgs) -> Result<Layout> {
  Config::located(home_layout(global)?)
}

fn build_session(global: &GlobalArgs) -> Result<Session> {
  let layout = build_layout(global)?;
  let config = match &global.registry_path {
    Some(path) => luthier_core::config::from_path(path),
    None => Config::load(&layout)?,
  };
  let mut session = Session::new(layout, config)?.offline(global.offline);
  if let Some(url) = &global.releases_api {
    session = session.releases_api(url)?;
  }
  Ok(session)
}

/// Download progress, as stderr can show it.
fn progress(global: &GlobalArgs) -> BarProgress {
  BarProgress::new(Display::for_stderr(
    global.quiet,
    global.json,
    std::io::stderr().is_terminal(),
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
  // Under `--json` the plan is not shown, since the outcome is the one
  // document; asking at a terminal would be asking about a list nobody saw.
  if global.json {
    return Err(Error::InvalidArgument(
      "--json shows no plan to confirm; re-run with --yes to proceed".into(),
    ));
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

/// Says so when a newer Luthier is out, after `update` and `outdated`. Never
/// fails or holds up the command; see [`Session::newer_luthier`].
async fn tell_of_a_newer_luthier(session: &Session, global: &GlobalArgs, reporter: &Reporter) {
  // A line `--json` and `--quiet` would leave out is not worth asking for.
  if global.json || global.quiet {
    return;
  }
  if let Some(newer) = session.newer_luthier().await {
    reporter.newer_luthier(&newer);
  }
}

/// `upgrade --self`: shows what upgrading Luthier itself would do, asks, does it.
async fn upgrade_luthier(
  session: &Session,
  global: &GlobalArgs,
  reporter: &Reporter,
) -> Result<()> {
  let release = session.latest_release().await?;
  let plan = session.plan_self_update(&release)?;
  if plan.is_current() {
    reporter.luthier_is_current(&plan);
    return Ok(());
  }
  reporter.self_update_preview(&plan);
  if !confirm(global, "\nProceed?")? {
    return Err(Error::Cancelled);
  }
  let mut bar = progress(global);
  session.self_update(&plan, &mut bar).await?;
  reporter.self_update_done(&plan);
  Ok(())
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

  // A session is one command, and whether it stops at the first package
  // that fails is part of what that command was asked.
  let keep_going = matches!(
    &cli.command,
    Command::Install {
      keep_going: true,
      ..
    } | Command::Upgrade {
      keep_going: true,
      ..
    } | Command::Import {
      keep_going: true,
      ..
    }
  );
  let session = build_session(global)?.keep_going(keep_going);

  // A registry that could not be read is reported before the command
  // answers, so a half-configured setup never looks like a complete one.
  //
  // Named as the commands that consult a registry rather than as the ones
  // that do not: asking costs a full merge of every source, and a command
  // that only reads the state file — `list`, `verify`, `pin`, `unpin`,
  // `export` — has no reason to pay for one or to warn about
  // sources it never opens. A deny-list had to be extended by hand for each
  // new registry-free command, and was not.
  //
  // The same commands bring a stale package list up to date first, so
  // nobody has to remember `update` to see what is new. `update` itself is
  // not among them: it fetches the lists whether they are due or not.
  if matches!(
    cli.command,
    Command::Search { .. }
      | Command::Info { .. }
      | Command::Install { .. }
      | Command::Outdated
      | Command::Upgrade { luthier: false, .. }
      | Command::Remove { .. }
      | Command::Autoremove { .. }
      | Command::Import { .. }
  ) {
    for outcome in session.refresh_due().await {
      reporter.automatic_refresh(&outcome);
    }
    for problem in session.registry_problems() {
      reporter.warn(format!("registry unavailable: {problem}"));
    }
  }

  match &cli.command {
    Command::Export { output, loose } => {
      let file = session.export(!*loose)?;
      let text = file.to_toml()?;
      match output {
        Some(path) => {
          std::fs::write(path, &text).map_err(|e| Error::io("write the exported file", path, e))?;
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

    Command::Import { file, prune, .. } => {
      // Reading the file from stdin leaves nothing to ask the confirmation
      // on: stdin is at EOF by the time the question is put, so a pipe is
      // refused as non-interactive and a terminal reads an empty answer as
      // no. Said here, before the file is read, rather than discovered
      // after the work.
      if file.as_os_str() == "-" && !global.yes {
        return Err(Error::InvalidArgument(
          "reading the file from stdin leaves nothing to confirm on; \
             pass --yes, or give the file a path"
            .into(),
        ));
      }
      let text = if file.as_os_str() == "-" {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
          .map_err(|e| Error::io("read the file to import", file, e))?;
        buffer
      } else {
        std::fs::read_to_string(file).map_err(|e| Error::io("read the file to import", file, e))?
      };
      let env_file = luthier_core::envfile::EnvFile::from_toml(&text, &file.display().to_string())?;

      let plan = session.plan_import(&env_file, *prune)?;
      // The plan says what the prune removes, and why it removes nothing
      // when a source could not be read.
      reporter.prune_withheld(plan.prune_withheld.as_deref());
      let prunable = plan.prune.clone();
      if plan.is_blocked() || (plan.actionable().next().is_none() && prunable.is_empty()) {
        reporter.install_plan(&plan);
        if let Some(error) = plan.refusal() {
          return Err(error);
        }
        // Nothing to install; what the plan marks is still recorded, and
        // the plan already said so.
        session.record_marks(&plan)?;
        reporter.import_unchanged(&plan);
        return Ok(());
      }

      if plan.actionable().next().is_some() {
        reporter.install_preview(&plan);
      }
      if !prunable.is_empty() {
        reporter.note("\nNot in the file, so removed:");
        reporter.removal_preview(&session.plan_remove(&prunable)?);
      }
      if !confirm(global, "\nProceed?")? {
        return Err(Error::Cancelled);
      }

      let mut bar = progress(global);
      let outcome = session.import(&env_file, *prune, &plan, &mut bar).await?;
      reporter.import_outcome(&outcome);
      if let Some(error) = outcome.installed.incomplete() {
        return Err(error);
      }
    }

    Command::Update => {
      let outcomes = session.refresh().await?;
      reporter.refresh(&outcomes);
      // What the lists just fetched mean for this installation, as apt
      // says after its own `update`. The fetch has succeeded by now, so
      // what goes wrong here is a warning rather than the command failing.
      match session.outdated() {
        Ok(outdated) => reporter.upgradable(&outdated),
        Err(error) => reporter.warn(format!("could not tell what can be upgraded: {error}")),
      }
      tell_of_a_newer_luthier(&session, global, reporter).await;
    }

    Command::Outdated => {
      reporter.updates(&session.outdated()?);
      tell_of_a_newer_luthier(&session, global, reporter).await;
    }

    Command::Search { query } => {
      reporter.search(&session.search(query)?);
    }

    Command::Info { package } => {
      let id = session.parse_id(package)?;
      reporter.info(&session.info(&id)?);
    }

    Command::List {
      packages,
      unmanaged,
    } => {
      if *unmanaged {
        reporter.scan(&session.scan()?);
      } else if packages.is_empty() {
        reporter.list(&session.list()?);
      } else {
        let ids = ids(&session, packages)?;
        reporter.files(&session.installed(&ids)?);
      }
    }

    Command::Install {
      packages, force, ..
    } => {
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
        // Asked for by name, so no longer a dependency `autoremove` may
        // take; the plan already said so.
        session.record_marks(&plan)?;
        return Ok(());
      }

      reporter.install_preview(&plan);
      if !confirm(global, "\nProceed?")? {
        return Err(Error::Cancelled);
      }

      let mut bar = progress(global);
      let outcome = session.install(&ids, *force, &mut bar).await?;
      reporter.install_outcome(&outcome);
      if let Some(error) = outcome.incomplete() {
        return Err(error);
      }
    }

    Command::Remove { packages, force } => {
      let ids = ids(&session, packages)?;
      let plan = session.plan_remove(&ids)?;
      // A preview, silent under `--json`: the outcome below is the one
      // document, as it is for `install`.
      reporter.removal_preview(&plan);

      if !confirm(global, "Proceed?")? {
        return Err(Error::Cancelled);
      }
      let outcome = session.remove(&ids, *force)?;
      reporter.remove_outcome(&outcome);
    }

    Command::Upgrade {
      packages, luthier, ..
    } => {
      if *luthier {
        return upgrade_luthier(&session, global, reporter).await;
      }
      // The named packages, or with none named every explicitly installed
      // one; which those are is the session's call. Upgrading everything
      // asks first, as every upgrade does, so it is never silent (§26).
      let named = ids(&session, packages)?;
      // §27: a pin is a decision to stay put and is reported, not silently
      // obeyed. The plan carries what it leaves (`held_back`), or `upgrade
      // <pinned>` would say "up to date" while `outdated` says a newer
      // release is being held back — the same question, two answers.
      let plan = session.plan_upgrade(&named)?;
      if plan.is_blocked() || plan.actionable().next().is_none() {
        reporter.upgrade_plan(&plan);
        if let Some(error) = plan.refusal() {
          return Err(error);
        }
        reporter.held_back(&plan.held_back);
        return Ok(());
      }
      reporter.install_preview(&plan);
      reporter.held_back(&plan.held_back);
      if !confirm(global, "\nProceed?")? {
        return Err(Error::Cancelled);
      }
      let mut bar = progress(global);
      let outcome = session.upgrade(&plan, &mut bar).await?;
      reporter.install_outcome(&outcome);
      if let Some(error) = outcome.incomplete() {
        return Err(error);
      }
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

    Command::Autoremove { dry_run } => {
      // The list the user is shown is the list removed: `autoremove`
      // refuses if one of them changed in between, rather than this
      // recomputing it after the confirmation and removing something
      // nobody was shown.
      let plan = session.plan_autoremove()?;
      reporter.kept_engines(&plan.kept);
      if *dry_run || plan.ids.is_empty() {
        reporter.autoremove_plan(&plan);
        return Ok(());
      }
      reporter.autoremove_preview(&plan);
      if !confirm(global, "Proceed?")? {
        return Err(Error::Cancelled);
      }
      reporter.remove_outcome(&session.autoremove(&plan)?);
    }

    Command::Sources => {
      if global.registry_path.is_some() {
        reporter.warn("--registry-path replaces the sources below for this command only");
      }
      reporter.sources(&session.sources());
    }

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
      reporter.search_path(&storage.search_path()?);
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
