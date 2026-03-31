use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::sync::Semaphore;

use crate::coord::ArtifactCoord;
use crate::error::{MvnError, Result};
use crate::metadata::{MavenMetadata, Versioning, Versions};
use crate::repository::{LocalRepository, RemoteRepository};
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_CONCURRENT_UPLOADS: usize = 8;

// ---------------------------------------------------------------------------
// UploadResult
// ---------------------------------------------------------------------------

/// Result of uploading a single artifact.
#[derive(Clone, Debug)]
pub struct UploadResult {
    pub coord: ArtifactCoord,
    /// Files that were uploaded (artifact, pom, checksums).
    pub uploaded_files: Vec<String>,
    /// Whether the artifact was skipped (already exists remotely).
    pub skipped: bool,
}

// ---------------------------------------------------------------------------
// ArtifactUploader
// ---------------------------------------------------------------------------

pub struct ArtifactUploader {
    client: Client,
    local: LocalRepository,
    retry_config: UploadRetryConfig,
}

/// Configuration for upload retry with exponential backoff.
#[derive(Debug, Clone)]
pub struct UploadRetryConfig {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub backoff_multiplier: f64,
}

impl Default for UploadRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30_000,
            backoff_multiplier: 2.0,
        }
    }
}

impl UploadRetryConfig {
    fn delay_ms(&self, attempt: u32) -> u64 {
        let delay = self.initial_backoff_ms as f64 * self.backoff_multiplier.powi(attempt as i32);
        let delay = (delay.min(self.max_backoff_ms as f64)) as u64;
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

impl ArtifactUploader {
    pub fn new(local: LocalRepository) -> Self {
        Self::with_config(local, UploadRetryConfig::default(), None)
    }

    /// Build from parsed settings — uses local repo path and proxy.
    pub fn from_settings(settings: &Settings) -> Self {
        let local = match settings.local_repository_path() {
            Some(path) => LocalRepository::new(path),
            None => LocalRepository::default_location(),
        };
        let proxy = settings.active_proxy();
        Self::with_config(local, UploadRetryConfig::default(), proxy)
    }

    pub fn with_config(
        local: LocalRepository,
        retry_config: UploadRetryConfig,
        proxy: Option<&crate::settings::Proxy>,
    ) -> Self {
        let mut builder = Client::builder()
            .user_agent("mvn-rs/0.1")
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(600));

        if let Some(proxy_cfg) = proxy {
            let proxy_url = proxy_cfg.url();
            if let Ok(mut reqwest_proxy) = reqwest::Proxy::all(&proxy_url) {
                if let (Some(u), Some(p)) = (&proxy_cfg.username, &proxy_cfg.password) {
                    reqwest_proxy = reqwest_proxy.basic_auth(u, p);
                }
                builder = builder.proxy(reqwest_proxy);
            }
        }

        let client = builder.build().expect("failed to build HTTP client");
        Self {
            client,
            local,
            retry_config,
        }
    }

    /// Access the local repository.
    pub fn local_repo(&self) -> &LocalRepository {
        &self.local
    }

    // -----------------------------------------------------------------------
    // HTTP PUT with retry
    // -----------------------------------------------------------------------

    /// PUT bytes to a URL with retry and optional basic auth.
    async fn put_bytes(
        &self,
        url: &str,
        data: Vec<u8>,
        credentials: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut last_err: Option<MvnError> = None;

        for attempt in 0..=self.retry_config.max_retries {
            if attempt > 0 {
                let delay_ms = self.retry_config.delay_ms(attempt - 1);
                tracing::warn!(
                    "retrying PUT {} (attempt {}/{}, backoff {}ms)",
                    url, attempt, self.retry_config.max_retries, delay_ms
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            tracing::debug!("PUT {} (attempt {})", url, attempt);
            let mut req = self.client.put(url).body(data.clone());
            if let Some((user, pass)) = credentials {
                req = req.basic_auth(user, Some(pass));
            }

            match req.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() || status == reqwest::StatusCode::CREATED {
                        return Ok(());
                    }
                    let body = response.text().await.unwrap_or_default();
                    let err = MvnError::UploadError {
                        url: url.to_string(),
                        message: format!("HTTP {} — {}", status, body.chars().take(200).collect::<String>()),
                    };
                    if is_retryable_upload(&err) && attempt < self.retry_config.max_retries {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
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

    /// PUT text content to a URL.
    async fn put_text(
        &self,
        url: &str,
        content: &str,
        credentials: Option<(&str, &str)>,
    ) -> Result<()> {
        self.put_bytes(url, content.as_bytes().to_vec(), credentials).await
    }

    // -----------------------------------------------------------------------
    // Single artifact upload
    // -----------------------------------------------------------------------

    /// Upload a single artifact (JAR/POM + checksums) to a remote repository.
    ///
    /// Reads from the local Maven repository and PUTs to the remote.
    /// Generates and uploads SHA-1 and SHA-256 checksums alongside.
    pub async fn upload_artifact(
        &self,
        coord: &ArtifactCoord,
        target: &RemoteRepository,
    ) -> Result<UploadResult> {
        let creds = target.credentials();
        let mut uploaded = Vec::new();

        // 1. Upload the artifact file (JAR, WAR, etc.)
        let artifact_path = self.local.artifact_path(coord);
        if artifact_path.exists() {
            let data = std::fs::read(&artifact_path).map_err(MvnError::IoError)?;
            let url = target.artifact_url(coord);
            self.upload_file_with_checksums(&url, &data, creds).await?;
            uploaded.push(url);
        }

        // 2. Upload the POM
        let pom_path = self.local.pom_path(coord);
        if pom_path.exists() {
            let data = std::fs::read(&pom_path).map_err(MvnError::IoError)?;
            let url = target.pom_url(coord);
            self.upload_file_with_checksums(&url, &data, creds).await?;
            uploaded.push(url);
        }

        if uploaded.is_empty() {
            return Err(MvnError::ArtifactNotFound {
                coord: format!(
                    "{} (not in local repo at {})",
                    coord,
                    self.local.root.display()
                ),
            });
        }

        Ok(UploadResult {
            coord: coord.clone(),
            uploaded_files: uploaded,
            skipped: false,
        })
    }

    /// Upload a file and its SHA-1 + SHA-256 checksums.
    async fn upload_file_with_checksums(
        &self,
        url: &str,
        data: &[u8],
        credentials: Option<(&str, &str)>,
    ) -> Result<()> {
        // Upload the file itself
        self.put_bytes(url, data.to_vec(), credentials).await?;

        // Compute and upload SHA-1
        let sha1_hex = {
            let mut hasher = Sha1::new();
            hasher.update(data);
            hex::encode(hasher.finalize())
        };
        let sha1_url = format!("{url}.sha1");
        self.put_text(&sha1_url, &sha1_hex, credentials).await?;

        // Compute and upload SHA-256
        let sha256_hex = {
            let mut hasher = Sha256::new();
            hasher.update(data);
            hex::encode(hasher.finalize())
        };
        let sha256_url = format!("{url}.sha256");
        self.put_text(&sha256_url, &sha256_hex, credentials).await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Batch upload
    // -----------------------------------------------------------------------

    /// Upload multiple artifacts concurrently.
    pub async fn upload_artifacts(
        &self,
        coords: &[ArtifactCoord],
        target: &RemoteRepository,
    ) -> Vec<(ArtifactCoord, Result<UploadResult>)> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS));
        let mut futures = FuturesUnordered::new();

        for coord in coords {
            let sem = semaphore.clone();
            let coord = coord.clone();
            let target = target.clone();

            futures.push(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");
                let result = self.upload_artifact(&coord, &target).await;
                (coord, result)
            });
        }

        let mut results = Vec::new();
        while let Some(item) = futures.next().await {
            results.push(item);
        }
        results
    }

    /// Upload multiple artifacts with progress reporting.
    pub async fn upload_artifacts_with_progress(
        &self,
        coords: &[ArtifactCoord],
        target: &RemoteRepository,
        multi_progress: &MultiProgress,
    ) -> Vec<(ArtifactCoord, Result<UploadResult>)> {
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS));
        let mut futures = FuturesUnordered::new();

        for coord in coords {
            let sem = semaphore.clone();
            let coord = coord.clone();
            let target = target.clone();
            let mp = multi_progress.clone();

            futures.push(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");

                let spin = mp.add(ProgressBar::new_spinner());
                spin.set_style(
                    ProgressStyle::with_template("    {spinner:.cyan} {msg}")
                        .unwrap_or_else(|_| ProgressStyle::default_spinner())
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                );
                spin.set_message(format!("↑ {}", coord));
                spin.enable_steady_tick(Duration::from_millis(80));

                let result = self.upload_artifact(&coord, &target).await;

                spin.finish_and_clear();
                mp.remove(&spin);

                (coord, result)
            });
        }

        let mut results = Vec::new();
        while let Some(item) = futures.next().await {
            results.push(item);
        }
        results
    }

