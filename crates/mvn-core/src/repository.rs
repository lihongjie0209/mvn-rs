use std::path::PathBuf;
use std::sync::RwLock;

use crate::coord::ArtifactCoord;
use crate::error::{MvnError, Result};
use crate::settings::{self, Mirror, Server, Settings};

// ---------------------------------------------------------------------------
// RemoteRepository
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RemoteRepository {
    pub id: String,
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl RemoteRepository {
    pub fn new(id: impl Into<String>, url: impl Into<String>) -> Self {
        let url = url.into().trim_end_matches('/').to_string();
        Self {
            id: id.into(),
            url,
            username: None,
            password: None,
        }
    }

    /// Create a remote repository with basic-auth credentials.
    pub fn with_credentials(
        id: impl Into<String>,
        url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        let url = url.into().trim_end_matches('/').to_string();
        Self {
            id: id.into(),
            url,
            username: Some(username.into()),
            password: Some(password.into()),
        }
    }

    pub fn maven_central() -> Self {
        Self::new("central", "https://repo.maven.apache.org/maven2")
    }

    /// Returns `(username, password)` if credentials are configured.
    pub fn credentials(&self) -> Option<(&str, &str)> {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => Some((u.as_str(), p.as_str())),
            _ => None,
        }
    }

    pub fn artifact_url(&self, coord: &ArtifactCoord) -> String {
        format!("{}/{}", self.url, coord.repository_path())
    }

    pub fn pom_url(&self, coord: &ArtifactCoord) -> String {
        format!("{}/{}", self.url, coord.pom_path())
    }

    pub fn metadata_url(&self, group_id: &str, artifact_id: &str) -> String {
        let group_path = group_id.replace('.', "/");
        format!(
            "{}/{}/{}/maven-metadata.xml",
            self.url, group_path, artifact_id
        )
    }

    /// URL for version-level metadata (used for SNAPSHOT timestamp resolution).
    pub fn version_metadata_url(&self, coord: &ArtifactCoord) -> String {
        let group_path = coord.group_id.replace('.', "/");
        format!(
            "{}/{}/{}/{}/maven-metadata.xml",
            self.url, group_path, coord.artifact_id, coord.version
        )
    }
}

// ---------------------------------------------------------------------------
// LocalRepository
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct LocalRepository {
    pub root: PathBuf,
}

impl LocalRepository {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn default_location() -> Self {
        let home = dirs::home_dir().expect("unable to determine home directory");
        Self::new(home.join(".m2").join("repository"))
    }

    pub fn artifact_path(&self, coord: &ArtifactCoord) -> PathBuf {
        self.root.join(coord.repository_path())
    }

    pub fn pom_path(&self, coord: &ArtifactCoord) -> PathBuf {
        self.root.join(coord.pom_path())
    }

    pub fn has_artifact(&self, coord: &ArtifactCoord) -> bool {
        self.artifact_path(coord).exists()
    }

    pub fn read_pom(&self, coord: &ArtifactCoord) -> Result<String> {
        let path = self.pom_path(coord);
        std::fs::read_to_string(&path).map_err(|e| MvnError::IoError(e))
    }

    pub fn store_artifact(&self, coord: &ArtifactCoord, data: &[u8]) -> Result<PathBuf> {
        let path = self.artifact_path(coord);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;
        Ok(path)
    }

