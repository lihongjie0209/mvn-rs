//! Dependency resolution engine.
//!
//! Implements Maven-style dependency resolution in three phases:
//! 1. **Collect** – recursively fetch POMs and build a dependency tree
//! 2. **Flatten** – BFS "nearest wins" conflict resolution
//! 3. **Download** – fetch resolved artifact files

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use tracing::warn;

use crate::coord::{ArtifactCoord, DependencyScope, Exclusion};
use crate::downloader::ArtifactDownloader;
use crate::error::Result;
use crate::pom;

/// Maximum recursion depth for transitive dependency collection.
const MAX_DEPTH: usize = 50;

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
pub struct DependencyResolver<'a> {
    downloader: &'a ArtifactDownloader,
}

impl<'a> DependencyResolver<'a> {
    pub fn new(downloader: &'a ArtifactDownloader) -> Self {
        Self { downloader }
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

    // -- Phase 1: Collect ---------------------------------------------------

    /// Build the dependency tree by recursively fetching POMs.
    async fn collect(&self, coord: &ArtifactCoord) -> Result<Vec<DependencyNode>> {
        let mut visited = HashSet::new();
        visited.insert((coord.group_id.clone(), coord.artifact_id.clone()));

        let pom = self.fetch_and_prepare_pom(coord).await?;
        self.collect_children(pom, Vec::new(), &mut visited, 0)
            .await
    }

    /// Recursively collect dependency nodes for a prepared POM.
    ///
    /// Uses manual `Pin<Box<…>>` to support async recursion.
    fn collect_children<'s, 'v>(
        &'s self,
        pom: pom::Pom,
        parent_exclusions: Vec<Exclusion>,
        visited: &'v mut HashSet<(String, String)>,
        depth: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<DependencyNode>>> + Send + 'v>>
    where
        's: 'v,
    {
        Box::pin(async move {
            if depth >= MAX_DEPTH {
                warn!("maximum dependency depth ({MAX_DEPTH}) reached; truncating");
                return Ok(Vec::new());
            }

            // -- Pass 1: filter deps & mark visited (sequential, cheap) -----
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

                let version = match &dep.version {
                    Some(v) if !v.is_empty() => v.clone(),
                    _ => {
                        warn!(
                            "skipping {}:{} — no version specified",
                            dep.group_id, dep.artifact_id
                        );
                        continue;
                    }
                };

                let extension = dep.dep_type.as_deref().unwrap_or("jar");
                let mut dep_coord =
                    ArtifactCoord::new(&dep.group_id, &dep.artifact_id, &version);
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
                let needs_recurse = visited.insert(key);

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
            let mut pom_map: std::collections::HashMap<String, Result<pom::Pom>> =
                pom_results
                    .into_iter()
                    .map(|(coord, result)| (coord.to_string(), result))
                    .collect();

            // -- Pass 3: build nodes, recurse into children sequentially ----
            let mut nodes = Vec::new();

            for info in dep_infos {
                if !info.needs_recurse {
                    nodes.push(DependencyNode {
                        coord: info.coord,
                        scope: info.scope,
                        optional: false,
                        exclusions: Vec::new(),
                        children: Vec::new(),
                    });
                    continue;
                }

                let children = match pom_map.remove(&info.coord.to_string()) {
                    Some(Ok(raw_pom)) => {
                        let mut child_pom = raw_pom;
                        pom::interpolate_pom(&mut child_pom);
                        pom::inject_dependency_management(&mut child_pom);
                        self.collect_children(
                            child_pom,
                            info.child_exclusions,
                            visited,
                            depth + 1,
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
                    Some(Err(e)) => {
                        warn!("failed to fetch POM for {}: {e}", info.coord);
                        Vec::new()
                    }
                    None => {
                        warn!("POM result missing for {}", info.coord);
                        Vec::new()
                    }
                };

                nodes.push(DependencyNode {
                    coord: info.coord,
                    scope: info.scope,
                    optional: false,
                    exclusions: info.exclusions,
                    children,
                });
            }

            Ok(nodes)
        })
    }

    /// Fetch a POM and apply interpolation + dependency management injection.
    async fn fetch_and_prepare_pom(&self, coord: &ArtifactCoord) -> Result<pom::Pom> {
        let mut pom = self.downloader.fetch_pom(coord).await?;
        pom::interpolate_pom(&mut pom);
        pom::inject_dependency_management(&mut pom);
        Ok(pom)
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
    async fn download_all(&self, deps: &mut [ResolvedDependency]) -> Result<()> {
        let coords: Vec<_> = deps.iter().map(|d| d.coord.clone()).collect();
        let results = self.downloader.download_artifacts(&coords).await;
        for (i, (_coord, result)) in results.into_iter().enumerate() {
            match result {
                Ok(path) => deps[i].path = Some(path),
                Err(e) => tracing::warn!("failed to download {}: {}", deps[i].coord, e),
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
}
