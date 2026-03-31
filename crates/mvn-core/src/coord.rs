use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ArtifactCoord
// ---------------------------------------------------------------------------

/// Maven artifact coordinates (GAV + optional classifier/extension).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactCoord {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub classifier: Option<String>,
    #[serde(default = "default_extension")]
    pub extension: String,
}

fn default_extension() -> String {
    "jar".to_string()
}

impl ArtifactCoord {
    /// Create coordinates with default extension "jar" and no classifier.
    pub fn new(
        group_id: impl Into<String>,
        artifact_id: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            group_id: group_id.into(),
            artifact_id: artifact_id.into(),
            version: version.into(),
            classifier: None,
            extension: "jar".to_string(),
        }
    }

    /// Create coordinates with a custom extension and no classifier.
    pub fn with_extension(
        group_id: impl Into<String>,
        artifact_id: impl Into<String>,
        version: impl Into<String>,
        extension: impl Into<String>,
    ) -> Self {
        Self {
            group_id: group_id.into(),
            artifact_id: artifact_id.into(),
            version: version.into(),
            classifier: None,
            extension: extension.into(),
        }
    }

    /// Create coordinates with both classifier and extension.
    pub fn with_classifier(
        group_id: impl Into<String>,
        artifact_id: impl Into<String>,
        version: impl Into<String>,
        classifier: impl Into<String>,
        extension: impl Into<String>,
    ) -> Self {
        Self {
            group_id: group_id.into(),
            artifact_id: artifact_id.into(),
            version: version.into(),
            classifier: Some(classifier.into()),
            extension: extension.into(),
        }
    }

    /// Compute the repository-relative path for this artifact.
    ///
    /// Example: `org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.jar`
    pub fn repository_path(&self) -> String {
        let group_path = self.group_id.replace('.', "/");
        let classifier_part = match &self.classifier {
            Some(c) => format!("-{c}"),
            None => String::new(),
        };
        format!(
            "{}/{}/{}/{}-{}{}{}",
            group_path,
            self.artifact_id,
            self.version,
            self.artifact_id,
            self.version,
            classifier_part,
            format_args!(".{}", self.extension),
        )
    }

    /// Compute the repository-relative path for the POM of this artifact.
    pub fn pom_path(&self) -> String {
        let group_path = self.group_id.replace('.', "/");
        format!(
            "{}/{}/{}/{}-{}.pom",
            group_path, self.artifact_id, self.version, self.artifact_id, self.version,
        )
    }
}

// -- FromStr ----------------------------------------------------------------

impl FromStr for ArtifactCoord {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            // groupId:artifactId:version
            3 => Ok(Self::new(parts[0], parts[1], parts[2])),
            // groupId:artifactId:extension:version
            4 => Ok(Self::with_extension(parts[0], parts[1], parts[3], parts[2])),
            // groupId:artifactId:extension:classifier:version
            5 => Ok(Self::with_classifier(
                parts[0], parts[1], parts[4], parts[3], parts[2],
            )),
            _ => Err(format!(
                "invalid artifact coordinates '{}': expected 3, 4, or 5 colon-separated parts",
                s
            )),
        }
    }
}

// -- Display ----------------------------------------------------------------

