//! Maven POM (Project Object Model) parsing and manipulation.

use std::collections::HashMap;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::coord::ArtifactCoord;
use crate::error::Result;

// ============================================================================
// POM Model Structs
// ============================================================================

/// Maven POM (Project Object Model) representation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pom {
    #[serde(rename = "modelVersion", default)]
    pub model_version: Option<String>,
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
    #[serde(rename = "artifactId", default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub packaging: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub parent: Option<Parent>,
    #[serde(default)]
    pub properties: Properties,
    #[serde(rename = "dependencyManagement", default)]
    pub dependency_management: Option<DependencyManagement>,
    #[serde(default)]
    pub dependencies: Dependencies,
    #[serde(default)]
    pub repositories: Repositories,
    #[serde(rename = "distributionManagement", default)]
    pub distribution_management: Option<DistributionManagement>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Parent {
    #[serde(rename = "groupId")]
    pub group_id: String,
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub version: String,
    #[serde(rename = "relativePath", default)]
    pub relative_path: Option<String>,
}

/// A dependency declared in a POM.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PomDependency {
    #[serde(rename = "groupId")]
    pub group_id: String,
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "type", default)]
    pub dep_type: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub classifier: Option<String>,
    #[serde(default)]
    pub optional: Option<String>,
    #[serde(default)]
    pub exclusions: Exclusions,
}

/// A dependency exclusion in POM XML.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PomExclusion {
    #[serde(rename = "groupId")]
    pub group_id: String,
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
}

/// A repository declared in a POM.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PomRepository {
    #[serde(default)]
    pub id: Option<String>,
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub layout: Option<String>,
}

/// Distribution management section (for relocation support).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DistributionManagement {
    #[serde(default)]
    pub relocation: Option<Relocation>,
}

/// Artifact relocation information.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relocation {
    #[serde(rename = "groupId", default)]
    pub group_id: Option<String>,
    #[serde(rename = "artifactId", default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Dependency management section of a POM.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependencyManagement {
    #[serde(default)]
    pub dependencies: Dependencies,
}

// ============================================================================
// XML List Wrapper Types
// ============================================================================

/// Wrapper for `<dependencies><dependency>...</dependency></dependencies>`.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Dependencies {
    #[serde(rename = "dependency", default)]
    pub dependency: Vec<PomDependency>,
}

/// Wrapper for `<repositories><repository>...</repository></repositories>`.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Repositories {
    #[serde(rename = "repository", default)]
    pub repository: Vec<PomRepository>,
}

/// Wrapper for `<exclusions><exclusion>...</exclusion></exclusions>`.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Exclusions {
    #[serde(rename = "exclusion", default)]
    pub exclusion: Vec<PomExclusion>,
}

// ============================================================================
// Properties (custom deserialization for arbitrary XML elements)
// ============================================================================

/// Maven POM properties — arbitrary `<key>value</key>` entries.
#[derive(Default, Clone, Debug)]
pub struct Properties {
    pub entries: HashMap<String, String>,
}

impl Serialize for Properties {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (k, v) in &self.entries {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct PropertiesVisitor;

        impl<'de> Visitor<'de> for PropertiesVisitor {
            type Value = Properties;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map of XML property elements")
            }

            fn visit_map<M>(self, mut access: M) -> std::result::Result<Properties, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut entries = HashMap::new();
                while let Some((key, value)) = access.next_entry::<String, String>()? {
                    entries.insert(key, value);
                }
                Ok(Properties { entries })
            }
        }

        deserializer.deserialize_map(PropertiesVisitor)
    }
}

// ============================================================================
// Parsing
// ============================================================================

/// Parse a POM XML string into a [`Pom`] struct.
pub fn parse_pom(xml: &str) -> Result<Pom> {
    let cleaned = strip_xml_namespaces(xml);
    let pom: Pom = quick_xml::de::from_str(&cleaned)?;
    Ok(pom)
}

/// Remove `xmlns`, `xmlns:*`, and `xsi:*` attributes from XML.
pub(crate) fn strip_xml_namespaces(xml: &str) -> String {
    let mut result = xml.to_string();
    for prefix in &["xmlns:", "xmlns=", "xsi:"] {
        loop {
            let Some(start) = result.find(prefix) else {
                break;
            };
            let mut ws_start = start;
            while ws_start > 0 && result.as_bytes()[ws_start - 1] == b' ' {
                ws_start -= 1;
            }
            if let Some(end) = find_attribute_end(&result, start) {
                result.replace_range(ws_start..end, "");
            } else {
                break;
            }
        }
    }
    result
}

