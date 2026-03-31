//! Dependency resolution engine.
//!
//! Implements Maven-style dependency resolution in three phases:
//! 1. **Collect** – recursively fetch POMs and build a dependency tree
//! 2. **Flatten** – BFS "nearest wins" conflict resolution
//! 3. **Download** – fetch resolved artifact files

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::coord::{ArtifactCoord, DependencyScope, Exclusion};
use crate::downloader::ArtifactDownloader;
use crate::error::Result;
use crate::pom;
use crate::version::{Version, VersionConstraint};

/// Maximum recursion depth for transitive dependency collection.
const MAX_DEPTH: usize = 50;

/// Maximum parent POM chain depth to prevent infinite loops.
const MAX_PARENT_DEPTH: usize = 20;

// ============================================================================
// Data Structures
// ============================================================================

/// A node in the dependency tree.
#[derive(Clone, Debug)]
pub struct DependencyNode {
    pub coord: ArtifactCoord,
    pub scope: DependencyScope,
    pub optional: bool,
    pub exclusions: Vec<Exclusion>,
    pub children: Vec<DependencyNode>,
}

/// A flattened resolved dependency with its effective scope.
#[derive(Clone, Debug)]
pub struct ResolvedDependency {
    pub coord: ArtifactCoord,
    pub scope: DependencyScope,
    pub path: Option<PathBuf>,
}

/// Result of dependency resolution.
#[derive(Clone, Debug)]
pub struct ResolutionResult {
    pub root: ArtifactCoord,
    pub tree: Vec<DependencyNode>,
    pub dependencies: Vec<ResolvedDependency>,
}

// ============================================================================
// Scope Propagation & Filtering
// ============================================================================

/// Compute the effective scope of a transitive dependency.
///
/// Returns `None` if the transitive dependency should be excluded.
///
/// ```text
/// Parent \ Child | compile  | runtime  | provided | test
/// ---------------|----------|----------|----------|------
/// compile        | compile  | runtime  | –        | –
/// runtime        | runtime  | runtime  | –        | –
/// provided       | provided | provided | –        | –
/// test           | test     | test     | –        | –
/// ```
pub fn propagate_scope(
    parent_scope: DependencyScope,
    child_scope: DependencyScope,
) -> Option<DependencyScope> {
    use DependencyScope::*;
    match (parent_scope, child_scope) {
        // provided, test, system, import children are never transitive
        (_, Provided | Test | System | Import) => None,

        (Compile, Compile) => Some(Compile),
        (Compile, Runtime) => Some(Runtime),
        (Runtime, Compile | Runtime) => Some(Runtime),
        (Provided, Compile | Runtime) => Some(Provided),
        (Test, Compile | Runtime) => Some(Test),

        // system / import parents don't propagate
        (System | Import, _) => None,
    }
}

/// Check whether a dependency scope should be included given a filter scope.
///
/// - `Compile`  → compile, provided
/// - `Runtime`  → compile, runtime
/// - `Test`     → compile, runtime, test
/// - other      → exact match only
pub fn scope_matches(dep_scope: DependencyScope, filter: DependencyScope) -> bool {
    use DependencyScope::*;
    match filter {
        Compile => matches!(dep_scope, Compile | Provided),
        Runtime => matches!(dep_scope, Compile | Runtime),
        Test => matches!(dep_scope, Compile | Runtime | Test),
        other => dep_scope == other,
    }
}

// ============================================================================
// Tree Display
// ============================================================================

use colored::Colorize;

/// Colorize a scope tag string based on the dependency scope.
fn colored_scope(scope: DependencyScope) -> String {
    let label = format!("[{}]", scope);
    match scope {
        DependencyScope::Compile => label.green().to_string(),
        DependencyScope::Runtime => label.yellow().to_string(),
        DependencyScope::Test => label.cyan().to_string(),
        DependencyScope::Provided => label.magenta().to_string(),
        DependencyScope::System => label.red().to_string(),
        DependencyScope::Import => label.dimmed().to_string(),
    }
}

impl DependencyNode {
    /// Format this node (and children recursively) as a tree string (plain, no colors).
    pub fn display_tree_plain(&self, prefix: &str, is_last: bool) -> String {
        let mut out = String::new();
        let connector = if is_last { "└── " } else { "├── " };
        out.push_str(prefix);
        out.push_str(connector);
        out.push_str(&format!("{} [{}]\n", self.coord, self.scope));

        let child_prefix = format!(
            "{}{}",
            prefix,
            if is_last { "    " } else { "│   " }
        );
        let len = self.children.len();
        for (i, child) in self.children.iter().enumerate() {
            out.push_str(&child.display_tree_plain(&child_prefix, i + 1 == len));
        }
        out
    }

    /// Format this node (and children recursively) as a colored tree string.
    pub fn display_tree(&self, prefix: &str, is_last: bool) -> String {
        let mut out = String::new();
        let connector = if is_last { "└── " } else { "├── " };
        out.push_str(&prefix.dimmed().to_string());
        out.push_str(&connector.dimmed().to_string());

        let coord_str = self.coord.to_string();
        let scope_str = colored_scope(self.scope);
        if self.optional {
            out.push_str(&format!(
                "{} {} {}\n",
                coord_str.dimmed(),
                scope_str,
                "(optional)".dimmed()
            ));
        } else {
            out.push_str(&format!("{} {}\n", coord_str, scope_str));
        }

        let child_prefix = format!(
            "{}{}",
            prefix,
            if is_last { "    " } else { "│   " }
        );
        let len = self.children.len();
        for (i, child) in self.children.iter().enumerate() {
            out.push_str(&child.display_tree(&child_prefix, i + 1 == len));
        }
        out
    }
}

/// Format the full dependency tree for CLI display (colored).
pub fn format_tree(root: &ArtifactCoord, nodes: &[DependencyNode]) -> String {
    let mut out = format!("{}\n", root.to_string().bold());
    let len = nodes.len();
    for (i, node) in nodes.iter().enumerate() {
        out.push_str(&node.display_tree("", i + 1 == len));
    }
    out
}

/// Format the full dependency tree for CLI display (plain, no colors).
pub fn format_tree_plain(root: &ArtifactCoord, nodes: &[DependencyNode]) -> String {
    let mut out = format!("{root}\n");
    let len = nodes.len();
    for (i, node) in nodes.iter().enumerate() {
        out.push_str(&node.display_tree_plain("", i + 1 == len));
    }
    out
}

// ============================================================================
// DependencyResolver
// ============================================================================

/// Resolves Maven dependencies using a three-phase algorithm.
///
/// Faithfully replicates Maven's core resolution logic:
/// - Recursive parent POM chain resolution (up to `MAX_PARENT_DEPTH`)
/// - BOM (`<scope>import</scope>`) resolution with circular import detection
/// - Root `<dependencyManagement>` propagation to ALL transitive dependencies
/// - Version range resolution via `maven-metadata.xml`
/// - POM relocation following with cycle detection
/// - Scope propagation matrix & "Nearest Wins" conflict resolution
pub struct DependencyResolver<'a> {
    downloader: &'a ArtifactDownloader,
    /// Cache for resolved effective POMs to avoid re-fetching the same parent
    /// chain multiple times during transitive resolution.
    pom_cache: Arc<RwLock<HashMap<String, pom::Pom>>>,
}

