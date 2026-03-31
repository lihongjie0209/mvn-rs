# mvn-rs

Maven 核心依赖解析与下载工具的 Rust 实现。

## 功能

- 🔍 **查询 Artifact 信息** — 获取 POM 详情（坐标、描述、依赖数等）
- 🌳 **依赖解析** — 完整的传递依赖解析，支持树形/列表显示
- ⬇️ **下载 Artifact** — 从 Maven Central 下载 JAR 及其依赖
- 📋 **版本查询** — 搜索可用版本列表

### 核心特性

- **"Nearest Wins" 冲突解决** — 忠实复刻 Maven 的 BFS 最近优先策略
- **Scope 传递** — 完整的 compile/runtime/test/provided 传递矩阵
- **Exclusion 支持** — 包括通配符排除
- **SHA-1 校验** — 下载时自动校验 checksum
- **本地仓库兼容** — 使用标准 `~/.m2/repository` 布局

## 安装

```bash
cargo build --release
```

## 使用

### 搜索可用版本

```bash
mvn-rs search org.apache.commons:commons-lang3
```

### 查看 Artifact 信息

```bash
mvn-rs info org.apache.commons:commons-lang3:3.17.0
```

### 查看依赖树

```bash
mvn-rs deps org.apache.commons:commons-lang3:3.17.0 --tree

# 按 scope 过滤
mvn-rs deps com.google.guava:guava:33.0-jre --tree --scope compile
```

### 下载 Artifact

```bash
# 下载单个 JAR
mvn-rs download org.apache.commons:commons-lang3:3.17.0

# 下载 JAR 及所有依赖
mvn-rs download org.apache.commons:commons-lang3:3.17.0 --with-deps

# 下载到指定目录
mvn-rs download org.apache.commons:commons-lang3:3.17.0 --with-deps --output ./libs
```

## 坐标格式

支持多种 Maven 坐标格式：

| 格式 | 示例 |
|------|------|
| GAV | `groupId:artifactId:version` |
| GAVE | `groupId:artifactId:extension:version` |
| GAVCE | `groupId:artifactId:extension:classifier:version` |

## 项目结构

```
mvn-rs/
├── crates/
│   ├── mvn-core/          # 核心库
│   │   ├── coord.rs       # Maven 坐标
│   │   ├── version.rs     # 版本解析与比较
│   │   ├── pom.rs         # POM 解析与处理
│   │   ├── repository.rs  # 仓库管理
│   │   ├── resolver.rs    # 依赖解析引擎
│   │   ├── downloader.rs  # Artifact 下载
│   │   ├── metadata.rs    # Maven Metadata
│   │   └── error.rs       # 错误类型
│   └── mvn-cli/           # CLI 工具
└── README.md
```

## License

MIT
