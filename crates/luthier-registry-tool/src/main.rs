//! `luthier-registry` — the registry's own tooling.
//!
//! Two jobs. First, validation: this is what the registry repository's CI runs
//! over every pull request (§44), in strict mode, so a typo or an
//! uninstallable package is caught before it can reach a user. Second,
//! authoring: computing a real checksum and reading a real archive's layout, so
//! manifests are written from what upstream actually publishes rather than
//! from guesswork (§26).

#[cfg(feature = "authoring")]
mod upstream;

use clap::{Parser, Subcommand};
#[cfg(feature = "authoring")]
use luthier_core::{archive, install};
#[cfg(feature = "authoring")]
use luthier_manifest::ArchiveFormat;
use luthier_manifest::{Content, EnginesFile, ParseMode, Severity, validate, validate_engines};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "luthier-registry", version, about = "Luthier registry tooling")]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
  /// Check every manifest in a registry directory.
  Validate {
    /// The registry root. Defaults to the working directory.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Also send a HEAD request to every artifact URL.
    #[cfg(feature = "authoring")]
    #[arg(long)]
    check_urls: bool,

    /// Treat warnings as errors.
    #[arg(long)]
    strict: bool,
  },

  /// Download a URL and print its SHA-256 and size, for pasting into a manifest.
  #[cfg(feature = "authoring")]
  HashUrl { url: String },

  /// List an archive's contents and suggest install rules.
  #[cfg(feature = "authoring")]
  Inspect {
    archive: PathBuf,

    /// Container format. Inferred from the filename when omitted.
    #[arg(long)]
    format: Option<String>,

    /// The package ID, for an archive of sample content.
    ///
    /// Content installs under the ID rather than under whatever the archive
    /// wrapped it in, so the suggestion needs one to be exact.
    #[arg(long)]
    id: Option<String>,
  },

  /// Generate an Ed25519 key pair for signing this bench's snapshots.
  ///
  /// The secret key is written to a file only its owner can read; the public
  /// key is printed, to publish where users can find it and to hand to
  /// `luthier bench trust`.
  #[cfg(feature = "authoring")]
  Keygen {
    /// Where to write the secret key. Never overwritten.
    #[arg(long, default_value = "luthier-bench.key")]
    out: PathBuf,
  },

  /// Sign a snapshot, writing the detached signature beside it.
  ///
  /// What is signed is the snapshot's SHA-256, which is what the manager
  /// records and what it checks the signature against.
  #[cfg(feature = "authoring")]
  Sign {
    /// The snapshot tarball to sign.
    snapshot: PathBuf,

    /// The secret key file written by `keygen`.
    #[arg(long)]
    key: PathBuf,

    /// Write the signature here instead of <snapshot>.sig.
    #[arg(long)]
    out: Option<PathBuf>,
  },

  /// Report which manifests are behind their upstream project.
  ///
  /// Asks each package's forge for its newest tag and compares. Reports
  /// only; a new version needs a checksum derived from the real file, so
  /// `hash-url` stays a human step (§26).
  #[cfg(feature = "authoring")]
  CheckUpdates {
    /// The registry root. Defaults to the working directory.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// A GitHub token, raising the rate limit from 60/hour to 5000.
    /// Falls back to the GITHUB_TOKEN environment variable.
    #[arg(long)]
    token: Option<String>,

    /// Show every package, not only the ones that are behind.
    #[arg(long)]
    all: bool,

    /// Emit JSON, for CI to turn into an issue.
    #[arg(long)]
    json: bool,

    /// Exit non-zero when something is behind. Off by default: being
    /// behind is news, not a broken build.
    #[arg(long)]
    exit_code: bool,

    /// Where to send API requests. For testing.
    #[arg(long, hide = true, default_value = upstream::GitHub::PUBLIC_API)]
    api: String,
  },

  /// Print the JSON Schema for a v1 manifest.
  Schema,
}

