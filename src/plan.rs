use crate::{discovery::deduplicate, model::{Role, Scheme, Target, checked_file, read_file}, version::validate_literal};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;
use std::{collections::BTreeMap, fs, io::Write, path::{Path, PathBuf}};
use tempfile::NamedTempFile;

#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub path: PathBuf,
    pub label: String,
    pub role: Role,
    pub from: String,
    pub to: String,
}

#[derive(Debug)]
struct FileEdit { path: PathBuf, before: String, after: String }

#[derive(Debug)]
pub struct Plan {
    root: PathBuf,
    pub from: String,
    pub to: String,
    pub changes: Vec<Change>,
    edits: Vec<FileEdit>,
}

fn validate_scheme(version: &str, scheme: Scheme) -> Result<()> {
    validate_literal(version)?;
    match scheme {
        Scheme::Semver => { semver::Version::parse(version).with_context(|| format!("{version:?} is not a semantic version (expected major.minor.patch)"))?; }
        Scheme::Numeric => {
            let parts: Vec<_> = version.split('.').collect();
            ensure!((1..=4).contains(&parts.len()) && parts.iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
                "{version:?} is not a numeric dotted version (one to four components)");
        }
        Scheme::Flexible => (),
    }
    Ok(())
}

pub fn valid_for_targets(version: &str, targets: &[Target], from: &str) -> Result<()> {
    validate_literal(version)?;
    for target in targets.iter().filter(|t| t.current == from) {
        validate_scheme(version, target.scheme).with_context(|| format!("{} — {}", target.path.display(), target.label))?;
    }
    Ok(())
}

impl Plan {
    pub fn build(root: &Path, targets: &[Target], from: &str, to: &str) -> Result<Self> {
        valid_for_targets(to, targets, from)?;
        let selected = deduplicate(targets.iter().filter(|target| target.current == from).cloned().collect())?;
        let mut grouped: BTreeMap<PathBuf, Vec<Target>> = BTreeMap::new();
        for target in selected { grouped.entry(target.path.clone()).or_default().push(target); }
        let mut changes = Vec::new();
        let mut edits = Vec::new();
        for (path, fields) in grouped {
            let before = read_file(root, &path)?.with_context(|| format!("File disappeared: {}", path.display()))?;
            let mut after = before.clone();
            // Later byte ranges are edited first, retaining earlier offsets.
            for target in fields.iter().rev() {
                ensure!(before.get(target.range.clone()) == Some(target.current.as_str()),
                    "Version field changed during discovery: {} ({})", path.display(), target.label);
                if from != to { after.replace_range(target.range.clone(), to); }
            }
            if before != after {
                for target in fields {
                    changes.push(Change { path: path.clone(), label: target.label, role: target.role,
                        from: from.into(), to: to.into() });
                }
                edits.push(FileEdit { path, before, after });
            }
        }
        Ok(Self { root: root.into(), from: from.into(), to: to.into(), changes, edits })
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> { self.edits.iter().map(|edit| edit.path.as_path()) }
    pub fn file_count(&self) -> usize { self.edits.len() }
    pub fn is_empty(&self) -> bool { self.edits.is_empty() }

    fn verify(&self, edit: &FileEdit, expected: &str) -> Result<PathBuf> {
        let absolute = checked_file(&self.root, &edit.path)?.with_context(|| format!("File disappeared: {}", edit.path.display()))?;
        let current = fs::read_to_string(&absolute).with_context(|| format!("Reading {}", edit.path.display()))?;
        ensure!(current == expected, "File changed since the plan was prepared: {}", edit.path.display());
        Ok(absolute)
    }

    fn prepare(absolute: &Path, content: &str) -> Result<NamedTempFile> {
        let permissions = fs::metadata(absolute)?.permissions();
        let parent = absolute.parent().context("Invalid manifest path")?;
        let mut file = NamedTempFile::new_in(parent)?;
        file.write_all(content.as_bytes())?;
        file.as_file().set_permissions(permissions)?;
        file.as_file().sync_all()?;
        Ok(file)
    }

    fn replace(file: NamedTempFile, absolute: &Path) -> Result<()> {
        file.persist(absolute).map_err(|error| error.error)
            .with_context(|| format!("Replacing {}", absolute.display()))?;
        Ok(())
    }

    /// Stage every replacement before modifying any original. Each file is
    /// replaced atomically, and already written files are restored if a later
    /// replacement fails (without overwriting a concurrent external edit).
    pub fn apply(&self) -> Result<()> {
        let mut prepared = Vec::new();
        for edit in &self.edits {
            let absolute = self.verify(edit, &edit.before)?;
            prepared.push(Self::prepare(&absolute, &edit.after)
                .with_context(|| format!("Preparing {}", edit.path.display()))?);
        }
        let mut completed: Vec<&FileEdit> = Vec::new();
        for (edit, file) in self.edits.iter().zip(prepared) {
            let result = self.verify(edit, &edit.before).and_then(|absolute| Self::replace(file, &absolute));
            if let Err(error) = result {
                let mut failed = Vec::new();
                for previous in completed.into_iter().rev() {
                    let restored = self.verify(previous, &previous.after).and_then(|absolute| {
                        Self::replace(Self::prepare(&absolute, &previous.before)?, &absolute)
                    });
                    if let Err(restore_error) = restored { failed.push(format!("{}: {restore_error:#}", previous.path.display())); }
                }
                if failed.is_empty() { return Err(error).context("Version update canceled; earlier writes restored"); }
                return Err(anyhow!("{error:#}; could not restore these files:\n{}", failed.join("\n")));
            }
            completed.push(edit);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replaces_only_target_ranges_and_checks_stale_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let before = "version = '1.2.3'\r\ndependency = '1.2.3'\r\n";
        fs::write(root.join("manifest"), before).unwrap();
        let start = before.find("1.2.3").unwrap();
        let target = Target::new("manifest", before, start..start+5, "version", Role::Primary, Scheme::Semver).unwrap();
        let plan = Plan::build(&root, std::slice::from_ref(&target), "1.2.3", "1.2.10").unwrap();
        assert_eq!(plan.file_count(), 1);
        assert_eq!(fs::read_to_string(root.join("manifest")).unwrap(), before);
        plan.apply().unwrap();
        assert_eq!(fs::read_to_string(root.join("manifest")).unwrap(), "version = '1.2.10'\r\ndependency = '1.2.3'\r\n");
        assert!(plan.apply().is_err());
        assert!(Plan::build(&root, &[target], "1.2.3", "not-semver").is_err());
    }
    #[test]
    fn refuses_changed_files_before_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut targets = Vec::new();
        for path in ["a", "b"] {
            fs::write(root.join(path), "1.0.0").unwrap();
            targets.push(Target::new(path, "1.0.0", 0..5, "version", Role::Primary, Scheme::Semver).unwrap());
        }
        let plan = Plan::build(&root, &targets, "1.0.0", "2.0.0").unwrap();
        fs::write(root.join("b"), "unrelated edit").unwrap();
        assert!(plan.apply().is_err());
        assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "1.0.0");
    }
}
