use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use comfy_table::{presets::UTF8_FULL_CONDENSED, ContentArrangement, Table};
use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::Semaphore;

use mvn_core::coord::{ArtifactCoord, DependencyScope};
use mvn_core::downloader::{ArtifactDownloader, RetryConfig};
use mvn_core::repository::RemoteRepository;
use mvn_core::resolver::{format_tree, DependencyResolver};
use mvn_core::settings;
use mvn_core::uploader::ArtifactUploader;

// ---------------------------------------------------------------------------
// Global MultiProgress handle for routing tracing output through indicatif
// ---------------------------------------------------------------------------

/// Global multi-progress handle.  When set, the tracing subscriber routes
/// all log output through `mp.println()` so warnings scroll below the
/// progress bars instead of overwriting them.
static GLOBAL_MP: OnceLock<MultiProgress> = OnceLock::new();

/// A writer that routes output through the global [`MultiProgress`] when
/// available, falling back to stderr otherwise.
#[derive(Clone)]
struct IndicatifWriter;

impl std::io::Write for IndicatifWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(mp) = GLOBAL_MP.get() {
            let s = String::from_utf8_lossy(buf);
            let s = s.trim_end_matches('\n');
            if !s.is_empty() {
                let _ = mp.println(s);
            }
        } else {
            std::io::stderr().write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for IndicatifWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Maximum concurrent artifact downloads.
const MAX_DOWNLOAD_CONCURRENCY: usize = 8;

/// Resolve dependencies (shared between `deps` and `download` commands).
async fn resolve_deps(
    downloader: &ArtifactDownloader,
    coord: &ArtifactCoord,
    scope_filter: Option<DependencyScope>,
) -> Result<mvn_core::resolver::ResolutionResult> {
    let resolver = DependencyResolver::new(downloader);
    resolver
        .resolve_no_download(coord, scope_filter)
        .await
        .context("dependency resolution failed")
}

#[derive(Parser)]
#[command(name = "mvn-rs", version, about = "Maven dependency resolver and downloader in Rust")]
struct Cli {
    /// Path to a custom settings.xml file (default: ~/.m2/settings.xml)
    #[arg(long, global = true)]
    settings: Option<String>,

    /// Maximum number of retry attempts for failed downloads (default: 3)
    #[arg(long, global = true, default_value = "3")]
    retries: u32,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show artifact information (POM details)
    Info {
        /// Artifact coordinates (groupId:artifactId:version)
        coord: String,
    },
    /// Show dependency tree or list
    Deps {
        /// Artifact coordinates (groupId:artifactId:version)
        coord: String,
        /// Show dependency tree instead of flat list
        #[arg(long)]
        tree: bool,
        /// Filter by scope (compile, runtime, test, all)
        #[arg(long, default_value = "all")]
        scope: String,
    },
    /// Download artifact and all transitive dependencies
    Download {
        /// Artifact coordinates (groupId:artifactId:version), can specify multiple
        coords: Vec<String>,
        /// File containing coordinates, one per line
        #[arg(long, short = 'f')]
        file: Option<String>,
        /// Skip downloading transitive dependencies (only download the artifact itself)
        #[arg(long)]
        no_deps: bool,
        /// Copy downloaded files to this directory
        #[arg(long)]
        output: Option<String>,
        /// Scope filter for dependency downloads (compile, runtime, test, all)
        #[arg(long, default_value = "compile")]
        scope: String,
    },
    /// Search available versions for an artifact
    Search {
        /// Artifact coordinates (groupId:artifactId, version optional)
        coord: String,
    },
    /// Upload artifact and dependencies to a remote repository
    Upload {
        /// Artifact coordinates (groupId:artifactId:version), can specify multiple
        coords: Vec<String>,
        /// File containing coordinates, one per line
        #[arg(long, short = 'f')]
        file: Option<String>,
        /// Target repository URL (e.g. http://localhost:8081/repository/maven-releases/)
        #[arg(long)]
        repo_url: Option<String>,
        /// Repository ID — used to look up credentials in settings.xml
        #[arg(long)]
        repo_id: Option<String>,
        /// Username for the target repository (overrides settings.xml)
        #[arg(long)]
        username: Option<String>,
        /// Password for the target repository (overrides settings.xml)
        #[arg(long)]
        password: Option<String>,
        /// Skip uploading transitive dependencies (only upload the artifact itself)
        #[arg(long)]
        no_deps: bool,
        /// Scope filter for dependency uploads (compile, runtime, test, all)
        #[arg(long, default_value = "compile")]
        scope: String,
        /// Also update remote maven-metadata.xml
        #[arg(long)]
        update_metadata: bool,
    },
}

fn parse_coord(s: &str) -> Result<ArtifactCoord> {
    ArtifactCoord::from_str(s).map_err(|e| anyhow::anyhow!(e))
}

fn parse_scope_filter(s: &str) -> Result<Option<DependencyScope>> {
    if s.eq_ignore_ascii_case("all") {
        Ok(None)
    } else {
        let scope = DependencyScope::from_str(s).map_err(|e| anyhow::anyhow!(e))?;
        Ok(Some(scope))
    }
}

/// Collect coordinates from positional args + optional `--file` (one per line).
fn collect_coords(args: &[String], file: Option<&str>) -> Result<Vec<ArtifactCoord>> {
    let mut coords = Vec::new();

    for s in args {
        coords.push(parse_coord(s)?);
    }

    if let Some(path) = file {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read coordinate file '{path}'"))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            coords.push(
                parse_coord(trimmed)
                    .with_context(|| format!("invalid coordinate in file: '{trimmed}'"))?,
            );
        }
    }

    if coords.is_empty() {
        anyhow::bail!("no coordinates provided — pass them as arguments or via --file");
    }
    Ok(coords)
}