/// Find the end position (exclusive) of an XML attribute starting at `attr_start`.
pub(crate) fn find_attribute_end(s: &str, attr_start: usize) -> Option<usize> {
    let eq_pos = attr_start + s[attr_start..].find('=')?;
    let after_eq = eq_pos + 1;
    let ws_len = s[after_eq..].len() - s[after_eq..].trim_start().len();
    let quote_start = after_eq + ws_len;
    let quote_char = s[quote_start..].chars().next()?;
    if quote_char != '"' && quote_char != '\'' {
        return None;
    }
    let content_start = quote_start + 1;
    let close_offset = s[content_start..].find(quote_char)?;
    Some(content_start + close_offset + 1)
}

// ============================================================================
// Pom Helper Methods
// ============================================================================

impl Pom {
    /// Get the effective group_id (falls back to parent's).
    pub fn effective_group_id(&self) -> Option<&str> {
        self.group_id
            .as_deref()
            .or_else(|| self.parent.as_ref().map(|p| p.group_id.as_str()))
    }

    /// Get the effective version (falls back to parent's).
    pub fn effective_version(&self) -> Option<&str> {
        self.version
            .as_deref()
            .or_else(|| self.parent.as_ref().map(|p| p.version.as_str()))
    }

    /// Get packaging or default `"jar"`.
    pub fn effective_packaging(&self) -> &str {
        self.packaging.as_deref().unwrap_or("jar")
    }

    /// Convert to [`ArtifactCoord`] (requires group_id, artifact_id, version).
    pub fn to_coord(&self) -> Option<ArtifactCoord> {
        let group_id = self.effective_group_id()?;
        let artifact_id = self.artifact_id.as_deref()?;
        let version = self.effective_version()?;
        Some(ArtifactCoord::with_extension(
            group_id,
            artifact_id,
            version,
            self.effective_packaging(),
        ))
    }

    /// Get the parent as an [`ArtifactCoord`].
    pub fn parent_coord(&self) -> Option<ArtifactCoord> {
        let parent = self.parent.as_ref()?;
        Some(ArtifactCoord::with_extension(
            &parent.group_id,
            &parent.artifact_id,
            &parent.version,
            "pom",
        ))
    }
}

// ============================================================================
// Property Interpolation
// ============================================================================

/// Interpolate `${...}` placeholders in a string using the given properties.
///
/// If a property is not found, the placeholder is left as-is.
/// Handles recursive references with a depth limit.
pub fn interpolate(input: &str, properties: &HashMap<String, String>) -> String {
    const MAX_DEPTH: usize = 10;
    let mut result = input.to_string();

    for _ in 0..MAX_DEPTH {
        let mut new = String::with_capacity(result.len());
        let mut changed = false;
        let mut chars = result.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '$' && chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let mut key = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    key.push(c);
                }
                if closed {
                    if let Some(val) = properties.get(&key) {
                        new.push_str(val);
                        changed = true;
                    } else {
                        new.push_str("${");
                        new.push_str(&key);
                        new.push('}');
                    }
                } else {
                    new.push_str("${");
                    new.push_str(&key);
                }
            } else {
                new.push(ch);
            }
        }

        result = new;
        if !changed {
            break;
        }
    }

    result
}

