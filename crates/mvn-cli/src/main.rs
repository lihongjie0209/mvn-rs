use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use comfy_table::{presets::UTF8_FULL_CONDENSED, ContentArrangement, Table};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use mvn_core::coord::{ArtifactCoord, DependencyScope};
use mvn_core::downloader::{ArtifactDownloader, RetryConfig};
use mvn_core::resolver::{format_tree, DependencyResolver};
use mvn_core::settings;

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
    /// Download artifact (and optionally its dependencies)
    Download {
        /// Artifact coordinates (groupId:artifactId:version)
        coord: String,
        /// Also download all transitive dependencies
        #[arg(long)]
        with_deps: bool,
        /// Copy downloaded files to this directory
        #[arg(long)]
        output: Option<String>,
        /// Scope filter when using --with-deps
        #[arg(long, default_value = "compile")]
        scope: String,
    },
    /// Search available versions for an artifact
    Search {
        /// Artifact coordinates (groupId:artifactId, version optional)
        coord: String,
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

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
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
    let resolver = DependencyResolver::new(downloader);
    let result = resolver
        .resolve(&coord, scope_filter)
        .await
        .context("dependency resolution failed")?;
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
    coord_str: &str,
    with_deps: bool,
    output: Option<&str>,
    scope_str: &str,
) -> Result<()> {
    let coord = parse_coord(coord_str).context("invalid coordinates")?;

    let mut downloaded_paths: Vec<PathBuf> = Vec::new();

    if with_deps {
        let scope_filter = parse_scope_filter(scope_str).context("invalid scope")?;

        let pb = spinner("Resolving dependencies...");
        let resolver = DependencyResolver::new(downloader);
        let result = resolver
            .resolve(&coord, scope_filter)
            .await
            .context("dependency resolution failed")?;
        pb.finish_and_clear();

        // Collect already-downloaded dependency paths
        for dep in &result.dependencies {
            if let Some(path) = &dep.path {
                downloaded_paths.push(path.clone());
            }
        }

        // Download root artifact with progress
        let multi = MultiProgress::new();
        let main_pb = multi.add(ProgressBar::new(1));
        main_pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        main_pb.set_message(format!("⬇️  Downloading {coord}..."));
        main_pb.enable_steady_tick(std::time::Duration::from_millis(80));
        match downloader.download_artifact(&coord).await {
            Ok(path) => {
                main_pb.finish_and_clear();
                downloaded_paths.push(path);
            }
            Err(e) => {
                main_pb.finish_and_clear();
                eprintln!("{}", format!("Warning: could not download root artifact: {e}").yellow());
            }
        }

        println!(
            "{}",
            format!("✅ Downloaded {} artifacts", downloaded_paths.len())
                .green()
                .bold()
        );
    } else {
        let pb = spinner(&format!("⬇️  Downloading {coord}..."));
        let path = downloader
            .download_artifact(&coord)
            .await
            .context("failed to download artifact")?;
        pb.finish_and_clear();
        println!("{}", "✅ Download complete".green().bold());
        downloaded_paths.push(path);
    }

    if let Some(output_dir) = output {
        let dest = Path::new(output_dir);
        std::fs::create_dir_all(dest)
            .with_context(|| format!("failed to create output directory '{output_dir}'"))?;

        let copy_pb = ProgressBar::new(downloaded_paths.len() as u64);
        copy_pb.set_style(
            ProgressStyle::with_template("  [{bar:30.cyan/dim}] {pos}/{len} copied")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );

        for src in &downloaded_paths {
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
            format!("✅ Copied {} files to {}", downloaded_paths.len(), dest.display())
                .green()
        );
    } else {
        for p in &downloaded_paths {
            println!("  {}", p.display());
        }
    }

    println!();
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

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    let downloader = match build_downloader(&cli) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", format!("Error: {e:#}").red());
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Commands::Info { coord } => cmd_info(&downloader, &coord).await,
        Commands::Deps { coord, tree, scope } => cmd_deps(&downloader, &coord, tree, &scope).await,
        Commands::Download {
            coord,
            with_deps,
            output,
            scope,
        } => cmd_download(&downloader, &coord, with_deps, output.as_deref(), &scope).await,
        Commands::Search { coord } => cmd_search(&downloader, &coord).await,
    };

    if let Err(e) = result {
        eprintln!("{}", format!("Error: {e:#}").red());
        std::process::exit(1);
    }
}
