//! Fetching and verifying artifacts.
//!
//! Nothing is ever installed before its checksum has been checked (§12, §13).
//! The cache is content-addressed — an entry's filename *is* its SHA-256 — so a
//! cache hit cannot resurrect a file whose contents no longer match what the
//! manifest expects, and a reinstall of a removed package can reuse a verified
//! download (§28).

use crate::error::{DownloadError, Error, Result};
use crate::fsutil;
use luthier_manifest::Sha256Hash;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

/// Receives progress while an artifact is fetched.
pub trait Progress: Send {
  /// Called once, with the total size when the server reports one.
  fn start(&mut self, _label: &str, _total: Option<u64>) {}
  /// Called for each chunk written.
  fn advance(&mut self, _bytes: u64) {}
  /// Called once, whatever the outcome.
  fn finish(&mut self) {}
}

/// A sink that discards progress. Used by tests and non-interactive runs.
pub struct NoProgress;
impl Progress for NoProgress {}

/// Where an artifact ended up, and whether the network was involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
  pub path: PathBuf,
  pub sha256: Sha256Hash,
  pub bytes: u64,
  /// True when the artifact was already in the cache and re-verified.
  pub from_cache: bool,
}

/// The ceiling applied to an artifact whose manifest declares no size.
///
/// Well above any artifact that arrives without a declared size — plugin
/// tarballs run to a few hundred megabytes — but low enough that a runaway
/// response cannot fill the disk.
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// How far past a declared size a response may run before it is abandoned.
///
/// A response that overshoots at all is already wrong, and [`fetch`] rejects it
/// with `SizeMismatch` once it ends. The margin only decides which error a
/// caller sees for a small overshoot; the ceiling is there for the response
/// that never ends.
///
/// [`fetch`]: Downloader::fetch
const SIZE_MARGIN: u64 = 1024 * 1024;

/// How long to wait for a connection to be established.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a transfer may go without delivering a byte.
///
/// Deliberately not a total timeout: a five-gigabyte library legitimately takes
/// a long time, and a deadline on the whole transfer would punish a slow link
/// rather than a stalled one. What is never legitimate is a connection that has
/// stopped saying anything.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Attempts per artifact, the first included.
const MAX_ATTEMPTS: usize = 4;

/// The wait before the second attempt; each later one doubles it.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Fetches artifacts into a content-addressed cache.
pub struct Downloader {
  /// Built on the first request rather than up front. It needs the system's
  /// CA certificates, and a machine without any — a minimal container, a
  /// build sandbox — can still read `file://` URLs, the cache and
  /// `--offline`. Building it eagerly made all of those panic.
  client: std::sync::OnceLock<std::result::Result<reqwest::Client, String>>,
  cache_dir: PathBuf,
  offline: bool,
  default_max_bytes: u64,
}