impl<'a> DependencyResolver<'a> {
    pub fn new(downloader: &'a ArtifactDownloader) -> Self {
        Self {
            downloader,
            pom_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Full resolution: collect → flatten → download artifacts.
    pub async fn resolve(
        &self,
        coord: &ArtifactCoord,
        scope_filter: Option<DependencyScope>,
    ) -> Result<ResolutionResult> {
        let tree = self.collect(coord).await?;
        let mut dependencies = self.flatten(&tree, scope_filter);
        self.download_all(&mut dependencies).await?;
        Ok(ResolutionResult {
            root: coord.clone(),
            tree,
            dependencies,
        })
    }

    /// Collect and flatten dependencies without downloading artifact JARs.
    pub async fn resolve_no_download(
        &self,
        coord: &ArtifactCoord,
        scope_filter: Option<DependencyScope>,
    ) -> Result<ResolutionResult> {
        let tree = self.collect(coord).await?;
        let dependencies = self.flatten(&tree, scope_filter);
        Ok(ResolutionResult {
            root: coord.clone(),
            tree,
            dependencies,
        })
    }

    // -- Phase 1: Collect ---------------------------------------------------

    /// Build the dependency tree by recursively fetching POMs.
    ///
    /// Resolves the root POM's effective model (parent chain, interpolation,
    /// BOM imports, DM injection) and extracts root-level `dependencyManagement`
    /// entries. These are propagated through the entire transitive tree so that
    /// the root project's version overrides always win — matching Maven behavior.
    async fn collect(&self, coord: &ArtifactCoord) -> Result<Vec<DependencyNode>> {
        let visited = Arc::new(Mutex::new(HashSet::new()));
        {
            let mut v = visited.lock().await;
            v.insert((coord.group_id.clone(), coord.artifact_id.clone()));
        }

        let pom = self.fetch_and_prepare_pom(coord).await?;

        // Extract root-level dependencyManagement as a lookup map.
        // Maven propagates the root project's DM to ALL transitive deps.
        let root_managed = Arc::new(Self::build_managed_map(&pom));

        self.collect_children(pom, Vec::new(), visited, 0, root_managed)
            .await
    }

    /// Build a `(groupId, artifactId) → PomDependency` map from a POM's
    /// `<dependencyManagement>` section.
    fn build_managed_map(pom: &pom::Pom) -> HashMap<(String, String), pom::PomDependency> {
        match &pom.dependency_management {
            Some(dm) => dm
                .dependencies
                .dependency
                .iter()
                .map(|d| ((d.group_id.clone(), d.artifact_id.clone()), d.clone()))
                .collect(),
            None => HashMap::new(),
        }
    }

    /// Recursively collect dependency nodes for a prepared POM.
    ///
    /// `root_managed` carries the root project's `dependencyManagement` entries
    /// through the entire transitive tree so that version overrides from the
    /// top-level POM always win.
    ///
    /// Pass 3 runs transitive POM preparation and child collection concurrently
    /// using `FuturesUnordered` for maximum parallelism.
    fn collect_children(
        &self,
        pom: pom::Pom,
        parent_exclusions: Vec<Exclusion>,
        visited: Arc<Mutex<HashSet<(String, String)>>>,
        depth: usize,
        root_managed: Arc<HashMap<(String, String), pom::PomDependency>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DependencyNode>>> + Send + '_>> {
        Box::pin(async move {
            if depth >= MAX_DEPTH {
                warn!("maximum dependency depth ({MAX_DEPTH}) reached; truncating");
                return Ok(Vec::new());
            }

            // -- Pass 1: filter deps & mark visited -------------------------
            struct DepInfo {
                coord: ArtifactCoord,
                scope: DependencyScope,
                exclusions: Vec<Exclusion>,
                child_exclusions: Vec<Exclusion>,
                needs_recurse: bool,
            }

            let mut dep_infos: Vec<DepInfo> = Vec::new();

            for dep in &pom.dependencies.dependency {
                let scope = dep
                    .scope
                    .as_deref()
                    .and_then(|s| s.parse::<DependencyScope>().ok())
                    .unwrap_or_default();

                if matches!(scope, DependencyScope::Import | DependencyScope::System) {
                    continue;
                }
                if dep.optional.as_deref() == Some("true") {
                    continue;
                }

                // Root depMgmt override: if the root project's DM specifies a
                // version for this GA, use it instead of whatever the current
                // POM declares. This is the key Maven behaviour that ensures
                // the top-level project controls transitive versions.
                let root_key = (dep.group_id.clone(), dep.artifact_id.clone());
                let version = if let Some(managed) = root_managed.get(&root_key) {
                    // Root DM wins
                    managed.version.clone().unwrap_or_default()
                } else {
                    match &dep.version {
                        Some(v) if !v.is_empty() && !v.contains("${") => v.clone(),
                        Some(v) if !v.is_empty() => {
                            // Version still contains unresolved ${...}; skip
                            warn!(
                                "skipping {}:{} — unresolved version: {}",
                                dep.group_id, dep.artifact_id, v
                            );
                            continue;
                        }
                        _ => {
                            warn!(
                                "skipping {}:{} — no version specified",
                                dep.group_id, dep.artifact_id
                            );
                            continue;
                        }
                    }
                };

                if version.is_empty() || version.contains("${") {
                    warn!(
                        "skipping {}:{} — unresolved version after DM lookup: {}",
                        dep.group_id, dep.artifact_id, version
                    );
                    continue;
                }

                // Resolve version ranges like [1.0,2.0)
                let resolved_version = if version.starts_with('[')
                    || version.starts_with('(')
                {
                    match self
                        .resolve_version_range(
                            &dep.group_id,
                            &dep.artifact_id,
                            &version,
                        )
                        .await
                    {
                        Some(v) => v,
                        None => {
                            warn!(
                                "no version matches range {} for {}:{}",
                                version, dep.group_id, dep.artifact_id
                            );
                            continue;
                        }
                    }
                } else {
                    version
                };

                let extension = dep.dep_type.as_deref().unwrap_or("jar");
                let mut dep_coord = ArtifactCoord::new(
                    &dep.group_id,
                    &dep.artifact_id,
                    &resolved_version,
                );
                if extension != "jar" {
                    dep_coord.extension = extension.to_string();
                }
                if let Some(ref c) = dep.classifier {
                    dep_coord.classifier = Some(c.clone());
                }

                if parent_exclusions.iter().any(|e| e.matches(&dep_coord)) {
                    continue;
                }

                let dep_exclusions: Vec<Exclusion> = dep
                    .exclusions
                    .exclusion
                    .iter()
                    .map(|e| Exclusion::new(&e.group_id, &e.artifact_id))
                    .collect();

                let key = (dep.group_id.clone(), dep.artifact_id.clone());
                let needs_recurse = {
                    let mut v = visited.lock().await;
                    v.insert(key)
                };

                let mut child_exclusions = parent_exclusions.clone();
                if needs_recurse {
                    for exc in &dep.exclusions.exclusion {
                        child_exclusions
                            .push(Exclusion::new(&exc.group_id, &exc.artifact_id));
                    }
                }

                dep_infos.push(DepInfo {
                    coord: dep_coord,
                    scope,
                    exclusions: dep_exclusions,
                    child_exclusions,
                    needs_recurse,
                });
            }

            // -- Pass 2: batch-fetch all POMs in parallel -------------------
            let coords_to_fetch: Vec<ArtifactCoord> = dep_infos
                .iter()
                .filter(|d| d.needs_recurse)
                .map(|d| d.coord.clone())
                .collect();

            let pom_results = self.downloader.fetch_poms(&coords_to_fetch).await;
            let mut pom_map: HashMap<String, Result<pom::Pom>> = pom_results
                .into_iter()
                .map(|(coord, result)| (coord.to_string(), result))
                .collect();

            // -- Pass 3: build nodes, recurse concurrently ------------------
            // Separate leaf nodes (no recursion needed) from recurse nodes.
            let mut leaf_nodes: Vec<(usize, DependencyNode)> = Vec::new();
            let mut recurse_tasks = FuturesUnordered::new();

            for (idx, info) in dep_infos.into_iter().enumerate() {
                if !info.needs_recurse {
                    leaf_nodes.push((
                        idx,
                        DependencyNode {
                            coord: info.coord,
                            scope: info.scope,
                            optional: false,
                            exclusions: Vec::new(),
                            children: Vec::new(),
                        },
                    ));
                    continue;
                }

                let pom_result = pom_map.remove(&info.coord.to_string());
                let visited = visited.clone();
                let root_managed = root_managed.clone();

                recurse_tasks.push(async move {
                    let children = match pom_result {
                        Some(Ok(raw_pom)) => {
                            let mut child_pom = raw_pom;
                            self.prepare_pom(&mut child_pom, 0).await;

                            // Check for POM relocation
                            if let Some(relocated) =
                                Self::check_relocation(&child_pom, &info.coord)
                            {
                                warn!(
                                    "artifact {} relocated to {}",
                                    info.coord, relocated
                                );
                                match self.fetch_and_prepare_pom(&relocated).await {
                                    Ok(relocated_pom) => {
                                        self.collect_children(
                                            relocated_pom,
                                            info.child_exclusions,
                                            visited,
                                            depth + 1,
                                            root_managed,
                                        )
                                        .await
                                        .unwrap_or_else(|e| {
                                            warn!(
                                                "failed collecting deps for relocated {}: {e}",
                                                relocated
                                            );
                                            Vec::new()
                                        })
                                    }
                                    Err(e) => {
                                        warn!(
                                            "failed to fetch relocated POM {}: {e}",
                                            relocated
                                        );
                                        Vec::new()
                                    }
                                }
                            } else {
                                self.collect_children(
                                    child_pom,
                                    info.child_exclusions,
                                    visited,
                                    depth + 1,
                                    root_managed,
                                )
                                .await
                                .unwrap_or_else(|e| {
                                    warn!(
                                        "failed to collect transitive deps for {}: {e}",
                                        info.coord
                                    );
                                    Vec::new()
                                })
                            }
                        }
                        Some(Err(e)) => {
                            warn!("failed to fetch POM for {}: {e}", info.coord);
                            Vec::new()
                        }
                        None => {
                            warn!("POM result missing for {}", info.coord);
                            Vec::new()
                        }
                    };

                    (
                        idx,
                        DependencyNode {
                            coord: info.coord,
                            scope: info.scope,
                            optional: false,
                            exclusions: info.exclusions,
                            children,
                        },
                    )
                });
            }

            // Collect all results (leaf + concurrent)
            let mut indexed_nodes: Vec<(usize, DependencyNode)> = leaf_nodes;
            while let Some((idx, node)) = recurse_tasks.next().await {
                indexed_nodes.push((idx, node));
            }

            // Restore original order
            indexed_nodes.sort_by_key(|(idx, _)| *idx);
            let nodes: Vec<DependencyNode> =
                indexed_nodes.into_iter().map(|(_, n)| n).collect();

            Ok(nodes)
        })
    }

    /// Check if a POM declares a relocation and return the relocated coordinate.
    fn check_relocation(
        pom: &pom::Pom,
        original: &ArtifactCoord,
    ) -> Option<ArtifactCoord> {
        let dm = pom.distribution_management.as_ref()?;
        let reloc = dm.relocation.as_ref()?;

        let group = reloc
            .group_id
            .as_deref()
            .unwrap_or(&original.group_id);
        let artifact = reloc
            .artifact_id
            .as_deref()
            .unwrap_or(&original.artifact_id);
        let version = reloc
            .version
            .as_deref()
            .unwrap_or(&original.version);

        let relocated = ArtifactCoord::new(group, artifact, version);
        // Guard against self-relocation
        if relocated.group_id == original.group_id
            && relocated.artifact_id == original.artifact_id
            && relocated.version == original.version
        {
            return None;
        }
        Some(relocated)
    }

    /// Resolve a version range like `[1.0,2.0)` by fetching maven-metadata.xml,
    /// filtering available versions through the range, and picking the highest.
    async fn resolve_version_range(
        &self,
        group_id: &str,
        artifact_id: &str,
        range_str: &str,
    ) -> Option<String> {
        let constraint = VersionConstraint::parse(range_str).ok()?;
        let metadata = self
            .downloader
            .fetch_metadata(group_id, artifact_id)
            .await
            .ok()?;

        let versions = metadata.available_versions();
        let mut matching: Vec<(&str, Version)> = versions
            .into_iter()
            .filter_map(|v| {
                let parsed = Version::new(v);
                if constraint.contains(&parsed) {
                    Some((v, parsed))
                } else {
                    None
                }
            })
            .collect();

        // Sort descending, pick highest
        matching.sort_by(|a, b| b.1.cmp(&a.1));
        matching.first().map(|(s, _)| (*s).to_string())
    }

    /// Fetch a POM and apply the full effective-model pipeline.
    async fn fetch_and_prepare_pom(&self, coord: &ArtifactCoord) -> Result<pom::Pom> {
        self.resolve_effective_pom(coord.clone(), 0).await
    }

    /// Fetch a POM and resolve its full effective model by walking the parent
    /// chain recursively.
    ///
    /// Results are cached in `pom_cache` to avoid re-fetching the same parent
    /// POMs repeatedly during transitive resolution. This dramatically improves
    /// performance for large dependency trees where many artifacts share common
    /// parents (e.g., `org.apache:apache`, Spring Boot parent, etc.).
    fn resolve_effective_pom(
        &self,
        coord: ArtifactCoord,
        parent_depth: usize,
    ) -> Pin<Box<dyn Future<Output = Result<pom::Pom>> + Send + '_>> {
        Box::pin(async move {
            let cache_key = coord.to_string();

            // Check cache first (read lock, cheap)
            {
                let cache = self.pom_cache.read().await;
                if let Some(pom) = cache.get(&cache_key) {
                    return Ok(pom.clone());
                }
            }

            // Cache miss — fetch and prepare
            let mut pom = self.downloader.fetch_pom(&coord).await?;
            self.prepare_pom(&mut pom, parent_depth).await;

            // Store in cache (write lock)
            {
                let mut cache = self.pom_cache.write().await;
                cache.insert(cache_key, pom.clone());
            }

            Ok(pom)
        })
    }

    /// Apply parent chain resolution, interpolation, BOM imports, and depMgmt
    /// injection to an already-fetched POM.
    fn prepare_pom<'s>(
        &'s self,
        pom: &'s mut pom::Pom,
        parent_depth: usize,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 's>> {
        Box::pin(async move {
            // Step 1: Resolve parent chain
            if parent_depth < MAX_PARENT_DEPTH {
                if let Some(parent_ref) = pom.parent.clone() {
                    // Try to resolve parent version from child's own properties
                    let mut parent_version = parent_ref.version.clone();
                    for (k, v) in &pom.properties.entries {
                        parent_version =
                            parent_version.replace(&format!("${{{k}}}"), v);
                    }

                    if parent_version.is_empty() || parent_version.contains("${") {
                        warn!(
                            "cannot resolve parent version for {}:{}: {}",
                            parent_ref.group_id, parent_ref.artifact_id, parent_ref.version
                        );
                    } else {
                        let parent_coord = ArtifactCoord::new(
                            &parent_ref.group_id,
                            &parent_ref.artifact_id,
                            &parent_version,
                        );
                        match self
                            .resolve_effective_pom(parent_coord, parent_depth + 1)
                            .await
                        {
                            Ok(effective_parent) => {
                                pom::merge_parent(pom, &effective_parent);
                            }
                            Err(e) => {
                                warn!(
                                    "failed to fetch parent POM {}:{}:{}: {e}",
                                    parent_ref.group_id,
                                    parent_ref.artifact_id,
                                    parent_version
                                );
                            }
                        }
                    }
                }
            } else {
                warn!("max parent POM depth ({MAX_PARENT_DEPTH}) reached");
            }

            // Step 2: Interpolate (after parent merge so parent properties are available)
            pom::interpolate_pom(pom);

            // Step 3: Resolve BOM imports (import-scope entries in dependencyManagement)
            self.resolve_bom_imports(pom, parent_depth).await;

            // Step 4: Re-interpolate after BOM imports to resolve any remaining
            // property references introduced by imported BOM entries
            pom::interpolate_pom(pom);

            // Step 5: Inject dependency management into dependencies
            pom::inject_dependency_management(pom);
        })
    }

