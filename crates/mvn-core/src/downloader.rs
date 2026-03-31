use std::path::PathBuf;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use sha1::{Digest, Sha1};
use tokio::sync::Semaphore;

use crate::coord::ArtifactCoord;
use crate::error::{MvnError, Result};
use crate::metadata::{parse_metadata, MavenMetadata};
use crate::pom::{parse_pom, Pom};
use crate::repository::RepositorySystem;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// RetryConfig
// ---------------------------------------------------------------------------

/// Maximum number of concurrent artifact downloads.
const MAX_CONCURRENT_DOWNLOADS: usize = 8;

/// Configuration for HTTP retry with exponential backoff.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Initial delay before the first retry (milliseconds).
    pub initial_backoff_ms: u64,
    /// Maximum delay between retries (milliseconds).
    pub max_backoff_ms: u64,
    /// Multiplier applied to the delay after each retry.
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30_000,
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Create a config that disables retries.
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            ..Default::default()
        }
    }

    /// Compute the delay for a given attempt (0-based).
    fn delay_ms(&self, attempt: u32) -> u64 {
        let delay = self.initial_backoff_ms as f64 * self.backoff_multiplier.powi(attempt as i32);
        let delay = (delay.min(self.max_backoff_ms as f64)) as u64;
        // Add jitter: ±25%
        let jitter_range = delay / 4;
        if jitter_range > 0 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as u64;
            let jitter = nanos % (jitter_range * 2);
            delay.saturating_sub(jitter_range) + jitter
        } else {
            delay
        }
    }
}