impl Downloader {
  pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
    Self {
      client: std::sync::OnceLock::new(),
      cache_dir: cache_dir.into(),
      offline: false,
      default_max_bytes: DEFAULT_MAX_BYTES,
    }
  }

  fn client(&self, url: &Url) -> Result<&reqwest::Client> {
    let built = self.client.get_or_init(|| {
      reqwest::Client::builder()
        .user_agent(concat!("luthier/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| transport_reason(&e))
    });
    built.as_ref().map_err(|reason| {
      Error::Download(DownloadError::NoHttpClient {
        url: url.to_string(),
        reason: reason.clone(),
      })
    })
  }

  /// Refuses any network access. Cached artifacts still work.
  pub fn offline(mut self, offline: bool) -> Self {
    self.offline = offline;
    self
  }

  /// Lowers the ceiling used when a manifest declares no size.
  ///
  /// Exists so the refusal can be tested against a few kilobytes rather than
  /// by generating a multi-gigabyte fixture.
  pub fn max_bytes(mut self, max_bytes: u64) -> Self {
    self.default_max_bytes = max_bytes;
    self
  }

  /// How many bytes this artifact is allowed to be.
  ///
  /// A declared size is the better ceiling in both directions: it lets a
  /// 5 GiB sample library through, and it holds a 2 MB plugin to 2 MB rather
  /// than to four gigabytes. The checksum is what establishes that the bytes
  /// are the right ones; the ceiling's only job is to stop a response that
  /// has run away.
  fn ceiling(&self, expected_size: Option<u64>) -> u64 {
    match expected_size {
      Some(size) => size.saturating_add(SIZE_MARGIN),
      None => self.default_max_bytes,
    }
  }

  pub fn cache_path(&self, digest: &Sha256Hash) -> PathBuf {
    self.cache_dir.join(digest.to_string())
  }

  /// Reads a small document at `url` into memory: one attempt, no cache.
  ///
  /// For an answer only worth having fresh, such as which release is the
  /// latest. There is no digest to check it against, so what reads it treats
  /// it as a claim; anything it leads to downloading is checked as usual.
  pub async fn get(&self, url: &Url, max_bytes: u64) -> Result<Vec<u8>> {
    use futures_util::StreamExt;

    let too_large = || {
      Error::Download(DownloadError::TooLarge {
        url: url.to_string(),
        limit: max_bytes,
      })
    };
    match url.scheme() {
      "file" => {
        let path = url.to_file_path().map_err(|_| {
          Error::Download(DownloadError::BadFileUrl {
            url: url.to_string(),
          })
        })?;
        let body = std::fs::read(&path).map_err(|e| Error::io("read", &path, e))?;
        if body.len() as u64 > max_bytes {
          return Err(too_large());
        }
        Ok(body)
      }
      "http" | "https" => {
        if self.offline {
          return Err(Error::Download(DownloadError::Offline {
            url: url.to_string(),
          }));
        }
        let transport = |e: reqwest::Error| {
          Error::Download(DownloadError::Transport {
            url: url.to_string(),
            reason: transport_reason(&e),
          })
        };
        let response = self
          .client(url)?
          .get(url.clone())
          .send()
          .await
          .map_err(transport)?;
        let status = response.status();
        if !status.is_success() {
          return Err(Error::Download(DownloadError::HttpStatus {
            url: url.to_string(),
            status: status.as_u16(),
          }));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
          let chunk = chunk.map_err(transport)?;
          if (body.len() + chunk.len()) as u64 > max_bytes {
            return Err(too_large());
          }
          body.extend_from_slice(&chunk);
        }
        Ok(body)
      }
      other => Err(Error::Download(DownloadError::UnsupportedScheme {
        url: url.to_string(),
        scheme: other.to_owned(),
      })),
    }
  }

  /// Returns a verified local copy of the artifact at `url`.
  ///
  /// The digest is computed while the bytes stream past, so a mismatch is
  /// detected without a second pass and the partial file never gets a name
  /// the cache would hand out.
  pub async fn fetch(
    &self,
    url: &Url,
    expected: &Sha256Hash,
    expected_size: Option<u64>,
    progress: &mut dyn Progress,
  ) -> Result<Fetched> {
    fsutil::ensure_dir(&self.cache_dir)?;
    let final_path = self.cache_path(expected);

    if final_path.is_file() {
      // §28: a cached entry is still verified. Content addressing makes
      // this cheap to reason about, but the check is not skipped.
      let actual = fsutil::hash_file(&final_path)?;
      if &actual == expected {
        let bytes = std::fs::metadata(&final_path)
          .map_err(|e| Error::io("inspect", &final_path, e))?
          .len();
        return Ok(Fetched {
          path: final_path,
          sha256: actual,
          bytes,
          from_cache: true,
        });
      }
      // Only reachable if something outside the manager corrupted the
      // cache; drop the entry and fetch again.
      tracing::warn!(path = %final_path.display(), "cached artifact failed verification; refetching");
      fsutil::remove_any(&final_path)?;
    }

    let temporary = self.cache_dir.join(format!("{expected}.part"));

    let result = match url.scheme() {
      "file" => {
        let _ = fsutil::remove_any(&temporary);
        self
          .fetch_file(url, &temporary, expected_size, progress)
          .await
      }
      "http" | "https" => {
        self
          .fetch_with_retries(url, &temporary, expected, expected_size, progress)
          .await
      }
      other => {
        let _ = fsutil::remove_any(&temporary);
        Err(Error::Download(DownloadError::UnsupportedScheme {
          url: url.to_string(),
          scheme: other.to_owned(),
        }))
      }
    };

    let (actual, bytes) = match result {
      Ok(pair) => pair,
      Err(e) => {
        // The point of keeping a `.part` is the next *command*, not the next
        // attempt: a download abandoned when the network went away should
        // pick up where it stopped when the user tries again tomorrow.
        if !keeps_partial(&e) {
          let _ = fsutil::remove_any(&temporary);
        }
        return Err(e);
      }
    };

    if &actual != expected {
      // Abort before anything is named as if it were trustworthy.
      let _ = fsutil::remove_any(&temporary);
      return Err(Error::Download(DownloadError::ChecksumMismatch {
        url: url.to_string(),
        expected: *expected,
        actual,
      }));
    }

    if let Some(declared) = expected_size
      && declared != bytes
    {
      let _ = fsutil::remove_any(&temporary);
      return Err(Error::Download(DownloadError::SizeMismatch {
        url: url.to_string(),
        expected: declared,
        actual: bytes,
      }));
    }

    std::fs::rename(&temporary, &final_path)
      .map_err(|e| Error::io("move into cache", &final_path, e))?;

    Ok(Fetched {
      path: final_path,
      sha256: actual,
      bytes,
      from_cache: false,
    })
  }

  /// Fetches over HTTP, retrying what is worth retrying and resuming where the
  /// last attempt stopped.
  ///
  /// A five-gigabyte sample library is the case this exists for: without it a
  /// connection dropped at 90% costs the whole download, and one transient 503
  /// costs the whole command.
  async fn fetch_with_retries(
    &self,
    url: &Url,
    temporary: &Path,
    expected: &Sha256Hash,
    expected_size: Option<u64>,
    progress: &mut dyn Progress,
  ) -> Result<(Sha256Hash, u64)> {
    // A `.part` left by an earlier run is resumable, but only its bytes can
    // say so, and only the final checksum can confirm it. Anything else about
    // it — which URL wrote it, how old it is — is not worth trusting, because
    // the checksum settles it either way.
    let mut fresh_only = false;
    let mut last: Option<Error> = None;

    for attempt in 0..MAX_ATTEMPTS {
      if attempt > 0 {
        let backoff = RETRY_BASE_DELAY * 2u32.saturating_pow(attempt as u32 - 1);
        tracing::debug!(url = %url, attempt, ?backoff, "retrying download");
        tokio::time::sleep(backoff).await;
      }

      let resume_from = if fresh_only {
        let _ = fsutil::remove_any(temporary);
        0
      } else {
        resumable_bytes(temporary)
      };

      match self
        .fetch_http(url, temporary, resume_from, expected_size, progress)
        .await
      {
        Ok((actual, bytes)) => {
          // A resumed download that does not verify has one likely cause: the
          // bytes already on disk were not this artifact's. That is worth one
          // clean attempt, and exactly one — a second failure is upstream's.
          if &actual != expected && resume_from > 0 && !fresh_only {
            tracing::warn!(url = %url, "resumed download failed verification; starting over");
            fresh_only = true;
            last = None;
            continue;
          }
          return Ok((actual, bytes));
        }
        Err(e) => {
          if !is_retryable(&e) {
            return Err(e);
          }
          last = Some(e);
        }
      }
    }

    Err(last.expect("a retryable error was recorded before the attempts ran out"))
  }

  /// One HTTP attempt, continuing from `resume_from` bytes already on disk.
  async fn fetch_http(
    &self,
    url: &Url,
    temporary: &Path,
    resume_from: u64,
    expected_size: Option<u64>,
    progress: &mut dyn Progress,
  ) -> Result<(Sha256Hash, u64)> {
    use futures_util::StreamExt;

    if self.offline {
      return Err(Error::Download(DownloadError::Offline {
        url: url.to_string(),
      }));
    }

    let mut request = self.client(url)?.get(url.clone());
    if resume_from > 0 {
      request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }
    let response = request.send().await.map_err(|e| {
      Error::Download(DownloadError::Transport {
        url: url.to_string(),
        reason: transport_reason(&e),
      })
    })?;

    let status = response.status();
    if !status.is_success() {
      return Err(Error::Download(DownloadError::HttpStatus {
        url: url.to_string(),
        status: status.as_u16(),
      }));
    }

    // Asking to resume is a request, not an instruction: a server that ignores
    // `Range` answers 200 with the whole file, and then the existing bytes are
    // worthless. Only 206 means the response continues where we stopped.
    let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT && resume_from > 0;

    let ceiling = self.ceiling(expected_size);
    let total = response
      .content_length()
      .map(|len| len + if resumed { resume_from } else { 0 })
      .or(expected_size);
    progress.start(file_label(url), total);

    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut file = if resumed {
      // The digest has to cover the whole file, so the bytes already on disk
      // are hashed before the new ones arrive.
      let existing = std::fs::read(temporary).map_err(|e| Error::io("read", temporary, e))?;
      hasher.update(&existing);
      written = existing.len() as u64;
      progress.advance(written);
      std::fs::OpenOptions::new()
        .append(true)
        .open(temporary)
        .map_err(|e| Error::io("append to", temporary, e))?
    } else {
      std::fs::File::create(temporary).map_err(|e| Error::io("create", temporary, e))?
    };
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
      let chunk = chunk.map_err(|e| {
        Error::Download(DownloadError::Transport {
          url: url.to_string(),
          reason: transport_reason(&e),
        })
      })?;
      written += chunk.len() as u64;
      if written > ceiling {
        return Err(Error::Download(DownloadError::TooLarge {
          url: url.to_string(),
          limit: ceiling,
        }));
      }
      hasher.update(&chunk);
      file
        .write_all(&chunk)
        .map_err(|e| Error::io("write", temporary, e))?;
      progress.advance(chunk.len() as u64);
    }

    file
      .sync_all()
      .map_err(|e| Error::io("flush", temporary, e))?;
    progress.finish();
    Ok((Sha256Hash::from_bytes(hasher.finalize().into()), written))
  }

  async fn fetch_file(
    &self,
    url: &Url,
    temporary: &Path,
    expected_size: Option<u64>,
    progress: &mut dyn Progress,
  ) -> Result<(Sha256Hash, u64)> {
    let source = url.to_file_path().map_err(|_| {
      Error::Download(DownloadError::BadFileUrl {
        url: url.to_string(),
      })
    })?;
    if !source.is_file() {
      return Err(Error::Download(DownloadError::BadFileUrl {
        url: url.to_string(),
      }));
    }
    let size = std::fs::metadata(&source)
      .map_err(|e| Error::io("inspect", &source, e))?
      .len();
    // A local file states its size before a byte is copied, so the ceiling is
    // applied to the metadata rather than while writing.
    let ceiling = self.ceiling(expected_size);
    if size > ceiling {
      return Err(Error::Download(DownloadError::TooLarge {
        url: url.to_string(),
        limit: ceiling,
      }));
    }
    progress.start(file_label(url), Some(size));
    std::fs::copy(&source, temporary).map_err(|e| Error::io("copy", &source, e))?;
    progress.advance(size);
    progress.finish();
    Ok((fsutil::hash_file(temporary)?, size))
  }
}

