use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{fs, io::ErrorKind, ops::Range, path::{Component, Path, PathBuf}};

/// A primary field declares a project version; a linked field mirrors it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role { Primary, Linked }

/// Strict semantic versions are required by Cargo and npm; other formats may
/// use a different numbering convention and can receive an explicit version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme { Semver, Numeric, Flexible }

/// The exact bytes of a version, without surrounding quotes or whitespace.
/// Adapters identify fields; the planner applies the edits without serializing
/// the rest of a manifest, preserving its style, comments, and line endings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub current: String,
    pub label: String,
    pub role: Role,
    pub scheme: Scheme,
}

impl Target {
    pub fn new(path: impl Into<PathBuf>, source: &str, range: Range<usize>,
               label: impl Into<String>, role: Role, scheme: Scheme) -> Result<Self> {
        let path = normalize_relative(&path.into())?;
        let current = source.get(range.clone()).context("Invalid version byte range")?;
        ensure!(!current.is_empty() && current.trim() == current,
            "Empty or whitespace-padded version in {}", path.display());
        Ok(Self { path, range, current: current.into(), label: label.into(), role, scheme })
    }
}

/// Normalize manifest-relative paths, rejecting paths that escape the project.
pub fn normalize_relative(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => result.push(name),
            Component::CurDir => (),
            Component::ParentDir => {
                ensure!(result.pop(), "Path escapes the project: {}", path.display());
            }
            _ => bail!("Expected a project-relative path: {}", path.display()),
        }
    }
    Ok(result)
}

/// Resolve a real file inside the canonical project root, not a symlink alias.
pub fn checked_file(root: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let relative = normalize_relative(path)?;
    let absolute = root.join(&relative);
    match fs::symlink_metadata(&absolute) {
        Ok(_) => (),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Accessing {}", absolute.display())),
    }
    let canonical = absolute.canonicalize().with_context(|| format!("Resolving {}", absolute.display()))?;
    ensure!(canonical.starts_with(root) && canonical == absolute,
        "Refusing a symlinked or out-of-project file: {}", relative.display());
    ensure!(canonical.is_file(), "Expected a file: {}", relative.display());
    Ok(Some(canonical))
}

pub fn read_file(root: &Path, path: &Path) -> Result<Option<String>> {
    checked_file(root, path)?.map(|absolute| {
        fs::read_to_string(&absolute).with_context(|| format!("Reading {}", absolute.display()))
    }).transpose()
}

/// Convert a parser's quoted-string span to the inner literal span. Escaped
/// versions are deliberately not rewritten as raw text.
pub fn string_range(source: &str, span: Range<usize>, value: &str) -> Result<Range<usize>> {
    let raw = source.get(span.clone()).context("Invalid string span")?;
    if raw == value { return Ok(span); }
    if raw.len() >= 2 && ((raw.starts_with('"') && raw.ends_with('"')) ||
        (raw.starts_with('\'') && raw.ends_with('\''))) && raw.get(1..raw.len()-1) == Some(value) {
        return Ok(span.start + 1..span.end - 1);
    }
    bail!("Unsupported escaped or nonliteral version string: {raw:?}")
}
