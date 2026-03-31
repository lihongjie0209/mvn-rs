use serde::{Deserialize, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// Maven Metadata Model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MavenMetadata {
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
    #[serde(rename = "artifactId", default)]
    pub artifact_id: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub versioning: Option<Versioning>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Versioning {
    pub latest: Option<String>,
    pub release: Option<String>,
    #[serde(default)]
    pub versions: Versions,
    #[serde(rename = "lastUpdated", default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Versions {
    #[serde(rename = "version", default)]
    pub version: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub timestamp: Option<String>,
    #[serde(rename = "buildNumber", default)]
    pub build_number: Option<u32>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

pub fn parse_metadata(xml: &str) -> Result<MavenMetadata> {
    let metadata: MavenMetadata = quick_xml::de::from_str(xml)?;
    Ok(metadata)
}

// ---------------------------------------------------------------------------
// Helper methods
// ---------------------------------------------------------------------------

impl MavenMetadata {
    /// Get all available versions as strings.
    pub fn available_versions(&self) -> Vec<&str> {
        match &self.versioning {
            Some(v) => v.versions.version.iter().map(|s| s.as_str()).collect(),
            None => vec![],
        }
    }

    /// Get the latest release version.
    pub fn latest_release(&self) -> Option<&str> {
        self.versioning
            .as_ref()
            .and_then(|v| v.release.as_deref())
    }

    /// Get the latest version (including snapshots).
    pub fn latest_version(&self) -> Option<&str> {
        self.versioning
            .as_ref()
            .and_then(|v| v.latest.as_deref())
    }
}

/// Merge two metadata objects, combining version lists and keeping the
/// latest `release`/`latest`/`lastUpdated` values. Matches Java Maven's
/// metadata merge from multiple repositories.
pub fn merge_metadata(mut base: MavenMetadata, other: MavenMetadata) -> MavenMetadata {
    match (&mut base.versioning, other.versioning) {
        (Some(bv), Some(ov)) => {
            // Merge version lists (union, deduplicated)
            let mut seen: std::collections::HashSet<String> =
                bv.versions.version.iter().cloned().collect();
            for v in ov.versions.version {
                if seen.insert(v.clone()) {
                    bv.versions.version.push(v);
                }
            }
            // Keep the latest `release` and `latest` by lastUpdated timestamp
            if ov.last_updated > bv.last_updated {
                if ov.release.is_some() {
                    bv.release = ov.release;
                }
                if ov.latest.is_some() {
                    bv.latest = ov.latest;
                }
                bv.last_updated = ov.last_updated;
            }
            // Keep snapshot from the newer metadata
            if ov.snapshot.is_some() && bv.snapshot.is_none() {
                bv.snapshot = ov.snapshot;
            }
        }
        (None, some) => {
            base.versioning = some;
        }
        _ => {}
    }
    base
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_METADATA: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata>
  <groupId>org.apache.commons</groupId>
  <artifactId>commons-lang3</artifactId>
  <versioning>
    <latest>3.14.0</latest>
    <release>3.14.0</release>
    <versions>
      <version>3.0</version>
      <version>3.1</version>
      <version>3.12.0</version>
      <version>3.13.0</version>
      <version>3.14.0</version>
    </versions>
    <lastUpdated>20231215123456</lastUpdated>
  </versioning>
</metadata>"#;

    #[test]
    fn parse_metadata_succeeds() {
        let meta = parse_metadata(SAMPLE_METADATA).unwrap();
        assert_eq!(meta.group_id.as_deref(), Some("org.apache.commons"));
        assert_eq!(meta.artifact_id.as_deref(), Some("commons-lang3"));
    }

    #[test]
    fn available_versions_returns_correct_list() {
        let meta = parse_metadata(SAMPLE_METADATA).unwrap();
        let versions = meta.available_versions();
        assert_eq!(versions, vec!["3.0", "3.1", "3.12.0", "3.13.0", "3.14.0"]);
    }

    #[test]
    fn latest_release_returns_correct_value() {
        let meta = parse_metadata(SAMPLE_METADATA).unwrap();
        assert_eq!(meta.latest_release(), Some("3.14.0"));
    }

    #[test]
    fn latest_version_returns_correct_value() {
        let meta = parse_metadata(SAMPLE_METADATA).unwrap();
        assert_eq!(meta.latest_version(), Some("3.14.0"));
    }

    #[test]
    fn parse_empty_versioning() {
        let xml = r#"<metadata>
  <groupId>com.example</groupId>
  <artifactId>test</artifactId>
</metadata>"#;
        let meta = parse_metadata(xml).unwrap();
        assert!(meta.versioning.is_none());
        assert!(meta.available_versions().is_empty());
        assert_eq!(meta.latest_release(), None);
        assert_eq!(meta.latest_version(), None);
    }

    // ==== Edge-case tests ====

    #[test]
    fn parse_malformed_xml() {
        assert!(parse_metadata("<not valid").is_err());
    }

    #[test]
    fn parse_snapshot_metadata() {
        let xml = r#"<metadata>
  <groupId>com.example</groupId>
  <artifactId>snap-lib</artifactId>
  <version>1.0-SNAPSHOT</version>
  <versioning>
    <snapshot>
      <timestamp>20231215.123456</timestamp>
      <buildNumber>42</buildNumber>
    </snapshot>
    <lastUpdated>20231215123456</lastUpdated>
  </versioning>
</metadata>"#;
        let meta = parse_metadata(xml).unwrap();
        let snap = meta
            .versioning
            .as_ref()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap();
        assert_eq!(snap.timestamp.as_deref(), Some("20231215.123456"));
        assert_eq!(snap.build_number, Some(42));
    }

    #[test]
    fn parse_metadata_only_version() {
        let xml = r#"<metadata>
  <version>5.0</version>
</metadata>"#;
        let meta = parse_metadata(xml).unwrap();
        assert_eq!(meta.version.as_deref(), Some("5.0"));
        assert!(meta.group_id.is_none());
        assert!(meta.artifact_id.is_none());
    }

    #[test]
    fn parse_metadata_empty_versions_list() {
        let xml = r#"<metadata>
  <groupId>com.example</groupId>
  <artifactId>empty</artifactId>
  <versioning>
    <versions/>
    <lastUpdated>20240101000000</lastUpdated>
  </versioning>
</metadata>"#;
        let meta = parse_metadata(xml).unwrap();
        assert!(meta.available_versions().is_empty());
    }

    #[test]
    fn latest_version_differs_from_release() {
        let xml = r#"<metadata>
  <groupId>com.example</groupId>
  <artifactId>mixed</artifactId>
  <versioning>
    <latest>2.0-SNAPSHOT</latest>
    <release>1.0</release>
    <versions>
      <version>1.0</version>
      <version>2.0-SNAPSHOT</version>
    </versions>
  </versioning>
</metadata>"#;
        let meta = parse_metadata(xml).unwrap();
        assert_eq!(meta.latest_version(), Some("2.0-SNAPSHOT"));
        assert_eq!(meta.latest_release(), Some("1.0"));
    }

    #[test]
    fn available_versions_large_list() {
        let versions: Vec<String> = (1..=100).map(|i| format!("      <version>1.{i}</version>")).collect();
        let xml = format!(
            r#"<metadata>
  <groupId>com.example</groupId>
  <artifactId>big</artifactId>
  <versioning>
    <versions>
{}
    </versions>
  </versioning>
</metadata>"#,
            versions.join("\n")
        );
        let meta = parse_metadata(&xml).unwrap();
        let avail = meta.available_versions();
        assert_eq!(avail.len(), 100);
        assert_eq!(avail[0], "1.1");
        assert_eq!(avail[99], "1.100");
    }

    #[test]
    fn merge_metadata_combines_versions() {
        let base = MavenMetadata {
            group_id: Some("org.example".into()),
            artifact_id: Some("lib".into()),
            version: None,
            versioning: Some(Versioning {
                latest: Some("1.2".into()),
                release: Some("1.2".into()),
                versions: Versions {
                    version: vec!["1.0".into(), "1.1".into(), "1.2".into()],
                },
                last_updated: Some("20240101000000".into()),
                snapshot: None,
            }),
        };
        let other = MavenMetadata {
            group_id: Some("org.example".into()),
            artifact_id: Some("lib".into()),
            version: None,
            versioning: Some(Versioning {
                latest: Some("1.4".into()),
                release: Some("1.4".into()),
                versions: Versions {
                    version: vec!["1.2".into(), "1.3".into(), "1.4".into()],
                },
                last_updated: Some("20240601000000".into()),
                snapshot: None,
            }),
        };

        let merged = merge_metadata(base, other);
        let versions = merged.available_versions();
        assert_eq!(versions.len(), 5); // 1.0, 1.1, 1.2, 1.3, 1.4 (deduplicated)
        assert!(versions.contains(&"1.0"));
        assert!(versions.contains(&"1.4"));
        assert_eq!(merged.latest_release(), Some("1.4")); // newer wins
    }

    #[test]
    fn merge_metadata_base_only() {
        let base = MavenMetadata {
            group_id: Some("org.example".into()),
            artifact_id: Some("lib".into()),
            version: None,
            versioning: Some(Versioning {
                latest: Some("1.0".into()),
                release: Some("1.0".into()),
                versions: Versions {
                    version: vec!["1.0".into()],
                },
                last_updated: Some("20240101000000".into()),
                snapshot: None,
            }),
        };
        let other = MavenMetadata {
            group_id: None,
            artifact_id: None,
            version: None,
            versioning: None,
        };

        let merged = merge_metadata(base, other);
        assert_eq!(merged.available_versions().len(), 1);
    }
}