fn main() -> ExitCode {
  let cli = Cli::parse();

  let result = match cli.command {
    Command::Validate {
      path,
      #[cfg(feature = "authoring")]
      check_urls,
      strict,
    } => {
      #[cfg(feature = "authoring")]
      {
        run_validate(&path, check_urls, strict)
      }
      #[cfg(not(feature = "authoring"))]
      {
        run_validate(&path, strict)
      }
    }

    #[cfg(feature = "authoring")]
    Command::HashUrl { url } => runtime().block_on(cmd_hash_url(&url)),

    #[cfg(feature = "authoring")]
    Command::Inspect {
      archive,
      format,
      id,
    } => cmd_inspect(&archive, format.as_deref(), id.as_deref()),

    #[cfg(feature = "authoring")]
    Command::Keygen { out } => cmd_keygen(&out),

    #[cfg(feature = "authoring")]
    Command::Sign { snapshot, key, out } => cmd_sign(&snapshot, &key, out.as_deref()),

    #[cfg(feature = "authoring")]
    Command::CheckUpdates {
      path,
      token,
      all,
      json,
      exit_code,
      api,
    } => runtime().block_on(cmd_check_updates(&path, token, all, json, exit_code, &api)),

    Command::Schema => {
      print!("{}", luthier_manifest::json_schema_text());
      Ok(true)
    }
  };

  match result {
    Ok(true) => ExitCode::SUCCESS,
    Ok(false) => ExitCode::FAILURE,
    Err(e) => {
      eprintln!("error: {e}");
      ExitCode::FAILURE
    }
  }
}

/// Validating the documents, then optionally checking that every artifact URL
/// still resolves. The two are separate because only the second needs a
/// network, and the first is what runs on every pull request.
#[cfg(feature = "authoring")]
fn run_validate(
  root: &Path,
  check_urls: bool,
  warnings_are_errors: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
  let validated = cmd_validate(root, warnings_are_errors)?;
  if check_urls {
    let reachable = runtime().block_on(check_artifact_urls(&validated.urls))?;
    return Ok(validated.ok && reachable);
  }
  Ok(validated.ok)
}

/// Without the authoring features there is no URL sweep to run.
#[cfg(not(feature = "authoring"))]
fn run_validate(
  root: &Path,
  warnings_are_errors: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
  Ok(cmd_validate(root, warnings_are_errors)?.ok)
}

/// Built only where something actually awaits. Validation does not, which is
/// the whole point of the `authoring` split.
#[cfg(feature = "authoring")]
fn runtime() -> tokio::runtime::Runtime {
  tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .expect("runtime builds")
}

/// Returns Ok(false) when validation found problems.
///
/// Synchronous on purpose: this is what the registry's CI runs on every pull
/// request, and it must build without an async runtime or an HTTP stack. The
/// URL sweep that does need those is a separate, feature-gated step.
fn cmd_validate(
  root: &Path,
  warnings_are_errors: bool,
) -> Result<Validated, Box<dyn std::error::Error>> {
  if !root.is_dir() {
    return Err(format!("{} is not a directory", root.display()).into());
  }

  let files = luthier_manifest::manifest_files(root)?;
  if files.is_empty() {
    println!("No manifests found under {}.", root.display());
    return Ok(Validated {
      ok: true,
      urls: Vec::new(),
    });
  }

  let mut errors = 0usize;
  let mut warnings = 0usize;
  let mut seen_ids: BTreeMap<String, PathBuf> = BTreeMap::new();
  let mut urls: Vec<(String, String)> = Vec::new();
  let mut declared_content: Vec<(String, Content)> = Vec::new();

  for path in &files {
    let display = path
      .strip_prefix(root)
      .unwrap_or(path)
      .display()
      .to_string();
    let text = std::fs::read_to_string(path)?;

    // Strict: an unrecognised field in a pull request is a typo until
    // proven otherwise, and silently ignoring it would hide the mistake.
    let parsed = match luthier_manifest::from_toml(&text, &display, ParseMode::Strict) {
      Ok(parsed) => parsed,
      Err(e) => {
        println!("error: {display}: {e}");
        if let Some(hint) = e.hint() {
          println!("       {hint}");
        }
        errors += 1;
        continue;
      }
    };
    let manifest = parsed.manifest;

    let stem = path
      .file_stem()
      .unwrap_or_default()
      .to_string_lossy()
      .into_owned();
    if manifest.id.as_str() != stem {
      println!(
        "error: {display}: declares id {} but must be filed as {}.toml",
        manifest.id, manifest.id
      );
      errors += 1;
    }

    if let Some(first) = seen_ids.get(manifest.id.as_str()) {
      println!(
        "error: {display}: package id {} is already defined in {}",
        manifest.id,
        first.display()
      );
      errors += 1;
    } else {
      seen_ids.insert(manifest.id.to_string(), path.clone());
    }

    let report = validate(&manifest);
    for diagnostic in &report.diagnostics {
      let label = match diagnostic.severity {
        Severity::Error => {
          errors += 1;
          "error"
        }
        Severity::Warning => {
          warnings += 1;
          "warning"
        }
      };
      println!(
        "{label}: {display}: {}: {}",
        diagnostic.path, diagnostic.message
      );
      if let Some(hint) = &diagnostic.hint {
        println!("       {hint}");
      }
    }

    for release in &manifest.releases {
      for artifact in &release.artifacts {
        urls.push((display.clone(), artifact.source.url.to_string()));
      }
    }
    for content in &manifest.content {
      declared_content.push((display.clone(), content.clone()));
    }
  }

  // Content no engine plays could never be installed: the manager refuses
  // it on every machine. That is a registry mistake, so it is caught here.
  let played = check_engines(root, &mut errors, &mut warnings)?;
  for (display, content) in &declared_content {
    if !played.contains(content) {
      println!(
        "error: {display}: content: {content} has no engine in {}",
        luthier_manifest::ENGINES_FILE
      );
      println!("       Add an [[engine]] that plays it, or the package can never be installed.");
      errors += 1;
    }
  }

  println!(
    "\nChecked {} manifest(s): {errors} error(s), {warnings} warning(s).",
    files.len()
  );

  Ok(Validated {
    ok: errors == 0 && !(warnings_are_errors && warnings > 0),
    urls,
  })
}