/// Apply property interpolation to all string fields in the POM.
///
/// Automatically populates `project.groupId`, `project.artifactId`,
/// `project.version`, and `project.packaging` from the POM itself.
pub fn interpolate_pom(pom: &mut Pom) {
    let mut props = pom.properties.entries.clone();

    // Populate project.* properties
    if let Some(gid) = &pom.group_id {
        props.entry("project.groupId".into()).or_insert_with(|| gid.clone());
    } else if let Some(parent) = &pom.parent {
        props.entry("project.groupId".into()).or_insert_with(|| parent.group_id.clone());
    }
    if let Some(aid) = &pom.artifact_id {
        props.entry("project.artifactId".into()).or_insert_with(|| aid.clone());
    }
    if let Some(ver) = &pom.version {
        props.entry("project.version".into()).or_insert_with(|| ver.clone());
    } else if let Some(parent) = &pom.parent {
        props.entry("project.version".into()).or_insert_with(|| parent.version.clone());
    }
    props
        .entry("project.packaging".into())
        .or_insert_with(|| pom.effective_packaging().to_string());

    // Populate env.* properties from environment variables
    for (key, value) in std::env::vars() {
        props.entry(format!("env.{key}")).or_insert(value);
    }

    // Populate common built-in properties
    props.entry("java.version".into())
        .or_insert_with(|| std::env::var("JAVA_VERSION").unwrap_or_default());
    props.entry("os.name".into())
        .or_insert_with(|| std::env::consts::OS.to_string());
    props.entry("os.arch".into())
        .or_insert_with(|| std::env::consts::ARCH.to_string());

    // Interpolate property values themselves (handles cross-references)
    let keys: Vec<String> = props.keys().cloned().collect();
    for key in &keys {
        let val = props[key].clone();
        let new_val = interpolate(&val, &props);
        props.insert(key.clone(), new_val);
    }

    // Interpolate top-level POM fields
    if let Some(ref mut v) = pom.group_id {
        *v = interpolate(v, &props);
    }
    if let Some(ref mut v) = pom.artifact_id {
        *v = interpolate(v, &props);
    }
    if let Some(ref mut v) = pom.version {
        *v = interpolate(v, &props);
    }
    if let Some(ref mut v) = pom.name {
        *v = interpolate(v, &props);
    }
    if let Some(ref mut v) = pom.description {
        *v = interpolate(v, &props);
    }
    if let Some(ref mut v) = pom.url {
        *v = interpolate(v, &props);
    }

    // Write interpolated properties back to pom
    pom.properties.entries = props.clone();

    // Interpolate dependencies
    for dep in &mut pom.dependencies.dependency {
        interpolate_dep(dep, &props);
    }

    // Interpolate dependency management
    if let Some(ref mut dm) = pom.dependency_management {
        for dep in &mut dm.dependencies.dependency {
            interpolate_dep(dep, &props);
        }
    }
}

fn interpolate_dep(dep: &mut PomDependency, props: &HashMap<String, String>) {
    dep.group_id = interpolate(&dep.group_id, props);
    dep.artifact_id = interpolate(&dep.artifact_id, props);
    if let Some(ref mut v) = dep.version {
        *v = interpolate(v, props);
    }
    if let Some(ref mut v) = dep.scope {
        *v = interpolate(v, props);
    }
    if let Some(ref mut v) = dep.classifier {
        *v = interpolate(v, props);
    }
    if let Some(ref mut v) = dep.dep_type {
        *v = interpolate(v, props);
    }
}

// ============================================================================
// Parent POM Inheritance Merge
// ============================================================================

/// Merge parent POM fields into child POM.
///
/// Parent values are used as defaults — child values take priority.
/// Dependencies are NOT inherited (only dependencyManagement is).
pub fn merge_parent(child: &mut Pom, parent: &Pom) {
    if child.group_id.is_none() {
        child.group_id = parent.group_id.clone();
    }
    if child.version.is_none() {
        child.version = parent.version.clone();
    }

    // Properties: parent defaults, child overrides
    let mut merged = parent.properties.entries.clone();
    for (k, v) in &child.properties.entries {
        merged.insert(k.clone(), v.clone());
    }
    child.properties.entries = merged;

    // DependencyManagement: parent entries added; child overrides by (groupId, artifactId)
    if let Some(parent_dm) = &parent.dependency_management {
        let child_dm = child
            .dependency_management
            .get_or_insert_with(|| DependencyManagement {
                dependencies: Dependencies::default(),
            });

        let child_keys: std::collections::HashSet<(String, String)> = child_dm
            .dependencies
            .dependency
            .iter()
            .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
            .collect();

        for dep in &parent_dm.dependencies.dependency {
            let key = (dep.group_id.clone(), dep.artifact_id.clone());
            if !child_keys.contains(&key) {
                child_dm.dependencies.dependency.push(dep.clone());
            }
        }
    }

    // Repositories: parent's appended to child's
    child
        .repositories
        .repository
        .extend(parent.repositories.repository.clone());
}