fn spinner(msg: &str) -> ProgressBar {
    let pb = if let Some(mp) = GLOBAL_MP.get() {
        mp.add(ProgressBar::new_spinner())
    } else {
        ProgressBar::new_spinner()
    };
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

/// Colorize a scope string based on the dependency scope.
fn colored_scope_str(scope: &DependencyScope) -> String {
    let label = scope.to_string();
    match scope {
        DependencyScope::Compile => label.green().to_string(),
        DependencyScope::Runtime => label.yellow().to_string(),
        DependencyScope::Test => label.cyan().to_string(),
        DependencyScope::Provided => label.magenta().to_string(),
        DependencyScope::System => label.red().to_string(),
        DependencyScope::Import => label.dimmed().to_string(),
    }
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

async fn cmd_info(downloader: &ArtifactDownloader, coord_str: &str) -> Result<()> {
    let coord = parse_coord(coord_str).context("invalid coordinates")?;

    let pb = spinner("Fetching POM...");
    let pom = downloader
        .fetch_pom(&coord)
        .await
        .context("failed to fetch POM")?;
    pb.finish_and_clear();

    println!("\n{}\n", "📦 Artifact Information".bold());

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);

    table.add_row(vec!["Group ID", &coord.group_id]);
    table.add_row(vec!["Artifact ID", &coord.artifact_id]);
    table.add_row(vec!["Version", &coord.version]);

    let packaging = pom.effective_packaging();
    table.add_row(vec!["Packaging", &packaging]);

    if let Some(name) = &pom.name {
        table.add_row(vec!["Name", name]);
    }
    if let Some(desc) = &pom.description {
        let desc = desc.trim();
        if !desc.is_empty() {
            let truncated;
            let display_desc = if desc.len() > 100 {
                truncated = format!("{}...", &desc[..100]);
                &truncated
            } else {
                desc
            };
            table.add_row(vec!["Description", display_desc]);
        }
    }
    if let Some(url) = &pom.url {
        table.add_row(vec!["URL", url]);
    }
    if let Some(parent) = &pom.parent {
        let parent_coord = format!(
            "{}:{}:{}",
            parent.group_id, parent.artifact_id, parent.version
        );
        table.add_row(vec!["Parent".to_string(), parent_coord]);
    }

    let dep_count = pom.dependencies.dependency.len().to_string();
    table.add_row(vec!["Dependencies", &dep_count]);

    println!("{table}");

    if !pom.repositories.repository.is_empty() {
        println!("\n  {}:", "Repositories".bold());
        for repo in &pom.repositories.repository {
            let id = repo.id.as_deref().unwrap_or("(none)");
            println!("    - {id}: {}", repo.url);
        }
    }

    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// deps
// ---------------------------------------------------------------------------

async fn cmd_deps(
    downloader: &ArtifactDownloader,
    coord_str: &str,
    tree: bool,
    scope_str: &str,
) -> Result<()> {
    let coord = parse_coord(coord_str).context("invalid coordinates")?;
    let scope_filter = parse_scope_filter(scope_str).context("invalid scope")?;

    let pb = spinner("Resolving dependencies...");
    let result = resolve_deps(downloader, &coord, scope_filter).await?;
    pb.finish_and_clear();

    if tree {
        println!("\n{}\n", "🌳 Dependency Tree".bold());
        print!("{}", format_tree(&result.root, &result.tree));
    } else {
        println!("\n{}\n", "🌳 Dependencies".bold());

        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL_CONDENSED)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["#", "Artifact", "Scope"]);

        for (i, dep) in result.dependencies.iter().enumerate() {
            let coord_str = format!(
                "{}:{}:{}",
                dep.coord.group_id, dep.coord.artifact_id, dep.coord.version
            );
            table.add_row(vec![
                (i + 1).to_string(),
                coord_str,
                colored_scope_str(&dep.scope),
            ]);
        }

        println!("{table}");
    }

    let n = result.dependencies.len();
    println!(
        "\n{}",
        format!("✅ {n} dependencies resolved").green().bold()
    );
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------

async fn cmd_download(
    downloader: &ArtifactDownloader,
    coords: &[ArtifactCoord],
    no_deps: bool,
    output: Option<&str>,
    scope_str: &str,
) -> Result<()> {
    // Simple single-artifact download (--no-deps)
    if no_deps {
        let mp = GLOBAL_MP.get().cloned().unwrap_or_else(MultiProgress::new);
        let main_bar = mp.add(ProgressBar::new(coords.len() as u64));
        main_bar.set_style(
            ProgressStyle::with_template("  {bar:40.green/dark_gray} {pos}/{len}  {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("━━─"),
        );
        let mut paths = Vec::new();
        for coord in coords {
            main_bar.set_message(coord.to_string());
            let path = downloader
                .download_artifact(coord)
                .await
                .with_context(|| format!("failed to download {coord}"))?;
            paths.push(path);
            main_bar.inc(1);
        }
        main_bar.finish_and_clear();
        println!("{}", "✅ Download complete".green().bold());
        maybe_copy_files(&paths, output)?;
        if output.is_none() {
            for p in &paths {
                println!("  {}", p.display());
            }
        }
        println!();
        return Ok(());
    }

    // ----- Full dependency download with real-time progress -----

    let scope_filter = parse_scope_filter(scope_str).context("invalid scope")?;

    // Phase 1: Resolve dependencies for ALL requested coordinates
    let pb = spinner("Resolving dependencies...");
    let mut all_download_coords: Vec<ArtifactCoord> = Vec::new();
    let mut pom_only_coords: Vec<ArtifactCoord> = Vec::new();
    let mut root_coords: Vec<ArtifactCoord> = Vec::new();

    for coord in coords {
        let result = resolve_deps(downloader, coord, scope_filter).await?;
        root_coords.push(coord.clone());
        for d in &result.dependencies {
            if d.coord.extension == "pom" {
                if !pom_only_coords.iter().any(|c| c == &d.coord) {
                    pom_only_coords.push(d.coord.clone());
                }
            } else if !all_download_coords.iter().any(|c| c == &d.coord) {
                all_download_coords.push(d.coord.clone());
            }
        }
    }
    pb.finish_and_clear();

    // Include root artifacts + deduplicated deps
    let mut combined: Vec<ArtifactCoord> = Vec::new();
    for rc in &root_coords {
        if !combined.iter().any(|c| c == rc) {
            combined.push(rc.clone());
        }
    }
    for dc in &all_download_coords {
        if !combined.iter().any(|c| c == dc) {
            combined.push(dc.clone());
        }
    }
    let total = combined.len();

    // Phase 2: Build progress display
    //
    //  📦 com.google.guava:guava:33.4.0-jre (+7 dependencies)
    //  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 7/7
    //  resolved 7, reused 5, downloaded 2 (1.2 MB), added 7, done
    //  (warnings scroll below)
    //
    let mp = GLOBAL_MP.get().cloned().unwrap_or_else(MultiProgress::new);

    // Header (static)
    let header = mp.add(ProgressBar::new(0));
    header.set_style(
        ProgressStyle::with_template("{msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    let header_msg = if root_coords.len() == 1 {
        format!(
            "\n  {} {} {}",
            "📦".to_string(),
            root_coords[0].to_string().bold(),
            format!("(+{} dependencies)", total - 1).dimmed(),
        )
    } else {
        format!(
            "\n  {} {} artifacts {}",
            "📦".to_string(),
            root_coords.len().to_string().bold(),
            format!("({} total to download)", total).dimmed(),
        )
    };
    header.set_message(header_msg);
    header.finish();

    // Main progress bar
    let main_bar = mp.add(ProgressBar::new(total as u64));
    main_bar.set_style(
        ProgressStyle::with_template(
            "  {bar:40.green/dark_gray} {pos}/{len}  {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("━━─"),
    );
    main_bar.set_message("");

    // Stats line (updated in real-time)
    let stats_bar = mp.add(ProgressBar::new(0));
    stats_bar.set_style(
        ProgressStyle::with_template("  {msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    stats_bar.set_message("waiting...");

    // Spacer between stats and active downloads
    let spacer = mp.add(ProgressBar::new(0));
    spacer.set_style(
        ProgressStyle::with_template("{msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spacer.set_message("");
    spacer.finish();

    // Shared counters
    let reused = Arc::new(AtomicUsize::new(0));
    let downloaded = Arc::new(AtomicUsize::new(0));
    let dl_failed = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));

    // Closure to refresh the stats line
    let update_stats = {
        let stats_bar = stats_bar.clone();
        let reused = reused.clone();
        let downloaded = downloaded.clone();
        let dl_failed = dl_failed.clone();
        let added = added.clone();
        let total_bytes = total_bytes.clone();
        move || {
            let r = reused.load(Ordering::Relaxed);
            let d = downloaded.load(Ordering::Relaxed);
            let f = dl_failed.load(Ordering::Relaxed);
            let a = added.load(Ordering::Relaxed);
            let b = total_bytes.load(Ordering::Relaxed);
            let done = r + d + f;

            let mut parts = Vec::new();
            parts.push(format!("resolved {}", total.to_string().cyan()));
            if r > 0 {
                parts.push(format!("reused {}", r.to_string().blue()));
            }
            if d > 0 || f == 0 {
                parts.push(format!(
                    "downloaded {} ({})",
                    d.to_string().green(),
                    HumanBytes(b)
                ));
            }
            if f > 0 {
                parts.push(format!("failed {}", f.to_string().red()));
            }
            parts.push(format!("added {}", a.to_string().green()));
            if done == total {
                parts.push("done".green().bold().to_string());
            }

            stats_bar.set_message(parts.join(", "));
        }
    };

    update_stats();

    // Phase 3: Download concurrently with per-artifact spinners
    let semaphore = Arc::new(Semaphore::new(MAX_DOWNLOAD_CONCURRENCY));
    let mut futures = FuturesUnordered::new();

    for dl_coord in combined {
        let sem = semaphore.clone();
        let reused = reused.clone();
        let downloaded = downloaded.clone();
        let dl_failed = dl_failed.clone();
        let added = added.clone();
        let total_bytes = total_bytes.clone();
        let main_bar = main_bar.clone();
        let mp = mp.clone();
        let update_stats = update_stats.clone();
        let spacer = spacer.clone();

        futures.push(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            // Show active spinner for this artifact
            let spin = mp.insert_before(&spacer, ProgressBar::new_spinner());
            spin.set_style(
                ProgressStyle::with_template("    {spinner:.cyan} {msg}")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner())
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            let short_name = format!(
                "{}:{}:{}",
                dl_coord.group_id, dl_coord.artifact_id, dl_coord.version
            );
            spin.set_message(short_name.clone());
            spin.enable_steady_tick(std::time::Duration::from_millis(80));

            // Check local cache
            let was_cached = downloader
                .repo_system()
                .local
                .artifact_path(&dl_coord)
                .exists();

            let result = downloader.download_artifact(&dl_coord).await;

            // Remove spinner
            spin.finish_and_clear();
            mp.remove(&spin);

            // Update counters
            match &result {
                Ok(path) => {
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    if was_cached {
                        reused.fetch_add(1, Ordering::Relaxed);
                    } else {
                        downloaded.fetch_add(1, Ordering::Relaxed);
                        total_bytes.fetch_add(size, Ordering::Relaxed);
                    }
                    added.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    dl_failed.fetch_add(1, Ordering::Relaxed);
                }
            }

            main_bar.inc(1);
            update_stats();

            (dl_coord, result)
        });
    }

    let mut downloaded_paths: Vec<PathBuf> = Vec::new();

    while let Some((dl_coord, result)) = futures.next().await {
        match result {
            Ok(path) => {
                downloaded_paths.push(path);
                // Also include the POM file alongside the artifact
                let pom_path = downloader.repo_system().local.pom_path(&dl_coord);
                if pom_path.exists() {
                    downloaded_paths.push(pom_path);
                }
            }
            Err(e) => {
                tracing::warn!("failed to download {}: {}", dl_coord, e);
            }
        }
    }

    // Finish bars
    main_bar.finish();
    stats_bar.finish();
    spacer.finish_and_clear();

    // Include POM files for pom-only dependencies (already in local repo from resolution)
    for pc in &pom_only_coords {
        let pom_path = downloader.repo_system().local.pom_path(pc);
        if pom_path.exists() {
            downloaded_paths.push(pom_path);
        }
    }

    println!();

    // Phase 4: Copy to output directory if requested
    maybe_copy_files(&downloaded_paths, output)?;

    println!();
    Ok(())
}

/// Copy downloaded files to an output directory if specified.
fn maybe_copy_files(paths: &[PathBuf], output: Option<&str>) -> Result<()> {
    let Some(output_dir) = output else {
        return Ok(());
    };
    let dest = Path::new(output_dir);
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create output directory '{output_dir}'"))?;

    let copy_pb = ProgressBar::new(paths.len() as u64);
    copy_pb.set_style(
        ProgressStyle::with_template("  [{bar:30.cyan/dim}] {pos}/{len} copied")
            .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );

    for src in paths {
        if let Some(file_name) = src.file_name() {
            let dst = dest.join(file_name);
            std::fs::copy(src, &dst).with_context(|| {
                format!("failed to copy {} to {}", src.display(), dst.display())
            })?;
        }
        copy_pb.inc(1);
    }
    copy_pb.finish_and_clear();
    println!(
        "{}",
        format!(
            "  📁 Copied {} files to {}",
            paths.len(),
            dest.display()
        )
        .green()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

async fn cmd_search(downloader: &ArtifactDownloader, coord_str: &str) -> Result<()> {
    // Parse at least groupId:artifactId (version is optional/ignored)
    let parts: Vec<&str> = coord_str.split(':').collect();
    if parts.len() < 2 {
        anyhow::bail!("expected at least groupId:artifactId, got '{coord_str}'");
    }
    let group_id = parts[0];
    let artifact_id = parts[1];

    let pb = spinner("Fetching metadata...");
    let metadata = downloader
        .fetch_metadata(group_id, artifact_id)
        .await
        .context("failed to fetch metadata")?;
    pb.finish_and_clear();

    let versions = metadata.available_versions();
    let latest = metadata.latest_release();

    println!(
        "\n{}\n",
        format!("🔍 Versions for {group_id}:{artifact_id}").bold()
    );

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Version"]);

    for v in &versions {
        if latest == Some(*v) {
            table.add_row(vec![format!("→ {} (latest release)", v).green().bold().to_string()]);
        } else {
            table.add_row(vec![v.to_string()]);
        }
    }

    println!("{table}");

    println!(
        "\n  {} version(s) available",
        versions.len().to_string().bold()
    );
    if let Some(rel) = latest {
        println!("  Latest release: {}", rel.to_string().green().bold());
    }

    println!();
    Ok(())
}

/// Build an `ArtifactDownloader` from CLI flags.
fn build_downloader(cli: &Cli) -> anyhow::Result<ArtifactDownloader> {
    let settings_path = cli.settings.as_ref().map(|s| Path::new(s.as_str()));
    let settings = settings::load_settings(settings_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let retry_config = RetryConfig {
        max_retries: cli.retries,
        ..RetryConfig::default()
    };

    Ok(ArtifactDownloader::from_settings_with_retry(&settings, retry_config))
}

/// Build a target `RemoteRepository` for upload from CLI flags + settings.
fn build_upload_target(
    cli: &Cli,
    repo_url: Option<&str>,
    repo_id: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> anyhow::Result<RemoteRepository> {
    let settings_path = cli.settings.as_ref().map(|s| Path::new(s.as_str()));
    let settings = settings::load_settings(settings_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let url = repo_url
        .ok_or_else(|| anyhow::anyhow!("--repo-url is required for upload"))?;

    let id = repo_id.unwrap_or("upload-target");

    // CLI credentials override settings.xml
    let (user, pass) = match (username, password) {
        (Some(u), Some(p)) => (Some(u.to_string()), Some(p.to_string())),
        _ => {
            // Look up in settings.xml by repo ID
            if let Some(server) = settings.find_server(id) {
                (server.username.clone(), server.password.clone())
            } else {
                (None, None)
            }
        }
    };

    let mut repo = RemoteRepository::new(id, url);
    repo.username = user;
    repo.password = pass;
    Ok(repo)
}

// ---------------------------------------------------------------------------
// upload
// ---------------------------------------------------------------------------

async fn cmd_upload(
    downloader: &ArtifactDownloader,
    cli: &Cli,
    coords: &[ArtifactCoord],
    repo_url: Option<&str>,
    repo_id: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
    no_deps: bool,
    scope_str: &str,
    update_metadata: bool,
) -> Result<()> {
    let target = build_upload_target(cli, repo_url, repo_id, username, password)?;

    let settings_path = cli.settings.as_ref().map(|s| Path::new(s.as_str()));
    let settings = settings::load_settings(settings_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let uploader = Arc::new(ArtifactUploader::from_settings(&settings));

    // Phase 1: Ensure artifacts are in local repo (download if needed)
    let mut upload_coords: Vec<ArtifactCoord> = Vec::new();

    if no_deps {
        // Just the root artifacts
        for coord in coords {
            if !uploader.local_repo().has_artifact(coord) {
                let pb = spinner(&format!("Downloading {} to local repo...", coord));
                downloader.download_artifact(coord).await
                    .with_context(|| format!("failed to download {coord}"))?;
                pb.finish_and_clear();
            }
            if !upload_coords.iter().any(|c| c == coord) {
                upload_coords.push(coord.clone());
            }
        }
    } else {
        // Resolve + download dependencies first
        let scope_filter = parse_scope_filter(scope_str).context("invalid scope")?;

        let pb = spinner("Resolving dependencies...");
        for coord in coords {
            let result = resolve_deps(downloader, coord, scope_filter).await?;
            if !upload_coords.iter().any(|c| c == coord) {
                upload_coords.push(coord.clone());
            }
            for d in &result.dependencies {
                if d.coord.extension != "pom" && !upload_coords.iter().any(|c| c == &d.coord) {
                    upload_coords.push(d.coord.clone());
                }
            }
        }
        pb.finish_and_clear();

        // Ensure all are in local repo
        let missing: Vec<_> = upload_coords
            .iter()
            .filter(|c| !uploader.local_repo().artifact_path(c).exists())
            .cloned()
            .collect();
        if !missing.is_empty() {
            let pb = spinner(&format!("Downloading {} missing artifacts...", missing.len()));
            for coord in &missing {
                let _ = downloader.download_artifact(coord).await;
            }
            pb.finish_and_clear();
        }
    }

    let total = upload_coords.len();

    // Phase 2: Upload with progress
    let mp = GLOBAL_MP.get().cloned().unwrap_or_else(MultiProgress::new);

    let header = mp.add(ProgressBar::new(0));
    header.set_style(
        ProgressStyle::with_template("{msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    header.set_message(format!(
        "\n  {} Uploading {} artifact(s) to {}",
        "⬆️ ".to_string(),
        total.to_string().bold(),
        target.url.bold(),
    ));
    header.finish();

    let main_bar = mp.add(ProgressBar::new(total as u64));
    main_bar.set_style(
        ProgressStyle::with_template("  {bar:40.cyan/dark_gray} {pos}/{len}  {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("━━─"),
    );

    let stats_bar = mp.add(ProgressBar::new(0));
    stats_bar.set_style(
        ProgressStyle::with_template("  {msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    stats_bar.set_message("waiting...");

    let spacer = mp.add(ProgressBar::new(0));
    spacer.set_style(
        ProgressStyle::with_template("{msg}").unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    spacer.set_message("");
    spacer.finish();

    let uploaded_count = Arc::new(AtomicUsize::new(0));
    let failed_count = Arc::new(AtomicUsize::new(0));
    let skipped_count = Arc::new(AtomicUsize::new(0));

    let update_stats = {
        let stats_bar = stats_bar.clone();
        let uploaded_count = uploaded_count.clone();
        let failed_count = failed_count.clone();
        let skipped_count = skipped_count.clone();
        move || {
            let u = uploaded_count.load(Ordering::Relaxed);
            let f = failed_count.load(Ordering::Relaxed);
            let s = skipped_count.load(Ordering::Relaxed);
            let done = u + f + s;

            let mut parts = Vec::new();
            if u > 0 {
                parts.push(format!("uploaded {}", u.to_string().green()));
            }
            if s > 0 {
                parts.push(format!("skipped {}", s.to_string().blue()));
            }
            if f > 0 {
                parts.push(format!("failed {}", f.to_string().red()));
            }
            if done == total {
                parts.push("done".green().bold().to_string());
            }
            if parts.is_empty() {
                stats_bar.set_message("uploading...");
            } else {
                stats_bar.set_message(parts.join(", "));
            }
        }
    };
    update_stats();

    let semaphore = Arc::new(Semaphore::new(MAX_DOWNLOAD_CONCURRENCY));
    let mut futures = FuturesUnordered::new();

    for coord in upload_coords.clone() {
        let sem = semaphore.clone();
        let uploaded_count = uploaded_count.clone();
        let failed_count = failed_count.clone();
        let main_bar = main_bar.clone();
        let mp = mp.clone();
        let update_stats = update_stats.clone();
        let spacer = spacer.clone();
        let target = target.clone();
        let uploader = uploader.clone();

        futures.push(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            let spin = mp.insert_before(&spacer, ProgressBar::new_spinner());
            spin.set_style(
                ProgressStyle::with_template("    {spinner:.cyan} {msg}")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner())
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            spin.set_message(format!("↑ {}", coord));
            spin.enable_steady_tick(std::time::Duration::from_millis(80));

            let result = uploader.upload_artifact(&coord, &target).await;

            spin.finish_and_clear();
            mp.remove(&spin);

            match &result {
                Ok(_) => { uploaded_count.fetch_add(1, Ordering::Relaxed); }
                Err(_) => { failed_count.fetch_add(1, Ordering::Relaxed); }
            }

            main_bar.inc(1);
            update_stats();

            (coord, result)
        });
    }

    let mut upload_results = Vec::new();
    while let Some((coord, result)) = futures.next().await {
        match &result {
            Ok(ur) => {
                tracing::debug!("uploaded {} ({} files)", coord, ur.uploaded_files.len());
            }
            Err(e) => {
                eprintln!("  ⚠ failed to upload {}: {}", coord, e);
            }
        }
        upload_results.push((coord, result));
    }

    main_bar.finish();
    stats_bar.finish();
    spacer.finish_and_clear();

    // Phase 3: Update remote metadata if requested
    if update_metadata {
        let pb = spinner("Updating remote metadata...");
        for coord in &upload_coords {
            if let Err(e) = uploader.update_remote_metadata(coord, &target).await {
                tracing::warn!("failed to update metadata for {}: {}", coord, e);
            }
        }
        pb.finish_and_clear();
    }

    // Summary
    let u = uploaded_count.load(Ordering::Relaxed);
    let f = failed_count.load(Ordering::Relaxed);
    println!();
    if f == 0 {
        println!(
            "{}",
            format!("  ✅ {u} artifact(s) uploaded to {}", target.url)
                .green()
                .bold()
        );
    } else {
        println!(
            "{}",
            format!("  ⚠️  {u} uploaded, {f} failed").yellow().bold()
        );
    }
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Initialise the global MultiProgress *before* tracing so that any
    // warn! / info! output from the core crate is routed through it.
    let mp = MultiProgress::new();
    let _ = GLOBAL_MP.set(mp);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(IndicatifWriter)
        .with_ansi(true)
        .init();

    let cli = Cli::parse();

    let downloader = match build_downloader(&cli) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", format!("Error: {e:#}").red());
            std::process::exit(1);
        }
    };

    let result = match &cli.command {
        Commands::Info { coord } => cmd_info(&downloader, coord).await,
        Commands::Deps { coord, tree, scope } => cmd_deps(&downloader, coord, *tree, scope).await,
        Commands::Download {
            coords,
            file,
            no_deps,
            output,
            scope,
        } => {
            let parsed = collect_coords(coords, file.as_deref());
            match parsed {
                Ok(coord_list) => {
                    cmd_download(&downloader, &coord_list, *no_deps, output.as_deref(), scope).await
                }
                Err(e) => Err(e),
            }
        }
        Commands::Search { coord } => cmd_search(&downloader, coord).await,
        Commands::Upload {
            coords,
            file,
            repo_url,
            repo_id,
            username,
            password,
            no_deps,
            scope,
            update_metadata,
        } => {
            let parsed = collect_coords(coords, file.as_deref());
            match parsed {
                Ok(coord_list) => {
                    cmd_upload(
                        &downloader,
                        &cli,
                        &coord_list,
                        repo_url.as_deref(),
                        repo_id.as_deref(),
                        username.as_deref(),
                        password.as_deref(),
                        *no_deps,
                        scope,
                        *update_metadata,
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        }
    };

    if let Err(e) = result {
        eprintln!("{}", format!("Error: {e:#}").red());
        std::process::exit(1);
    }
}
