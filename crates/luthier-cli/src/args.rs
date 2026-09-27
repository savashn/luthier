//! Command-line surface.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Package manager for FOSS Linux audio software.
#[derive(Debug, Parser)]
#[command(
    name = "luthier",
    version,
    about = "Package manager for open-source audio plugins and libraries",
    after_help = EXIT_CODES,
    disable_help_subcommand = true
)]
pub struct Cli {
  #[command(flatten)]
  pub global: GlobalArgs,

  #[command(subcommand)]
  pub command: Command,
}

/// Documented alongside `docs/EXIT_CODES.md` so scripts can rely on them (§34).
const EXIT_CODES: &str = "\
Exit codes:
  0  success
  1  generic error
  2  invalid arguments
  3  package not found
  4  verification failure
  5  installation failure
  6  dependency resolution failure";

#[derive(Debug, Args, Clone)]
pub struct GlobalArgs {
  /// Increase log detail. Repeat for more (-v debug, -vv trace).
  #[arg(short, long, global = true, action = clap::ArgAction::Count)]
  pub verbose: u8,

  /// Suppress progress and non-essential output.
  #[arg(short, long, global = true, conflicts_with = "verbose")]
  pub quiet: bool,

  /// Emit machine-readable JSON instead of a table.
  #[arg(long, global = true)]
  pub json: bool,

  /// Assume yes for confirmation prompts.
  #[arg(short = 'y', long, global = true)]
  pub yes: bool,

  /// Never access the network; use cached data only.
  #[arg(long, global = true)]
  pub offline: bool,

  /// Read manifests from this directory instead of the built-in sources.
  ///
  /// For developing the bench against a checkout of `bench/`. Hidden: Luthier
  /// reads the Open Audio Stack registry and its own bench, and nothing else.
  #[arg(long, global = true, value_name = "DIR", hide = true)]
  pub registry_path: Option<PathBuf>,

  /// Confine every path to this directory. For testing and sandboxing.
  #[arg(long, global = true, value_name = "DIR")]
  pub root: Option<PathBuf>,

  /// Ignore plugins installed outside Luthier.
  ///
  /// Dependency resolution normally counts an `external` package as
  /// satisfied when the distribution provides it. This resolves as though
  /// the machine were bare, which is what makes a result reproducible
  /// somewhere else.
  #[arg(long, global = true)]
  pub no_system_plugins: bool,

  /// Act on this environment instead of the default one.
  ///
  /// Overrides LUTHIER_ENV, which is what `luthier env activate` exports.
  #[arg(long, global = true, value_name = "NAME")]
  pub env: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
  /// Create and switch between named sets of installed software.
  Env {
    #[command(subcommand)]
    command: EnvCommand,
  },

  /// Find packages by name, category or description.
  Search {
    /// What to look for. Omit to list everything.
    #[arg(default_value = "")]
    query: String,
  },

  /// Show everything known about a package.
  Info { package: String },

  /// Download, verify and install packages and their dependencies.
  Install {
    #[arg(required = true)]
    packages: Vec<String>,

    /// Reinstall even if already at the selected version.
    #[arg(long)]
    force: bool,
  },

  /// Remove packages, deleting only files Luthier installed.
  Remove {
    #[arg(required = true)]
    packages: Vec<String>,

    /// Remove even if another installed package still needs it.
    #[arg(long)]
    force: bool,
  },

  /// List installed packages.
  List {
    /// Also show plugins present but not installed by Luthier.
    #[arg(long)]
    unmanaged: bool,
  },

  /// Show available updates, or update the named packages.
  Update {
    /// Leave empty to only report what is available.
    packages: Vec<String>,
  },

  /// Fetch the latest registry metadata.
  Refresh {
    /// Accept the bench's snapshot even though no signature was published
    /// beside it.
    ///
    /// For one run, and for that case only: a signature that does not verify,
    /// or one made with a key this build does not carry, is refused whatever
    /// this says. The next refresh asks the same question again.
    #[arg(long)]
    allow_unsigned: bool,
  },

  /// Check installed files still match what was recorded.
  Verify {
    /// Leave empty to verify everything.
    packages: Vec<String>,
  },