/// Validates the registry's `engines.toml`, if it has one, and returns every
/// content value some engine in it plays.
fn check_engines(
  root: &Path,
  errors: &mut usize,
  warnings: &mut usize,
) -> Result<BTreeSet<Content>, Box<dyn std::error::Error>> {
  let name = luthier_manifest::ENGINES_FILE;
  // Every build carries engines for the formats it knows, so a bench needs
  // an `engines.toml` only to add to them.
  let mut played = luthier_manifest::builtin_content();
  let path = root.join(name);
  if !path.is_file() {
    return Ok(played);
  }
  let file = match EnginesFile::parse(&std::fs::read_to_string(&path)?) {
    Ok(file) => file,
    Err(e) => {
      println!("error: {name}: {e}");
      *errors += 1;
      return Ok(played);
    }
  };

  // Strict, as for manifests: an unknown field here is a typo.
  for field in file.unknown_fields() {
    println!("error: {name}: {field}: unknown field");
    *errors += 1;
  }
  for diagnostic in &validate_engines(&file).diagnostics {
    let label = match diagnostic.severity {
      Severity::Error => {
        *errors += 1;
        "error"
      }
      Severity::Warning => {
        *warnings += 1;
        "warning"
      }
    };
    println!(
      "{label}: {name}: {}: {}",
      diagnostic.path, diagnostic.message
    );
    if let Some(hint) = &diagnostic.hint {
      println!("       {hint}");
    }
  }

  played.extend(
    file
      .entries
      .iter()
      .flat_map(|entry| entry.plays.iter().cloned()),
  );
  Ok(played)
}

/// What validation found, plus the artifact URLs a reachability sweep would
/// check. Collected even when nothing will use them, so both passes read the
/// same set of manifests exactly once.
struct Validated {
  ok: bool,
  /// `(manifest path, url)`, so an unreachable URL names its file.
  ///
  /// Collected unconditionally so that the validation pass is identical in
  /// both builds; without the authoring features there is simply no sweep to
  /// hand them to.
  #[cfg_attr(not(feature = "authoring"), allow(dead_code))]
  urls: Vec<(String, String)>,
}

/// HEAD every artifact URL. Reachability depends on third-party hosts, so this
/// is deliberately a separate step from validating the documents themselves.
#[cfg(feature = "authoring")]
async fn check_artifact_urls(
  urls: &[(String, String)],
) -> Result<bool, Box<dyn std::error::Error>> {
  println!("\nChecking {} artifact URL(s)...", urls.len());
  let client = reqwest::Client::builder()
    .user_agent(concat!("luthier-registry/", env!("CARGO_PKG_VERSION")))
    .build()?;
  let mut errors = 0usize;
  for (manifest, url) in urls {
    match client.head(url).send().await {
      Ok(response) if response.status().is_success() => {}
      Ok(response) => {
        println!(
          "error: {manifest}: {url} returned HTTP {}",
          response.status()
        );
        errors += 1;
      }
      Err(e) => {
        println!("error: {manifest}: {url} is unreachable: {e}");
        errors += 1;
      }
    }
  }
  Ok(errors == 0)
}

