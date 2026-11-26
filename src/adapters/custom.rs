//! Declarative rules cover version fields that have no built-in adapter.
use crate::model::{Role, Scheme, Target, checked_file, normalize_relative, read_file};
use anyhow::{Context, Result, bail, ensure};
use glob::{MatchOptions, Pattern};
use regex::Regex;
use serde::Deserialize;
use std::{collections::BTreeSet, fs, io::ErrorKind, path::{Path, PathBuf}};

fn yes() -> bool { true }

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auto: bool,
    pub recursive: bool,
    pub files: Vec<Rule>,
}
impl Default for Config {
    fn default() -> Self { Self { auto: true, recursive: false, files: Vec::new() } }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub path: String,
    pub pattern: String,
    #[serde(default)] pub label: Option<String>,
    #[serde(default = "yes")] pub primary: bool,
    #[serde(default)] pub semver: bool,
    #[serde(default)] pub multiple: bool,
    #[serde(default = "yes")] pub required: bool,
}

impl Rule {
    pub fn explicit(path: String, pattern: String) -> Self {
        Self { path, pattern, label: None, primary: true, semver: false, multiple: false, required: true }
    }
}

pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Config> {
    let path = if let Some(path) = explicit {
        if path.is_absolute() {
            path.strip_prefix(root).context("The config must be in the selected project")?.to_path_buf()
        } else { normalize_relative(path)? }
    } else {
        let visible = PathBuf::from("vershift.toml");
        let hidden = PathBuf::from(".vershift.toml");
        match (root.join(&visible).exists(), root.join(&hidden).exists()) {
            (true, true) => bail!("Both vershift.toml and .vershift.toml exist; choose one with --config"),
            (_, true) => hidden,
            _ => visible,
        }
    };
    match read_file(root, &path)? {
        Some(source) => toml_edit::de::from_str(&source).with_context(|| format!("Parsing {}", path.display())),
        None if explicit.is_some() => bail!("Config file not found: {}", path.display()),
        None => Ok(Config::default()),
    }
}

fn matching_files(root: &Path, rule: &Rule) -> Result<Vec<PathBuf>> {
    ensure!(!rule.path.is_empty(), "A custom rule needs a path");
    let pattern = normalize_relative(Path::new(&rule.path))?;
    ensure!(!pattern.as_os_str().is_empty(), "A custom rule must match a file");
    let pattern_text = pattern.to_str().context("Custom globs require a UTF-8 path")?;
    if !pattern_text.contains(['*', '?', '[']) {
        let files = checked_file(root, &pattern)?.map(|_| vec![pattern]).unwrap_or_default();
        ensure!(!files.is_empty() || !rule.required, "Custom rule {:?} matched no files", rule.path);
        return Ok(files);
    }
    // Match relative path components rather than globbing an absolute string:
    // canonical Windows paths may contain a verbatim \\?\ prefix, and the root
    // itself may contain characters that have meaning in a wildcard pattern.
    let parts: Vec<_> = pattern.iter().map(|part| {
        let text = part.to_str().context("Custom globs require UTF-8 path components")?;
        Ok((text.to_string(), Pattern::new(text)?))
    }).collect::<Result<Vec<_>>>()?;
    fn visit(root: &Path, path: &Path, parts: &[(String, Pattern)], matches: &mut BTreeSet<PathBuf>) -> Result<()> {
        let metadata = match fs::symlink_metadata(root.join(path)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("Accessing {}", path.display())),
        };
        if metadata.file_type().is_symlink() { return Ok(()); }
        let Some(((text, pattern), tail)) = parts.split_first() else {
            if metadata.is_file() { matches.insert(path.to_path_buf()); }
            return Ok(());
        };
        if text == "**" { visit(root, path, tail, matches)?; }
        if !metadata.is_dir() { return Ok(()); }
        if !text.contains(['*', '?', '[']) {
            return visit(root, &path.join(text), tail, matches);
        }
        let options = MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: true };
        for entry in fs::read_dir(root.join(path))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name_text) = name.to_str() else { continue; };
            if name_text == ".git" { continue; }
            if text == "**" || pattern.matches_with(name_text, options) {
                visit(root, &path.join(name), if text == "**" { parts } else { tail }, matches)?;
            }
        }
        Ok(())
    }
    let mut matches = BTreeSet::new();
    visit(root, Path::new(""), &parts, &mut matches)?;
    ensure!(!matches.is_empty() || !rule.required, "Custom rule {:?} matched no files", rule.path);
    Ok(matches.into_iter().collect())
}

pub fn discover(root: &Path, rules: &[Rule]) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    for rule in rules {
        let expression = Regex::new(&rule.pattern).with_context(|| format!("Invalid version pattern for {:?}", rule.path))?;
        let named = expression.capture_names().any(|name| name == Some("version"));
        ensure!(named || expression.captures_len() == 2,
            "Pattern for {:?} needs a named capture (?P<version>...) or exactly one capture group", rule.path);
        for path in matching_files(root, rule)? {
            let Some(source) = read_file(root, &path)? else { bail!("Matched file disappeared: {}", path.display()); };
            let captures: Vec<_> = expression.captures_iter(&source).map(|capture| {
                if named { capture.name("version") } else { capture.get(1) }
            }).collect();
            ensure!(rule.multiple || captures.len() <= 1,
                "Rule for {} matched {} times; narrow the pattern or set multiple = true", path.display(), captures.len());
            ensure!(!captures.is_empty() || !rule.required,
                "Version pattern did not match {}", path.display());
            for capture in captures {
                let value = capture.with_context(|| format!("Missing captured version in {}", path.display()))?;
                targets.push(Target::new(&path, &source, value.range(), rule.label.as_deref().unwrap_or("Custom version"),
                    if rule.primary { Role::Primary } else { Role::Linked },
                    if rule.semver { Scheme::Semver } else { Scheme::Flexible })?);
            }
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_rules_and_matches_only_captures() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("vershift.toml"), r#"auto = false
[[files]]
path = "*.h"
pattern = '(?m)^#define APP_VERSION "(?P<version>[^"\n]+)"$'
semver = true
"#).unwrap();
        std::fs::write(root.join("version.h"), "#define APP_VERSION \"1.2.3\"\n#define DEP_VERSION \"1.2.3\"\n").unwrap();
        let config = load(&root, None).unwrap();
        assert!(!config.auto);
        let targets = discover(&root, &config.files).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].current, "1.2.3");
        assert_eq!(targets[0].scheme, Scheme::Semver);
    }
    #[test]
    fn rejects_ambiguous_or_outside_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("versions"), "v=1.2.3\nv=1.2.3\n").unwrap();
        let rule = Rule::explicit("versions".into(), r"v=(\S+)".into());
        assert!(discover(&root, &[rule]).is_err());
        let outside = Rule::explicit("../other".into(), r"(\S+)".into());
        assert!(discover(&root, &[outside]).is_err());
    }
}