    // -----------------------------------------------------------------------
    // Metadata update
    // -----------------------------------------------------------------------

    /// Update the remote `maven-metadata.xml` for an artifact to include
    /// the given version.
    pub async fn update_remote_metadata(
        &self,
        coord: &ArtifactCoord,
        target: &RemoteRepository,
    ) -> Result<()> {
        let creds = target.credentials();
        let metadata_url = target.metadata_url(&coord.group_id, &coord.artifact_id);

        // Try to fetch existing metadata
        let existing = self.fetch_remote_text(&metadata_url, creds).await;

        let metadata = match existing {
            Ok(Some(xml)) => {
                match crate::metadata::parse_metadata(&xml) {
                    Ok(mut meta) => {
                        // Add version if not already present
                        if let Some(ref mut versioning) = meta.versioning {
                            if !versioning.versions.version.contains(&coord.version) {
                                versioning.versions.version.push(coord.version.clone());
                            }
                            versioning.latest = Some(coord.version.clone());
                            if !coord.version.contains("SNAPSHOT") {
                                versioning.release = Some(coord.version.clone());
                            }
                            versioning.last_updated = Some(now_timestamp());
                        }
                        meta
                    }
                    Err(_) => self.build_fresh_metadata(coord),
                }
            }
            _ => self.build_fresh_metadata(coord),
        };

        let xml = serialize_metadata(&metadata);
        self.upload_file_with_checksums(&metadata_url, xml.as_bytes(), creds)
            .await?;

        Ok(())
    }