#[cfg(feature = "authoring")]
async fn cmd_hash_url(url: &str) -> Result<bool, Box<dyn std::error::Error>> {
  use luthier_core::download::{Downloader, NoProgress};
  use luthier_manifest::Sha256Hash;

  let parsed = url::Url::parse(url)?;
  let scratch = tempdir()?;
  let downloader = Downloader::new(scratch.path());

  // The digest is what we are trying to find out, so the first attempt is
  // expected to "fail" verification and report the real value.
  let placeholder = Sha256Hash::from_bytes([0; 32]);
  let (digest, bytes) = match downloader
    .fetch(&parsed, &placeholder, None, &mut NoProgress)
    .await
  {
    Ok(fetched) => (fetched.sha256, fetched.bytes),
    Err(luthier_core::error::Error::Download(
      luthier_core::error::DownloadError::ChecksumMismatch { actual, .. },
    )) => {
      let fetched = downloader
        .fetch(&parsed, &actual, None, &mut NoProgress)
        .await?;
      (fetched.sha256, fetched.bytes)
    }
    Err(e) => return Err(Box::new(e)),
  };

  let inferred = parsed
    .path_segments()
    .and_then(|mut s| s.rfind(|p| !p.is_empty()))
    .and_then(ArchiveFormat::from_filename);

  println!("        source: {{ type: http, url: \"{url}\" }}");
  if let Some(format) = inferred {
    println!("        archive: {format}");
  }
  println!("        size: {bytes}");
  println!("        checksum: {{ sha256: \"{digest}\" }}");
  Ok(true)
}

#[cfg(feature = "authoring")]
fn cmd_inspect(
  archive_path: &Path,
  format: Option<&str>,
  id: Option<&str>,
) -> Result<bool, Box<dyn std::error::Error>> {
  let declared: ArchiveFormat = match format {
    Some(raw) => raw.parse().expect("parsing is infallible"),
    None => {
      archive::sniff(archive_path)?.ok_or("cannot determine the archive format; pass --format")?
    }
  };

  let destination = tempdir()?;
  let report = archive::extract(
    archive_path,
    &declared,
    destination.path(),
    archive::ExtractLimits::default(),
  )?;

  println!("Format: {declared}");
  println!("Entries: {}, {} bytes\n", report.entries, report.bytes);

  let derived = install::derive::from_tree(destination.path())?;

  for entry in derived.listing.iter().take(200) {
    println!("{}", render_entry(entry));
  }
  if derived.listing.len() > 200 {
    println!("... and {} more", derived.listing.len() - 200);
  }

  if derived.rules.is_empty() {
    println!("\nNo CLAP, VST3 or LV2 plugins found in this archive.");
  } else {
    println!("\nSuggested install rules:\n");
    println!("        install:");
    for rule in &derived.rules {
      println!("{}", render_rule(rule));
    }
  }

  // The same answer the manager reaches for a package whose manifest
  // declares `library`, printed here so a contributor writing rules by hand
  // can see what deriving them would have produced.
  if let Some(content) = &derived.content {
    println!("\n{}", render_content(content, id));
  }

  Ok(true)
}

#[cfg(feature = "authoring")]
/// What a package of content would install, and where.
fn render_content(content: &install::ContentSource, id: Option<&str>) -> String {
  let name = id.unwrap_or("<package id>");
  let what = match content {
    install::ContentSource::Directory(path) => format!("{} (the archive's one directory)", path),
    install::ContentSource::Root => "everything in the archive".to_owned(),
  };
  format!(
    "As sample content, a manifest declaring `provides = [\"library\"]` and no install\n\
     rules would install {what} as:\n\
     \n    <library root>/{name}\n\
     \nThe name is the package ID rather than anything in the archive, so the path\n\
     stays put across releases."
  )
}

#[cfg(feature = "authoring")]
/// One install rule, in the shape it is pasted into a manifest.
///
/// A rule names a format, a path *inside the archive* and what kind of entry
/// that path is. It never names a destination: that is derived from the
/// format's root, and a manifest that could name one would be an
/// arbitrary-write primitive for anyone with a merged pull request (§14).
fn render_rule(rule: &luthier_manifest::InstallRule) -> String {
  format!(
    "          - {{ format: {}, source: \"{}\", kind: {} }}",
    rule.format,
    rule.source.as_str(),
    rule.kind
  )
}