  /// List packages nothing needs any more. Removes nothing.
  Cleanup,

  /// Inspect and prune the downloaded-artifact cache.
  Cache {
    #[command(subcommand)]
    command: CacheCommand,
  },

  /// Choose which disk downloads, sample libraries and plugins go to.
  ///
  /// Each is a directory you create yourself, for instance on an external
  /// disk. While that disk is not mounted, anything that would write to it
  /// is refused rather than written to the disk underneath.
  Location {
    #[command(subcommand)]
    command: Option<LocationCommand>,
  },

  /// Show where packages are read from.
  Bench {
    #[command(subcommand)]
    command: BenchCommand,
  },

  /// Hold a package at a version so updates skip it.
  Pin {
    package: String,
    /// Defaults to the installed version.
    version: Option<String>,
  },

  /// Allow a pinned package to be updated again.
  Unpin { package: String },

  /// Print a shell completion script.
  ///
  /// Completes commands, subcommands and flags. Package names are not
  /// completed: that would mean reading the registry index on every keypress.
  ///
  ///     luthier completions zsh > ~/.zfunc/_luthier
  Completions { shell: clap_complete::Shell },

  /// Print the man page, in roff, on standard output.
  ///
  ///     luthier man > /usr/share/man/man1/luthier.1
  #[command(hide = true)]
  Man,
}

#[derive(Debug, Subcommand)]
pub enum BenchCommand {
  /// List the sources packages are read from, in the order they are
  /// consulted: Luthier's own bench, then the Open Audio Stack registry.
  List,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
  /// Show what the cache holds and what still needs it.
  List,

  /// Delete cached artifacts no installed package recorded.
  Clean {
    /// Report what would go without deleting anything.
    #[arg(long)]
    dry_run: bool,
  },
}

#[derive(Debug, Subcommand)]
pub enum LocationCommand {
  /// Show where each part lives. The default.
  Show,

  /// Put one part in a directory of your choosing from now on.
  ///
  /// Nothing already installed is moved. Packages installed in the current
  /// location must be removed first, and installed again afterwards.
  Set {
    #[arg(value_parser = LOCATION_KINDS)]
    kind: String,
    /// An existing directory, given as an absolute path.
    dir: PathBuf,
  },

  /// Put one part back in its default place.
  Reset {
    #[arg(value_parser = LOCATION_KINDS)]
    kind: String,
  },

  /// Print the exports that let hosts find plugins in a chosen location.
  ///
  /// Hosts search ~/.clap, ~/.vst3 and ~/.lv2 by themselves; anywhere else
  /// has to be on CLAP_PATH, VST3_PATH and LV2_PATH:
  ///
  ///     eval "$(luthier location search-path)"
  SearchPath,
}

/// What `location set` and `location reset` accept.
const LOCATION_KINDS: [&str; 3] = ["cache", "libraries", "plugins"];

#[derive(Debug, Subcommand)]
pub enum EnvCommand {
  /// List environments.
  List,

  /// Create an environment.
  Create { name: String },

  /// Delete an environment and everything installed in it.
  Remove { name: String },

  /// Print the shell commands that put an environment on the search path.
  ///
  /// Made effective by evaluating them:
  ///
  ///     eval "$(luthier env activate mixing)"
  Activate { name: String },

  /// Print the shell commands that undo `activate`.
  Deactivate,

  /// Write what is installed to a portable environment file.
  ///
  /// The file pins the exact version of every package, so importing it on
  /// another machine reproduces the same set rather than whatever is newest.
  Export {
    /// Write here instead of standard output.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Record package names without versions.
    ///
    /// Produces a file that installs the current release of each package
    /// on whatever machine reads it. Portable between registry states,
    /// but not reproducible.
    #[arg(long)]
    loose: bool,
  },

  /// Install everything an exported environment file describes.
  Import {
    /// The file to read, or `-` for standard input.
    file: PathBuf,
  },

  /// Print an environment's directory.
  Path {
    /// Defaults to the active environment.
    name: Option<String>,
  },

  /// Show which environment is in use.
  Show,
}
