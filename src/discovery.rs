use crate::{adapters, model::{Role, Scheme, Target, normalize_relative}};
use anyhow::{Context, Result, ensure};
use std::{collections::{BTreeMap, BTreeSet}, path::{Path, PathBuf}};

const MANIFESTS: &[&str] = &["Cargo.toml", "package.json", "pnpm-workspace.yaml", "pom.xml",
    "build.gradle", "build.gradle.kts", "gradle.properties", "settings.gradle", "settings.gradle.kts",
    "pyproject.toml", "setup.cfg", "Directory.Build.props", "composer.json", "pubspec.yaml", "CMakeLists.txt",
    "VERSION", "VERSION.txt", "version.txt", ".version"];
const OMIT_DIRS: &[&str] = &[".git", ".hg", ".svn", "node_modules", "target", "vendor", "dist", "build",
    ".gradle", ".cache", ".venv", "venv", "__pycache__", "bin", "obj"];

fn project_dirs(root: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let mut dirs = BTreeSet::from([PathBuf::new()]);
    if recursive {
        let mut walker = ignore::WalkBuilder::new(root);
        walker.hidden(true).follow_links(false).require_git(false).git_global(false)
            .filter_entry(|entry| entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_dir()) ||
                !entry.file_name().to_str().is_some_and(|name| OMIT_DIRS.contains(&name)));
        for entry in walker.build() {
            let entry = entry.context("Discovering nested projects")?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) { continue; }
            let path = entry.path();
            let known = entry.file_name().to_str().is_some_and(|name| MANIFESTS.contains(&name)) ||
                matches!(path.extension().and_then(|ext| ext.to_str()), Some("csproj" | "fsproj" | "vbproj" | "gemspec"));
            if known {
                let relative = path.parent().context("Manifest has no parent")?.strip_prefix(root)?;
                dirs.insert(normalize_relative(relative)?);
            }
        }
    }
    Ok(dirs.into_iter().collect())
}

pub fn deduplicate(mut targets: Vec<Target>) -> Result<Vec<Target>> {
    targets.sort_by(|a, b| (&a.path, a.range.start, a.range.end).cmp(&(&b.path, b.range.start, b.range.end)));
    let mut result: Vec<Target> = Vec::new();
    for target in targets {
        if let Some(previous) = result.last_mut().filter(|p| p.path == target.path) {
            if previous.range == target.range {
                ensure!(previous.current == target.current, "Version readers disagree in {}", target.path.display());
                if target.role == Role::Primary { previous.role = Role::Primary; }
                if previous.scheme == Scheme::Flexible { previous.scheme = target.scheme; }
                else if target.scheme != Scheme::Flexible {
                    ensure!(previous.scheme == target.scheme, "Conflicting version formats in {}", target.path.display());
                }
                continue;
            }
            ensure!(previous.range.end <= target.range.start,
                "Overlapping version fields in {}: {} and {}", target.path.display(), previous.label, target.label);
        }
        result.push(target);
    }
    Ok(result)
}

pub fn discover(root: &Path, config: &adapters::custom::Config) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    if config.auto {
        let starts = project_dirs(root, config.recursive)?;
        targets.extend(adapters::cargo::discover(root, &starts).context("Discovering Cargo projects")?);
        targets.extend(adapters::node::discover(root, &starts).context("Discovering Node projects")?);
        targets.extend(adapters::java::discover(root, &starts).context("Discovering Java projects")?);
        targets.extend(adapters::extra::discover(root, &starts).context("Discovering other project formats")?);
    }
    targets.extend(adapters::custom::discover(root, &config.files)?);
    deduplicate(targets)
}

pub fn versions(targets: &[Target]) -> BTreeMap<String, Vec<PathBuf>> {
    let mut versions = BTreeMap::<String, BTreeSet<PathBuf>>::new();
    for target in targets.iter().filter(|target| target.role == Role::Primary) {
        versions.entry(target.current.clone()).or_default().insert(target.path.clone());
    }
    versions.into_iter().map(|(version, paths)| (version, paths.into_iter().collect())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recursion_omits_dependencies_and_build_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for path in ["app/package.json", "vendor/a/Cargo.toml", "node_modules/a/package.json", "target/Cargo.toml"] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let dirs = project_dirs(&root, true).unwrap();
        assert_eq!(dirs, vec![PathBuf::new(), PathBuf::from("app")]);
    }
}