#[cfg(feature = "authoring")]
/// One listing line, in the shape `inspect` has always printed.
fn render_entry(entry: &install::Listed) -> String {
  let slash = if entry.is_dir { "/" } else { "" };
  // LV2 bundles were annotated when they produced no rule; the note stays
  // because a bundle and a directory that merely ends in `.lv2` look alike.
  let note = if entry.format == Some(luthier_manifest::Format::Lv2) {
    "  (LV2 bundle)"
  } else {
    ""
  };
  format!("  {}{slash}{note}", entry.path)
}

// ------------------------------------------------------------------ signing --

/// Writes a new key pair.
///
/// Refuses to overwrite: a key file is the one thing here that cannot be
/// regenerated, and replacing one silently would retire a bench's identity
/// without anybody deciding to.
#[cfg(feature = "authoring")]
fn cmd_keygen(out: &Path) -> Result<bool, Box<dyn std::error::Error>> {
  use luthier_core::registry::signature::SecretKey;

  if out.exists() {
    return Err(format!("{} already exists; move it aside first", out.display()).into());
  }

  let secret = SecretKey::generate()?;
  std::fs::write(out, format!("{}\n", secret.to_hex()))?;
  restrict(out)?;

  println!("Secret key written to {}", out.display());
  println!("Public key:  {}", secret.public());
  println!(
    "\nPublish the public key where users can check it against a signature, and keep \n\
     the secret key off the machine that builds snapshots if you can. Users pin it with\n\
     \n    luthier bench trust <bench> {}\n",
    secret.public()
  );
  Ok(true)
}

/// Signs a snapshot, writing `<snapshot>.sig` beside it.
#[cfg(feature = "authoring")]
fn cmd_sign(
  snapshot: &Path,
  key_file: &Path,
  out: Option<&Path>,
) -> Result<bool, Box<dyn std::error::Error>> {
  use luthier_core::registry::signature::SecretKey;

  let secret = SecretKey::parse(&std::fs::read_to_string(key_file)?)?;
  // The digest the manager computes as it downloads, and records: signing
  // the same thing is what ties the audit trail to a key.
  let digest = luthier_core::fsutil::hash_file(snapshot)?;

  let destination = match out {
    Some(path) => path.to_path_buf(),
    // `.sig` appended to the whole name, not to the stem: the manager
    // looks for it beside the snapshot under exactly this name.
    None => PathBuf::from(format!("{}.sig", snapshot.display())),
  };
  std::fs::write(&destination, secret.sign(&digest).render())?;

  println!("sha256 {digest}");
  println!("key    {}", secret.public());
  println!("Signature written to {}", destination.display());
  println!("\nPublish it beside the snapshot, at the snapshot's own URL with `.sig` on the end.");
  Ok(true)
}

/// Owner-read-only, for a file that is the bench's identity.
#[cfg(all(feature = "authoring", unix))]
fn restrict(path: &Path) -> std::io::Result<()> {
  use std::os::unix::fs::PermissionsExt;
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(all(feature = "authoring", not(unix)))]
fn restrict(_path: &Path) -> std::io::Result<()> {
  Ok(())
}

#[cfg(feature = "authoring")]
fn tempdir() -> std::io::Result<TempDir> {
  TempDir::new()
}

#[cfg(feature = "authoring")]
/// A scratch directory removed on drop.
struct TempDir(PathBuf);

#[cfg(feature = "authoring")]
impl TempDir {
  fn new() -> std::io::Result<Self> {
    let nanos = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.as_nanos())
      .unwrap_or(0);
    let path =
      std::env::temp_dir().join(format!("luthier-registry-{}-{nanos:x}", std::process::id()));
    std::fs::create_dir_all(&path)?;
    Ok(Self(path))
  }

  fn path(&self) -> &Path {
    &self.0
  }
}

#[cfg(feature = "authoring")]
impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

// ------------------------------------------------------------ check-updates --