    fn build_fresh_metadata(&self, coord: &ArtifactCoord) -> MavenMetadata {
        MavenMetadata {
            group_id: Some(coord.group_id.clone()),
            artifact_id: Some(coord.artifact_id.clone()),
            version: None,
            versioning: Some(Versioning {
                latest: Some(coord.version.clone()),
                release: if coord.version.contains("SNAPSHOT") {
                    None
                } else {
                    Some(coord.version.clone())
                },
                versions: Versions {
                    version: vec![coord.version.clone()],
                },
                last_updated: Some(now_timestamp()),
                snapshot: None,
            }),
        }
    }

    /// Fetch text from a remote URL (GET). Returns None on 404.
    async fn fetch_remote_text(
        &self,
        url: &str,
        credentials: Option<(&str, &str)>,
    ) -> Result<Option<String>> {
        let mut req = self.client.get(url);
        if let Some((user, pass)) = credentials {
            req = req.basic_auth(user, Some(pass));
        }
        match req.send().await {
            Ok(resp) => {
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if !resp.status().is_success() {
                    return Ok(None);
                }
                Ok(Some(resp.text().await.map_err(|e| MvnError::NetworkError(e.to_string()))?))
            }
            Err(e) => Err(MvnError::NetworkError(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_retryable_upload(err: &MvnError) -> bool {
    match err {
        MvnError::NetworkError(_) => true,
        MvnError::UploadError { message, .. } => {
            message.contains("HTTP 5")
                || message.contains("HTTP 408")
                || message.contains("HTTP 429")
        }
        _ => false,
    }
}

/// Generatea timestamp string in Maven's format: yyyyMMddHHmmss
fn now_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple conversion — enough for metadata timestamp
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days since epoch to date (simplified Gregorian)
    let mut y = 1970i64;
    let mut remaining_days = days as i64;
    loop {
        let year_days = if is_leap(y) { 366 } else { 365 };
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        y += 1;
    }
    let month_days: &[i64] = if is_leap(y) {
        &[31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        &[31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 1u32;
    for &md in month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        m += 1;
    }
    let d = remaining_days + 1;

    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        y, m, d, hours, minutes, seconds
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Serialize a MavenMetadata to XML string.
fn serialize_metadata(meta: &MavenMetadata) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n");

    if let Some(ref gid) = meta.group_id {
        xml.push_str(&format!("  <groupId>{gid}</groupId>\n"));
    }
    if let Some(ref aid) = meta.artifact_id {
        xml.push_str(&format!("  <artifactId>{aid}</artifactId>\n"));
    }
    if let Some(ref v) = meta.version {
        xml.push_str(&format!("  <version>{v}</version>\n"));
    }

    if let Some(ref versioning) = meta.versioning {
        xml.push_str("  <versioning>\n");
        if let Some(ref latest) = versioning.latest {
            xml.push_str(&format!("    <latest>{latest}</latest>\n"));
        }
        if let Some(ref release) = versioning.release {
            xml.push_str(&format!("    <release>{release}</release>\n"));
        }
        xml.push_str("    <versions>\n");
        for v in &versioning.versions.version {
            xml.push_str(&format!("      <version>{v}</version>\n"));
        }
        xml.push_str("    </versions>\n");
        if let Some(ref lu) = versioning.last_updated {
            xml.push_str(&format!("    <lastUpdated>{lu}</lastUpdated>\n"));
        }
        xml.push_str("  </versioning>\n");
    }

    xml.push_str("</metadata>\n");
    xml
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_timestamp_format() {
        let ts = now_timestamp();
        assert_eq!(ts.len(), 14, "timestamp should be 14 chars: {ts}");
        assert!(ts.chars().all(|c| c.is_ascii_digit()), "should be all digits: {ts}");
    }

    #[test]
    fn is_leap_years() {
        assert!(is_leap(2000));
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(!is_leap(2023));
    }

    #[test]
    fn serialize_metadata_roundtrip() {
        let meta = MavenMetadata {
            group_id: Some("org.example".into()),
            artifact_id: Some("test-lib".into()),
            version: None,
            versioning: Some(Versioning {
                latest: Some("2.0".into()),
                release: Some("2.0".into()),
                versions: Versions {
                    version: vec!["1.0".into(), "1.5".into(), "2.0".into()],
                },
                last_updated: Some("20240601120000".into()),
                snapshot: None,
            }),
        };

        let xml = serialize_metadata(&meta);
        assert!(xml.contains("<groupId>org.example</groupId>"));
        assert!(xml.contains("<artifactId>test-lib</artifactId>"));
        assert!(xml.contains("<latest>2.0</latest>"));
        assert!(xml.contains("<release>2.0</release>"));
        assert!(xml.contains("<version>1.5</version>"));
        assert!(xml.contains("<lastUpdated>20240601120000</lastUpdated>"));

        // Verify it can be parsed back
        let parsed = crate::metadata::parse_metadata(&xml).unwrap();
        assert_eq!(parsed.group_id.as_deref(), Some("org.example"));
        assert_eq!(parsed.available_versions().len(), 3);
        assert_eq!(parsed.latest_release(), Some("2.0"));
    }

    #[test]
    fn serialize_metadata_snapshot() {
        let meta = MavenMetadata {
            group_id: Some("com.example".into()),
            artifact_id: Some("snap".into()),
            version: None,
            versioning: Some(Versioning {
                latest: Some("1.0-SNAPSHOT".into()),
                release: None,
                versions: Versions {
                    version: vec!["1.0-SNAPSHOT".into()],
                },
                last_updated: Some("20240601120000".into()),
                snapshot: None,
            }),
        };

        let xml = serialize_metadata(&meta);
        assert!(xml.contains("<latest>1.0-SNAPSHOT</latest>"));
        assert!(!xml.contains("<release>"));
    }

    #[test]
    fn serialize_metadata_minimal() {
        let meta = MavenMetadata {
            group_id: Some("g".into()),
            artifact_id: Some("a".into()),
            version: None,
            versioning: None,
        };
        let xml = serialize_metadata(&meta);
        assert!(xml.contains("<groupId>g</groupId>"));
        assert!(!xml.contains("<versioning>"));
    }

    #[test]
    fn upload_retry_config_defaults() {
        let cfg = UploadRetryConfig::default();
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.initial_backoff_ms, 1000);
        assert_eq!(cfg.max_backoff_ms, 30_000);
        assert!((cfg.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn upload_retry_delay_increases() {
        let cfg = UploadRetryConfig {
            max_retries: 3,
            initial_backoff_ms: 1000,
            max_backoff_ms: 30_000,
            backoff_multiplier: 2.0,
        };
        let d0 = cfg.delay_ms(0);
        let d1 = cfg.delay_ms(1);
        let d2 = cfg.delay_ms(2);
        // With jitter, exact values vary, but base trend should be 1000, 2000, 4000
        assert!(d0 >= 750 && d0 <= 1250, "d0={d0}");
        assert!(d1 >= 1500 && d1 <= 2500, "d1={d1}");
        assert!(d2 >= 3000 && d2 <= 5000, "d2={d2}");
    }

    #[test]
    fn upload_retry_delay_capped() {
        let cfg = UploadRetryConfig {
            max_retries: 10,
            initial_backoff_ms: 1000,
            max_backoff_ms: 5000,
            backoff_multiplier: 10.0,
        };
        let d5 = cfg.delay_ms(5);
        assert!(d5 <= 6250, "should be capped at max_backoff + jitter: {d5}");
    }

    #[test]
    fn is_retryable_upload_errors() {
        assert!(is_retryable_upload(&MvnError::NetworkError("timeout".into())));
        assert!(is_retryable_upload(&MvnError::UploadError {
            url: "http://x".into(),
            message: "HTTP 500 — error".into(),
        }));
        assert!(is_retryable_upload(&MvnError::UploadError {
            url: "http://x".into(),
            message: "HTTP 429 — rate limited".into(),
        }));
        assert!(!is_retryable_upload(&MvnError::UploadError {
            url: "http://x".into(),
            message: "HTTP 403 — forbidden".into(),
        }));
        assert!(!is_retryable_upload(&MvnError::ArtifactNotFound {
            coord: "g:a:v".into(),
        }));
    }

    #[test]
    fn build_fresh_metadata_release() {
        let uploader = ArtifactUploader::new(LocalRepository::new(std::path::PathBuf::from("/tmp/repo")));
        let coord = ArtifactCoord::new("org.example", "lib", "1.0");
        let meta = uploader.build_fresh_metadata(&coord);
        assert_eq!(meta.group_id.as_deref(), Some("org.example"));
        assert_eq!(meta.artifact_id.as_deref(), Some("lib"));
        let v = meta.versioning.as_ref().unwrap();
        assert_eq!(v.latest.as_deref(), Some("1.0"));
        assert_eq!(v.release.as_deref(), Some("1.0"));
        assert_eq!(v.versions.version, vec!["1.0"]);
    }

    #[test]
    fn build_fresh_metadata_snapshot() {
        let uploader = ArtifactUploader::new(LocalRepository::new(std::path::PathBuf::from("/tmp/repo")));
        let coord = ArtifactCoord::new("org.example", "lib", "1.0-SNAPSHOT");
        let meta = uploader.build_fresh_metadata(&coord);
        let v = meta.versioning.as_ref().unwrap();
        assert_eq!(v.latest.as_deref(), Some("1.0-SNAPSHOT"));
        assert!(v.release.is_none(), "SNAPSHOT should not set release");
    }

    #[tokio::test]
    async fn upload_artifact_not_in_local_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let local = LocalRepository::new(tmp.path().to_path_buf());
        let uploader = ArtifactUploader::new(local);
        let target = RemoteRepository::new("test", "http://localhost:1/repo");
        let coord = ArtifactCoord::new("org.example", "nonexistent", "1.0");
        let result = uploader.upload_artifact(&coord, &target).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not in local repo"), "got: {err}");
    }

    #[tokio::test]
    async fn put_bytes_connection_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let local = LocalRepository::new(tmp.path().to_path_buf());
        let uploader = ArtifactUploader::with_config(
            local,
            UploadRetryConfig { max_retries: 0, ..Default::default() },
            None,
        );
        let result = uploader
            .put_bytes("http://127.0.0.1:1/test", vec![1, 2, 3], None)
            .await;
        assert!(result.is_err());
    }
}