/// How many bytes of a previous attempt are on disk and worth continuing from.
///
/// An empty or unreadable `.part` is worth nothing, and resuming from zero is
/// the same as starting.
fn resumable_bytes(temporary: &Path) -> u64 {
  std::fs::metadata(temporary)
    .ok()
    .filter(|m| m.is_file())
    .map(|m| m.len())
    .unwrap_or(0)
}

/// Whether trying again could plausibly produce a different answer.
///
/// The distinction is between the server being briefly unable to answer and the
/// server having answered. A 404 is an answer, and so is a checksum mismatch:
/// repeating either just wastes the user's time and bandwidth.
fn is_retryable(error: &Error) -> bool {
  match error {
    Error::Download(DownloadError::Transport { .. }) => true,
    Error::Download(DownloadError::HttpStatus { status, .. }) => {
      *status >= 500 || *status == 408 || *status == 429
    }
    _ => false,
  }
}

/// Whether the bytes already written are still worth keeping after `error`.
///
/// They are worth keeping when the failure says nothing about them — the
/// network went away, the server was briefly unwell, the run is offline. They
/// are not when the failure is about the bytes themselves, and leaving those on
/// disk would mean the next attempt resumed from a file known to be wrong.
fn keeps_partial(error: &Error) -> bool {
  is_retryable(error) || matches!(error, Error::Download(DownloadError::Offline { .. }))
}