#[cfg(feature = "authoring")]
/// Reports which manifests are behind upstream.
///
/// Returns `Ok(false)` only when `--exit-code` was asked for and something is
/// behind. Being behind is news rather than a broken build, so a scheduled CI
/// run defaults to succeeding and turning the report into an issue.
async fn cmd_check_updates(
  root: &Path,
  token: Option<String>,
  show_all: bool,
  as_json: bool,
  exit_code: bool,
  api: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
  use upstream::{Checked, GitHub, Status};

  if !root.is_dir() {
    return Err(format!("{} is not a directory", root.display()).into());
  }

  let files = luthier_manifest::manifest_files(root)?;
  if files.is_empty() {
    println!("No manifests found under {}.", root.display());
    return Ok(true);
  }

  let token = token.or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()));
  let github = GitHub::new(url::Url::parse(api)?, token);
  let mut results: Vec<Checked> = Vec::new();

  for path in &files {
    let display = path
      .strip_prefix(root)
      .unwrap_or(path)
      .display()
      .to_string();
    let text = std::fs::read_to_string(path)?;
    // Lenient: this command's job is version tracking, not validation.
    // A manifest with a field this build does not know still has a version.
    let manifest = match luthier_manifest::from_toml(&text, &display, ParseMode::Lenient) {
      Ok(parsed) => parsed.manifest,
      Err(e) => {
        eprintln!("warning: skipping {display}: {e}");
        continue;
      }
    };
    // Nothing to ask a forge about: an `external` package declares no
    // releases, and a `pack`'s version is the registry's own rather than
    // any upstream project's. Both would otherwise be reported as having
    // no upstream, which is noise rather than news.
    if manifest.releases.is_empty() || manifest.kind == luthier_manifest::PackageKind::Pack {
      continue;
    }
    results.push(upstream::check(&manifest, &github).await);
  }

  results.sort_by(|a, b| a.id.cmp(&b.id));
  let behind = results.iter().filter(|r| r.status.is_actionable()).count();

  if as_json {
    println!("{}", check_updates_json(&results));
    return Ok(!(exit_code && behind > 0));
  }

  let shown: Vec<&Checked> = results
    .iter()
    .filter(|r| show_all || r.status.is_actionable())
    .collect();

  if shown.is_empty() {
    println!("Every tracked package is current.");
  } else {
    let rows: Vec<[String; 4]> = shown
      .iter()
      .map(|r| {
        [
          r.id.clone(),
          r.current
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".into()),
          describe(&r.status),
          r.upstream
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".into()),
        ]
      })
      .collect();
    print_table(&["ID", "CURRENT", "UPSTREAM", "SOURCE"], &rows);
  }

  let failed = results
    .iter()
    .filter(|r| matches!(r.status, Status::Failed { .. }))
    .count();
  let unsupported = results
    .iter()
    .filter(|r| matches!(r.status, Status::Unsupported { .. }))
    .count();

  println!(
    "\nChecked {} package(s): {behind} behind, {unsupported} on unsupported hosts, \
         {failed} failed.",
    results.len()
  );
  if behind > 0 {
    println!("Derive the new checksum with `luthier-registry hash-url <url>` before editing.");
  }

  Ok(!(exit_code && behind > 0))
}

#[cfg(feature = "authoring")]
fn describe(status: &upstream::Status) -> String {
  use upstream::Status;
  match status {
    Status::Current => "current".into(),
    Status::Behind { latest, tag } => format!("{latest}  (tag {tag})"),
    Status::Ahead { latest, .. } => format!("ahead of {latest}"),
    Status::UnreadableTag { tag } => format!("unreadable tag {tag:?}"),
    Status::NoUpstreamVersions => "no upstream versions".into(),
    Status::Unsupported { host } => format!("no API for {host}"),
    Status::Failed { reason } => format!("failed: {reason}"),
  }
}

#[cfg(feature = "authoring")]
fn check_updates_json(results: &[upstream::Checked]) -> String {
  use upstream::Status;
  let items: Vec<serde_json::Value> = results
    .iter()
    .map(|r| {
      let (state, latest, tag, detail) = match &r.status {
        Status::Current => ("current", None, None, None),
        Status::Behind { latest, tag } => {
          ("behind", Some(latest.to_string()), Some(tag.clone()), None)
        }
        Status::Ahead { latest, tag } => {
          ("ahead", Some(latest.to_string()), Some(tag.clone()), None)
        }
        Status::UnreadableTag { tag } => ("unreadable-tag", None, Some(tag.clone()), None),
        Status::NoUpstreamVersions => ("no-upstream-versions", None, None, None),
        Status::Unsupported { host } => ("unsupported", None, None, Some(host.clone())),
        Status::Failed { reason } => ("failed", None, None, Some(reason.clone())),
      };
      serde_json::json!({
          "id": r.id,
          "state": state,
          "current": r.current.as_ref().map(ToString::to_string),
          "latest": latest,
          "tag": tag,
          "source": r.upstream.as_ref().map(ToString::to_string),
          "detail": detail,
      })
    })
    .collect();
  serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into())
}