    /// Resolve `<scope>import</scope>` entries in `<dependencyManagement>`.
    ///
    /// These are BOMs (Bill of Materials) whose dependencyManagement sections
    /// should be merged into the current POM. Includes circular import detection
    /// and BOM-level exclusion filtering.
    fn resolve_bom_imports<'s>(
        &'s self,
        pom: &'s mut pom::Pom,
        parent_depth: usize,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 's>> {
        Box::pin(async move {
            let imports: Vec<pom::PomDependency> = match &pom.dependency_management {
                Some(dm) => dm
                    .dependencies
                    .dependency
                    .iter()
                    .filter(|d| {
                        d.scope.as_deref() == Some("import")
                            && d.dep_type.as_deref() == Some("pom")
                    })
                    .cloned()
                    .collect(),
                None => return,
            };

            if imports.is_empty() {
                return;
            }

            // Remove import entries from dependencyManagement
            if let Some(ref mut dm) = pom.dependency_management {
                dm.dependencies.dependency.retain(|d| {
                    !(d.scope.as_deref() == Some("import")
                        && d.dep_type.as_deref() == Some("pom"))
                });
            }

            // Build current POM's identity for cycle detection
            let pom_id = format!(
                "{}:{}:{}",
                pom.group_id.as_deref().unwrap_or("?"),
                pom.artifact_id.as_deref().unwrap_or("?"),
                pom.version.as_deref().unwrap_or("?"),
            );
            let mut import_ids: HashSet<String> = HashSet::new();
            import_ids.insert(pom_id);

            // Fetch each BOM and merge its dependencyManagement
            for import in &imports {
                let version = match &import.version {
                    Some(v) if !v.is_empty() => v.clone(),
                    _ => continue,
                };

                let import_id =
                    format!("{}:{}:{}", import.group_id, import.artifact_id, version);

                // Circular import detection
                if !import_ids.insert(import_id.clone()) {
                    warn!("circular BOM import detected: {import_id}; skipping");
                    continue;
                }

                let bom_coord =
                    ArtifactCoord::new(&import.group_id, &import.artifact_id, &version);

                match self
                    .resolve_effective_pom(bom_coord, parent_depth + 1)
                    .await
                {
                    Ok(bom_pom) => {
                        if let Some(bom_dm) = &bom_pom.dependency_management {
                            let child_dm =
                                pom.dependency_management.get_or_insert_with(|| {
                                    pom::DependencyManagement {
                                        dependencies: pom::Dependencies::default(),
                                    }
                                });

                            let existing_keys: HashSet<(String, String)> = child_dm
                                .dependencies
                                .dependency
                                .iter()
                                .map(|d| (d.group_id.clone(), d.artifact_id.clone()))
                                .collect();

                            // BOM import exclusions filter imported entries
                            let import_exclusions: Vec<Exclusion> = import
                                .exclusions
                                .exclusion
                                .iter()
                                .map(|e| Exclusion::new(&e.group_id, &e.artifact_id))
                                .collect();

                            for dep in &bom_dm.dependencies.dependency {
                                let key =
                                    (dep.group_id.clone(), dep.artifact_id.clone());
                                if existing_keys.contains(&key) {
                                    continue;
                                }
                                // Check import-level exclusions
                                let excluded = import_exclusions.iter().any(|exc| {
                                    let tmp = ArtifactCoord::new(
                                        &dep.group_id,
                                        &dep.artifact_id,
                                        dep.version.as_deref().unwrap_or(""),
                                    );
                                    exc.matches(&tmp)
                                });
                                if !excluded {
                                    child_dm
                                        .dependencies
                                        .dependency
                                        .push(dep.clone());
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "failed to fetch BOM {}:{}:{}: {e}",
                            import.group_id, import.artifact_id, version
                        );
                    }
                }
            }
        })
    }

    // -- Phase 2: Flatten ---------------------------------------------------

    /// Flatten the tree using BFS "nearest wins" conflict resolution.
    pub fn flatten(
        &self,
        tree: &[DependencyNode],
        scope_filter: Option<DependencyScope>,
    ) -> Vec<ResolvedDependency> {
        let mut queue: VecDeque<(&DependencyNode, DependencyScope)> = VecDeque::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut result: Vec<ResolvedDependency> = Vec::new();

        // Seed with root-level deps (their scope is as declared)
        for node in tree {
            queue.push_back((node, node.scope));
        }

        while let Some((node, effective_scope)) = queue.pop_front() {
            let key = (
                node.coord.group_id.clone(),
                node.coord.artifact_id.clone(),
            );

            // Nearest wins: skip if we've already accepted this GA
            if !seen.insert(key) {
                continue;
            }

            // Scope filter
            if let Some(filter) = scope_filter {
                if !scope_matches(effective_scope, filter) {
                    continue;
                }
            }

            result.push(ResolvedDependency {
                coord: node.coord.clone(),
                scope: effective_scope,
                path: None,
            });

            // Enqueue children with propagated scope
            for child in &node.children {
                if let Some(propagated) = propagate_scope(effective_scope, child.scope) {
                    queue.push_back((child, propagated));
                }
            }
        }

        result
    }

    // -- Phase 3: Download --------------------------------------------------

    /// Download artifacts for all resolved dependencies concurrently.
    ///
    /// Skips JAR download for artifacts with `pom` packaging (extension).
    async fn download_all(&self, deps: &mut [ResolvedDependency]) -> Result<()> {
        // Filter out pom-only artifacts (no JAR to download)
        let coords: Vec<_> = deps
            .iter()
            .filter(|d| d.coord.extension != "pom")
            .map(|d| d.coord.clone())
            .collect();
        let results = self.downloader.download_artifacts(&coords).await;

        // Map results back to deps
        let result_map: HashMap<String, std::result::Result<PathBuf, _>> = results
            .into_iter()
            .map(|(c, r)| (c.to_string(), r))
            .collect();

        for dep in deps.iter_mut() {
            if let Some(result) = result_map.get(&dep.coord.to_string()) {
                match result {
                    Ok(path) => dep.path = Some(path.clone()),
                    Err(e) => {
                        tracing::warn!("failed to download {}: {}", dep.coord, e);
                    }
                }
            }
        }
        Ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::{ArtifactCoord, DependencyScope};
    use crate::downloader::ArtifactDownloader;
    use crate::repository::{LocalRepository, RemoteRepository, RepositorySystem};

    /// Helper: create a dependency node.
    fn node(
        group: &str,
        artifact: &str,
        version: &str,
        scope: DependencyScope,
        children: Vec<DependencyNode>,
    ) -> DependencyNode {
        DependencyNode {
            coord: ArtifactCoord::new(group, artifact, version),
            scope,
            optional: false,
            exclusions: Vec::new(),
            children,
        }
    }

    /// Helper: build a test resolver (downloader is never called in sync tests).
    fn test_downloader() -> ArtifactDownloader {
        let local = LocalRepository::new("target/test-resolver-repo");
        let remotes = vec![RemoteRepository::maven_central()];
        let system = RepositorySystem::new(local, remotes);
        ArtifactDownloader::new(system)
    }

    // ---- propagate_scope --------------------------------------------------

    #[test]
    fn propagate_compile_compile() {
        assert_eq!(
            propagate_scope(DependencyScope::Compile, DependencyScope::Compile),
            Some(DependencyScope::Compile),
        );
    }

    #[test]
    fn propagate_compile_runtime() {
        assert_eq!(
            propagate_scope(DependencyScope::Compile, DependencyScope::Runtime),
            Some(DependencyScope::Runtime),
        );
    }

    #[test]
    fn propagate_compile_provided() {
        assert_eq!(
            propagate_scope(DependencyScope::Compile, DependencyScope::Provided),
            None,
        );
    }

    #[test]
    fn propagate_compile_test() {
        assert_eq!(
            propagate_scope(DependencyScope::Compile, DependencyScope::Test),
            None,
        );
    }

    #[test]
    fn propagate_runtime_compile() {
        assert_eq!(
            propagate_scope(DependencyScope::Runtime, DependencyScope::Compile),
            Some(DependencyScope::Runtime),
        );
    }

    #[test]
    fn propagate_runtime_runtime() {
        assert_eq!(
            propagate_scope(DependencyScope::Runtime, DependencyScope::Runtime),
            Some(DependencyScope::Runtime),
        );
    }

    #[test]
    fn propagate_provided_compile() {
        assert_eq!(
            propagate_scope(DependencyScope::Provided, DependencyScope::Compile),
            Some(DependencyScope::Provided),
        );
    }

    #[test]
    fn propagate_provided_runtime() {
        assert_eq!(
            propagate_scope(DependencyScope::Provided, DependencyScope::Runtime),
            Some(DependencyScope::Provided),
        );
    }

    #[test]
    fn propagate_test_compile() {
        assert_eq!(
            propagate_scope(DependencyScope::Test, DependencyScope::Compile),
            Some(DependencyScope::Test),
        );
    }

    #[test]
    fn propagate_test_runtime() {
        assert_eq!(
            propagate_scope(DependencyScope::Test, DependencyScope::Runtime),
            Some(DependencyScope::Test),
        );
    }

    #[test]
    fn propagate_system_excluded() {
        assert_eq!(
            propagate_scope(DependencyScope::System, DependencyScope::Compile),
            None,
        );
    }

    #[test]
    fn propagate_import_excluded() {
        assert_eq!(
            propagate_scope(DependencyScope::Import, DependencyScope::Runtime),
            None,
        );
    }

    #[test]
    fn propagate_child_system_excluded() {
        assert_eq!(
            propagate_scope(DependencyScope::Compile, DependencyScope::System),
            None,
        );
    }

    #[test]
    fn propagate_child_import_excluded() {
        assert_eq!(
            propagate_scope(DependencyScope::Runtime, DependencyScope::Import),
            None,
        );
    }

    // ---- scope_matches ----------------------------------------------------

    #[test]
    fn scope_filter_compile() {
        assert!(scope_matches(DependencyScope::Compile, DependencyScope::Compile));
        assert!(scope_matches(DependencyScope::Provided, DependencyScope::Compile));
        assert!(!scope_matches(DependencyScope::Runtime, DependencyScope::Compile));
        assert!(!scope_matches(DependencyScope::Test, DependencyScope::Compile));
    }

    #[test]
    fn scope_filter_runtime() {
        assert!(scope_matches(DependencyScope::Compile, DependencyScope::Runtime));
        assert!(scope_matches(DependencyScope::Runtime, DependencyScope::Runtime));
        assert!(!scope_matches(DependencyScope::Provided, DependencyScope::Runtime));
        assert!(!scope_matches(DependencyScope::Test, DependencyScope::Runtime));
    }

    #[test]
    fn scope_filter_test() {
        assert!(scope_matches(DependencyScope::Compile, DependencyScope::Test));
        assert!(scope_matches(DependencyScope::Runtime, DependencyScope::Test));
        assert!(scope_matches(DependencyScope::Test, DependencyScope::Test));
        assert!(!scope_matches(DependencyScope::Provided, DependencyScope::Test));
    }

    // ---- flatten / nearest wins -------------------------------------------

    #[test]
    fn flatten_diamond_nearest_wins() {
        // Root → B → D:1.0
        // Root → C → D:2.0
        // BFS: B and C at depth 1, D:1.0 and D:2.0 at depth 2.
        // D:1.0 is encountered first (via B) → wins.
        let tree = vec![
            node("org", "b", "1.0", DependencyScope::Compile, vec![
                node("org", "d", "1.0", DependencyScope::Compile, vec![]),
            ]),
            node("org", "c", "1.0", DependencyScope::Compile, vec![
                node("org", "d", "2.0", DependencyScope::Compile, vec![]),
            ]),
        ];

        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        let result = resolver.flatten(&tree, None);

        assert_eq!(result.len(), 3); // B, C, D
        assert_eq!(result[0].coord.artifact_id, "b");
        assert_eq!(result[1].coord.artifact_id, "c");
        assert_eq!(result[2].coord.artifact_id, "d");
        assert_eq!(result[2].coord.version, "1.0"); // D:1.0 wins
    }

    #[test]
    fn flatten_scope_propagation() {
        // Root dep A (runtime) has child B (compile) → effective runtime
        let tree = vec![
            node("org", "a", "1.0", DependencyScope::Runtime, vec![
                node("org", "b", "1.0", DependencyScope::Compile, vec![]),
            ]),
        ];

        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        let result = resolver.flatten(&tree, None);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].scope, DependencyScope::Runtime); // A
        assert_eq!(result[1].scope, DependencyScope::Runtime); // B propagated
    }