/// Determine if an error is retryable (network / server errors).
fn is_retryable(err: &MvnError) -> bool {
    match err {
        MvnError::NetworkError(_) => true,
        MvnError::DownloadError { message, .. } => {
            // Retry on 5xx, 408 (timeout), 429 (rate limit)
            message.contains("HTTP 5")
                || message.contains("HTTP 408")
                || message.contains("HTTP 429")
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// ArtifactDownloader
// ---------------------------------------------------------------------------

pub struct ArtifactDownloader {
    client: Client,
    repo_system: RepositorySystem,
    retry_config: RetryConfig,
}

impl ArtifactDownloader {
    pub fn new(repo_system: RepositorySystem) -> Self {
        Self::with_config(repo_system, RetryConfig::default(), None)
    }

    pub fn with_defaults() -> Self {
        Self::new(RepositorySystem::with_defaults())
    }

    /// Build a downloader from parsed `Settings`.
    pub fn from_settings(settings: &Settings) -> Self {
        let repo_system = RepositorySystem::from_settings(settings);
        let proxy = settings.active_proxy();
        Self::with_config(repo_system, RetryConfig::default(), proxy)
    }

    /// Build a downloader from settings with custom retry config.
    pub fn from_settings_with_retry(settings: &Settings, retry_config: RetryConfig) -> Self {
        let repo_system = RepositorySystem::from_settings(settings);
        let proxy = settings.active_proxy();
        Self::with_config(repo_system, retry_config, proxy)
    }

    /// Full constructor with all config options.
    pub fn with_config(
        repo_system: RepositorySystem,
        retry_config: RetryConfig,
        proxy: Option<&crate::settings::Proxy>,
    ) -> Self {
        let mut builder = Client::builder().user_agent("mvn-rs/0.1");

        // Configure proxy if provided
        if let Some(proxy_cfg) = proxy {
            let proxy_url = proxy_cfg.url();
            if let Ok(mut reqwest_proxy) = reqwest::Proxy::all(&proxy_url) {
                if let (Some(u), Some(p)) = (&proxy_cfg.username, &proxy_cfg.password) {
                    reqwest_proxy = reqwest_proxy.basic_auth(u, p);
                }
                builder = builder.proxy(reqwest_proxy);
                tracing::info!("using proxy {}", proxy_url);
            } else {
                tracing::warn!("failed to configure proxy {}", proxy_url);
            }
        }

        let client = builder.build().expect("failed to build HTTP client");

        Self {
            client,
            repo_system,
            retry_config,
        }
    }

    /// Access the repository system.
    pub fn repo_system(&self) -> &RepositorySystem {
        &self.repo_system
    }

    /// Access the retry config.
    pub fn retry_config(&self) -> &RetryConfig {
        &self.retry_config
    }

    // -- Private helpers (with retry) ---------------------------------------

    /// Download raw bytes from a URL with optional auth. Returns `None` if 404.
    async fn fetch_bytes(
        &self,
        url: &str,
        credentials: Option<(&str, &str)>,
    ) -> Result<Option<Vec<u8>>> {
        let mut last_err: Option<MvnError> = None;

        for attempt in 0..=self.retry_config.max_retries {
            if attempt > 0 {
                let delay_ms = self.retry_config.delay_ms(attempt - 1);
                tracing::warn!(
                    "retrying {} (attempt {}/{}, backoff {}ms)",
                    url,
                    attempt,
                    self.retry_config.max_retries,
                    delay_ms
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            tracing::debug!("fetching bytes from {} (attempt {})", url, attempt);
            let mut req = self.client.get(url);
            if let Some((user, pass)) = credentials {
                req = req.basic_auth(user, Some(pass));
            }

            let result = req.send().await;

            match result {
                Ok(response) => {
                    if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Ok(None);
                    }
                    if !response.status().is_success() {
                        let err = MvnError::DownloadError {
                            url: url.to_string(),
                            message: format!("HTTP {}", response.status()),
                        };
                        if is_retryable(&err) && attempt < self.retry_config.max_retries {
                            last_err = Some(err);
                            continue;
                        }
                        return Err(err);
                    }
                    match response.bytes().await {
                        Ok(bytes) => return Ok(Some(bytes.to_vec())),
                        Err(e) => {
                            let err = MvnError::NetworkError(e.to_string());
                            if attempt < self.retry_config.max_retries {
                                last_err = Some(err);
                                continue;
                            }
                            return Err(err);
                        }
                    }
                }
                Err(e) => {
                    let err = MvnError::NetworkError(e.to_string());
                    if attempt < self.retry_config.max_retries {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| MvnError::NetworkError("max retries exceeded".into())))
    }

    /// Download text content from a URL with optional auth. Returns `None` if 404.
    async fn fetch_text(
        &self,
        url: &str,
        credentials: Option<(&str, &str)>,
    ) -> Result<Option<String>> {
        let mut last_err: Option<MvnError> = None;

        for attempt in 0..=self.retry_config.max_retries {
            if attempt > 0 {
                let delay_ms = self.retry_config.delay_ms(attempt - 1);
                tracing::warn!(
                    "retrying {} (attempt {}/{}, backoff {}ms)",
                    url,
                    attempt,
                    self.retry_config.max_retries,
                    delay_ms
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            tracing::debug!("fetching text from {} (attempt {})", url, attempt);
            let mut req = self.client.get(url);
            if let Some((user, pass)) = credentials {
                req = req.basic_auth(user, Some(pass));
            }

            let result = req.send().await;

            match result {
                Ok(response) => {
                    if response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Ok(None);
                    }
                    if !response.status().is_success() {
                        let err = MvnError::DownloadError {
                            url: url.to_string(),
                            message: format!("HTTP {}", response.status()),
                        };
                        if is_retryable(&err) && attempt < self.retry_config.max_retries {
                            last_err = Some(err);
                            continue;
                        }
                        return Err(err);
                    }
                    match response.text().await {
                        Ok(text) => return Ok(Some(text)),
                        Err(e) => {
                            let err = MvnError::NetworkError(e.to_string());
                            if attempt < self.retry_config.max_retries {
                                last_err = Some(err);
                                continue;
                            }
                            return Err(err);
                        }
                    }
                }
                Err(e) => {
                    let err = MvnError::NetworkError(e.to_string());
                    if attempt < self.retry_config.max_retries {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| MvnError::NetworkError("max retries exceeded".into())))
    }

    /// Download artifact bytes and verify SHA-1 checksum.
    async fn download_with_checksum(
        &self,
        url: &str,
        coord: &ArtifactCoord,
        credentials: Option<(&str, &str)>,
    ) -> Result<Vec<u8>> {
        let data = self
            .fetch_bytes(url, credentials)
            .await?
            .ok_or_else(|| MvnError::ArtifactNotFound {
                coord: coord.to_string(),
            })?;

        let sha1_url = format!("{}.sha1", url);
        match self.fetch_text(&sha1_url, credentials).await? {
            Some(checksum_content) => {
                if let Some(expected) = parse_sha1_checksum(&checksum_content) {
                    let mut hasher = Sha1::new();
                    hasher.update(&data);
                    let actual = hex::encode(hasher.finalize());

                    if actual != expected {
                        return Err(MvnError::ChecksumMismatch {
                            artifact: coord.to_string(),
                            expected,
                            actual,
                        });
                    }
                    tracing::debug!("checksum verified for {}", coord);
                } else {
                    tracing::warn!(
                        "could not parse .sha1 content for {}, skipping verification",
                        coord
                    );
                }
            }
            None => {
                tracing::warn!("no .sha1 available for {}, skipping verification", coord);
            }
        }

        Ok(data)
    }

    // -- Public API ---------------------------------------------------------

    /// Download an artifact to the local repository.
    ///
    /// If the artifact already exists locally, returns its path immediately.
    /// Otherwise tries each remote repository in order (with retry).
    pub async fn download_artifact(&self, coord: &ArtifactCoord) -> Result<PathBuf> {
        if self.repo_system.local.has_artifact(coord) {
            tracing::debug!("artifact {} found in local repository", coord);
            return Ok(self.repo_system.local.artifact_path(coord));
        }

        for remote in self.repo_system.remotes() {
            let url = remote.artifact_url(coord);
            let creds = remote.credentials();
            tracing::debug!("trying {} from {}", coord, remote.id);

            match self.download_with_checksum(&url, coord, creds).await {
                Ok(data) => {
                    let path = self.repo_system.local.store_artifact(coord, &data)?;
                    tracing::debug!("stored {} at {}", coord, path.display());
                    return Ok(path);
                }
                Err(MvnError::ArtifactNotFound { .. }) => {
                    tracing::debug!("{} not found in {}", coord, remote.id);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(MvnError::ArtifactNotFound {
            coord: coord.to_string(),
        })
    }

    /// Download a POM file and return its content as a string.
    ///
    /// Checks the local repository first, then tries each remote.
    pub async fn download_pom(&self, coord: &ArtifactCoord) -> Result<String> {
        // Try local first
        if let Ok(content) = self.repo_system.local.read_pom(coord) {
            tracing::debug!("POM for {} found in local repository", coord);
            return Ok(content);
        }

        let pom_coord = ArtifactCoord::with_extension(
            &coord.group_id,
            &coord.artifact_id,
            &coord.version,
            "pom",
        );

        for remote in self.repo_system.remotes() {
            let url = remote.pom_url(coord);
            let creds = remote.credentials();
            tracing::debug!("trying POM for {} from {}", coord, remote.id);

            match self.fetch_text(&url, creds).await? {
                Some(text) => {
                    // Store in local repo
                    self.repo_system
                        .local
                        .store_artifact(&pom_coord, text.as_bytes())?;
                    return Ok(text);
                }
                None => {
                    tracing::debug!("POM for {} not found in {}", coord, remote.id);
                    continue;
                }
            }
        }

        Err(MvnError::ArtifactNotFound {
            coord: coord.to_string(),
        })
    }

    /// Download and parse a POM file.
    pub async fn fetch_pom(&self, coord: &ArtifactCoord) -> Result<Pom> {
        let content = self.download_pom(coord).await?;
        parse_pom(&content)
    }

    /// Fetch `maven-metadata.xml` for a groupId:artifactId.
    ///
    /// Tries each remote repository and returns the first successful result.
    pub async fn fetch_metadata(
        &self,
        group_id: &str,
        artifact_id: &str,
    ) -> Result<MavenMetadata> {
        for remote in self.repo_system.remotes() {
            let url = remote.metadata_url(group_id, artifact_id);
            let creds = remote.credentials();
            tracing::debug!("fetching metadata from {}", url);

            match self.fetch_text(&url, creds).await? {
                Some(text) => {
                    return parse_metadata(&text);
                }
                None => {
                    tracing::debug!(
                        "metadata for {}:{} not found in {}",
                        group_id,
                        artifact_id,
                        remote.id
                    );
                    continue;
                }
            }
        }

        Err(MvnError::ArtifactNotFound {
            coord: format!("{}:{}", group_id, artifact_id),
        })
    }

    // -- Batch / concurrent API ---------------------------------------------

    /// Download multiple artifacts concurrently with bounded parallelism.
    /// Returns a Vec of (ArtifactCoord, Result<PathBuf>) for each artifact.
    pub async fn download_artifacts(
        &self,
        coords: &[ArtifactCoord],
    ) -> Vec<(ArtifactCoord, Result<PathBuf>)> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));

        let mut futures: FuturesUnordered<_> = coords
            .iter()
            .map(|coord| {
                let sem = Arc::clone(&semaphore);
                let coord = coord.clone();
                async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let result = self.download_artifact(&coord).await;
                    if result.is_ok() {
                        tracing::info!("downloaded {}", coord);
                    }
                    (coord, result)
                }
            })
            .collect();

        let mut results = Vec::with_capacity(coords.len());
        while let Some(item) = futures.next().await {
            results.push(item);
        }
        results
    }

    /// Download multiple artifacts concurrently with progress reporting.
    pub async fn download_artifacts_with_progress(
        &self,
        coords: &[ArtifactCoord],
        multi_progress: &MultiProgress,
    ) -> Vec<(ArtifactCoord, Result<PathBuf>)> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
        let style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner());

        let mut futures: FuturesUnordered<_> = coords
            .iter()
            .map(|coord| {
                let sem = Arc::clone(&semaphore);
                let coord = coord.clone();
                let pb = multi_progress.add(ProgressBar::new_spinner());
                pb.set_style(style.clone());
                pb.set_message(format!("downloading {coord}"));
                async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let result = self.download_artifact(&coord).await;
                    match &result {
                        Ok(_) => pb.finish_with_message(format!("✔ {coord}")),
                        Err(_) => pb.finish_with_message(format!("✘ {coord}")),
                    }
                    (coord, result)
                }
            })
            .collect();

        let mut results = Vec::with_capacity(coords.len());
        while let Some(item) = futures.next().await {
            results.push(item);
        }
        results
    }

    /// Fetch multiple POMs concurrently.
    pub async fn fetch_poms(
        &self,
        coords: &[ArtifactCoord],
    ) -> Vec<(ArtifactCoord, Result<Pom>)> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));

        let mut futures: FuturesUnordered<_> = coords
            .iter()
            .map(|coord| {
                let sem = Arc::clone(&semaphore);
                let coord = coord.clone();
                async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");
                    let result = self.fetch_pom(&coord).await;
                    (coord, result)
                }
            })
            .collect();

        let mut results = Vec::with_capacity(coords.len());
        while let Some(item) = futures.next().await {
            results.push(item);
        }
        results
    }
}

// ---------------------------------------------------------------------------
// Checksum parsing helper
// ---------------------------------------------------------------------------

/// Parse a SHA-1 checksum from `.sha1` file content.
///
/// The file may contain just the hex hash, or `"hash  filename"`.
fn parse_sha1_checksum(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Take the first whitespace-delimited token
    let hash = trimmed.split_whitespace().next()?;
    // Validate: SHA-1 is 40 hex characters
    if hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash.to_lowercase())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{LocalRepository, RemoteRepository, RepositorySystem};

    #[test]
    fn downloader_construction() {
        let local = LocalRepository::new("target/test-repo");
        let remotes = vec![RemoteRepository::maven_central()];
        let system = RepositorySystem::new(local, remotes);
        let dl = ArtifactDownloader::new(system);
        assert_eq!(dl.repo_system().remotes().len(), 1);
        assert_eq!(dl.repo_system().remotes()[0].id, "central");
        assert_eq!(dl.retry_config().max_retries, 3); // default
    }

    #[test]
    fn downloader_with_defaults() {
        let dl = ArtifactDownloader::with_defaults();
        assert_eq!(dl.repo_system().remotes().len(), 1);
    }

    #[test]
    fn url_generation_delegates_to_remote() {
        let remote = RemoteRepository::maven_central();
        let coord = ArtifactCoord::new("org.apache.commons", "commons-lang3", "3.12.0");
        assert_eq!(
            remote.artifact_url(&coord),
            "https://repo.maven.apache.org/maven2/org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.jar"
        );
        assert_eq!(
            remote.pom_url(&coord),
            "https://repo.maven.apache.org/maven2/org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.pom"
        );
    }

    #[test]
    fn parse_sha1_hash_only() {
        let content = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        assert_eq!(
            parse_sha1_checksum(content),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string())
        );
    }

    #[test]
    fn parse_sha1_with_filename() {
        let content = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2  commons-lang3-3.12.0.jar";
        assert_eq!(
            parse_sha1_checksum(content),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string())
        );
    }

    #[test]
    fn parse_sha1_with_trailing_whitespace() {
        let content = "  a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2  \n";
        assert_eq!(
            parse_sha1_checksum(content),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string())
        );
    }

    #[test]
    fn parse_sha1_uppercase_normalized() {
        let content = "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4E5F6A1B2";
        assert_eq!(
            parse_sha1_checksum(content),
            Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string())
        );
    }

    #[test]
    fn parse_sha1_empty_returns_none() {
        assert_eq!(parse_sha1_checksum(""), None);
        assert_eq!(parse_sha1_checksum("   "), None);
    }

    #[test]
    fn parse_sha1_invalid_length_returns_none() {
        assert_eq!(parse_sha1_checksum("abc123"), None);
    }

    #[test]
    fn parse_sha1_non_hex_returns_none() {
        assert_eq!(
            parse_sha1_checksum("g1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
            None
        );
    }

    // -----------------------------------------------------------------------
    // Async integration tests with HTTP mocking
    // -----------------------------------------------------------------------

    use tempfile::TempDir;
    use wiremock::matchers::{any, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sha1_hex(data: &[u8]) -> String {
        let hash = Sha1::digest(data);
        hex::encode(hash)
    }

    async fn setup_test() -> (MockServer, TempDir, ArtifactDownloader) {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let remote = RemoteRepository::new("test", &server.uri());
        let repo_system = RepositorySystem::new(local, vec![remote]);
        let downloader =
            ArtifactDownloader::with_config(repo_system, RetryConfig::no_retry(), None);
        (server, tmp, downloader)
    }

    fn test_coord() -> ArtifactCoord {
        ArtifactCoord::new("com.example", "mylib", "1.0.0")
    }

    #[tokio::test]
    async fn download_artifact_from_remote() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();
        let jar_bytes = b"fake-jar-content";
        let checksum = sha1_hex(jar_bytes);

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_bytes.to_vec()))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar.sha1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&checksum))
            .mount(&server)
            .await;

        let result = downloader.download_artifact(&coord).await.unwrap();
        assert!(result.exists());
        assert_eq!(std::fs::read(&result).unwrap(), jar_bytes);
    }

    #[tokio::test]
    async fn download_artifact_local_cache_hit() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();
        let jar_bytes = b"cached-jar-content";

        downloader
            .repo_system()
            .local
            .store_artifact(&coord, jar_bytes)
            .unwrap();

        // If any request reaches the server this mock will cause expect(0) to fail
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .named("no remote requests expected")
            .mount(&server)
            .await;

        let result = downloader.download_artifact(&coord).await.unwrap();
        assert!(result.exists());
        assert_eq!(std::fs::read(&result).unwrap(), jar_bytes);
    }

    #[tokio::test]
    async fn download_artifact_404() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();

        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = downloader.download_artifact(&coord).await.unwrap_err();
        assert!(
            matches!(err, MvnError::ArtifactNotFound { .. }),
            "expected ArtifactNotFound, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn download_artifact_checksum_mismatch() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();
        let jar_bytes = b"fake-jar-content";
        let wrong_checksum = "0000000000000000000000000000000000000000";

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_bytes.to_vec()))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar.sha1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(wrong_checksum))
            .mount(&server)
            .await;

        let err = downloader.download_artifact(&coord).await.unwrap_err();
        match err {
            MvnError::ChecksumMismatch {
                artifact,
                expected,
                actual,
            } => {
                assert!(artifact.contains("com.example"));
                assert_eq!(expected, wrong_checksum);
                assert_eq!(actual, sha1_hex(jar_bytes));
            }
            other => panic!("expected ChecksumMismatch, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_artifact_no_checksum() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();
        let jar_bytes = b"jar-without-checksum";

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_bytes.to_vec()))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar.sha1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let result = downloader.download_artifact(&coord).await.unwrap();
        assert!(result.exists());
        assert_eq!(std::fs::read(&result).unwrap(), jar_bytes);
    }

    #[tokio::test]
    async fn download_pom_success() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();
        let pom_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>mylib</artifactId>
  <version>1.0.0</version>