#[cfg(feature = "authoring")]
/// A left-aligned table, padded to the widest cell in each column.
fn print_table(headers: &[&str; 4], rows: &[[String; 4]]) {
  let mut widths: [usize; 4] = std::array::from_fn(|i| headers[i].len());
  for row in rows {
    for (i, cell) in row.iter().enumerate() {
      widths[i] = widths[i].max(cell.len());
    }
  }
  let line = |cells: [&str; 4]| {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
      if i + 1 == cells.len() {
        out.push_str(cell);
      } else {
        out.push_str(&format!("{cell:<width$}  ", width = widths[i]));
      }
    }
    println!("{}", out.trim_end());
  };
  line(*headers);
  for row in rows {
    line([&row[0], &row[1], &row[2], &row[3]]);
  }
}

#[cfg(all(test, feature = "authoring"))]
mod tests {
  use super::{render_entry, render_rule};
  use luthier_core::install::{Listed, derive};
  use luthier_manifest::Format;
  use std::fs;

  /// The layout a DPF-style release ships: a version-named top directory
  /// with one entry per format, plus the VST2 shared object that has no
  /// installer here.
  fn version_nested(base: &std::path::Path) {
    let root = base.join("wstd-eq-v1.1.1");
    fs::create_dir_all(root.join("WSTD_EQ.vst3/Contents")).unwrap();
    fs::create_dir_all(root.join("WSTD_EQ.lv2")).unwrap();
    fs::write(root.join("WSTD_EQ.lv2/manifest.ttl"), b"").unwrap();
    fs::write(root.join("WSTD_EQ.clap"), b"").unwrap();
    fs::write(root.join("WSTD_EQ-vst.so"), b"").unwrap();
  }

  /// Everything at the archive root, which is how a single-plugin release
  /// with nothing else to ship is usually packed.
  fn flat(base: &std::path::Path) {
    fs::create_dir_all(base.join("Fire.vst3/Contents/x86_64-linux")).unwrap();
    fs::write(base.join("Fire.vst3/Contents/x86_64-linux/Fire.so"), b"").unwrap();
    fs::create_dir_all(base.join("Fire.lv2")).unwrap();
    fs::write(base.join("Fire.lv2/manifest.ttl"), b"").unwrap();
    fs::write(base.join("Fire.clap"), b"").unwrap();
    fs::write(base.join("README.md"), b"").unwrap();
  }

  /// One directory per format, which is how a release carrying several
  /// plugins in each format is usually sorted.
  fn format_nested(base: &std::path::Path) {
    for (dir, plugin) in [("clap", "ZamEQ2.clap"), ("clap", "ZamComp.clap")] {
      fs::create_dir_all(base.join(dir)).unwrap();
      fs::write(base.join(dir).join(plugin), b"").unwrap();
    }
    fs::create_dir_all(base.join("vst3/ZamEQ2.vst3/Contents")).unwrap();
    fs::create_dir_all(base.join("lv2/ZamEQ2.lv2")).unwrap();
    fs::write(base.join("lv2/ZamEQ2.lv2/manifest.ttl"), b"").unwrap();
    // Not a format this manager installs: it is listed and no rule is made.
    fs::create_dir_all(base.join("vst2")).unwrap();
    fs::write(base.join("vst2/ZamEQ2-vst.so"), b"").unwrap();
  }

  /// The rules `inspect` would print for a tree, in printed order.
  fn rules_for(base: &std::path::Path) -> Vec<String> {
    derive::from_tree(base)
      .unwrap()
      .rules
      .iter()
      .map(render_rule)
      .collect()
  }