    /// List locally cached versions for a groupId:artifactId by scanning
    /// the directory structure. Returns an empty Vec if the directory does
    /// not exist.
    pub fn list_versions(&self, group_id: &str, artifact_id: &str) -> Vec<String> {
        let group_path = group_id.replace('.', std::path::MAIN_SEPARATOR_STR);
        let dir = self.root.join(group_path).join(artifact_id);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| {
                let e = e.ok()?;
                if e.file_type().ok()?.is_dir() {
                    Some(e.file_name().to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// RepositorySystem
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RepositorySystem {
    pub local: LocalRepository,
    remotes: RwLock<Vec<RemoteRepository>>,
}

impl Clone for RepositorySystem {
    fn clone(&self) -> Self {
        let remotes = self.remotes.read().unwrap().clone();
        Self {
            local: self.local.clone(),
            remotes: RwLock::new(remotes),
        }
    }
}

impl RepositorySystem {
    pub fn new(local: LocalRepository, remotes: Vec<RemoteRepository>) -> Self {
        Self {
            local,
            remotes: RwLock::new(remotes),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(
            LocalRepository::default_location(),
            vec![RemoteRepository::maven_central()],
        )
    }

    /// Build a `RepositorySystem` from parsed `Settings`.
    ///
    /// - Uses `localRepository` from settings if specified.
    /// - Collects repositories from active profiles.
    /// - Applies mirror resolution (replacing repo URLs with mirror URLs).
    /// - Looks up server credentials for each effective repo/mirror ID.
    pub fn from_settings(settings: &Settings) -> Self {
        let local = match settings.local_repository_path() {
            Some(path) => LocalRepository::new(path),
            None => LocalRepository::default_location(),
        };

        // Collect repos from active profiles
        let mut remotes: Vec<RemoteRepository> = settings
            .active_repositories()
            .iter()
            .map(|repo| {
                let id = repo.id.as_deref().unwrap_or("unnamed");
                let mut remote = RemoteRepository::new(id, &repo.url);
                // Look up server credentials
                if let Some(server) = settings.find_server(id) {
                    remote.username = server.username.clone();
                    remote.password = server.password.clone();
                }
                remote
            })
            .collect();

        // Always include Maven Central if no remotes configured
        if remotes.is_empty() {
            remotes.push(RemoteRepository::maven_central());
        }

        // Apply mirrors
        remotes = apply_mirrors(&remotes, &settings.mirrors.mirror, &settings.servers.server);

        Self::new(local, remotes)
    }

    pub fn local(&self) -> &LocalRepository {
        &self.local
    }

    pub fn remotes(&self) -> Vec<RemoteRepository> {
        self.remotes.read().unwrap().clone()
    }

    /// Add a remote repository if no existing remote has the same ID or URL.
    /// Returns `true` if the repository was added.
    pub fn add_remote_if_absent(&self, repo: RemoteRepository) -> bool {
        let mut remotes = self.remotes.write().unwrap();
        let url_normalized = repo.url.trim_end_matches('/');
        let already_exists = remotes.iter().any(|r| {
            r.id == repo.id || r.url.trim_end_matches('/') == url_normalized
        });
        if already_exists {
            return false;
        }
        remotes.push(repo);
        true
    }
}

// ---------------------------------------------------------------------------
// Mirror resolution
// ---------------------------------------------------------------------------

/// Replace repository URLs with mirror URLs where `mirrorOf` patterns match.
///
/// For each remote repository, if a mirror pattern matches its ID, the repo is
/// replaced with the mirror (inheriting the mirror's URL and server credentials).
pub fn apply_mirrors(
    repos: &[RemoteRepository],
    mirrors: &[Mirror],
    servers: &[Server],
) -> Vec<RemoteRepository> {
    repos
        .iter()
        .map(|repo| {
            if let Some(mirror) = mirrors
                .iter()
                .find(|m| settings::mirror_of_matches(&m.mirror_of, &repo.id))
            {
                let mirror_id = mirror.id.as_deref().unwrap_or(&repo.id);
                let mut mirrored = RemoteRepository::new(mirror_id, &mirror.url);
                // Look up credentials for the mirror id
                if let Some(server) = servers.iter().find(|s| s.id == mirror_id) {
                    mirrored.username = server.username.clone();
                    mirrored.password = server.password.clone();
                }
                mirrored
            } else {
                repo.clone()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_artifact_url() {
        let repo = RemoteRepository::new("central", "https://repo.maven.apache.org/maven2");
        let coord = ArtifactCoord::new("org.apache.commons", "commons-lang3", "3.12.0");
        assert_eq!(
            repo.artifact_url(&coord),
            "https://repo.maven.apache.org/maven2/org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.jar"
        );
    }

    #[test]
    fn remote_pom_url() {
        let repo = RemoteRepository::new("central", "https://repo.maven.apache.org/maven2");
        let coord = ArtifactCoord::new("org.apache.commons", "commons-lang3", "3.12.0");
        assert_eq!(
            repo.pom_url(&coord),
            "https://repo.maven.apache.org/maven2/org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.pom"
        );
    }

    #[test]
    fn remote_metadata_url() {
        let repo = RemoteRepository::new("central", "https://repo.maven.apache.org/maven2");
        assert_eq!(
            repo.metadata_url("org.apache.commons", "commons-lang3"),
            "https://repo.maven.apache.org/maven2/org/apache/commons/commons-lang3/maven-metadata.xml"
        );
    }

    #[test]
    fn remote_url_trailing_slash_trimmed() {
        let repo = RemoteRepository::new("test", "https://example.com/repo/");
        assert_eq!(repo.url, "https://example.com/repo");
    }

    #[test]
    fn maven_central_defaults() {
        let repo = RemoteRepository::maven_central();
        assert_eq!(repo.id, "central");
        assert_eq!(repo.url, "https://repo.maven.apache.org/maven2");
    }

    #[test]
    fn local_artifact_path() {
        let local = LocalRepository::new("/home/user/.m2/repository");
        let coord = ArtifactCoord::new("org.apache.commons", "commons-lang3", "3.12.0");
        let expected = PathBuf::from("/home/user/.m2/repository")
            .join("org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.jar");
        assert_eq!(local.artifact_path(&coord), expected);
    }

    #[test]
    fn local_pom_path() {
        let local = LocalRepository::new("/home/user/.m2/repository");
        let coord = ArtifactCoord::new("org.apache.commons", "commons-lang3", "3.12.0");
        let expected = PathBuf::from("/home/user/.m2/repository")
            .join("org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.pom");
        assert_eq!(local.pom_path(&coord), expected);
    }

    #[test]
    fn repository_system_creation() {
        let local = LocalRepository::new("/tmp/repo");
        let remotes = vec![RemoteRepository::maven_central()];
        let system = RepositorySystem::new(local, remotes);
        assert_eq!(system.remotes().len(), 1);
        assert_eq!(system.remotes()[0].id, "central");
    }

    // -----------------------------------------------------------------------
    // I/O tests using tempfile
    // -----------------------------------------------------------------------

    use tempfile::TempDir;

    #[test]
    fn store_and_read_artifact() {
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let coord = ArtifactCoord::new("org.example", "mylib", "1.0.0");
        let data = b"fake jar content";

        let path = local.store_artifact(&coord, data).unwrap();
        assert!(local.has_artifact(&coord));
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn has_artifact_missing() {
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let coord = ArtifactCoord::new("com.nonexistent", "ghost", "0.0.1");
        assert!(!local.has_artifact(&coord));
    }

    #[test]
    fn read_pom_from_stored() {
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let coord = ArtifactCoord::new("org.example", "mylib", "2.0.0");
        let pom_content = r#"<project><groupId>org.example</groupId></project>"#;

        // Store the pom file at the expected location
        let pom_path = local.pom_path(&coord);
        std::fs::create_dir_all(pom_path.parent().unwrap()).unwrap();
        std::fs::write(&pom_path, pom_content).unwrap();

        let read_back = local.read_pom(&coord).unwrap();
        assert_eq!(read_back, pom_content);
    }

    #[test]
    fn read_pom_missing() {
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let coord = ArtifactCoord::new("com.missing", "nope", "0.1.0");
        let result = local.read_pom(&coord);
        assert!(result.is_err());
        match result.unwrap_err() {
            MvnError::IoError(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected IoError, got: {other:?}"),
        }
    }

    #[test]
    fn store_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let coord = ArtifactCoord::new(
            "com.deeply.nested.group.id",
            "deep-artifact",
            "3.2.1",
        );
        let data = b"deep content";

        let path = local.store_artifact(&coord, data).unwrap();
        assert!(path.exists());
        assert!(path.parent().unwrap().is_dir());
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn default_location_exists() {
        // Should not panic — just verify it constructs a valid path
        let local = LocalRepository::default_location();
        assert!(local.root.ends_with("repository"));
    }

    #[test]
    fn with_defaults_creates_system() {
        let system = RepositorySystem::with_defaults();
        assert!(!system.remotes().is_empty());
        assert_eq!(system.remotes()[0].id, "central");
        assert!(system.local().root.ends_with("repository"));
    }

    #[test]
    fn remote_url_multiple_trailing_slashes() {
        let repo = RemoteRepository::new("test", "https://example.com///");
        assert_eq!(repo.url, "https://example.com");
    }

    #[test]
    fn remote_url_empty() {
        let repo = RemoteRepository::new("empty", "");
        assert_eq!(repo.url, "");
    }

    #[test]
    fn remote_with_credentials() {
        let repo = RemoteRepository::with_credentials("my-repo", "https://example.com", "user", "pass");
        assert_eq!(repo.credentials(), Some(("user", "pass")));
    }

    #[test]
    fn remote_no_credentials() {
        let repo = RemoteRepository::new("test", "https://example.com");
        assert!(repo.credentials().is_none());
    }

    // -----------------------------------------------------------------------
    // from_settings tests
    // -----------------------------------------------------------------------

    use crate::settings::parse_settings;

    #[test]
    fn from_settings_custom_local_repo() {
        let xml = r#"<settings><localRepository>/custom/repo</localRepository></settings>"#;
        let settings = parse_settings(xml).unwrap();
        let system = RepositorySystem::from_settings(&settings);
        assert_eq!(system.local.root, std::path::PathBuf::from("/custom/repo"));
    }

    #[test]
    fn from_settings_default_local_when_empty() {
        let settings = crate::settings::Settings::default();
        let system = RepositorySystem::from_settings(&settings);
        // Should default to ~/.m2/repository
        assert!(system.local.root.ends_with("repository"));
    }

    #[test]
    fn from_settings_with_profiles_and_repos() {
        let xml = r#"<settings>
            <profiles>
                <profile>
                    <id>nexus</id>
                    <repositories>
                        <repository>
                            <id>nexus-releases</id>
                            <url>https://nexus.example.com/releases</url>
                        </repository>
                    </repositories>
                </profile>
            </profiles>
            <activeProfiles>
                <activeProfile>nexus</activeProfile>
            </activeProfiles>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        let system = RepositorySystem::from_settings(&settings);
        assert_eq!(system.remotes().len(), 1);
        assert_eq!(system.remotes()[0].id, "nexus-releases");
        assert_eq!(system.remotes()[0].url, "https://nexus.example.com/releases");
    }

    #[test]
    fn from_settings_no_profiles_defaults_to_central() {
        let xml = r#"<settings></settings>"#;
        let settings = parse_settings(xml).unwrap();
        let system = RepositorySystem::from_settings(&settings);
        assert_eq!(system.remotes().len(), 1);
        assert_eq!(system.remotes()[0].id, "central");
    }

    #[test]
    fn from_settings_with_mirror_replaces_repo() {
        let xml = r#"<settings>
            <mirrors>
                <mirror>
                    <id>aliyun</id>
                    <mirrorOf>central</mirrorOf>
                    <url>https://maven.aliyun.com/repository/central</url>
                </mirror>
            </mirrors>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        let system = RepositorySystem::from_settings(&settings);
        // Central should be replaced by aliyun mirror
        assert_eq!(system.remotes().len(), 1);
        assert_eq!(system.remotes()[0].id, "aliyun");
        assert_eq!(
            system.remotes()[0].url,
            "https://maven.aliyun.com/repository/central"
        );
    }

    #[test]
    fn from_settings_mirror_with_server_credentials() {
        let xml = r#"<settings>
            <mirrors>
                <mirror>
                    <id>nexus</id>
                    <mirrorOf>*</mirrorOf>
                    <url>https://nexus.corp.com/maven</url>
                </mirror>
            </mirrors>
            <servers>
                <server>
                    <id>nexus</id>
                    <username>deploy</username>
                    <password>secret</password>
                </server>
            </servers>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        let system = RepositorySystem::from_settings(&settings);
        assert_eq!(system.remotes()[0].id, "nexus");
        assert_eq!(system.remotes()[0].credentials(), Some(("deploy", "secret")));
    }

    #[test]
    fn apply_mirrors_no_match() {
        let repos = vec![RemoteRepository::new("my-repo", "https://my.repo.com")];
        let mirrors = vec![];
        let servers = vec![];
        let result = apply_mirrors(&repos, &mirrors, &servers);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "my-repo");
    }

    #[test]
    fn list_versions_from_local_repo() {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalRepository::new(dir.path());

        // Create some version directories
        let base = dir.path().join("org").join("example").join("lib");
        std::fs::create_dir_all(base.join("1.0")).unwrap();
        std::fs::create_dir_all(base.join("2.0")).unwrap();
        std::fs::create_dir_all(base.join("3.0-beta")).unwrap();
        // Also create a file (should be ignored)
        std::fs::write(base.join("_remote.repositories"), "").unwrap();

        let mut versions = local.list_versions("org.example", "lib");
        versions.sort();
        assert_eq!(versions, vec!["1.0", "2.0", "3.0-beta"]);
    }

    #[test]
    fn list_versions_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalRepository::new(dir.path());
        let versions = local.list_versions("no.such", "artifact");
        assert!(versions.is_empty());
    }

    #[test]
    fn add_remote_if_absent_dedup_by_id() {
        let system = RepositorySystem::with_defaults();
        // "central" already present
        let added = system.add_remote_if_absent(
            RemoteRepository::new("central", "https://other.url.com/repo"),
        );
        assert!(!added);
        assert_eq!(system.remotes().len(), 1);
    }

    #[test]
    fn add_remote_if_absent_dedup_by_url() {
        let system = RepositorySystem::with_defaults();
        let added = system.add_remote_if_absent(
            RemoteRepository::new("other-id", "https://repo.maven.apache.org/maven2"),
        );
        assert!(!added);
        assert_eq!(system.remotes().len(), 1);
    }

    #[test]
    fn add_remote_if_absent_new_repo() {
        let system = RepositorySystem::with_defaults();
        let added = system.add_remote_if_absent(
            RemoteRepository::new("jboss", "https://repository.jboss.org/nexus"),
        );
        assert!(added);
        assert_eq!(system.remotes().len(), 2);
        assert_eq!(system.remotes()[1].id, "jboss");
    }

    #[test]
    fn version_metadata_url() {
        let remote =
            RemoteRepository::new("central", "https://repo.maven.apache.org/maven2");
        let coord = ArtifactCoord::new("org.example", "lib", "1.0-SNAPSHOT");
        assert_eq!(
            remote.version_metadata_url(&coord),
            "https://repo.maven.apache.org/maven2/org/example/lib/1.0-SNAPSHOT/maven-metadata.xml"
        );
    }
}