impl fmt::Display for ArtifactCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(classifier) = &self.classifier {
            write!(
                f,
                "{}:{}:{}:{}:{}",
                self.group_id, self.artifact_id, self.extension, classifier, self.version,
            )
        } else if self.extension == "jar" {
            write!(f, "{}:{}:{}", self.group_id, self.artifact_id, self.version)
        } else {
            write!(
                f,
                "{}:{}:{}:{}",
                self.group_id, self.artifact_id, self.extension, self.version,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Exclusion
// ---------------------------------------------------------------------------

/// A dependency exclusion pattern (supports `"*"` wildcards).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Exclusion {
    pub group_id: String,
    pub artifact_id: String,
}

impl Exclusion {
    pub fn new(group_id: impl Into<String>, artifact_id: impl Into<String>) -> Self {
        Self {
            group_id: group_id.into(),
            artifact_id: artifact_id.into(),
        }
    }

    /// Returns `true` if this exclusion matches the given coordinate.
    /// A field value of `"*"` matches any value.
    pub fn matches(&self, coord: &ArtifactCoord) -> bool {
        let group_match = self.group_id == "*" || self.group_id == coord.group_id;
        let artifact_match = self.artifact_id == "*" || self.artifact_id == coord.artifact_id;
        group_match && artifact_match
    }
}

// ---------------------------------------------------------------------------
// DependencyScope
// ---------------------------------------------------------------------------

/// Maven dependency scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyScope {
    Compile,
    Runtime,
    Test,
    Provided,
    System,
    Import,
}

impl Default for DependencyScope {
    fn default() -> Self {
        Self::Compile
    }
}

impl DependencyScope {
    /// Only `Compile` and `Runtime` scopes are transitive.
    pub fn is_transitive(&self) -> bool {
        matches!(self, Self::Compile | Self::Runtime)
    }
}

impl FromStr for DependencyScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "compile" => Ok(Self::Compile),
            "runtime" => Ok(Self::Runtime),
            "test" => Ok(Self::Test),
            "provided" => Ok(Self::Provided),
            "system" => Ok(Self::System),
            "import" => Ok(Self::Import),
            _ => Err(format!("unknown dependency scope '{}'", s)),
        }
    }
}