    #[test]
    fn flatten_excludes_non_transitive_child_scopes() {
        // Root dep A (compile) has child B (test) → excluded by propagation
        let tree = vec![
            node("org", "a", "1.0", DependencyScope::Compile, vec![
                node("org", "b", "1.0", DependencyScope::Test, vec![]),
            ]),
        ];

        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        let result = resolver.flatten(&tree, None);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].coord.artifact_id, "a");
    }

    #[test]
    fn flatten_with_scope_filter() {
        let tree = vec![
            node("org", "a", "1.0", DependencyScope::Compile, vec![]),
            node("org", "b", "1.0", DependencyScope::Runtime, vec![]),
            node("org", "c", "1.0", DependencyScope::Test, vec![]),
        ];

        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        // Runtime filter: include compile + runtime
        let result = resolver.flatten(&tree, Some(DependencyScope::Runtime));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].coord.artifact_id, "a");
        assert_eq!(result[1].coord.artifact_id, "b");
    }

    #[test]
    fn flatten_empty_tree() {
        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        let result = resolver.flatten(&[], None);
        assert!(result.is_empty());
    }

    #[test]
    fn flatten_deep_propagation_chain() {
        // Root dep A (test) → B (compile) → C (compile)
        // B effective = test, C effective = test (test + compile → test)
        let tree = vec![
            node("org", "a", "1.0", DependencyScope::Test, vec![
                node("org", "b", "1.0", DependencyScope::Compile, vec![
                    node("org", "c", "1.0", DependencyScope::Compile, vec![]),
                ]),
            ]),
        ];

        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        let result = resolver.flatten(&tree, None);

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].scope, DependencyScope::Test);
        assert_eq!(result[1].scope, DependencyScope::Test);
        assert_eq!(result[2].scope, DependencyScope::Test);
    }

    #[test]
    fn flatten_nearest_wins_shallower_depth() {
        // Root → D:1.0 (direct, depth 1)
        // Root → B → D:2.0 (transitive, depth 2)
        // D:1.0 wins because it's nearer (depth 1 vs depth 2).
        let tree = vec![
            node("org", "d", "1.0", DependencyScope::Compile, vec![]),
            node("org", "b", "1.0", DependencyScope::Compile, vec![
                node("org", "d", "2.0", DependencyScope::Compile, vec![]),
            ]),
        ];

        let dl = test_downloader();
        let resolver = DependencyResolver::new(&dl);
        let result = resolver.flatten(&tree, None);

        assert_eq!(result.len(), 2); // D and B
        // D:1.0 is at depth 1 (direct), processed first in BFS
        assert_eq!(result[0].coord.artifact_id, "d");
        assert_eq!(result[0].coord.version, "1.0");
        assert_eq!(result[1].coord.artifact_id, "b");
    }

    // ---- format_tree ------------------------------------------------------

    #[test]
    fn format_tree_basic() {
        let root = ArtifactCoord::new("org.example", "app", "1.0.0");
        let nodes = vec![
            node("org.apache", "lang3", "3.12", DependencyScope::Compile, vec![]),
            node("com.google", "guava", "31.0", DependencyScope::Compile, vec![]),
        ];

        let output = format_tree_plain(&root, &nodes);
        assert!(output.starts_with("org.example:app:1.0.0\n"));
        assert!(output.contains("├── org.apache:lang3:3.12 [compile]"));
        assert!(output.contains("└── com.google:guava:31.0 [compile]"));
    }

    #[test]
    fn format_tree_nested() {
        let root = ArtifactCoord::new("org.example", "app", "1.0.0");
        let nodes = vec![
            node(
                "org.apache", "lang3", "3.12", DependencyScope::Compile,
                vec![
                    node("org.apache", "text", "1.0", DependencyScope::Compile, vec![]),
                ],
            ),
        ];

        let output = format_tree_plain(&root, &nodes);
        assert!(output.contains("└── org.apache:lang3:3.12 [compile]"));
        assert!(output.contains("    └── org.apache:text:1.0 [compile]"));
    }

    #[test]
    fn format_tree_empty() {
        let root = ArtifactCoord::new("org.example", "app", "1.0.0");
        let output = format_tree_plain(&root, &[]);
        assert_eq!(output, "org.example:app:1.0.0\n");
    }

    #[test]
    fn format_tree_multiple_children_indentation() {
        let root = ArtifactCoord::new("org", "root", "1.0");
        let nodes = vec![
            node("org", "a", "1.0", DependencyScope::Compile, vec![
                node("org", "a1", "1.0", DependencyScope::Compile, vec![]),
                node("org", "a2", "1.0", DependencyScope::Runtime, vec![]),
            ]),
            node("org", "b", "1.0", DependencyScope::Test, vec![]),
        ];

        let output = format_tree_plain(&root, &nodes);
        // "a" is not last, so its prefix uses │
        assert!(output.contains("├── org:a:1.0 [compile]"));
        assert!(output.contains("│   ├── org:a1:1.0 [compile]"));
        assert!(output.contains("│   └── org:a2:1.0 [runtime]"));
        assert!(output.contains("└── org:b:1.0 [test]"));
    }

    // ====================================================================
    // Integration tests — wiremock-based full pipeline
    // ====================================================================

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Create a minimal POM XML with optional dependencies.
    fn make_pom(
        group: &str,
        artifact: &str,
        version: &str,
        deps: &[(&str, &str, &str, &str)],
    ) -> String {
        let dep_xml: String = deps
            .iter()
            .map(|(g, a, v, scope)| {
                format!(
                    "<dependency>\
                       <groupId>{g}</groupId>\
                       <artifactId>{a}</artifactId>\
                       <version>{v}</version>\
                       <scope>{scope}</scope>\
                     </dependency>"
                )
            })
            .collect();
        format!(
            "<project>\
               <modelVersion>4.0.0</modelVersion>\
               <groupId>{group}</groupId>\
               <artifactId>{artifact}</artifactId>\
               <version>{version}</version>\
               <dependencies>{dep_xml}</dependencies>\
             </project>"
        )
    }

    /// Setup helper: wiremock server + temp local repo + downloader.
    async fn setup_resolver() -> (MockServer, TempDir, ArtifactDownloader) {
        let server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let local = LocalRepository::new(tmp.path());
        let remote = RemoteRepository::new("test", &server.uri());
        let system = RepositorySystem::new(local, vec![remote]);
        let downloader = ArtifactDownloader::new(system);
        (server, tmp, downloader)
    }

    /// Mount a POM XML on the mock server for a given GAV.
    async fn mount_pom(server: &MockServer, group: &str, artifact: &str, version: &str, body: &str) {
        let pom_path = format!(
            "/{}/{}/{}/{}-{}.pom",
            group.replace('.', "/"),
            artifact,
            version,
            artifact,
            version,
        );
        Mock::given(method("GET"))
            .and(path(&pom_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    // 1. resolve_simple_no_deps
    #[tokio::test]
    async fn resolve_simple_no_deps() {
        let (server, _tmp, downloader) = setup_resolver().await;
        let pom_xml = make_pom("com.example", "simple", "1.0", &[]);
        mount_pom(&server, "com.example", "simple", "1.0", &pom_xml).await;

        let coord = ArtifactCoord::new("com.example", "simple", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert!(result.tree.is_empty());
        assert!(result.dependencies.is_empty());
    }

    // 2. resolve_single_dependency
    #[tokio::test]
    async fn resolve_single_dependency() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let root_pom = make_pom(
            "com.example", "root", "1.0",
            &[("com.example", "child", "2.0", "compile")],
        );
        let child_pom = make_pom("com.example", "child", "2.0", &[]);

        mount_pom(&server, "com.example", "root", "1.0", &root_pom).await;
        mount_pom(&server, "com.example", "child", "2.0", &child_pom).await;

        let coord = ArtifactCoord::new("com.example", "root", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert_eq!(result.tree.len(), 1);
        assert_eq!(result.tree[0].coord.artifact_id, "child");
        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.artifact_id, "child");
        assert_eq!(result.dependencies[0].scope, DependencyScope::Compile);
    }

    // 3. resolve_transitive_deps
    #[tokio::test]
    async fn resolve_transitive_deps() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let a_pom = make_pom("com.example", "a", "1.0", &[("com.example", "b", "1.0", "compile")]);
        let b_pom = make_pom("com.example", "b", "1.0", &[("com.example", "c", "1.0", "compile")]);
        let c_pom = make_pom("com.example", "c", "1.0", &[]);

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        mount_pom(&server, "com.example", "b", "1.0", &b_pom).await;
        mount_pom(&server, "com.example", "c", "1.0", &c_pom).await;

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        // tree: B (with child C)
        assert_eq!(result.tree.len(), 1);
        assert_eq!(result.tree[0].coord.artifact_id, "b");
        assert_eq!(result.tree[0].children.len(), 1);
        assert_eq!(result.tree[0].children[0].coord.artifact_id, "c");

        // flattened: B, C
        assert_eq!(result.dependencies.len(), 2);
        let ids: Vec<&str> = result.dependencies.iter().map(|d| d.coord.artifact_id.as_str()).collect();
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"c"));

        // C should remain compile scope (compile→compile = compile)
        let c_dep = result.dependencies.iter().find(|d| d.coord.artifact_id == "c").unwrap();
        assert_eq!(c_dep.scope, DependencyScope::Compile);
    }

    // 4. resolve_diamond_nearest_wins
    #[tokio::test]
    async fn resolve_diamond_nearest_wins() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A → B, A → C; B → D:1.0; C → D:2.0
        let a_pom = make_pom("com.example", "a", "1.0", &[
            ("com.example", "b", "1.0", "compile"),
            ("com.example", "c", "1.0", "compile"),
        ]);
        let b_pom = make_pom("com.example", "b", "1.0", &[("com.example", "d", "1.0", "compile")]);
        let c_pom = make_pom("com.example", "c", "1.0", &[("com.example", "d", "2.0", "compile")]);
        let d1_pom = make_pom("com.example", "d", "1.0", &[]);
        // d:2.0 won't be fetched since "d" is already visited after d:1.0

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        mount_pom(&server, "com.example", "b", "1.0", &b_pom).await;
        mount_pom(&server, "com.example", "c", "1.0", &c_pom).await;
        mount_pom(&server, "com.example", "d", "1.0", &d1_pom).await;

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        // D should appear exactly once in flattened results
        let d_deps: Vec<_> = result.dependencies.iter().filter(|d| d.coord.artifact_id == "d").collect();
        assert_eq!(d_deps.len(), 1);
        // B is processed before C → D:1.0 is encountered first
        assert_eq!(d_deps[0].coord.version, "1.0");
    }

    // 5. resolve_scope_propagation
    #[tokio::test]
    async fn resolve_scope_propagation() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A → B(compile) → C(runtime); C effective = runtime (compile→runtime = runtime)
        let a_pom = make_pom("com.example", "a", "1.0", &[("com.example", "b", "1.0", "compile")]);
        let b_pom = make_pom("com.example", "b", "1.0", &[("com.example", "c", "1.0", "runtime")]);
        let c_pom = make_pom("com.example", "c", "1.0", &[]);

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        mount_pom(&server, "com.example", "b", "1.0", &b_pom).await;
        mount_pom(&server, "com.example", "c", "1.0", &c_pom).await;

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        let c_dep = result.dependencies.iter().find(|d| d.coord.artifact_id == "c").unwrap();
        assert_eq!(c_dep.scope, DependencyScope::Runtime);
    }

    // 6. resolve_test_scope_not_transitive
    #[tokio::test]
    async fn resolve_test_scope_not_transitive() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A → B(compile) → C(test); test scope is NOT transitive
        let a_pom = make_pom("com.example", "a", "1.0", &[("com.example", "b", "1.0", "compile")]);
        let b_pom = make_pom("com.example", "b", "1.0", &[("com.example", "c", "1.0", "test")]);
        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        mount_pom(&server, "com.example", "b", "1.0", &b_pom).await;
        // C POM not mounted — resolver skips test-scope transitives

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        // C should NOT appear in flattened deps
        let has_c = result.dependencies.iter().any(|d| d.coord.artifact_id == "c");
        assert!(!has_c, "test-scope transitive dep C should be excluded");
    }

    // 7. resolve_exclusion_applied
    #[tokio::test]
    async fn resolve_exclusion_applied() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A → B (with exclusion on C), B → C. C should be excluded.
        let a_pom = format!(
            "<project>\
               <modelVersion>4.0.0</modelVersion>\
               <groupId>com.example</groupId>\
               <artifactId>a</artifactId>\
               <version>1.0</version>\
               <dependencies>\
                 <dependency>\
                   <groupId>com.example</groupId>\
                   <artifactId>b</artifactId>\
                   <version>1.0</version>\
                   <scope>compile</scope>\
                   <exclusions>\
                     <exclusion>\
                       <groupId>com.example</groupId>\
                       <artifactId>c</artifactId>\
                     </exclusion>\
                   </exclusions>\
                 </dependency>\
               </dependencies>\
             </project>"
        );
        let b_pom = make_pom("com.example", "b", "1.0", &[("com.example", "c", "1.0", "compile")]);
        let c_pom = make_pom("com.example", "c", "1.0", &[]);

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        mount_pom(&server, "com.example", "b", "1.0", &b_pom).await;
        mount_pom(&server, "com.example", "c", "1.0", &c_pom).await;

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        // B should be present, C should NOT
        let has_b = result.dependencies.iter().any(|d| d.coord.artifact_id == "b");
        let has_c = result.dependencies.iter().any(|d| d.coord.artifact_id == "c");
        assert!(has_b, "B should be in resolved deps");
        assert!(!has_c, "excluded dep C should not appear");
    }

    // 8. resolve_optional_deps_skipped
    #[tokio::test]
    async fn resolve_optional_deps_skipped() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A has B as optional dependency — resolver skips optional=true
        let a_pom = format!(
            "<project>\
               <modelVersion>4.0.0</modelVersion>\
               <groupId>com.example</groupId>\
               <artifactId>a</artifactId>\
               <version>1.0</version>\
               <dependencies>\
                 <dependency>\
                   <groupId>com.example</groupId>\
                   <artifactId>b</artifactId>\
                   <version>1.0</version>\
                   <scope>compile</scope>\
                   <optional>true</optional>\
                 </dependency>\
               </dependencies>\
             </project>"
        );

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        // B POM not needed — should be skipped entirely

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert!(result.dependencies.is_empty(), "optional dep should be skipped");
    }

    // 9. resolve_scope_filter
    #[tokio::test]
    async fn resolve_scope_filter() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A has compile dep B and test dep C
        let a_pom = make_pom("com.example", "a", "1.0", &[
            ("com.example", "b", "1.0", "compile"),
            ("com.example", "c", "1.0", "test"),
        ]);
        let b_pom = make_pom("com.example", "b", "1.0", &[]);
        let c_pom = make_pom("com.example", "c", "1.0", &[]);

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        mount_pom(&server, "com.example", "b", "1.0", &b_pom).await;
        mount_pom(&server, "com.example", "c", "1.0", &c_pom).await;

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);

        // Compile filter → only compile (+ provided) scopes
        let result = resolver.resolve(&coord, Some(DependencyScope::Compile)).await.unwrap();
        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.artifact_id, "b");
    }

    // 10. resolve_missing_pom_graceful
    #[tokio::test]
    async fn resolve_missing_pom_graceful() {
        let (server, _tmp, downloader) = setup_resolver().await;

        // A → B, but B's POM returns 404
        let a_pom = make_pom("com.example", "a", "1.0", &[("com.example", "b", "1.0", "compile")]);

        mount_pom(&server, "com.example", "a", "1.0", &a_pom).await;
        // Do NOT mount B's POM → 404

        let coord = ArtifactCoord::new("com.example", "a", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        // Resolution should complete without error
        // B appears in tree (node created) but with empty children
        assert_eq!(result.tree.len(), 1);
        assert_eq!(result.tree[0].coord.artifact_id, "b");
        assert!(result.tree[0].children.is_empty());
    }

    // ====================================================================
    // New feature tests — parent chain, BOM, root depMgmt, relocation
    // ====================================================================

    /// Helper: create a POM XML with parent reference.
    #[allow(dead_code)]
    fn make_pom_with_parent(
        group: &str,
        artifact: &str,
        version: &str,
        parent_group: &str,
        parent_artifact: &str,
        parent_version: &str,
        deps: &[(&str, &str, &str, &str)],
    ) -> String {
        let dep_xml: String = deps
            .iter()
            .map(|(g, a, v, scope)| {
                format!(
                    "<dependency>\
                       <groupId>{g}</groupId>\
                       <artifactId>{a}</artifactId>\
                       <version>{v}</version>\
                       <scope>{scope}</scope>\
                     </dependency>"
                )
            })
            .collect();
        format!(
            "<project>\
               <modelVersion>4.0.0</modelVersion>\
               <parent>\
                 <groupId>{parent_group}</groupId>\
                 <artifactId>{parent_artifact}</artifactId>\
                 <version>{parent_version}</version>\
               </parent>\
               <groupId>{group}</groupId>\
               <artifactId>{artifact}</artifactId>\
               <version>{version}</version>\
               <dependencies>{dep_xml}</dependencies>\
             </project>"
        )
    }

    // 11. Parent POM chain resolves version via dependencyManagement
    #[tokio::test]
    async fn resolve_parent_chain_injects_version() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let parent_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>parent</artifactId>\
              <version>1.0</version>\
              <packaging>pom</packaging>\
              <dependencyManagement>\
                <dependencies>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>lib</artifactId>\
                    <version>3.0</version>\
                  </dependency>\
                </dependencies>\
              </dependencyManagement>\
            </project>";

        let child_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <parent>\
                <groupId>com.example</groupId>\
                <artifactId>parent</artifactId>\
                <version>1.0</version>\
              </parent>\
              <groupId>com.example</groupId>\
              <artifactId>child</artifactId>\
              <version>1.0</version>\
              <dependencies>\
                <dependency>\
                  <groupId>com.example</groupId>\
                  <artifactId>lib</artifactId>\
                </dependency>\
              </dependencies>\
            </project>";

        let lib_pom = make_pom("com.example", "lib", "3.0", &[]);

        mount_pom(&server, "com.example", "parent", "1.0", parent_pom).await;
        mount_pom(&server, "com.example", "child", "1.0", child_pom).await;
        mount_pom(&server, "com.example", "lib", "3.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "child", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.artifact_id, "lib");
        assert_eq!(result.dependencies[0].coord.version, "3.0");
    }

    // 12. Parent POM properties inherited and interpolated
    #[tokio::test]
    async fn resolve_parent_properties_inherited() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let parent_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>parent</artifactId>\
              <version>1.0</version>\
              <packaging>pom</packaging>\
              <properties>\
                <lib.version>5.0</lib.version>\
              </properties>\
            </project>";

        let child_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <parent>\
                <groupId>com.example</groupId>\
                <artifactId>parent</artifactId>\
                <version>1.0</version>\
              </parent>\
              <groupId>com.example</groupId>\
              <artifactId>child</artifactId>\
              <version>1.0</version>\
              <dependencies>\
                <dependency>\
                  <groupId>com.example</groupId>\
                  <artifactId>lib</artifactId>\
                  <version>${lib.version}</version>\
                </dependency>\
              </dependencies>\
            </project>";

        let lib_pom = make_pom("com.example", "lib", "5.0", &[]);

        mount_pom(&server, "com.example", "parent", "1.0", parent_pom).await;
        mount_pom(&server, "com.example", "child", "1.0", child_pom).await;
        mount_pom(&server, "com.example", "lib", "5.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "child", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.version, "5.0");
    }

    // 13. Root depMgmt overrides transitive dependency version
    #[tokio::test]
    async fn resolve_root_depmgmt_overrides_transitive() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let root_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>root</artifactId>\
              <version>1.0</version>\
              <dependencyManagement>\
                <dependencies>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>lib</artifactId>\
                    <version>9.0</version>\
                  </dependency>\
                </dependencies>\
              </dependencyManagement>\
              <dependencies>\
                <dependency>\
                  <groupId>com.example</groupId>\
                  <artifactId>mid</artifactId>\
                  <version>1.0</version>\
                </dependency>\
              </dependencies>\
            </project>";

        let mid_pom = make_pom("com.example", "mid", "1.0", &[
            ("com.example", "lib", "2.0", "compile"),
        ]);
        let lib_pom = make_pom("com.example", "lib", "9.0", &[]);

        mount_pom(&server, "com.example", "root", "1.0", root_pom).await;
        mount_pom(&server, "com.example", "mid", "1.0", &mid_pom).await;
        mount_pom(&server, "com.example", "lib", "9.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "root", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        let lib_dep = result.dependencies.iter().find(|d| d.coord.artifact_id == "lib").unwrap();
        assert_eq!(lib_dep.coord.version, "9.0", "root depMgmt should override transitive version");
    }

    // 14. BOM import merges dependencyManagement
    #[tokio::test]
    async fn resolve_bom_import_merges_depmgmt() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let bom_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>bom</artifactId>\
              <version>1.0</version>\
              <packaging>pom</packaging>\
              <dependencyManagement>\
                <dependencies>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>lib</artifactId>\
                    <version>7.0</version>\
                  </dependency>\
                </dependencies>\
              </dependencyManagement>\
            </project>";

        let root_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>root</artifactId>\
              <version>1.0</version>\
              <dependencyManagement>\
                <dependencies>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>bom</artifactId>\
                    <version>1.0</version>\
                    <type>pom</type>\
                    <scope>import</scope>\
                  </dependency>\
                </dependencies>\
              </dependencyManagement>\
              <dependencies>\
                <dependency>\
                  <groupId>com.example</groupId>\
                  <artifactId>lib</artifactId>\
                </dependency>\
              </dependencies>\
            </project>";

        let lib_pom = make_pom("com.example", "lib", "7.0", &[]);

        mount_pom(&server, "com.example", "bom", "1.0", bom_pom).await;
        mount_pom(&server, "com.example", "root", "1.0", root_pom).await;
        mount_pom(&server, "com.example", "lib", "7.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "root", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.artifact_id, "lib");
        assert_eq!(result.dependencies[0].coord.version, "7.0");
    }

    // 15. Relocation is followed
    #[tokio::test]
    async fn resolve_relocation_followed() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let root_pom = make_pom("com.example", "root", "1.0", &[
            ("com.example", "old-lib", "1.0", "compile"),
        ]);

        let old_lib_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>old-lib</artifactId>\
              <version>1.0</version>\
              <distributionManagement>\
                <relocation>\
                  <groupId>com.example</groupId>\
                  <artifactId>new-lib</artifactId>\
                  <version>2.0</version>\
                </relocation>\
              </distributionManagement>\
            </project>";

        let new_lib_pom = make_pom("com.example", "new-lib", "2.0", &[
            ("com.example", "util", "1.0", "compile"),
        ]);
        let util_pom = make_pom("com.example", "util", "1.0", &[]);

        mount_pom(&server, "com.example", "root", "1.0", &root_pom).await;
        mount_pom(&server, "com.example", "old-lib", "1.0", old_lib_pom).await;
        mount_pom(&server, "com.example", "new-lib", "2.0", &new_lib_pom).await;
        mount_pom(&server, "com.example", "util", "1.0", &util_pom).await;

        let coord = ArtifactCoord::new("com.example", "root", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        let ids: Vec<&str> = result.dependencies.iter().map(|d| d.coord.artifact_id.as_str()).collect();
        assert!(ids.contains(&"util"), "relocated lib's transitive deps should be resolved");
    }

    // 16. resolve_no_download skips JAR download
    #[tokio::test]
    async fn resolve_no_download_method() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let root_pom = make_pom("com.example", "root", "1.0", &[
            ("com.example", "lib", "1.0", "compile"),
        ]);
        let lib_pom = make_pom("com.example", "lib", "1.0", &[]);

        mount_pom(&server, "com.example", "root", "1.0", &root_pom).await;
        mount_pom(&server, "com.example", "lib", "1.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "root", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve_no_download(&coord, None).await.unwrap();

        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.artifact_id, "lib");
        assert!(result.dependencies[0].path.is_none());
    }

    // 17. check_relocation utility
    #[test]
    fn check_relocation_returns_relocated_coord() {
        let pom = pom::Pom {
            model_version: None,
            group_id: Some("old.group".into()),
            artifact_id: Some("old-artifact".into()),
            version: Some("1.0".into()),
            packaging: None,
            name: None,
            description: None,
            url: None,
            parent: None,
            properties: Default::default(),
            dependency_management: None,
            dependencies: Default::default(),
            repositories: Default::default(),
            distribution_management: Some(pom::DistributionManagement {
                relocation: Some(pom::Relocation {
                    group_id: Some("new.group".into()),
                    artifact_id: Some("new-artifact".into()),
                    version: Some("2.0".into()),
                    message: None,
                }),
            }),
        };

        let original = ArtifactCoord::new("old.group", "old-artifact", "1.0");
        let relocated = DependencyResolver::check_relocation(&pom, &original);
        assert!(relocated.is_some());
        let r = relocated.unwrap();
        assert_eq!(r.group_id, "new.group");
        assert_eq!(r.artifact_id, "new-artifact");
        assert_eq!(r.version, "2.0");
    }

    // 18. check_relocation ignores self-relocation
    #[test]
    fn check_relocation_ignores_self() {
        let pom = pom::Pom {
            model_version: None,
            group_id: Some("org".into()),
            artifact_id: Some("lib".into()),
            version: Some("1.0".into()),
            packaging: None,
            name: None,
            description: None,
            url: None,
            parent: None,
            properties: Default::default(),
            dependency_management: None,
            dependencies: Default::default(),
            repositories: Default::default(),
            distribution_management: Some(pom::DistributionManagement {
                relocation: Some(pom::Relocation {
                    group_id: Some("org".into()),
                    artifact_id: Some("lib".into()),
                    version: Some("1.0".into()),
                    message: None,
                }),
            }),
        };

        let original = ArtifactCoord::new("org", "lib", "1.0");
        assert!(DependencyResolver::check_relocation(&pom, &original).is_none());
    }

    // 19. check_relocation partial (only groupId changed)
    #[test]
    fn check_relocation_partial() {
        let pom = pom::Pom {
            model_version: None,
            group_id: Some("old.group".into()),
            artifact_id: Some("lib".into()),
            version: Some("1.0".into()),
            packaging: None,
            name: None,
            description: None,
            url: None,
            parent: None,
            properties: Default::default(),
            dependency_management: None,
            dependencies: Default::default(),
            repositories: Default::default(),
            distribution_management: Some(pom::DistributionManagement {
                relocation: Some(pom::Relocation {
                    group_id: Some("new.group".into()),
                    artifact_id: None,
                    version: None,
                    message: None,
                }),
            }),
        };

        let original = ArtifactCoord::new("old.group", "lib", "1.0");
        let relocated = DependencyResolver::check_relocation(&pom, &original).unwrap();
        assert_eq!(relocated.group_id, "new.group");
        assert_eq!(relocated.artifact_id, "lib");
        assert_eq!(relocated.version, "1.0");
    }

    // 20. build_managed_map extracts depMgmt entries
    #[test]
    fn build_managed_map_basic() {
        let pom = pom::Pom {
            model_version: None,
            group_id: Some("org".into()),
            artifact_id: Some("root".into()),
            version: Some("1.0".into()),
            packaging: None,
            name: None,
            description: None,
            url: None,
            parent: None,
            properties: Default::default(),
            dependency_management: Some(pom::DependencyManagement {
                dependencies: pom::Dependencies {
                    dependency: vec![
                        pom::PomDependency {
                            group_id: "com.example".into(),
                            artifact_id: "lib".into(),
                            version: Some("3.0".into()),
                            dep_type: None,
                            scope: None,
                            classifier: None,
                            optional: None,
                            exclusions: Default::default(),
                        },
                    ],
                },
            }),
            dependencies: Default::default(),
            repositories: Default::default(),
            distribution_management: None,
        };

        let map = DependencyResolver::build_managed_map(&pom);
        assert_eq!(map.len(), 1);
        let entry = map.get(&("com.example".into(), "lib".into())).unwrap();
        assert_eq!(entry.version, Some("3.0".into()));
    }

    // 21. build_managed_map returns empty when no depMgmt
    #[test]
    fn build_managed_map_empty() {
        let pom = pom::Pom {
            model_version: None,
            group_id: Some("org".into()),
            artifact_id: Some("root".into()),
            version: Some("1.0".into()),
            packaging: None,
            name: None,
            description: None,
            url: None,
            parent: None,
            properties: Default::default(),
            dependency_management: None,
            dependencies: Default::default(),
            repositories: Default::default(),
            distribution_management: None,
        };

        let map = DependencyResolver::build_managed_map(&pom);
        assert!(map.is_empty());
    }

    // 22. BOM import with child override takes precedence
    #[tokio::test]
    async fn resolve_bom_child_override_wins() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let bom_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>bom</artifactId>\
              <version>1.0</version>\
              <packaging>pom</packaging>\
              <dependencyManagement>\
                <dependencies>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>lib</artifactId>\
                    <version>1.0</version>\
                  </dependency>\
                </dependencies>\
              </dependencyManagement>\
            </project>";

        let root_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>root</artifactId>\
              <version>1.0</version>\
              <dependencyManagement>\
                <dependencies>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>lib</artifactId>\
                    <version>2.0</version>\
                  </dependency>\
                  <dependency>\
                    <groupId>com.example</groupId>\
                    <artifactId>bom</artifactId>\
                    <version>1.0</version>\
                    <type>pom</type>\
                    <scope>import</scope>\
                  </dependency>\
                </dependencies>\
              </dependencyManagement>\
              <dependencies>\
                <dependency>\
                  <groupId>com.example</groupId>\
                  <artifactId>lib</artifactId>\
                </dependency>\
              </dependencies>\
            </project>";

        let lib_pom = make_pom("com.example", "lib", "2.0", &[]);

        mount_pom(&server, "com.example", "bom", "1.0", bom_pom).await;
        mount_pom(&server, "com.example", "root", "1.0", root_pom).await;
        mount_pom(&server, "com.example", "lib", "2.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "root", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.version, "2.0");
    }

    // 23. Multi-level parent chain
    #[tokio::test]
    async fn resolve_grandparent_chain() {
        let (server, _tmp, downloader) = setup_resolver().await;

        let grandparent_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <groupId>com.example</groupId>\
              <artifactId>grandparent</artifactId>\
              <version>1.0</version>\
              <packaging>pom</packaging>\
              <properties>\
                <lib.version>8.0</lib.version>\
              </properties>\
            </project>";

        let parent_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <parent>\
                <groupId>com.example</groupId>\
                <artifactId>grandparent</artifactId>\
                <version>1.0</version>\
              </parent>\
              <groupId>com.example</groupId>\
              <artifactId>parent</artifactId>\
              <version>1.0</version>\
              <packaging>pom</packaging>\
            </project>";

        let child_pom = "\
            <project>\
              <modelVersion>4.0.0</modelVersion>\
              <parent>\
                <groupId>com.example</groupId>\
                <artifactId>parent</artifactId>\
                <version>1.0</version>\
              </parent>\
              <groupId>com.example</groupId>\
              <artifactId>child</artifactId>\
              <version>1.0</version>\
              <dependencies>\
                <dependency>\
                  <groupId>com.example</groupId>\
                  <artifactId>lib</artifactId>\
                  <version>${lib.version}</version>\
                </dependency>\
              </dependencies>\
            </project>";

        let lib_pom = make_pom("com.example", "lib", "8.0", &[]);

        mount_pom(&server, "com.example", "grandparent", "1.0", grandparent_pom).await;
        mount_pom(&server, "com.example", "parent", "1.0", parent_pom).await;
        mount_pom(&server, "com.example", "child", "1.0", child_pom).await;
        mount_pom(&server, "com.example", "lib", "8.0", &lib_pom).await;

        let coord = ArtifactCoord::new("com.example", "child", "1.0");
        let resolver = DependencyResolver::new(&downloader);
        let result = resolver.resolve(&coord, None).await.unwrap();

        assert_eq!(result.dependencies.len(), 1);
        assert_eq!(result.dependencies[0].coord.version, "8.0",
            "property from grandparent should be inherited through chain");
    }
}