/// The filename part of a URL, for progress labels.
fn file_label(url: &Url) -> &str {
  url
    .path_segments()
    .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
    .unwrap_or("artifact")
}

/// A readable reason for a transport failure.
///
/// `reqwest`'s Display is terse and drops the underlying cause, which is
/// usually the only informative part (DNS failure, connection refused, TLS).
fn transport_reason(error: &reqwest::Error) -> String {
  let mut parts = vec![error.to_string()];
  let mut source = std::error::Error::source(error);
  while let Some(cause) = source {
    parts.push(cause.to_string());
    source = cause.source();
  }
  parts.dedup();
  parts.join(": ")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn digest_of(bytes: &[u8]) -> Sha256Hash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Sha256Hash::from_bytes(hasher.finalize().into())
  }

  fn file_url(path: &Path) -> Url {
    Url::from_file_path(path).unwrap()
  }

  #[tokio::test]
  async fn a_local_fetch_never_builds_the_http_client() {
    // Building it needs the system's CA certificates. A machine without
    // any used to panic here, before even a `file://` URL was read; the
    // Nix build sandbox is one such machine.
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"payload").unwrap();

    let downloader = Downloader::new(dir.path().join("cache"));
    downloader
      .fetch(
        &file_url(&artifact),
        &digest_of(b"payload"),
        None,
        &mut NoProgress,
      )
      .await
      .unwrap();
    assert!(downloader.client.get().is_none());
  }

  #[tokio::test]
  async fn fetches_and_verifies_a_local_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"payload").unwrap();
    let expected = digest_of(b"payload");

    let downloader = Downloader::new(dir.path().join("cache"));
    let fetched = downloader
      .fetch(&file_url(&artifact), &expected, None, &mut NoProgress)
      .await
      .unwrap();

    assert_eq!(fetched.sha256, expected);
    assert_eq!(fetched.bytes, 7);
    assert!(!fetched.from_cache);
    // The cache entry is named for its content.
    assert_eq!(
      fetched.path.file_name().unwrap().to_string_lossy(),
      expected.to_string()
    );
  }

  #[tokio::test]
  async fn a_corrupted_artifact_is_rejected_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"tampered").unwrap();
    let cache = dir.path().join("cache");

    let downloader = Downloader::new(&cache);
    let err = downloader
      .fetch(
        &file_url(&artifact),
        &digest_of(b"expected"),
        None,
        &mut NoProgress,
      )
      .await
      .unwrap_err();

    assert!(
      matches!(err, Error::Download(DownloadError::ChecksumMismatch { .. })),
      "{err}"
    );
    // Nothing usable, and no `.part` litter.
    let entries: Vec<_> = std::fs::read_dir(&cache).unwrap().flatten().collect();
    assert!(
      entries.is_empty(),
      "cache should be empty, found {entries:?}"
    );
  }

  #[tokio::test]
  async fn a_cached_artifact_is_reused_and_re_verified() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"payload").unwrap();
    let expected = digest_of(b"payload");
    let downloader = Downloader::new(dir.path().join("cache"));

    downloader
      .fetch(&file_url(&artifact), &expected, None, &mut NoProgress)
      .await
      .unwrap();
    // Remove the source: a genuine cache hit must not need it.
    std::fs::remove_file(&artifact).unwrap();

    let second = downloader
      .fetch(&file_url(&artifact), &expected, None, &mut NoProgress)
      .await
      .unwrap();
    assert!(second.from_cache);
  }

  #[tokio::test]
  async fn a_corrupted_cache_entry_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"payload").unwrap();
    let expected = digest_of(b"payload");

    fsutil::ensure_dir(&cache).unwrap();
    std::fs::write(cache.join(expected.to_string()), b"corrupted").unwrap();

    let downloader = Downloader::new(&cache);
    let fetched = downloader
      .fetch(&file_url(&artifact), &expected, None, &mut NoProgress)
      .await
      .unwrap();
    assert!(!fetched.from_cache, "a bad cache entry must be refetched");
    assert_eq!(std::fs::read(&fetched.path).unwrap(), b"payload");
  }

  #[tokio::test]
  async fn a_size_that_disagrees_with_the_manifest_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"payload").unwrap();
    let downloader = Downloader::new(dir.path().join("cache"));
    let err = downloader
      .fetch(
        &file_url(&artifact),
        &digest_of(b"payload"),
        Some(999),
        &mut NoProgress,
      )
      .await
      .unwrap_err();
    assert!(
      matches!(err, Error::Download(DownloadError::SizeMismatch { .. })),
      "{err}"
    );
  }

  #[tokio::test]
  async fn a_declared_size_raises_the_ceiling_above_the_default() {
    // The case that made this a bug: CrocellKit declares 5.26 GiB, which is
    // past the fixed ceiling this code used to apply to every artifact.
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("kit.tar.xz");
    std::fs::write(&artifact, b"payload").unwrap();

    let downloader = Downloader::new(dir.path().join("cache")).max_bytes(4);
    let fetched = downloader
      .fetch(
        &file_url(&artifact),
        &digest_of(b"payload"),
        Some(7),
        &mut NoProgress,
      )
      .await
      .unwrap();
    assert_eq!(fetched.bytes, 7);
  }

  #[tokio::test]
  async fn an_artifact_with_no_declared_size_is_held_to_the_default_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let artifact = dir.path().join("plugin.tar.gz");
    std::fs::write(&artifact, b"payload").unwrap();
    let cache = dir.path().join("cache");

    let downloader = Downloader::new(&cache).max_bytes(4);
    let err = downloader
      .fetch(
        &file_url(&artifact),
        &digest_of(b"payload"),
        None,
        &mut NoProgress,
      )
      .await
      .unwrap_err();
    assert!(
      matches!(
        err,
        Error::Download(DownloadError::TooLarge { limit: 4, .. })
      ),
      "{err}"
    );
    let entries: Vec<_> = std::fs::read_dir(&cache).unwrap().flatten().collect();
    assert!(entries.is_empty(), "found {entries:?}");
  }

  #[test]
  fn the_ceiling_follows_the_declared_size() {
    let downloader = Downloader::new("/nonexistent");
    assert_eq!(downloader.ceiling(None), DEFAULT_MAX_BYTES);
    assert_eq!(
      downloader.ceiling(Some(5_646_502_341)),
      5_646_502_341 + SIZE_MARGIN
    );
    // A small artifact is held to its own size, not to four gigabytes.
    assert_eq!(downloader.ceiling(Some(1024)), 1024 + SIZE_MARGIN);
    assert_eq!(downloader.ceiling(Some(u64::MAX)), u64::MAX);
  }

  #[test]
  fn only_a_failure_that_says_nothing_about_the_bytes_is_worth_retrying() {
    let transport = Error::Download(DownloadError::Transport {
      url: "https://example.invalid/a".into(),
      reason: "connection reset".into(),
    });
    let server_error = Error::Download(DownloadError::HttpStatus {
      url: "https://example.invalid/a".into(),
      status: 503,
    });
    let missing = Error::Download(DownloadError::HttpStatus {
      url: "https://example.invalid/a".into(),
      status: 404,
    });
    let too_large = Error::Download(DownloadError::TooLarge {
      url: "https://example.invalid/a".into(),
      limit: 10,
    });
    let offline = Error::Download(DownloadError::Offline {
      url: "https://example.invalid/a".into(),
    });

    assert!(is_retryable(&transport));
    assert!(is_retryable(&server_error));
    // A 404 is an answer. Asking again just wastes the user's time.
    assert!(!is_retryable(&missing));
    assert!(!is_retryable(&too_large));
    assert!(!is_retryable(&offline));

    // What survives on disk: the bytes are only discarded when the failure
    // was about the bytes.
    assert!(keeps_partial(&transport));
    assert!(keeps_partial(&offline));
    assert!(!keeps_partial(&too_large));
    assert!(!keeps_partial(&missing));
  }

  #[tokio::test]
  async fn a_dropped_connection_resumes_from_what_is_already_on_disk() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let expected = digest_of(&payload);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    // The first half is already on disk, as an interrupted attempt would
    // have left it.
    fsutil::ensure_dir(&cache).unwrap();
    let part = cache.join(format!("{expected}.part"));
    std::fs::write(&part, &payload[..2048]).unwrap();

    let server = MockServer::start().await;
    let body = payload.clone();
    Mock::given(method("GET"))
      .and(path("/kit.zip"))
      .respond_with(move |request: &Request| {
        // Honour `Range` the way a real release host does.
        let from = request
          .headers
          .get("range")
          .and_then(|v| v.to_str().ok())
          .and_then(|v| v.strip_prefix("bytes="))
          .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
        match from {
          Some(start) => ResponseTemplate::new(206)
            .insert_header(
              "content-range",
              format!("bytes {start}-{}/{}", body.len() - 1, body.len()).as_str(),
            )
            .set_body_bytes(&body[start..]),
          None => ResponseTemplate::new(200).set_body_bytes(&body[..]),
        }
      })
      .mount(&server)
      .await;

    let url = Url::parse(&format!("{}/kit.zip", server.uri())).unwrap();
    let downloader = Downloader::new(&cache);
    let fetched = downloader
      .fetch(&url, &expected, Some(4096), &mut NoProgress)
      .await
      .unwrap();

    assert_eq!(fetched.bytes, 4096);
    assert_eq!(std::fs::read(&fetched.path).unwrap(), payload);
    // The server was asked for only the missing half.
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(
      received[0].headers.get("range").unwrap().to_str().unwrap(),
      "bytes=2048-"
    );
  }

  #[tokio::test]
  async fn a_stale_partial_file_costs_one_restart_not_the_download() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let payload = b"the real artifact".to_vec();
    let expected = digest_of(&payload);
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");

    // Bytes from something else entirely, under this artifact's name.
    fsutil::ensure_dir(&cache).unwrap();
    std::fs::write(cache.join(format!("{expected}.part")), b"rubbish").unwrap();

    let server = MockServer::start().await;
    let body = payload.clone();
    Mock::given(method("GET"))
      .and(path("/a.tar.gz"))
      .respond_with(move |request: &Request| {
        let from = request
          .headers
          .get("range")
          .and_then(|v| v.to_str().ok())
          .and_then(|v| v.strip_prefix("bytes="))
          .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
        match from {
          Some(start) if start < body.len() => {
            ResponseTemplate::new(206).set_body_bytes(&body[start..])
          }
          Some(_) => ResponseTemplate::new(416),
          None => ResponseTemplate::new(200).set_body_bytes(&body[..]),
        }
      })
      .mount(&server)
      .await;

    let url = Url::parse(&format!("{}/a.tar.gz", server.uri())).unwrap();
    let fetched = Downloader::new(&cache)
      .fetch(&url, &expected, None, &mut NoProgress)
      .await
      .unwrap();
    assert_eq!(std::fs::read(&fetched.path).unwrap(), payload);
  }

  #[tokio::test]
  async fn a_transient_server_error_is_retried_and_a_404_is_not() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let payload = b"eventually".to_vec();
    let expected = digest_of(&payload);
    let dir = tempfile::tempdir().unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/flaky"))
      .respond_with(ResponseTemplate::new(503))
      .up_to_n_times(2)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/flaky"))
      .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/gone"))
      .respond_with(ResponseTemplate::new(404))
      .mount(&server)
      .await;

    let downloader = Downloader::new(dir.path().join("cache"));
    let url = Url::parse(&format!("{}/flaky", server.uri())).unwrap();
    let fetched = downloader
      .fetch(&url, &expected, None, &mut NoProgress)
      .await
      .unwrap();
    assert_eq!(std::fs::read(&fetched.path).unwrap(), payload);

    let gone = Url::parse(&format!("{}/gone", server.uri())).unwrap();
    let err = downloader
      .fetch(&gone, &digest_of(b"x"), None, &mut NoProgress)
      .await
      .unwrap_err();
    assert!(
      matches!(
        err,
        Error::Download(DownloadError::HttpStatus { status: 404, .. })
      ),
      "{err}"
    );
    // One request, not four: the answer was not going to change.
    let gone_requests = server
      .received_requests()
      .await
      .unwrap()
      .into_iter()
      .filter(|r| r.url.path() == "/gone")
      .count();
    assert_eq!(gone_requests, 1);
  }

  #[tokio::test]
  async fn offline_mode_refuses_the_network_but_allows_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let downloader = Downloader::new(dir.path().join("cache")).offline(true);
    let url = Url::parse("https://example.invalid/a.tar.gz").unwrap();
    let err = downloader
      .fetch(&url, &digest_of(b"x"), None, &mut NoProgress)
      .await
      .unwrap_err();
    assert!(
      matches!(err, Error::Download(DownloadError::Offline { .. })),
      "{err}"
    );
  }

  #[test]
  fn progress_labels_use_the_filename() {
    let url = Url::parse(
            "https://github.com/surge-synthesizer/releases-xt/releases/download/1.3.4/surge-xt-linux-1.3.4-pluginsonly.tar.gz",
        )
        .unwrap();
    assert_eq!(file_label(&url), "surge-xt-linux-1.3.4-pluginsonly.tar.gz");
  }
}