// ============================================================================
// DependencyManagement Injection
// ============================================================================

/// Inject version/scope/exclusions from `dependencyManagement` into dependencies.
pub fn inject_dependency_management(pom: &mut Pom) {
    let managed: HashMap<(String, String), PomDependency> = match &pom.dependency_management {
        Some(dm) => dm
            .dependencies
            .dependency
            .iter()
            .map(|d| ((d.group_id.clone(), d.artifact_id.clone()), d.clone()))
            .collect(),
        None => return,
    };

    for dep in &mut pom.dependencies.dependency {
        let key = (dep.group_id.clone(), dep.artifact_id.clone());
        if let Some(managed_dep) = managed.get(&key) {
            if dep.version.is_none() {
                dep.version = managed_dep.version.clone();
            }
            if dep.scope.is_none() {
                dep.scope = managed_dep.scope.clone();
            }
            for exc in &managed_dep.exclusions.exclusion {
                let exists = dep
                    .exclusions
                    .exclusion
                    .iter()
                    .any(|e| e.group_id == exc.group_id && e.artifact_id == exc.artifact_id);
                if !exists {
                    dep.exclusions.exclusion.push(exc.clone());
                }
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_pom() {
        let xml = r#"
        <project>
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.example</groupId>
            <artifactId>my-app</artifactId>
            <version>1.0.0</version>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        assert_eq!(pom.model_version.as_deref(), Some("4.0.0"));
        assert_eq!(pom.group_id.as_deref(), Some("org.example"));
        assert_eq!(pom.artifact_id.as_deref(), Some("my-app"));
        assert_eq!(pom.version.as_deref(), Some("1.0.0"));
        assert_eq!(pom.effective_packaging(), "jar");
    }

    #[test]
    fn parse_pom_with_parent() {
        let xml = r#"
        <project>
            <parent>
                <groupId>org.example</groupId>
                <artifactId>parent-pom</artifactId>
                <version>2.0.0</version>
            </parent>
            <artifactId>child-app</artifactId>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        let parent = pom.parent.as_ref().unwrap();
        assert_eq!(parent.group_id, "org.example");
        assert_eq!(parent.artifact_id, "parent-pom");
        assert_eq!(parent.version, "2.0.0");
        assert_eq!(pom.artifact_id.as_deref(), Some("child-app"));
    }

    #[test]
    fn parse_pom_with_dependencies() {
        let xml = r#"
        <project>
            <groupId>org.example</groupId>
            <artifactId>my-app</artifactId>
            <version>1.0.0</version>
            <dependencies>
                <dependency>
                    <groupId>junit</groupId>
                    <artifactId>junit</artifactId>
                    <version>4.13.2</version>
                    <scope>test</scope>
                </dependency>
            </dependencies>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        assert_eq!(pom.dependencies.dependency.len(), 1);
        let dep = &pom.dependencies.dependency[0];
        assert_eq!(dep.group_id, "junit");
        assert_eq!(dep.artifact_id, "junit");
        assert_eq!(dep.version.as_deref(), Some("4.13.2"));
        assert_eq!(dep.scope.as_deref(), Some("test"));
    }

    #[test]
    fn parse_pom_with_dependency_management() {
        let xml = r#"
        <project>
            <groupId>org.example</groupId>
            <artifactId>parent</artifactId>
            <version>1.0.0</version>
            <dependencyManagement>
                <dependencies>
                    <dependency>
                        <groupId>org.springframework</groupId>
                        <artifactId>spring-core</artifactId>
                        <version>6.0.0</version>
                    </dependency>
                </dependencies>
            </dependencyManagement>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        let dm = pom.dependency_management.as_ref().unwrap();
        assert_eq!(dm.dependencies.dependency.len(), 1);
        assert_eq!(
            dm.dependencies.dependency[0].version.as_deref(),
            Some("6.0.0")
        );
    }

    #[test]
    fn parse_pom_with_repositories() {
        let xml = r#"
        <project>
            <groupId>org.example</groupId>
            <artifactId>my-app</artifactId>
            <version>1.0.0</version>
            <repositories>
                <repository>
                    <id>central</id>
                    <url>https://repo.maven.apache.org/maven2</url>
                </repository>
            </repositories>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        assert_eq!(pom.repositories.repository.len(), 1);
        assert_eq!(
            pom.repositories.repository[0].id.as_deref(),
            Some("central")
        );
    }

    #[test]
    fn effective_group_id_inheritance() {
        let pom = parse_pom(
            r#"
            <project>
                <parent>
                    <groupId>org.parent</groupId>
                    <artifactId>parent</artifactId>
                    <version>1.0</version>
                </parent>
                <artifactId>child</artifactId>
            </project>
            "#,
        )
        .unwrap();
        assert_eq!(pom.effective_group_id(), Some("org.parent"));
        assert_eq!(pom.effective_version(), Some("1.0"));
    }

    #[test]
    fn to_coord_basic() {
        let pom = parse_pom(
            r#"
            <project>
                <groupId>org.example</groupId>
                <artifactId>my-app</artifactId>
                <version>1.0.0</version>
            </project>
            "#,
        )
        .unwrap();
        let coord = pom.to_coord().unwrap();
        assert_eq!(coord.group_id, "org.example");
        assert_eq!(coord.artifact_id, "my-app");
        assert_eq!(coord.version, "1.0.0");
        assert_eq!(coord.extension, "jar");
    }

    #[test]
    fn parent_coord_extraction() {
        let pom = parse_pom(
            r#"
            <project>
                <parent>
                    <groupId>org.example</groupId>
                    <artifactId>parent-pom</artifactId>
                    <version>2.0.0</version>
                </parent>
                <artifactId>child</artifactId>
            </project>
            "#,
        )
        .unwrap();
        let coord = pom.parent_coord().unwrap();
        assert_eq!(coord.group_id, "org.example");
        assert_eq!(coord.artifact_id, "parent-pom");
        assert_eq!(coord.version, "2.0.0");
        assert_eq!(coord.extension, "pom");
    }

    #[test]
    fn interpolation_custom_property() {
        let xml = r#"
        <project>
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.example</groupId>
            <artifactId>my-app</artifactId>
            <version>1.0.0</version>
            <properties>
                <spring.version>6.0.0</spring.version>
            </properties>
            <dependencies>
                <dependency>
                    <groupId>org.springframework</groupId>
                    <artifactId>spring-core</artifactId>
                    <version>${spring.version}</version>
                </dependency>
            </dependencies>
        </project>
        "#;
        let mut pom = parse_pom(xml).unwrap();
        interpolate_pom(&mut pom);
        assert_eq!(
            pom.dependencies.dependency[0].version.as_deref(),
            Some("6.0.0")
        );
    }

    #[test]
    fn interpolation_project_version() {
        let mut props = HashMap::new();
        props.insert("project.version".into(), "2.0.0".into());
        assert_eq!(interpolate("${project.version}", &props), "2.0.0");
    }

    #[test]
    fn interpolation_missing_property() {
        let props = HashMap::new();
        assert_eq!(interpolate("${unknown}", &props), "${unknown}");
    }

    #[test]
    fn interpolation_recursive() {
        let mut props = HashMap::new();
        props.insert("base".into(), "1.0".into());
        props.insert("ver".into(), "${base}.RELEASE".into());
        assert_eq!(interpolate("${ver}", &props), "1.0.RELEASE");
    }

    #[test]
    fn merge_parent_basic() {
        let parent = parse_pom(
            r#"
            <project>
                <groupId>org.parent</groupId>
                <artifactId>parent</artifactId>
                <version>1.0.0</version>
                <properties>
                    <java.version>17</java.version>
                    <encoding>UTF-8</encoding>
                </properties>
                <dependencyManagement>
                    <dependencies>
                        <dependency>
                            <groupId>org.springframework</groupId>
                            <artifactId>spring-core</artifactId>
                            <version>6.0.0</version>
                        </dependency>
                    </dependencies>
                </dependencyManagement>
            </project>
            "#,
        )
        .unwrap();

        let mut child = parse_pom(
            r#"
            <project>
                <artifactId>child</artifactId>
                <properties>
                    <encoding>ASCII</encoding>
                </properties>
            </project>
            "#,
        )
        .unwrap();

        merge_parent(&mut child, &parent);

        assert_eq!(child.group_id.as_deref(), Some("org.parent"));
        assert_eq!(child.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            child.properties.entries.get("java.version").map(String::as_str),
            Some("17")
        );
        // Child override takes priority
        assert_eq!(
            child.properties.entries.get("encoding").map(String::as_str),
            Some("ASCII")
        );
        assert!(child.dependency_management.is_some());
        assert_eq!(
            child
                .dependency_management
                .unwrap()
                .dependencies
                .dependency
                .len(),
            1
        );
    }

    #[test]
    fn inject_dependency_management_basic() {
        let mut pom = parse_pom(
            r#"
            <project>
                <groupId>org.example</groupId>
                <artifactId>my-app</artifactId>
                <version>1.0.0</version>
                <dependencyManagement>
                    <dependencies>
                        <dependency>
                            <groupId>org.springframework</groupId>
                            <artifactId>spring-core</artifactId>
                            <version>6.0.0</version>
                            <scope>compile</scope>
                        </dependency>
                    </dependencies>
                </dependencyManagement>
                <dependencies>
                    <dependency>
                        <groupId>org.springframework</groupId>
                        <artifactId>spring-core</artifactId>
                    </dependency>
                </dependencies>
            </project>
            "#,
        )
        .unwrap();

        inject_dependency_management(&mut pom);

        let dep = &pom.dependencies.dependency[0];
        assert_eq!(dep.version.as_deref(), Some("6.0.0"));
        assert_eq!(dep.scope.as_deref(), Some("compile"));
    }

    #[test]
    fn parse_pom_with_namespaces() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <project xmlns="http://maven.apache.org/POM/4.0.0"
                 xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                 xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd">
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.example</groupId>
            <artifactId>ns-test</artifactId>
            <version>1.0.0</version>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        assert_eq!(pom.group_id.as_deref(), Some("org.example"));
        assert_eq!(pom.artifact_id.as_deref(), Some("ns-test"));
    }

    // ===================================================================
    // Edge-case / comprehensive tests
    // ===================================================================

    #[test]
    fn parse_malformed_xml() {
        let result = parse_pom("<not valid xml");
        assert!(result.is_err(), "expected error for malformed XML");
    }

    #[test]
    fn parse_empty_project() {
        let pom = parse_pom("<project></project>").unwrap();
        assert!(pom.group_id.is_none());
        assert!(pom.artifact_id.is_none());
        assert!(pom.version.is_none());
        assert!(pom.packaging.is_none());
        assert!(pom.parent.is_none());
        assert!(pom.dependency_management.is_none());
        assert!(pom.dependencies.dependency.is_empty());
        assert!(pom.repositories.repository.is_empty());
    }

    #[test]
    fn to_coord_missing_fields() {
        // artifact_id is missing → to_coord should return None
        let pom = parse_pom(
            r#"<project>
                <groupId>org.example</groupId>
                <version>1.0.0</version>
            </project>"#,
        )
        .unwrap();
        assert!(pom.to_coord().is_none());
    }

    #[test]
    fn parent_coord_no_parent() {
        let pom = parse_pom(
            r#"<project>
                <groupId>org.example</groupId>
                <artifactId>app</artifactId>
                <version>1.0.0</version>
            </project>"#,
        )
        .unwrap();
        assert!(pom.parent_coord().is_none());
    }

    #[test]
    fn interpolation_circular_reference() {
        let mut props = HashMap::new();
        props.insert("a".into(), "${b}".into());
        props.insert("b".into(), "${a}".into());
        // Should terminate (depth limit) without infinite loop
        let result = interpolate("${a}", &props);
        // After depth limit, the circular ref will remain unresolved or partially resolved
        assert!(!result.is_empty());
    }

    #[test]
    fn interpolation_deeply_nested() {
        let mut props = HashMap::new();
        // Build a chain: p0 -> p1 -> p2 -> ... -> p8 -> "final_value"
        props.insert("p8".into(), "final_value".into());
        props.insert("p7".into(), "${p8}".into());
        props.insert("p6".into(), "${p7}".into());
        props.insert("p5".into(), "${p6}".into());
        props.insert("p4".into(), "${p5}".into());
        props.insert("p3".into(), "${p4}".into());
        props.insert("p2".into(), "${p3}".into());
        props.insert("p1".into(), "${p2}".into());
        props.insert("p0".into(), "${p1}".into());
        let result = interpolate("${p0}", &props);
        assert_eq!(result, "final_value");
    }

    #[test]
    fn interpolation_no_properties() {
        let props = HashMap::new();
        assert_eq!(interpolate("no placeholders here", &props), "no placeholders here");
        assert_eq!(interpolate("${missing}", &props), "${missing}");
    }

    #[test]
    fn inject_depmgmt_empty() {
        let mut pom = parse_pom(
            r#"<project>
                <groupId>org.example</groupId>
                <artifactId>app</artifactId>
                <version>1.0.0</version>
                <dependencies>
                    <dependency>
                        <groupId>junit</groupId>
                        <artifactId>junit</artifactId>
                        <version>4.13</version>
                    </dependency>
                </dependencies>
            </project>"#,
        )
        .unwrap();
        assert!(pom.dependency_management.is_none());
        // Should be a no-op
        inject_dependency_management(&mut pom);
        assert_eq!(pom.dependencies.dependency[0].version.as_deref(), Some("4.13"));
    }

    #[test]
    fn inject_depmgmt_no_match() {
        let mut pom = parse_pom(
            r#"<project>
                <groupId>org.example</groupId>
                <artifactId>app</artifactId>
                <version>1.0.0</version>
                <dependencyManagement>
                    <dependencies>
                        <dependency>
                            <groupId>org.unrelated</groupId>
                            <artifactId>unrelated-lib</artifactId>
                            <version>9.9.9</version>
                        </dependency>
                    </dependencies>
                </dependencyManagement>
                <dependencies>
                    <dependency>
                        <groupId>junit</groupId>
                        <artifactId>junit</artifactId>
                        <version>4.13</version>
                    </dependency>
                </dependencies>
            </project>"#,
        )
        .unwrap();
        inject_dependency_management(&mut pom);
        // junit dep should be unchanged
        let dep = &pom.dependencies.dependency[0];
        assert_eq!(dep.version.as_deref(), Some("4.13"));
        assert!(dep.scope.is_none());
    }

    #[test]
    fn merge_parent_repositories() {
        let parent = parse_pom(
            r#"<project>
                <groupId>org.parent</groupId>
                <artifactId>parent</artifactId>
                <version>1.0.0</version>
                <repositories>
                    <repository>
                        <id>parent-repo</id>
                        <url>https://parent.example.com/repo</url>
                    </repository>
                </repositories>
            </project>"#,
        )
        .unwrap();

        let mut child = parse_pom(
            r#"<project>
                <artifactId>child</artifactId>
                <repositories>
                    <repository>
                        <id>child-repo</id>
                        <url>https://child.example.com/repo</url>
                    </repository>
                </repositories>
            </project>"#,
        )
        .unwrap();

        merge_parent(&mut child, &parent);

        assert_eq!(child.repositories.repository.len(), 2);
        assert_eq!(child.repositories.repository[0].id.as_deref(), Some("child-repo"));
        assert_eq!(child.repositories.repository[1].id.as_deref(), Some("parent-repo"));
    }

    #[test]
    fn merge_parent_dep_management() {
        let parent = parse_pom(
            r#"<project>
                <groupId>org.parent</groupId>
                <artifactId>parent</artifactId>
                <version>1.0.0</version>
                <dependencyManagement>
                    <dependencies>
                        <dependency>
                            <groupId>org.lib</groupId>
                            <artifactId>lib-a</artifactId>
                            <version>1.0</version>
                        </dependency>
                        <dependency>
                            <groupId>org.lib</groupId>
                            <artifactId>lib-b</artifactId>
                            <version>2.0</version>
                        </dependency>
                    </dependencies>
                </dependencyManagement>
            </project>"#,
        )
        .unwrap();

        let mut child = parse_pom(
            r#"<project>
                <artifactId>child</artifactId>
                <dependencyManagement>
                    <dependencies>
                        <dependency>
                            <groupId>org.lib</groupId>
                            <artifactId>lib-a</artifactId>
                            <version>1.1-child</version>
                        </dependency>
                    </dependencies>
                </dependencyManagement>
            </project>"#,
        )
        .unwrap();

        merge_parent(&mut child, &parent);

        let dm = child.dependency_management.as_ref().unwrap();
        // lib-a should be the child's version (override), lib-b inherited from parent
        assert_eq!(dm.dependencies.dependency.len(), 2);
        let lib_a = dm.dependencies.dependency.iter().find(|d| d.artifact_id == "lib-a").unwrap();
        assert_eq!(lib_a.version.as_deref(), Some("1.1-child"));
        let lib_b = dm.dependencies.dependency.iter().find(|d| d.artifact_id == "lib-b").unwrap();
        assert_eq!(lib_b.version.as_deref(), Some("2.0"));
    }

    #[test]
    fn pom_with_all_dep_fields() {
        let xml = r#"
        <project>
            <groupId>org.example</groupId>
            <artifactId>app</artifactId>
            <version>1.0.0</version>
            <dependencies>
                <dependency>
                    <groupId>org.example</groupId>
                    <artifactId>full-dep</artifactId>
                    <version>2.0.0</version>
                    <type>war</type>
                    <classifier>sources</classifier>
                    <scope>provided</scope>
                    <optional>true</optional>
                    <exclusions>
                        <exclusion>
                            <groupId>org.unwanted</groupId>
                            <artifactId>unwanted-lib</artifactId>
                        </exclusion>
                    </exclusions>
                </dependency>
            </dependencies>
        </project>
        "#;
        let pom = parse_pom(xml).unwrap();
        let dep = &pom.dependencies.dependency[0];
        assert_eq!(dep.group_id, "org.example");
        assert_eq!(dep.artifact_id, "full-dep");
        assert_eq!(dep.version.as_deref(), Some("2.0.0"));
        assert_eq!(dep.dep_type.as_deref(), Some("war"));
        assert_eq!(dep.classifier.as_deref(), Some("sources"));
        assert_eq!(dep.scope.as_deref(), Some("provided"));
        assert_eq!(dep.optional.as_deref(), Some("true"));
        assert_eq!(dep.exclusions.exclusion.len(), 1);
        assert_eq!(dep.exclusions.exclusion[0].group_id, "org.unwanted");
        assert_eq!(dep.exclusions.exclusion[0].artifact_id, "unwanted-lib");
    }

    #[test]
    fn effective_packaging_default() {
        let pom = parse_pom(
            r#"<project>
                <groupId>org.example</groupId>
                <artifactId>app</artifactId>
                <version>1.0.0</version>
            </project>"#,
        )
        .unwrap();
        assert_eq!(pom.effective_packaging(), "jar");
    }

    #[test]
    fn effective_packaging_war() {
        let pom = parse_pom(
            r#"<project>
                <groupId>org.example</groupId>
                <artifactId>app</artifactId>
                <version>1.0.0</version>
                <packaging>war</packaging>
            </project>"#,
        )
        .unwrap();
        assert_eq!(pom.effective_packaging(), "war");
    }

    #[test]
    fn parse_pom_with_relocation() {
        let xml = r#"
        <project>
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.old</groupId>
            <artifactId>old-lib</artifactId>
            <version>1.0</version>
            <distributionManagement>
                <relocation>
                    <groupId>org.new</groupId>
                    <artifactId>new-lib</artifactId>
                    <version>2.0</version>
                    <message>Moved to org.new</message>
                </relocation>
            </distributionManagement>
        </project>"#;

        let pom = parse_pom(xml).unwrap();
        let dm = pom.distribution_management.unwrap();
        let reloc = dm.relocation.unwrap();
        assert_eq!(reloc.group_id.as_deref(), Some("org.new"));
        assert_eq!(reloc.artifact_id.as_deref(), Some("new-lib"));
        assert_eq!(reloc.version.as_deref(), Some("2.0"));
        assert_eq!(reloc.message.as_deref(), Some("Moved to org.new"));
    }

    #[test]
    fn interpolate_env_properties() {
        // Set a test env var
        std::env::set_var("MVN_RS_TEST_PROP", "test_value");
        let xml = r#"
        <project>
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.example</groupId>
            <artifactId>app</artifactId>
            <version>1.0</version>
            <properties>
                <my.os>${os.name}</my.os>
            </properties>
        </project>"#;

        let mut pom = parse_pom(xml).unwrap();
        interpolate_pom(&mut pom);
        // os.name should be populated
        let os_prop = pom.properties.entries.get("my.os").unwrap();
        assert!(!os_prop.contains("${"), "os.name should be interpolated, got: {os_prop}");
        std::env::remove_var("MVN_RS_TEST_PROP");
    }
}