  /// `inspect` renders whatever the shared derivation reports, so the check
  /// that matters here is the rendering, not the walking — that is covered
  /// in `luthier-core`.
  #[test]
  fn the_listing_keeps_the_shape_inspect_has_always_printed() {
    let dir = tempfile::tempdir().unwrap();
    version_nested(dir.path());
    let derived = derive::from_tree(dir.path()).unwrap();

    let lines: Vec<String> = derived.listing.iter().map(render_entry).collect();

    assert!(lines.contains(&"  wstd-eq-v1.1.1/".to_string()));
    assert!(lines.contains(&"  wstd-eq-v1.1.1/WSTD_EQ.clap".to_string()));
    assert!(lines.contains(&"  wstd-eq-v1.1.1/WSTD_EQ.vst3/".to_string()));
    assert!(lines.contains(&"  wstd-eq-v1.1.1/WSTD_EQ.lv2/  (LV2 bundle)".to_string()));
  }

  #[test]
  fn a_version_nested_release_suggests_a_rule_per_format() {
    let dir = tempfile::tempdir().unwrap();
    version_nested(dir.path());

    assert_eq!(
      rules_for(dir.path()),
      vec![
        "          - { format: clap, source: \"wstd-eq-v1.1.1/WSTD_EQ.clap\", kind: file }",
        "          - { format: vst3, source: \"wstd-eq-v1.1.1/WSTD_EQ.vst3\", kind: bundle }",
        // The LV2 arm once listed the bundle and suggested nothing, so
        // every manifest written with `inspect` silently lost the format
        // most Linux plugins ship.
        "          - { format: lv2, source: \"wstd-eq-v1.1.1/WSTD_EQ.lv2\", kind: bundle }",
      ]
    );
  }

  #[test]
  fn a_flat_release_suggests_rules_at_the_archive_root() {
    let dir = tempfile::tempdir().unwrap();
    flat(dir.path());

    assert_eq!(
      rules_for(dir.path()),
      vec![
        "          - { format: clap, source: \"Fire.clap\", kind: file }",
        "          - { format: vst3, source: \"Fire.vst3\", kind: bundle }",
        "          - { format: lv2, source: \"Fire.lv2\", kind: bundle }",
      ]
    );

    // Everything else in the archive is listed and left alone. A rule is
    // produced for what a format installer recognises, never for what
    // happens to sit beside it.
    let listing: Vec<String> = derive::from_tree(dir.path())
      .unwrap()
      .listing
      .iter()
      .map(render_entry)
      .collect();
    assert!(listing.contains(&"  README.md".to_string()), "{listing:?}");
  }

  #[test]
  fn a_format_nested_release_suggests_one_rule_per_plugin() {
    let dir = tempfile::tempdir().unwrap();
    format_nested(dir.path());

    assert_eq!(
      rules_for(dir.path()),
      vec![
        "          - { format: clap, source: \"clap/ZamComp.clap\", kind: file }",
        "          - { format: clap, source: \"clap/ZamEQ2.clap\", kind: file }",
        "          - { format: vst3, source: \"vst3/ZamEQ2.vst3\", kind: bundle }",
        "          - { format: lv2, source: \"lv2/ZamEQ2.lv2\", kind: bundle }",
      ]
    );
  }

  #[test]
  fn a_bundle_is_not_descended_into_and_its_contents_get_no_rules() {
    // A `.vst3` installs whole, so the shared object inside it is not a
    // second thing to install — and a `.clap` that ships *inside* another
    // bundle is not a CLAP to copy into `~/.clap`.
    let dir = tempfile::tempdir().unwrap();
    flat(dir.path());
    fs::write(dir.path().join("Fire.vst3/Contents/Inner.clap"), b"").unwrap();

    let rules = rules_for(dir.path());
    assert_eq!(rules.len(), 3, "{rules:?}");
    assert!(!rules.iter().any(|r| r.contains("Inner.clap")), "{rules:?}");
  }

  #[test]
  fn a_directory_named_like_a_clap_produces_no_rule() {
    // DPF-Plugins ships `ProM.clap` as a directory. Relaxing this would
    // install something no host is guaranteed to load.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("ProM.clap")).unwrap();
    assert!(rules_for(dir.path()).is_empty());
  }

  #[test]
  fn a_plain_directory_is_not_annotated_as_a_bundle() {
    let entry = Listed {
      path: "docs".to_owned(),
      is_dir: true,
      format: None,
    };
    assert_eq!(render_entry(&entry), "  docs/");
  }

  #[test]
  fn a_recognised_bundle_that_is_not_lv2_gets_no_note() {
    let entry = Listed {
      path: "Thing.vst3".to_owned(),
      is_dir: true,
      format: Some(Format::Vst3),
    };
    assert_eq!(render_entry(&entry), "  Thing.vst3/");
  }
}
