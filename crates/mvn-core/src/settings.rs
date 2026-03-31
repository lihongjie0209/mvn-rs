use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{MvnError, Result};
use crate::pom::strip_xml_namespaces;

// ---------------------------------------------------------------------------
// XML Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub local_repository: Option<String>,
    #[serde(default)]
    pub mirrors: MirrorList,
    #[serde(default)]
    pub servers: ServerList,
    #[serde(default)]
    pub proxies: ProxyList,
    #[serde(default)]
    pub profiles: ProfileList,
    #[serde(default)]
    pub active_profiles: ActiveProfileList,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MirrorList {
    #[serde(rename = "mirror", default)]
    pub mirror: Vec<Mirror>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerList {
    #[serde(rename = "server", default)]
    pub server: Vec<Server>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProxyList {
    #[serde(rename = "proxy", default)]
    pub proxy: Vec<Proxy>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileList {
    #[serde(rename = "profile", default)]
    pub profile: Vec<Profile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActiveProfileList {
    #[serde(rename = "activeProfile", default)]
    pub active_profile: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsRepositoryList {
    #[serde(rename = "repository", default)]
    pub repository: Vec<SettingsRepository>,
}

// ---------------------------------------------------------------------------
// Mirror
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mirror {
    pub id: Option<String>,
    pub name: Option<String>,
    pub url: String,
    pub mirror_of: String,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub id: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proxy {
    pub id: Option<String>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default = "default_http")]
    pub protocol: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub non_proxy_hosts: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_http() -> String {
    "http".to_string()
}
fn default_port() -> u16 {
    8080
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    pub id: Option<String>,
    #[serde(default)]
    pub repositories: SettingsRepositoryList,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsRepository {
    pub id: Option<String>,
    pub name: Option<String>,
    pub url: String,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a settings.xml string into a `Settings` struct.
pub fn parse_settings(xml: &str) -> Result<Settings> {
    let cleaned = strip_xml_namespaces(xml);
    quick_xml::de::from_str::<Settings>(&cleaned).map_err(|e| MvnError::SettingsParseError {
        message: format!("failed to parse settings.xml: {e}"),
    })
}

/// Load settings from a specific file.
pub fn load_settings_from(path: &Path) -> Result<Settings> {
    let content = std::fs::read_to_string(path).map_err(|e| MvnError::SettingsParseError {
        message: format!("failed to read {}: {e}", path.display()),
    })?;
    parse_settings(&content)
}

/// Load settings from the default location (`~/.m2/settings.xml`) or a custom path.
///
/// If `custom_path` is provided, loads from that path.
/// Otherwise, looks for `~/.m2/settings.xml`.
/// Returns `Settings::default()` if no settings file is found.
pub fn load_settings(custom_path: Option<&Path>) -> Result<Settings> {
    if let Some(path) = custom_path {
        return load_settings_from(path);
    }

    let home = dirs::home_dir().expect("unable to determine home directory");
    let user_settings = home.join(".m2").join("settings.xml");

    if user_settings.exists() {
        tracing::info!("loading settings from {}", user_settings.display());
        load_settings_from(&user_settings)
    } else {
        tracing::debug!("no settings.xml found at {}", user_settings.display());
        Ok(Settings::default())
    }
}

// ---------------------------------------------------------------------------
// Mirror matching
// ---------------------------------------------------------------------------

/// Check whether a `mirrorOf` pattern matches a repository ID.
///
/// Supported patterns:
/// - `*` — matches all repositories
/// - `central` — exact match by repository ID
/// - `repo1,repo2` — matches any of the listed IDs
/// - `*,!repo1` — matches all except `repo1`
/// - `external:*` — matches all non-localhost, non-file:// repos (simplified: matches all)
pub fn mirror_of_matches(pattern: &str, repo_id: &str) -> bool {
    let parts: Vec<&str> = pattern.split(',').map(|s| s.trim()).collect();

    let mut matched = false;
    let mut excluded = false;

    for part in &parts {
        if let Some(negated) = part.strip_prefix('!') {
            if negated == repo_id {
                excluded = true;
            }
        } else if *part == "*" {
            matched = true;
        } else if part.starts_with("external:") {
            // Simplified: treat external:* as matching all
            if &part[9..] == "*" {
                matched = true;
            }
        } else if *part == repo_id {
            matched = true;
        }
    }

    matched && !excluded
}

impl Settings {
    /// Find the active proxy (first with `active == true`).
    pub fn active_proxy(&self) -> Option<&Proxy> {
        self.proxies.proxy.iter().find(|p| p.active)
    }

    /// Find server credentials by server/repository ID.
    pub fn find_server(&self, id: &str) -> Option<&Server> {
        self.servers.server.iter().find(|s| s.id == id)
    }

    /// Find the mirror that applies to the given repository ID.
    pub fn find_mirror(&self, repo_id: &str) -> Option<&Mirror> {
        self.mirrors
            .mirror
            .iter()
            .find(|m| mirror_of_matches(&m.mirror_of, repo_id))
    }

    /// Get repository IDs of active profiles.
    pub fn active_profile_ids(&self) -> &[String] {
        &self.active_profiles.active_profile
    }

    /// Collect all repositories from active profiles.
    pub fn active_repositories(&self) -> Vec<&SettingsRepository> {
        let active_ids: std::collections::HashSet<&str> = self
            .active_profiles
            .active_profile
            .iter()
            .map(|s| s.as_str())
            .collect();

        self.profiles
            .profile
            .iter()
            .filter(|p| p.id.as_deref().map_or(false, |id| active_ids.contains(id)))
            .flat_map(|p| &p.repositories.repository)
            .collect()
    }

    /// Get the effective local repository path (or None for default).
    pub fn local_repository_path(&self) -> Option<PathBuf> {
        self.local_repository
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
    }
}

impl Proxy {
    /// Build a proxy URL like `http://host:port`.
    pub fn url(&self) -> String {
        format!("{}://{}:{}", self.protocol, self.host, self.port)
    }

    /// Check if a host should bypass this proxy.
    pub fn is_non_proxy_host(&self, host: &str) -> bool {
        let Some(ref non_proxy) = self.non_proxy_hosts else {
            return false;
        };
        non_proxy
            .split('|')
            .any(|pattern| {
                let pattern = pattern.trim();
                if let Some(suffix) = pattern.strip_prefix('*') {
                    host.ends_with(suffix)
                } else {
                    host == pattern
                }
            })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- mirror_of_matches --------------------------------------------------

    #[test]
    fn mirror_wildcard_matches_all() {
        assert!(mirror_of_matches("*", "central"));
        assert!(mirror_of_matches("*", "my-repo"));
    }

    #[test]
    fn mirror_exact_match() {
        assert!(mirror_of_matches("central", "central"));
        assert!(!mirror_of_matches("central", "my-repo"));
    }

    #[test]
    fn mirror_comma_separated() {
        assert!(mirror_of_matches("central,my-repo", "central"));
        assert!(mirror_of_matches("central,my-repo", "my-repo"));
        assert!(!mirror_of_matches("central,my-repo", "other"));
    }

    #[test]
    fn mirror_wildcard_with_exclusion() {
        assert!(mirror_of_matches("*,!my-repo", "central"));
        assert!(!mirror_of_matches("*,!my-repo", "my-repo"));
    }

    #[test]
    fn mirror_external_wildcard() {
        assert!(mirror_of_matches("external:*", "central"));
        assert!(mirror_of_matches("external:*", "my-repo"));
    }

    #[test]
    fn mirror_external_with_exclusion() {
        assert!(mirror_of_matches("external:*,!central", "my-repo"));
        assert!(!mirror_of_matches("external:*,!central", "central"));
    }

    #[test]
    fn mirror_no_match_empty_pattern_part() {
        assert!(!mirror_of_matches("", "central"));
    }

    // -- parse_settings -----------------------------------------------------

    #[test]
    fn parse_minimal_settings() {
        let xml = r#"<settings></settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert!(settings.local_repository.is_none());
        assert!(settings.mirrors.mirror.is_empty());
        assert!(settings.servers.server.is_empty());
        assert!(settings.proxies.proxy.is_empty());
    }

    #[test]
    fn parse_local_repository() {
        let xml = r#"<settings>
            <localRepository>/custom/repo</localRepository>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert_eq!(
            settings.local_repository.as_deref(),
            Some("/custom/repo")
        );
        assert_eq!(
            settings.local_repository_path(),
            Some(PathBuf::from("/custom/repo"))
        );
    }

    #[test]
    fn parse_mirrors() {
        let xml = r#"<settings>
            <mirrors>
                <mirror>
                    <id>aliyun</id>
                    <name>Aliyun Maven</name>
                    <url>https://maven.aliyun.com/repository/central</url>
                    <mirrorOf>central</mirrorOf>
                </mirror>
            </mirrors>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert_eq!(settings.mirrors.mirror.len(), 1);
        let m = &settings.mirrors.mirror[0];
        assert_eq!(m.id.as_deref(), Some("aliyun"));
        assert_eq!(m.url, "https://maven.aliyun.com/repository/central");
        assert_eq!(m.mirror_of, "central");
    }

    #[test]
    fn parse_servers() {
        let xml = r#"<settings>
            <servers>
                <server>
                    <id>my-repo</id>
                    <username>admin</username>
                    <password>secret123</password>
                </server>
            </servers>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert_eq!(settings.servers.server.len(), 1);
        let s = &settings.servers.server[0];
        assert_eq!(s.id, "my-repo");
        assert_eq!(s.username.as_deref(), Some("admin"));
        assert_eq!(s.password.as_deref(), Some("secret123"));
    }

    #[test]
    fn parse_proxies() {
        let xml = r#"<settings>
            <proxies>
                <proxy>
                    <id>my-proxy</id>
                    <active>true</active>
                    <protocol>http</protocol>
                    <host>proxy.example.com</host>
                    <port>3128</port>
                    <username>puser</username>
                    <password>ppass</password>
                    <nonProxyHosts>localhost|*.internal.com</nonProxyHosts>
                </proxy>
            </proxies>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert_eq!(settings.proxies.proxy.len(), 1);
        let p = &settings.proxies.proxy[0];
        assert_eq!(p.id.as_deref(), Some("my-proxy"));
        assert!(p.active);
        assert_eq!(p.protocol, "http");
        assert_eq!(p.host, "proxy.example.com");
        assert_eq!(p.port, 3128);
        assert_eq!(p.username.as_deref(), Some("puser"));
        assert_eq!(p.url(), "http://proxy.example.com:3128");
    }

    #[test]
    fn parse_profiles_and_active() {
        let xml = r#"<settings>
            <profiles>
                <profile>
                    <id>nexus</id>
                    <repositories>
                        <repository>
                            <id>nexus-releases</id>
                            <url>https://nexus.example.com/releases</url>
                        </repository>
                        <repository>
                            <id>nexus-snapshots</id>
                            <url>https://nexus.example.com/snapshots</url>
                        </repository>
                    </repositories>
                </profile>
                <profile>
                    <id>inactive</id>
                    <repositories>
                        <repository>
                            <id>inactive-repo</id>
                            <url>https://inactive.example.com</url>
                        </repository>
                    </repositories>
                </profile>
            </profiles>
            <activeProfiles>
                <activeProfile>nexus</activeProfile>
            </activeProfiles>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();

        assert_eq!(settings.profiles.profile.len(), 2);
        assert_eq!(settings.active_profiles.active_profile, vec!["nexus"]);

        let repos = settings.active_repositories();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].id.as_deref(), Some("nexus-releases"));
        assert_eq!(repos[1].id.as_deref(), Some("nexus-snapshots"));
    }

    #[test]
    fn parse_with_xml_namespaces() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <settings xmlns="http://maven.apache.org/SETTINGS/1.2.0"
                  xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                  xsi:schemaLocation="http://maven.apache.org/SETTINGS/1.2.0 https://maven.apache.org/xsd/settings-1.2.0.xsd">
            <localRepository>/my/repo</localRepository>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert_eq!(settings.local_repository.as_deref(), Some("/my/repo"));
    }

    #[test]
    fn parse_full_settings() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <settings>
            <localRepository>/opt/maven/repo</localRepository>
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
                    <password>deploy123</password>
                </server>
            </servers>
            <proxies>
                <proxy>
                    <id>corp-proxy</id>
                    <active>true</active>
                    <protocol>https</protocol>
                    <host>proxy.corp.com</host>
                    <port>8080</port>
                </proxy>
            </proxies>
            <profiles>
                <profile>
                    <id>corp</id>
                    <repositories>
                        <repository>
                            <id>corp-releases</id>
                            <url>https://nexus.corp.com/releases</url>
                        </repository>
                    </repositories>
                </profile>
            </profiles>
            <activeProfiles>
                <activeProfile>corp</activeProfile>
            </activeProfiles>
        </settings>"#;

        let settings = parse_settings(xml).unwrap();
        assert_eq!(
            settings.local_repository.as_deref(),
            Some("/opt/maven/repo")
        );
        assert_eq!(settings.mirrors.mirror.len(), 1);
        assert_eq!(settings.mirrors.mirror[0].mirror_of, "*");
        assert_eq!(settings.servers.server.len(), 1);
        assert_eq!(settings.servers.server[0].username.as_deref(), Some("deploy"));
        assert!(settings.active_proxy().is_some());
        assert_eq!(settings.active_proxy().unwrap().port, 8080);

        let server = settings.find_server("nexus");
        assert!(server.is_some());
        assert_eq!(server.unwrap().username.as_deref(), Some("deploy"));
        assert!(settings.find_server("nonexistent").is_none());

        let mirror = settings.find_mirror("central");
        assert!(mirror.is_some());
        assert_eq!(mirror.unwrap().url, "https://nexus.corp.com/maven");
    }

    // -- Proxy helpers ------------------------------------------------------

    #[test]
    fn proxy_url_format() {
        let proxy = Proxy {
            id: Some("test".into()),
            active: true,
            protocol: "https".into(),
            host: "proxy.example.com".into(),
            port: 3128,
            username: None,
            password: None,
            non_proxy_hosts: None,
        };
        assert_eq!(proxy.url(), "https://proxy.example.com:3128");
    }

    #[test]
    fn proxy_non_proxy_hosts() {
        let proxy = Proxy {
            id: None,
            active: true,
            protocol: "http".into(),
            host: "proxy.example.com".into(),
            port: 8080,
            username: None,
            password: None,
            non_proxy_hosts: Some("localhost|*.internal.com|10.0.0.1".into()),
        };
        assert!(proxy.is_non_proxy_host("localhost"));
        assert!(proxy.is_non_proxy_host("foo.internal.com"));
        assert!(proxy.is_non_proxy_host("10.0.0.1"));
        assert!(!proxy.is_non_proxy_host("external.com"));
    }

    #[test]
    fn proxy_no_non_proxy_hosts() {
        let proxy = Proxy {
            id: None,
            active: true,
            protocol: "http".into(),
            host: "proxy.example.com".into(),
            port: 8080,
            username: None,
            password: None,
            non_proxy_hosts: None,
        };
        assert!(!proxy.is_non_proxy_host("anything"));
    }

    // -- Settings helpers ---------------------------------------------------

    #[test]
    fn active_proxy_none() {
        let settings = Settings::default();
        assert!(settings.active_proxy().is_none());
    }

    #[test]
    fn active_proxy_selects_first_active() {
        let xml = r#"<settings>
            <proxies>
                <proxy>
                    <id>inactive</id>
                    <active>false</active>
                    <host>proxy1.example.com</host>
                </proxy>
                <proxy>
                    <id>active</id>
                    <active>true</active>
                    <host>proxy2.example.com</host>
                </proxy>
            </proxies>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        let proxy = settings.active_proxy().unwrap();
        assert_eq!(proxy.host, "proxy2.example.com");
    }

    #[test]
    fn local_repository_path_empty_string() {
        let xml = r#"<settings>
            <localRepository>  </localRepository>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert!(settings.local_repository_path().is_none());
    }

    #[test]
    fn active_repositories_no_profiles() {
        let settings = Settings::default();
        assert!(settings.active_repositories().is_empty());
    }

    // -- load_settings with file I/O ----------------------------------------

    #[test]
    fn load_settings_custom_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("settings.xml");
        std::fs::write(
            &path,
            r#"<settings><localRepository>/custom</localRepository></settings>"#,
        )
        .unwrap();

        let settings = load_settings(Some(&path)).unwrap();
        assert_eq!(settings.local_repository.as_deref(), Some("/custom"));
    }

    #[test]
    fn load_settings_missing_file() {
        let result = load_settings_from(Path::new("/nonexistent/settings.xml"));
        assert!(result.is_err());
        match result.unwrap_err() {
            MvnError::SettingsParseError { message } => {
                assert!(message.contains("failed to read"));
            }
            other => panic!("expected SettingsParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_settings_invalid_xml() {
        let result = parse_settings("<settings><broken");
        assert!(result.is_err());
        match result.unwrap_err() {
            MvnError::SettingsParseError { message } => {
                assert!(message.contains("failed to parse"));
            }
            other => panic!("expected SettingsParseError, got: {other:?}"),
        }
    }

    #[test]
    fn parse_settings_multiple_mirrors() {
        let xml = r#"<settings>
            <mirrors>
                <mirror>
                    <id>m1</id>
                    <mirrorOf>central</mirrorOf>
                    <url>https://mirror1.example.com</url>
                </mirror>
                <mirror>
                    <id>m2</id>
                    <mirrorOf>*</mirrorOf>
                    <url>https://mirror2.example.com</url>
                </mirror>
            </mirrors>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        assert_eq!(settings.mirrors.mirror.len(), 2);
        // find_mirror should return first match
        let m = settings.find_mirror("central").unwrap();
        assert_eq!(m.id.as_deref(), Some("m1"));
        // "other" matches the wildcard (m2)
        let m = settings.find_mirror("other").unwrap();
        assert_eq!(m.id.as_deref(), Some("m2"));
    }

    #[test]
    fn parse_settings_server_no_password() {
        let xml = r#"<settings>
            <servers>
                <server>
                    <id>readonly</id>
                    <username>reader</username>
                </server>
            </servers>
        </settings>"#;
        let settings = parse_settings(xml).unwrap();
        let s = &settings.servers.server[0];
        assert_eq!(s.username.as_deref(), Some("reader"));
        assert!(s.password.is_none());
    }
}
