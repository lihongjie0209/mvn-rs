<div align="center">

# mvn-rs

**Maven dependency resolution & download, reimplemented in Rust.**

[![CI](https://github.com/lihongjie0209/mvn-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/lihongjie0209/mvn-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A faithful Rust reimplementation of Maven's core dependency resolution engine.
Resolves transitive dependencies with the exact same semantics as `mvn dependency:tree`,
downloads artifacts concurrently, and renders real-time progress in the terminal.

</div>

---

## ✨ Features

| | Feature | Details |
|---|---|---|
| 🔍 | **Artifact Info** | Display POM metadata — coordinates, description, parent, dependency count |
| 🌳 | **Dependency Tree** | Full transitive resolution with tree or flat list output |
| ⬇️ | **Concurrent Download** | Download JARs + POMs with real-time indicatif progress bars |
| 📋 | **Version Search** | Query all available versions from remote repositories |
| ⬆️ | **Upload / Deploy** | Upload artifacts + deps to remote repos (Nexus, Artifactory, etc.) |
| ⚡ | **Async & Parallel** | Tokio + FuturesUnordered for concurrent POM fetching, downloads, and uploads |
| 🔒 | **Checksum Verification** | SHA-1 / SHA-256 validation with configurable retry & exponential backoff |
| 🪞 | **Mirror & Auth** | Respects `~/.m2/settings.xml` — mirrors, server credentials, proxies, profiles |
| 🧪 | **Well Tested** | 295 unit tests covering all core modules |

### Maven Compatibility

This tool is **not** a build system — it focuses on the **dependency resolution** and **artifact download** subsystem of Maven. The core resolution logic has been audited line-by-line against Java Maven source code across 4 rounds with 12 parallel audit agents, and all behavioral differences have been resolved.

| Behavior | Status |
|----------|--------|
| Nearest-wins (BFS) conflict resolution | ✅ Identical |
| Scope propagation matrix (compile / runtime / test / provided) | ✅ Identical |
| `dependencyManagement` override (version, scope, exclusions) | ✅ Identical |
| DM key = `groupId:artifactId:type` (3-tuple) | ✅ Identical |
| Exclusion inheritance through transitive tree | ✅ Identical |
| Wildcard exclusions (`*:*`) | ✅ Identical |
| Version ranges `[1.0,2.0)`, `(,1.0]`, union ranges | ✅ Identical |
| SNAPSHOT exclusion from version ranges | ✅ Identical |
| Parent POM chain resolution (max depth 20) | ✅ Identical |
| BOM (`<scope>import</scope>`) resolution | ✅ Identical |
| Relocation chain following with cycle detection | ✅ Identical |
| `ComparableVersion` qualifier ordering | ✅ Identical |
| Metadata merge from multiple repositories | ✅ Identical |
| `system` scope (`<systemPath>`) | 🟡 Skipped (deprecated in Maven) |

---

## 📦 Installation

### From Source

```bash
git clone https://github.com/lihongjie0209/mvn-rs.git
cd mvn-rs
cargo build --release

# Binary at: target/release/mvn-cli
```

### Pre-built Binaries

Download from [GitHub Releases](https://github.com/lihongjie0209/mvn-rs/releases) — builds available for:

| Platform | Target |
|----------|--------|
| Linux x86_64 | `x86_64-unknown-linux-gnu` / `musl` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS x86_64 | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

---

## 🚀 Usage

### Global Options

```
--settings <PATH>    Path to settings.xml (default: ~/.m2/settings.xml)
--retries <N>        Max retry attempts for failed downloads (default: 3)
```

### Search Available Versions

```bash
mvn-cli search org.apache.commons:commons-lang3
```

### View Artifact Info

```bash
mvn-cli info org.apache.commons:commons-lang3:3.17.0
```

### Resolve Dependency Tree

```bash
# Flat list (default)
mvn-cli deps com.google.guava:guava:33.4.0-jre

# Tree view
mvn-cli deps com.google.guava:guava:33.4.0-jre --tree

# Filter by scope
mvn-cli deps com.google.guava:guava:33.4.0-jre --tree --scope compile
```

### Download Artifacts

```bash
# Download JAR + all transitive dependencies + POMs
mvn-cli download org.apache.commons:commons-lang3:3.17.0

# Multiple artifacts at once
mvn-cli download org.slf4j:slf4j-api:2.0.16 com.google.code.gson:gson:2.11.0

# From a file (one coordinate per line, # comments supported)
mvn-cli download -f coords.txt

# Download only the root artifact (no transitive deps)
mvn-cli download org.apache.commons:commons-lang3:3.17.0 --no-deps

# Copy to a specific directory
mvn-cli download org.apache.commons:commons-lang3:3.17.0 --output ./libs

# Filter transitive scope
mvn-cli download com.google.guava:guava:33.4.0-jre --scope runtime
```

### Upload Artifacts

Upload artifacts (+ transitive dependencies) from local `~/.m2/repository` to a remote Maven repository.

```bash
# Upload a single artifact with all dependencies
mvn-cli upload com.google.code.gson:gson:2.11.0 \
  --repo-url http://localhost:8081/repository/maven-releases/ \
  --username admin --password secret

# Upload without transitive deps
mvn-cli upload org.apache.commons:commons-lang3:3.17.0 --no-deps \
  --repo-url http://nexus.example.com/repository/releases/

# Upload multiple artifacts
mvn-cli upload org.slf4j:slf4j-api:2.0.16 ch.qos.logback:logback-classic:1.5.12 \
  --repo-url http://nexus.example.com/repository/releases/

# From a file (one coordinate per line)
mvn-cli upload -f coords.txt \
  --repo-url http://nexus.example.com/repository/releases/

# Use credentials from settings.xml (matched by --repo-id)
mvn-cli upload com.example:my-lib:1.0 \
  --repo-url http://nexus.example.com/repository/releases/ \
  --repo-id releases

# Also update remote maven-metadata.xml
mvn-cli upload com.example:my-lib:1.0 \
  --repo-url http://nexus.example.com/repository/releases/ \
  --update-metadata
```

**Credential resolution order:**
1. `--username` / `--password` flags (highest priority)
2. `<server>` entry in `settings.xml` matching `--repo-id`

### Coordinate Formats

| Format | Example |
|--------|---------|
| `groupId:artifactId:version` | `com.google.guava:guava:33.4.0-jre` |
| `groupId:artifactId:type:version` | `io.netty:netty-tcnative:jar:2.0.65.Final` |
| `groupId:artifactId:type:classifier:version` | `io.netty:netty-tcnative:jar:linux-x86_64:2.0.65.Final` |

---

## 🏗️ Architecture

```
mvn-rs/
├── crates/
│   ├── mvn-core/                  # Core library (295 tests)
│   │   ├── coord.rs               # Artifact coordinates (GAV / GAVE / GAVCE)
│   │   ├── version.rs             # ComparableVersion + version ranges
│   │   ├── pom.rs                 # POM parsing, interpolation, DM injection
│   │   ├── resolver.rs            # 3-phase dependency engine
│   │   ├── uploader.rs             # Artifact upload with checksum generation
│   │   ├── downloader.rs          # HTTP download, checksum, retry
│   │   ├── repository.rs          # Local + remote repo management
│   │   ├── metadata.rs            # maven-metadata.xml parsing & merge
│   │   ├── settings.rs            # settings.xml (mirrors, servers, proxies)
│   │   └── error.rs               # Typed error hierarchy
│   └── mvn-cli/                   # CLI application (clap + indicatif)
│       └── main.rs                # 5 commands: info, deps, download, upload, search
├── .github/workflows/
│   ├── ci.yml                     # Test on push / PR
│   └── release.yml                # Multi-platform release on tag
└── README.md
```

### Resolution Pipeline

The resolver uses a **3-phase concurrent pipeline** modeled after Maven's `DefaultDependencyCollector`:

```
Phase 1 — Collect            Phase 2 — Flatten           Phase 3 — Download
┌─────────────────────┐     ┌─────────────────────┐     ┌─────────────────────┐
│ BFS traversal        │     │ Nearest-wins dedup   │     │ Concurrent download  │
│ Concurrent POM fetch │ ──► │ Scope propagation    │ ──► │ SHA-1/SHA-256 verify │
│ DM/exclusion inject  │     │ Scope filtering      │     │ Retry + backoff      │
│ Relocation following │     │ Flat dependency list  │     │ Progress bars        │
└─────────────────────┘     └─────────────────────┘     └─────────────────────┘
```

### Effective POM Pipeline (6 steps)

Each POM goes through a deterministic pipeline matching Java Maven's `DefaultModelBuilder`:

1. **Parse** raw XML
2. **Resolve parent chain** (recursive, up to depth 20)
3. **Merge parent** — properties, dependencyManagement, dependencies, repositories
4. **Interpolate** `${project.*}`, `${env.*}`, `${os.*}`, user properties
5. **Inject dependency management** — version, scope, type, classifier, exclusions
6. **Resolve BOM imports** — `<scope>import</scope>` in DM section

---

## ⚙️ Configuration

mvn-rs reads `~/.m2/settings.xml` (or a custom path via `--settings`) and supports:

- **Mirrors** — `<mirror>` with `<mirrorOf>` patterns (`*`, `central`, `*,!snapshots`)
- **Server credentials** — `<server>` with `<username>` / `<password>`
- **Proxies** — `<proxy>` with host, port, and optional authentication
- **Profiles** — `<profile>` with `<repositories>` (activated via `<activeProfiles>`)
- **Local repository** — standard `~/.m2/repository` layout, fully compatible with Maven

---

## 🧪 Testing

```bash
# Run all 295 tests
cargo test --workspace

# Run tests for a specific module
cargo test -p mvn-core -- resolver
cargo test -p mvn-core -- pom
cargo test -p mvn-core -- version
```

### Test Distribution

| Module | Tests | Coverage |
|--------|------:|----------|
| `resolver.rs` | 53 | Resolution, BFS, scope propagation, DM, exclusions, relocations |
| `downloader.rs` | 41 | HTTP download, checksum, retry, SNAPSHOT, metadata merge |
| `version.rs` | 39 | Parsing, comparison, ranges, qualifiers, edge cases |
| `pom.rs` | 38 | Parsing, interpolation, DM injection, parent merge |
| `repository.rs` | 32 | Local cache, remote fetch, atomic writes |
| `settings.rs` | 27 | Mirror matching, server auth, proxy, profiles |
| `coord.rs` | 23 | Coordinate parsing, path generation, exclusion matching |
| `error.rs` | 16 | Error types, display, conversion |
| `uploader.rs` | 13 | Upload, retry, metadata serialization, checksums |
| `metadata.rs` | 13 | Metadata parsing, version listing, multi-repo merge |
| **Total** | **295** | |

---

## 🔧 Technical Details

### Concurrency Model

- **POM resolution**: `FuturesUnordered` for parallel POM fetching during BFS traversal
- **Artifact download**: `Semaphore`-bounded (8 concurrent) parallel downloads
- **Artifact upload**: `Semaphore`-bounded (8 concurrent) parallel uploads
- **Progress UI**: Per-artifact indicatif progress bars with real-time byte tracking

### Retry & Resilience

- Exponential backoff: 1s → 2s → 4s (configurable)
- ±25% jitter to prevent thundering herd
- HTTP timeouts: 30s connect, 300s request
- Atomic file writes (temp + rename) to prevent corruption
- Graceful fallback across multiple repositories

### Version Comparison

Implements Maven's `ComparableVersion` algorithm:
- Qualifier ordering: `alpha` < `beta` < `milestone` < `rc` < `snapshot` < *(release)* < `sp`
- Numeric segments compared numerically, string segments compared lexically
- Version ranges: `[1.0,2.0)`, `(,1.0]`, `[1.0,)`, union ranges `[1.0,2.0),[3.0,4.0)`

---

## License

MIT
