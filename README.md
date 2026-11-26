# bumpr

[![CI](https://github.com/elonehoo/bumpr/actions/workflows/ci.yml/badge.svg)](https://github.com/elonehoo/bumpr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An interactive, cross-ecosystem version manager written in Rust. Inspired by [bumpp](https://github.com/antfu-collective/bumpp), but designed for projects beyond the JavaScript ecosystem. One executable can bump a Rust workspace, a Java service, a Node monorepo, or a mixed-language project.

[简体中文](README.zh-CN.md)

## Installation

Rust 1.88 or later is required to build from source. No Node.js, Java, or other language runtime is needed to inspect or edit version files.

```sh
cargo install --git https://github.com/elonehoo/bumpr --locked
```

From a local checkout:

```sh
cargo install --path . --locked
```

## Quick start

Run `bumpr` in your project directory to inspect the current version and interactively choose the next one:

```text
Select a new version (current: 0.1.0)
  patch       0.1.1
  minor       0.2.0
  major       1.0.0
  prepatch    0.1.1-beta.0
  preminor    0.2.0-beta.0
  premajor    1.0.0-beta.0
  prerelease  0.1.1-beta.0
  snapshot    0.1.1-SNAPSHOT
  custom      Enter a version
```

Explicit commands work in scripts and CI, too:

```sh
bumpr --current                     # Inspect discovered versions and linked fields
bumpr patch --dry-run               # Preview without writing anything
bumpr minor --yes                   # Bump the project version
bumpr prerelease --preid rc --yes    # Use an rc prerelease
bumpr stable --yes                  # Remove a prerelease suffix
bumpr snapshot --yes                # Next patch with a Maven-style SNAPSHOT suffix
bumpr 2.0.0 --yes                   # Set an explicit version
bumpr --path ../another-project patch --yes
```

The normal command updates files only. Git operations are opt-in:

```sh
bumpr patch --commit --yes          # Update and commit the changed version files
bumpr minor --tag --yes             # Update, commit, and create an annotated tag
bumpr patch --push --yes            # Update, commit, tag, and push to origin
```

`--push` (also available as `--release`) pushes the branch and new tag atomically. `--tag` implies a commit when files change. Git automation checks for a clean working tree and duplicate tags before editing; local-only bumps can be used in a dirty tree. Pushing a tag can trigger your project's existing CI/CD, but bumpr itself does not publish packages or create GitHub Releases.

Customize the remote, tag prefix, or message using `--remote`, `--tag-prefix`, and `--message`. Message templates accept `{version}`, `{oldVersion}`, and `{tag}`. Use `--json` with `--current` or a bump for machine-readable results.

## Supported project formats

| Ecosystem | Version discovery and updates |
| --- | --- |
| Rust | `Cargo.toml`, workspace members and shared package versions, matching local path dependency requirements, workspace entries in `Cargo.lock` |
| Node.js | `package.json`, npm/Yarn/pnpm workspace declarations, matching local dependency ranges, `package-lock.json`, `npm-shrinkwrap.json`, proven local-link specifiers in pnpm lockfile formats 6.0/9.0, and text `bun.lock` workspace versions |
| Java / Kotlin | Maven `pom.xml`, declared modules, local parent/dependency versions and direct revision properties; Gradle `gradle.properties`, literal project versions in `build.gradle` / `build.gradle.kts`, and statically declared subprojects |
| Python | Static `[project].version` or `[tool.poetry].version` in `pyproject.toml`; `[metadata] version` in `setup.cfg` |
| .NET | Direct `Version`, `PackageVersion`, and numeric `VersionPrefix` in project files or `Directory.Build.props` |
| PHP | Explicit top-level `version` in `composer.json` |
| Ruby | Literal gemspec version assignments |
| Dart / Flutter | Top-level version in `pubspec.yaml` |
| C / C++ | Literal `project(... VERSION ...)` in `CMakeLists.txt` |
| Any language | `VERSION`, `VERSION.txt`, `version.txt`, `.version`, custom version rules, or reachable semantic-version Git tags |

Built-in adapters identify **version fields**, not arbitrary occurrences of a number. Unrelated dependency versions, comments, and formatting stay intact. Existing Cargo/Node local version ranges are rewritten only when they can be associated with a known local package. Discovery follows declared workspace membership; it does not evaluate build scripts, execute package managers, or resolve/download dependencies.

### Workspaces and mixed-language repositories

Declared Cargo, npm, pnpm, Yarn, Maven and static Gradle workspace members are discovered automatically. To include independent projects in nested directories, opt into recursive discovery:

```sh
bumpr --current --recursive
bumpr minor --recursive --from 1.2.3 --yes
```

When several independent versions coexist, the interactive picker lets you select the current version. In noninteractive mode, `--from` selects which group to update. Only declarations and linked fields matching that version are edited. Recursive discovery respects ignore files and skips common installed-dependency and build-output directories.

### Arbitrary projects: custom rules

Any project with a textual version field can be supported without modifying bumpr. Create `bumpr.toml` (or `.bumpr.toml`) in the project root:

Existing `vershift.toml` or `.vershift.toml` files from before the rename are still recognized. Use `--config` if multiple configuration files are present.

```toml
# Set auto = false to use custom rules instead of built-in adapters.
auto = true
recursive = false

[[files]]
path = "include/**/version.h"
pattern = '(?m)^#define APP_VERSION "(?P<version>[^"\n]+)"$'
semver = true

[[files]]
path = "docs/about.txt"
pattern = '(?m)^Version: (?P<version>\S+)$'
primary = false # This field mirrors the version declared elsewhere.
```

Each rule matches a project-relative path or glob. The regular expression must contain a named `version` capture or exactly one capture group; only the captured bytes are replaced. Rules require exactly one match per file by default. Set `multiple = true` for intentional repeated fields or `required = false` for optional files. A `label` can customize the displayed field name.

For a one-off file you can also use the command line:

```sh
bumpr 2026.12 --no-auto --file src/version.h \
  --pattern '(?m)^#define APP_VERSION "([^"\n]+)"$' --yes
```

### Tag-versioned projects

When no literal version declarations exist, bumpr can read the highest reachable semantic-version Git tag. This suits Go modules, Swift packages, and projects that derive version numbers from Git:

```sh
bumpr patch --tag --yes
bumpr patch --tag --from 0.0.0 --yes  # First version, if no tag exists
```

Use `--tag-prefix ''` for repositories with bare version tags instead of `v`-prefixed tags.

## Version conventions and limitations

- Automatic bumps use semantic versioning, interpreting short numeric versions such as `1.2` as `1.2.0`. They remove existing build metadata. Explicit values also support alternative conventions such as `2026.12`, `1.2.3.4`, and Maven's `-SNAPSHOT` where the target format permits them.
- Cargo, npm and Dart version fields require valid semantic versions. Numeric CMake/.NET prefix fields cannot receive prerelease or build suffixes. Dynamic expressions, evaluated Maven/Gradle properties, and computed Python/Ruby versions are not executed; use custom rules for the authoritative literal instead.
- Only project-owned entries in supported text lockfiles are edited. pnpm lockfile formats 6.0 and 9.0 are supported for canonical shared lockfiles with `link:` resolutions to known workspace members. Yarn lock resolutions, injected pnpm packages and binary Bun lockfiles are not rewritten. For lockfile formats or dependency ranges that cannot be updated safely, use the local-only bump and refresh the lockfile with its package manager before committing.
- Version files are planned before writing, replaced individually using temporary files, and restored if a later replacement fails. Symlinks and paths outside the chosen project are not rewritten.

## Development

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

Tests cover semantic version choices, supported manifests, local workspaces, unrelated dependency preservation, Cargo lockfile validation, custom rules, and Git commit/tag/push against temporary repositories. CI runs on macOS, Linux, and Windows.

## License

[MIT](LICENSE)