impl fmt::Display for DependencyScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Compile => "compile",
            Self::Runtime => "runtime",
            Self::Test => "test",
            Self::Provided => "provided",
            Self::System => "system",
            Self::Import => "import",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gav() {
        let coord: ArtifactCoord = "org.apache.commons:commons-lang3:3.12.0".parse().unwrap();
        assert_eq!(coord.group_id, "org.apache.commons");
        assert_eq!(coord.artifact_id, "commons-lang3");
        assert_eq!(coord.version, "3.12.0");
        assert_eq!(coord.classifier, None);
        assert_eq!(coord.extension, "jar");
    }

    #[test]
    fn parse_gave() {
        let coord: ArtifactCoord =
            "org.apache.commons:commons-lang3:pom:3.12.0".parse().unwrap();
        assert_eq!(coord.group_id, "org.apache.commons");
        assert_eq!(coord.artifact_id, "commons-lang3");
        assert_eq!(coord.version, "3.12.0");
        assert_eq!(coord.classifier, None);
        assert_eq!(coord.extension, "pom");
    }

    #[test]
    fn parse_gavce() {
        let coord: ArtifactCoord = "org.apache.commons:commons-lang3:jar:sources:3.12.0"
            .parse()
            .unwrap();
        assert_eq!(coord.group_id, "org.apache.commons");
        assert_eq!(coord.artifact_id, "commons-lang3");
        assert_eq!(coord.version, "3.12.0");
        assert_eq!(coord.classifier, Some("sources".to_string()));
        assert_eq!(coord.extension, "jar");
    }

    #[test]
    fn parse_invalid_one_part() {
        assert!("just-one".parse::<ArtifactCoord>().is_err());
    }

    #[test]
    fn parse_invalid_six_parts() {
        assert!("a:b:c:d:e:f".parse::<ArtifactCoord>().is_err());
    }

    #[test]
    fn display_gav() {
        let coord = ArtifactCoord::new("org.example", "foo", "1.0");
        assert_eq!(coord.to_string(), "org.example:foo:1.0");
    }

    #[test]
    fn display_gave() {
        let coord = ArtifactCoord::with_extension("org.example", "foo", "1.0", "pom");
        assert_eq!(coord.to_string(), "org.example:foo:pom:1.0");
    }

    #[test]
    fn display_gavce() {
        let coord = ArtifactCoord::with_classifier("org.example", "foo", "1.0", "sources", "jar");
        assert_eq!(coord.to_string(), "org.example:foo:jar:sources:1.0");
    }

    #[test]
    fn display_roundtrip_gav() {
        let input = "org.apache.commons:commons-lang3:3.12.0";
        let coord: ArtifactCoord = input.parse().unwrap();
        assert_eq!(coord.to_string(), input);
    }

    #[test]
    fn display_roundtrip_gave() {
        let input = "org.apache.commons:commons-lang3:pom:3.12.0";
        let coord: ArtifactCoord = input.parse().unwrap();
        assert_eq!(coord.to_string(), input);
    }

    #[test]
    fn display_roundtrip_gavce() {
        let input = "org.apache.commons:commons-lang3:jar:sources:3.12.0";
        let coord: ArtifactCoord = input.parse().unwrap();
        assert_eq!(coord.to_string(), input);
    }

    #[test]
    fn repository_path_simple() {
        let coord = ArtifactCoord::new("org.apache.commons", "commons-lang3", "3.12.0");
        assert_eq!(
            coord.repository_path(),
            "org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.jar"
        );
    }

    #[test]
    fn repository_path_with_classifier() {
        let coord = ArtifactCoord::with_classifier(
            "org.apache.commons",
            "commons-lang3",
            "3.12.0",
            "sources",
            "jar",
        );
        assert_eq!(
            coord.repository_path(),
            "org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0-sources.jar"
        );
    }

    #[test]
    fn pom_path_computation() {
        let coord = ArtifactCoord::with_classifier(
            "org.apache.commons",
            "commons-lang3",
            "3.12.0",
            "sources",
            "jar",
        );
        assert_eq!(
            coord.pom_path(),
            "org/apache/commons/commons-lang3/3.12.0/commons-lang3-3.12.0.pom"
        );
    }

    #[test]
    fn exclusion_exact_match() {
        let exc = Exclusion::new("org.example", "foo");
        let coord = ArtifactCoord::new("org.example", "foo", "1.0");
        assert!(exc.matches(&coord));
    }

    #[test]
    fn exclusion_no_match() {
        let exc = Exclusion::new("org.example", "foo");
        let coord = ArtifactCoord::new("org.example", "bar", "1.0");
        assert!(!exc.matches(&coord));
    }

    #[test]
    fn exclusion_wildcard_group() {
        let exc = Exclusion::new("*", "foo");
        let coord = ArtifactCoord::new("com.anything", "foo", "2.0");
        assert!(exc.matches(&coord));
    }

    #[test]
    fn exclusion_wildcard_artifact() {
        let exc = Exclusion::new("org.example", "*");
        let coord = ArtifactCoord::new("org.example", "anything", "1.0");
        assert!(exc.matches(&coord));
    }

    #[test]
    fn exclusion_wildcard_both() {
        let exc = Exclusion::new("*", "*");
        let coord = ArtifactCoord::new("com.any", "thing", "0.1");
        assert!(exc.matches(&coord));
    }

    #[test]
    fn scope_parse_and_display() {
        for s in &["compile", "runtime", "test", "provided", "system", "import"] {
            let scope: DependencyScope = s.parse().unwrap();
            assert_eq!(&scope.to_string(), *s);
        }
    }

    #[test]
    fn scope_default_is_compile() {
        assert_eq!(DependencyScope::default(), DependencyScope::Compile);
    }

    #[test]
    fn scope_is_transitive() {
        assert!(DependencyScope::Compile.is_transitive());
        assert!(DependencyScope::Runtime.is_transitive());
        assert!(!DependencyScope::Test.is_transitive());
        assert!(!DependencyScope::Provided.is_transitive());
        assert!(!DependencyScope::System.is_transitive());
        assert!(!DependencyScope::Import.is_transitive());
    }

    #[test]
    fn scope_parse_invalid() {
        assert!("bogus".parse::<DependencyScope>().is_err());
    }
}
