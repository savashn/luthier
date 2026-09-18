//! Asking a forge what the newest version of a package is.
//!
//! A curated registry dies of neglect rather than of bad design: every manifest
//! is pinned to a tag that upstream will eventually move past, and nobody is
//! going to notice by reading release pages. This is the machinery behind
//! `luthier-registry check-updates`.
//!
//! Two decisions shape it.
//!
//! The upstream repository is derived from the **artifact URL**, not from the
//! manifest's `repository` field. Those disagree more often than you would
//! expect: Surge XT builds from `surge-synthesizer/surge` but publishes its
//! stable artifacts from `surge-synthesizer/releases-xt`, so asking the source
//! repository would report the wrong versions forever.
//!
//! Nothing here writes a manifest. A new version means a new checksum, and a
//! checksum must come from the real file (§26) — so this reports, and a human
//! runs `hash-url`.

use luthier_manifest::Manifest;
use semver::Version;
use url::Url;

/// Where a package's artifacts come from, as far as update checking goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
  /// A GitHub repository. Both release assets and tag archives land here.
  GitHub { owner: String, repo: String },
  /// A host this tool has no API for. Reported, never guessed at.
  Unsupported { host: String },
}

impl std::fmt::Display for Upstream {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Upstream::GitHub { owner, repo } => write!(f, "github:{owner}/{repo}"),
      Upstream::Unsupported { host } => write!(f, "{host}"),
    }
  }
}

/// Reads the upstream out of an artifact URL.
///
/// Recognises the two shapes GitHub actually serves:
/// `/{owner}/{repo}/releases/download/{tag}/{file}` for an uploaded asset, and
/// `/{owner}/{repo}/archive/refs/tags/{tag}.zip` for a generated tag archive.
pub fn upstream_of(url: &Url) -> Upstream {
  let host = url.host_str().unwrap_or("").to_owned();
  if host != "github.com" {
    return Upstream::Unsupported { host };
  }
  let mut segments = url.path_segments().into_iter().flatten();
  match (segments.next(), segments.next()) {
    (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => Upstream::GitHub {
      owner: owner.to_owned(),
      repo: repo.to_owned(),
    },
    _ => Upstream::Unsupported { host },
  }
}

/// Turns a forge tag into a version, when it looks like one.
///
/// Tags in the wild are not semver: the registry alone carries `v1.0.1`,
/// `1.3.4`, `v1.7` and `4.5`. A leading `v` is stripped and a missing minor or
/// patch is filled with zero, which is what the manifests already record —
/// `zam-plugins` tags `4.5` and the manifest says `4.5.0`.
///
/// Anything else returns `None` rather than being coerced. A tag like
/// `nightly` or `2024-06-01` is not a version, and pretending otherwise would
/// produce a confident wrong answer.
pub fn version_from_tag(tag: &str) -> Option<Version> {
  let trimmed = tag.trim();
  let stripped = trimmed
    .strip_prefix('v')
    .or_else(|| trimmed.strip_prefix('V'))
    .unwrap_or(trimmed);

  // Only pad the numeric core; a pre-release or build suffix means the tag
  // was already written as semver and should parse as-is.
  if stripped.contains('-') || stripped.contains('+') {
    return Version::parse(stripped).ok();
  }

  let parts: Vec<&str> = stripped.split('.').collect();
  if parts.is_empty() || parts.len() > 3 {
    return None;
  }
  if !parts
    .iter()
    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
  {
    return None;
  }
  let mut padded = parts.join(".");
  for _ in parts.len()..3 {
    padded.push_str(".0");
  }
  Version::parse(&padded).ok()
}

/// The newest version a manifest records, and the artifact it points at.
pub fn newest_release(manifest: &Manifest) -> Option<(Version, &Url)> {
  let release = manifest
    .releases
    .iter()
    .max_by(|a, b| a.version.cmp(&b.version))?;
  let artifact = release.artifacts.first()?;
  Some((release.version.clone(), &artifact.source.url))
}

/// What checking one manifest turned up.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
  /// The registry has the newest version the forge reports.
  Current,
  /// Upstream has moved on.
  Behind { latest: Version, tag: String },
  /// The manifest is newer than the forge's "latest".
  ///
  /// Not an error: GitHub's `/releases/latest` excludes pre-releases, and a
  /// manifest may legitimately point at one. Reported so it is visible.
  Ahead { latest: Version, tag: String },
  /// The forge answered with a tag that is not a version.
  UnreadableTag { tag: String },
  /// The project publishes no releases or tags at all.
  NoUpstreamVersions,
  /// A host with no API this tool knows.
  Unsupported { host: String },
  /// The query failed. Carries the reason so a run can be judged.
  Failed { reason: String },
}