</project>"#;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.pom"))
            .respond_with(ResponseTemplate::new(200).set_body_string(pom_xml))
            .mount(&server)
            .await;

        let content = downloader.download_pom(&coord).await.unwrap();
        assert_eq!(content, pom_xml);

        // Verify stored in local repo
        let stored = downloader.repo_system().local.read_pom(&coord).unwrap();
        assert_eq!(stored, pom_xml);
    }

    #[tokio::test]
    async fn fetch_pom_parses_xml() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();
        let pom_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>mylib</artifactId>
  <version>1.0.0</version>
  <name>My Library</name>
</project>"#;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.pom"))
            .respond_with(ResponseTemplate::new(200).set_body_string(pom_xml))
            .mount(&server)
            .await;

        let pom = downloader.fetch_pom(&coord).await.unwrap();
        assert_eq!(pom.group_id.as_deref(), Some("com.example"));
        assert_eq!(pom.artifact_id.as_deref(), Some("mylib"));
        assert_eq!(pom.version.as_deref(), Some("1.0.0"));
        assert_eq!(pom.name.as_deref(), Some("My Library"));
    }

    #[tokio::test]
    async fn fetch_metadata_success() {
        let (server, _tmp, downloader) = setup_test().await;
        let metadata_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>com.example</groupId>
  <artifactId>mylib</artifactId>
  <versioning>
    <latest>2.0.0</latest>
    <release>2.0.0</release>
    <versions>
      <version>1.0.0</version>
      <version>1.5.0</version>
      <version>2.0.0</version>
    </versions>
    <lastUpdated>20240101120000</lastUpdated>
  </versioning>
</metadata>"#;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/maven-metadata.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(metadata_xml))
            .mount(&server)
            .await;

        let meta = downloader
            .fetch_metadata("com.example", "mylib")
            .await
            .unwrap();
        assert_eq!(meta.group_id.as_deref(), Some("com.example"));
        assert_eq!(meta.artifact_id.as_deref(), Some("mylib"));
        let versioning = meta.versioning.unwrap();
        assert_eq!(versioning.latest.as_deref(), Some("2.0.0"));
        assert_eq!(versioning.release.as_deref(), Some("2.0.0"));
        assert_eq!(
            versioning.versions.version,
            vec!["1.0.0", "1.5.0", "2.0.0"]
        );
    }

    #[tokio::test]
    async fn download_artifact_tries_multiple_remotes() {
        let server1 = MockServer::start().await;
        let server2 = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let remote1 = RemoteRepository::new("first", &server1.uri());
        let remote2 = RemoteRepository::new("second", &server2.uri());
        let repo_system = RepositorySystem::new(local, vec![remote1, remote2]);
        let downloader = ArtifactDownloader::new(repo_system);
        let coord = test_coord();
        let jar_bytes = b"from-second-remote";
        let checksum = sha1_hex(jar_bytes);

        // First remote returns 404 for everything
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&server1)
            .await;

        // Second remote has the artifact
        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_bytes.to_vec()))
            .mount(&server2)
            .await;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar.sha1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&checksum))
            .mount(&server2)
            .await;

        let result = downloader.download_artifact(&coord).await.unwrap();
        assert_eq!(std::fs::read(&result).unwrap(), jar_bytes);
    }

    #[tokio::test]
    async fn download_artifact_server_error() {
        let (server, _tmp, downloader) = setup_test().await;
        let coord = test_coord();

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = downloader.download_artifact(&coord).await.unwrap_err();
        match err {
            MvnError::DownloadError { url, message } => {
                assert!(url.contains("mylib-1.0.0.jar"));
                assert!(message.contains("500"));
            }
            other => panic!("expected DownloadError, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Retry tests
    // -----------------------------------------------------------------------

    #[test]
    fn retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.initial_backoff_ms, 1000);
        assert_eq!(cfg.max_backoff_ms, 30_000);
    }

    #[test]
    fn retry_config_no_retry() {
        let cfg = RetryConfig::no_retry();
        assert_eq!(cfg.max_retries, 0);
    }

    #[test]
    fn retry_delay_exponential() {
        let cfg = RetryConfig {
            max_retries: 5,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30_000,
            backoff_multiplier: 2.0,
        };
        let d0 = cfg.delay_ms(0);
        let d1 = cfg.delay_ms(1);
        let d2 = cfg.delay_ms(2);
        assert!(d0 >= 750 && d0 <= 1250, "d0 = {d0}");
        assert!(d1 >= 1500 && d1 <= 2500, "d1 = {d1}");
        assert!(d2 >= 3000 && d2 <= 5000, "d2 = {d2}");
    }

    #[test]
    fn retry_delay_capped() {
        let cfg = RetryConfig {
            max_retries: 10,
            initial_backoff_ms: 1000,
            max_backoff_ms: 5000,
            backoff_multiplier: 2.0,
        };
        let d = cfg.delay_ms(8);
        assert!(d <= 6250, "delay should be capped, got {d}");
    }

    #[test]
    fn is_retryable_network_error() {
        assert!(is_retryable(&MvnError::NetworkError("timeout".into())));
    }

    #[test]
    fn is_retryable_5xx() {
        assert!(is_retryable(&MvnError::DownloadError {
            url: "test".into(),
            message: "HTTP 503".into(),
        }));
    }

    #[test]
    fn is_retryable_429() {
        assert!(is_retryable(&MvnError::DownloadError {
            url: "test".into(),
            message: "HTTP 429".into(),
        }));
    }

    #[test]
    fn not_retryable_404() {
        assert!(!is_retryable(&MvnError::ArtifactNotFound {
            coord: "test".into(),
        }));
    }

    #[test]
    fn not_retryable_checksum() {
        assert!(!is_retryable(&MvnError::ChecksumMismatch {
            artifact: "test".into(),
            expected: "a".into(),
            actual: "b".into(),
        }));
    }

    #[test]
    fn not_retryable_400() {
        assert!(!is_retryable(&MvnError::DownloadError {
            url: "test".into(),
            message: "HTTP 400".into(),
        }));
    }

    #[tokio::test]
    async fn retry_succeeds_after_failures() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let remote = RemoteRepository::new("test", &server.uri());
        let repo_system = RepositorySystem::new(local, vec![remote]);
        let retry_config = RetryConfig {
            max_retries: 3,
            initial_backoff_ms: 10,
            max_backoff_ms: 50,
            backoff_multiplier: 2.0,
        };
        let downloader = ArtifactDownloader::with_config(repo_system, retry_config, None);
        let coord = test_coord();
        let jar_bytes = b"retry-success";
        let checksum = sha1_hex(jar_bytes);

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_bytes.to_vec()))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar.sha1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&checksum))
            .mount(&server)
            .await;

        let result = downloader.download_artifact(&coord).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn retry_with_server_error_exhausted() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let remote = RemoteRepository::new("test", &server.uri());
        let repo_system = RepositorySystem::new(local, vec![remote]);
        let retry_config = RetryConfig {
            max_retries: 2,
            initial_backoff_ms: 10,
            max_backoff_ms: 50,
            backoff_multiplier: 2.0,
        };
        let downloader = ArtifactDownloader::with_config(repo_system, retry_config, None);
        let coord = test_coord();

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3..)
            .mount(&server)
            .await;

        let err = downloader.download_artifact(&coord).await.unwrap_err();
        match err {
            MvnError::DownloadError { message, .. } => {
                assert!(message.contains("500"), "got: {message}");
            }
            other => panic!("expected DownloadError, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_with_basic_auth() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let remote = RemoteRepository::with_credentials(
            "authed",
            &server.uri(),
            "user",
            "pass",
        );
        let repo_system = RepositorySystem::new(local, vec![remote]);
        let downloader =
            ArtifactDownloader::with_config(repo_system, RetryConfig::no_retry(), None);
        let coord = test_coord();
        let jar_bytes = b"authed-content";
        let checksum = sha1_hex(jar_bytes);

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar"))
            .and(wiremock::matchers::header_exists("Authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(jar_bytes.to_vec()))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/com/example/mylib/1.0.0/mylib-1.0.0.jar.sha1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&checksum))
            .mount(&server)
            .await;

        let result = downloader.download_artifact(&coord).await.unwrap();
        assert!(result.exists());
    }

    #[test]
    fn from_settings_creates_downloader() {
        let xml = r#"<settings>
            <localRepository>/tmp/mvn-test-repo</localRepository>
        </settings>"#;
        let settings = crate::settings::parse_settings(xml).unwrap();
        let dl = ArtifactDownloader::from_settings(&settings);
        assert_eq!(dl.repo_system().remotes().len(), 1);
        assert_eq!(dl.retry_config().max_retries, 3);
    }
}