impl Status {
  /// Whether this status should draw a maintainer's attention.
  pub fn is_actionable(&self) -> bool {
    matches!(self, Status::Behind { .. })
  }
}

/// One manifest's result.
#[derive(Debug, Clone)]
pub struct Checked {
  pub id: String,
  pub current: Option<Version>,
  pub upstream: Option<Upstream>,
  pub status: Status,
}

// ------------------------------------------------------------------ client --

#[derive(Debug)]
pub enum ForgeError {
  RateLimited,
  Http { status: u16 },
  Transport(String),
  Body(String),
}

impl std::fmt::Display for ForgeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ForgeError::RateLimited => {
        f.write_str("GitHub rate limit reached; set GITHUB_TOKEN or pass --token to raise it")
      }
      ForgeError::Http { status } => write!(f, "HTTP {status}"),
      ForgeError::Transport(e) => write!(f, "{e}"),
      ForgeError::Body(e) => write!(f, "unreadable response: {e}"),
    }
  }
}

/// A GitHub API client, with the base URL injected so tests can serve it
/// locally and the suite stays offline (§55).
pub struct GitHub {
  client: reqwest::Client,
  base: Url,
  token: Option<String>,
}

impl GitHub {
  pub const PUBLIC_API: &'static str = "https://api.github.com";

  pub fn new(base: Url, token: Option<String>) -> Self {
    Self {
      client: reqwest::Client::new(),
      base,
      token,
    }
  }

  /// The newest tag a repository publishes, or `None` when it publishes none.
  ///
  /// Releases are asked for first because a project that cuts releases means
  /// them; tags are the fallback for projects that only tag, which is how
  /// VSCO 2 ships.
  pub async fn latest_tag(&self, owner: &str, repo: &str) -> Result<Option<String>, ForgeError> {
    let release = self
      .get_json(&format!("repos/{owner}/{repo}/releases/latest"))
      .await?;
    if let Some(value) = release
      && let Some(tag) = value.get("tag_name").and_then(|t| t.as_str())
    {
      return Ok(Some(tag.to_owned()));
    }

    let tags = self.get_json(&format!("repos/{owner}/{repo}/tags")).await?;
    Ok(
      tags
        .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
        .and_then(|t| {
          t.get("name")
            .and_then(|n| n.as_str())
            .map(ToOwned::to_owned)
        }),
    )
  }

  /// `Ok(None)` for 404, which means "no such thing" rather than a failure.
  async fn get_json(&self, path: &str) -> Result<Option<serde_json::Value>, ForgeError> {
    let url = self
      .base
      .join(&format!("/{path}"))
      .map_err(|e| ForgeError::Transport(e.to_string()))?;

    let mut request = self
      .client
      .get(url)
      // GitHub refuses requests without one.
      .header("User-Agent", "luthier-registry")
      .header("Accept", "application/vnd.github+json");
    if let Some(token) = &self.token {
      request = request.header("Authorization", format!("Bearer {token}"));
    }

    let response = request
      .send()
      .await
      .map_err(|e| ForgeError::Transport(e.to_string()))?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
      return Ok(None);
    }
    // 403 with the remaining-quota header at zero is the documented shape
    // of a rate limit; a plain 403 is something else and says so.
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
      || (status == reqwest::StatusCode::FORBIDDEN
        && response
          .headers()
          .get("x-ratelimit-remaining")
          .and_then(|v| v.to_str().ok())
          == Some("0"))
    {
      return Err(ForgeError::RateLimited);
    }
    if !status.is_success() {
      return Err(ForgeError::Http {
        status: status.as_u16(),
      });
    }

    let body = response
      .text()
      .await
      .map_err(|e| ForgeError::Transport(e.to_string()))?;
    serde_json::from_str(&body)
      .map(Some)
      .map_err(|e| ForgeError::Body(e.to_string()))
  }
}

/// Checks one manifest against its forge.
pub async fn check(manifest: &Manifest, github: &GitHub) -> Checked {
  let id = manifest.id.to_string();

  let Some((current, url)) = newest_release(manifest) else {
    // `external` packages and anything else without a release have no
    // version to track. Not a problem, just nothing to do.
    return Checked {
      id,
      current: None,
      upstream: None,
      status: Status::NoUpstreamVersions,
    };
  };

  let upstream = upstream_of(url);
  let (owner, repo) = match &upstream {
    Upstream::GitHub { owner, repo } => (owner.clone(), repo.clone()),
    Upstream::Unsupported { host } => {
      return Checked {
        id,
        current: Some(current),
        upstream: Some(upstream.clone()),
        status: Status::Unsupported { host: host.clone() },
      };
    }
  };

  let status = match github.latest_tag(&owner, &repo).await {
    Err(e) => Status::Failed {
      reason: e.to_string(),
    },
    Ok(None) => Status::NoUpstreamVersions,
    Ok(Some(tag)) => match version_from_tag(&tag) {
      None => Status::UnreadableTag { tag },
      Some(latest) => match latest.cmp(&current) {
        std::cmp::Ordering::Greater => Status::Behind { latest, tag },
        std::cmp::Ordering::Equal => Status::Current,
        std::cmp::Ordering::Less => Status::Ahead { latest, tag },
      },
    },
  };

  Checked {
    id,
    current: Some(current),
    upstream: Some(upstream),
    status,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
  }

  #[test]
  fn a_release_asset_url_yields_the_publishing_repository() {
    // Surge XT is the case this exists for: it builds from
    // `surge-synthesizer/surge` and publishes from `releases-xt`, so the
    // manifest's `repository` field is the wrong thing to ask.
    assert_eq!(
      upstream_of(&url(
        "https://github.com/surge-synthesizer/releases-xt/releases/download/1.3.4/surge-xt-linux-1.3.4-pluginsonly.tar.gz"
      )),
      Upstream::GitHub {
        owner: "surge-synthesizer".into(),
        repo: "releases-xt".into()
      }
    );
  }

  #[test]
  fn a_generated_tag_archive_yields_the_same_repository() {
    // VSCO 2 publishes no release asset, so its manifest points at a tag
    // archive. Same owner/repo, different URL shape.
    assert_eq!(
      upstream_of(&url(
        "https://github.com/sgossner/VSCO-2-CE/archive/refs/tags/1.1.0.zip"
      )),
      Upstream::GitHub {
        owner: "sgossner".into(),
        repo: "VSCO-2-CE".into()
      }
    );
  }

  #[test]
  fn a_plain_website_is_reported_rather_than_guessed_at() {
    // The DrumGizmo kits are served from drumgizmo.org, which has no API.
    assert_eq!(
      upstream_of(&url("https://drumgizmo.org/kits/DRSKit/DRSKit2_1.zip")),
      Upstream::Unsupported {
        host: "drumgizmo.org".into()
      }
    );
  }

  #[test]
  fn tags_in_the_registrys_own_shapes_all_parse() {
    // Every tag form the registry actually contains today.
    for (tag, expected) in [
      ("1.8.10", "1.8.10"), // bsequencer
      ("v1.0.1", "1.0.1"),  // dexed
      ("v1.7", "1.7.0"),    // dpf-plugins
      ("3.2.10", "3.2.10"), // dragonfly-reverb
      ("v1.5.0", "1.5.0"),  // fire
      ("1.2.35", "1.2.35"), // lsp-plugins
      ("1.3.4", "1.3.4"),   // surge-xt
      ("4.5", "4.5.0"),     // zam-plugins
    ] {
      assert_eq!(
        version_from_tag(tag),
        Some(Version::parse(expected).unwrap()),
        "tag {tag}"
      );
    }
  }

  #[test]
  fn a_tag_that_is_not_a_version_is_refused_rather_than_coerced() {
    // Guessing here would produce a confident wrong answer, which is worse
    // than reporting that the tag could not be read.
    for tag in [
      "nightly",
      "2024-06-01",
      "release",
      "",
      "1.2.3.4",
      "v",
      "x.y",
    ] {
      assert_eq!(version_from_tag(tag), None, "tag {tag:?}");
    }
  }

  #[test]
  fn a_prerelease_tag_keeps_its_suffix() {
    assert_eq!(
      version_from_tag("v2.0.0-rc.1"),
      Some(Version::parse("2.0.0-rc.1").unwrap())
    );
  }

  #[test]
  fn behind_is_the_only_actionable_status() {
    assert!(
      Status::Behind {
        latest: Version::new(2, 0, 0),
        tag: "v2.0.0".into()
      }
      .is_actionable()
    );
    for status in [
      Status::Current,
      Status::NoUpstreamVersions,
      Status::Unsupported {
        host: "example.invalid".into(),
      },
      Status::UnreadableTag {
        tag: "nightly".into(),
      },
      Status::Failed {
        reason: "boom".into(),
      },
      Status::Ahead {
        latest: Version::new(1, 0, 0),
        tag: "v1.0.0".into(),
      },
    ] {
      assert!(!status.is_actionable(), "{status:?}");
    }
  }
}
