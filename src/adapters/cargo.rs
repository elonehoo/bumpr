use crate::model::{Role, Scheme, Target, normalize_relative, read_file, string_range};
use anyhow::{Context, Result, bail, ensure};
use glob::Pattern;
use semver::Version;
use std::{collections::{BTreeMap, BTreeSet}, ffi::{OsStr, OsString}, fs,
    io::ErrorKind, ops::Range, path::{Component, Path, PathBuf}};
use toml_edit::{ImDocument, Item, TableLike, Value};

type Documents = BTreeMap<PathBuf, ImDocument<String>>;
type Packages = BTreeMap<PathBuf, Package>;

struct Package {
    name: String,
    version: String,
}

fn manifest_path(dir: &Path) -> PathBuf { dir.join("Cargo.toml") }

fn document(root: &Path, path: &Path) -> Result<Option<ImDocument<String>>> {
    read_file(root, path)?.map(|source| {
        ImDocument::parse(source).with_context(|| format!("Parsing {}", path.display()))
    }).transpose()
}

fn load_manifest(root: &Path, documents: &mut Documents, dir: &Path) -> Result<bool> {
    if documents.contains_key(dir) { return Ok(true); }
    let Some(doc) = document(root, &manifest_path(dir))? else { return Ok(false); };
    documents.insert(dir.to_owned(), doc);
    Ok(true)
}

fn table<'a>(parent: &'a dyn TableLike, key: &str, path: &Path) -> Result<Option<&'a dyn TableLike>> {
    parent.get(key).map(|item| item.as_table_like()
        .with_context(|| format!("{}: {key} must be a table", path.display()))).transpose()
}

// Expand only declared member patterns, without following symlink directories.
enum Part {
    Literal(OsString),
    Glob(Pattern),
    Recursive,
}

struct MemberPattern {
    parts: Vec<Part>,
}

impl MemberPattern {
    fn parse(base: &Path, pattern: &str) -> Result<Self> {
        let mut parts = base.iter().map(|part| Part::Literal(part.to_owned())).collect::<Vec<_>>();
        for part in Path::new(pattern).components() {
            match part {
                Component::CurDir => (),
                Component::ParentDir => {
                    ensure!(parts.pop().is_some(), "Workspace pattern escapes the project: {pattern}");
                }
                Component::Normal(name) => {
                    let name = name.to_str().context("Workspace pattern must be UTF-8")?;
                    parts.push(if name == "**" { Part::Recursive }
                        else if name.contains(['*', '?', '[']) { Part::Glob(Pattern::new(name)?) }
                        else { Part::Literal(name.into()) });
                }
                _ => bail!("Workspace pattern must be relative: {pattern}"),
            }
        }
        Ok(Self { parts })
    }

    fn literal(&self) -> Option<PathBuf> {
        self.parts.iter().map(|part| match part {
            Part::Literal(name) => Some(name),
            _ => None,
        }).collect()
    }

    fn matches(&self, path: &Path) -> bool {
        fn matches(parts: &[Part], names: &[&OsStr]) -> bool {
            match parts.split_first() {
                None => names.is_empty(),
                Some((Part::Recursive, rest)) => matches(rest, names) ||
                    (!names.is_empty() && matches(parts, &names[1..])),
                Some((part, rest)) => names.split_first().is_some_and(|(name, remaining)| {
                    (match part {
                        Part::Literal(literal) => literal == *name,
                        Part::Glob(pattern) => name.to_str().is_some_and(|name| pattern.matches(name)),
                        Part::Recursive => unreachable!(),
                    }) && matches(rest, remaining)
                }),
            }
        }
        matches(&self.parts, &path.iter().collect::<Vec<_>>())
    }

    fn expand(&self, root: &Path, rules: &WorkspacePaths, dirs: &mut BTreeSet<PathBuf>) -> Result<()> {
        fn visit(root: &Path, dir: &Path, parts: &[Part], rules: &WorkspacePaths,
                 dirs: &mut BTreeSet<PathBuf>) -> Result<()> {
            if rules.excludes(dir) { return Ok(()); }
            let absolute = root.join(dir);
            match fs::symlink_metadata(&absolute) {
                Ok(metadata) if metadata.is_dir() => (),
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error).with_context(|| format!("Accessing {}", absolute.display())),
            }
            let Some((part, rest)) = parts.split_first() else {
                dirs.insert(dir.to_owned());
                return Ok(());
            };
            if let Part::Literal(name) = part {
                return visit(root, &dir.join(name), rest, rules, dirs);
            }
            if matches!(part, Part::Recursive) { visit(root, dir, rest, rules, dirs)?; }
            for entry in fs::read_dir(&absolute).with_context(|| format!("Reading {}", absolute.display()))? {
                let entry = entry?;
                let name = entry.file_name();
                if let Part::Glob(pattern) = part
                    && !name.to_str().is_some_and(|name| pattern.matches(name)) { continue; }
                if entry.file_type()?.is_dir() {
                    let next = if matches!(part, Part::Recursive) { parts } else { rest };
                    visit(root, &dir.join(name), next, rules, dirs)?;
                }
            }
            Ok(())
        }
        visit(root, Path::new(""), &self.parts, rules, dirs)
    }
}

struct WorkspacePaths {
    members: Vec<MemberPattern>,
    exclude: Vec<MemberPattern>,
}

impl WorkspacePaths {
    fn read(dir: &Path, workspace: &dyn TableLike) -> Result<Self> {
        let read = |key| -> Result<Vec<MemberPattern>> {
            let Some(item) = workspace.get(key) else { return Ok(Vec::new()); };
            let values = item.as_array().with_context(|| format!("workspace.{key} must be an array"))?;
            values.iter().map(|value| {
                let pattern = value.as_str().with_context(|| format!("workspace.{key} must contain strings"))?;
                MemberPattern::parse(dir, pattern)
                    .with_context(|| format!("Invalid workspace.{key} pattern {pattern:?}"))
            }).collect()
        };
        (|| -> Result<_> { Ok(Self { members: read("members")?, exclude: read("exclude")? }) })()
            .with_context(|| format!("Reading workspace paths in {}", manifest_path(dir).display()))
    }

    fn excludes(&self, path: &Path) -> bool {
        self.exclude.iter().any(|pattern| path.ancestors().any(|dir| pattern.matches(dir)))
    }

    fn declares(&self, path: &Path) -> bool {
        !self.excludes(path) && self.members.iter().any(|pattern| pattern.matches(path))
    }
}

fn inherited_version(package: &dyn TableLike, path: &Path) -> Result<bool> {
    let Some(version) = package.get("version") else { return Ok(false); };
    if version.as_str().is_some() { return Ok(false); }
    ensure!(version.as_table_like().and_then(|item| item.get("workspace"))
        .and_then(Item::as_bool) == Some(true),
        "{}: package.version must be a version string or inherit from the workspace", path.display());
    Ok(true)
}

fn explicit_workspace(package: &dyn TableLike, dir: &Path) -> Result<Option<PathBuf>> {
    package.get("workspace").map(|value| {
        let raw = value.as_str().with_context(|| format!("{}: package.workspace must be a path",
            manifest_path(dir).display()))?;
        normalize_relative(&dir.join(raw)).with_context(|| format!(
            "{}: workspace root is outside the project; run from the workspace root",
            manifest_path(dir).display()))
    }).transpose()
}

fn workspace_root(root: &Path, documents: &mut Documents, dir: &Path) -> Result<PathBuf> {
    let path = manifest_path(dir);
    let doc = &documents[dir];
    if table(doc.as_table(), "workspace", &path)?.is_some() { return Ok(dir.to_owned()); }
    let Some(package) = table(doc.as_table(), "package", &path)? else { return Ok(dir.to_owned()); };
    let inherits = inherited_version(package, &path)?;
    if let Some(workspace_dir) = explicit_workspace(package, dir)? {
        ensure!(load_manifest(root, documents, &workspace_dir)?,
            "{}: missing workspace manifest {}", path.display(), manifest_path(&workspace_dir).display());
        let workspace = table(documents[&workspace_dir].as_table(), "workspace", &manifest_path(&workspace_dir))?
            .with_context(|| format!("{}: package.workspace does not point to a workspace", path.display()))?;
        ensure!(!WorkspacePaths::read(&workspace_dir, workspace)?.excludes(dir),
            "{} is excluded by its declared workspace", path.display());
        return Ok(workspace_dir);
    }
    for ancestor in dir.ancestors().skip(1) {
        if !load_manifest(root, documents, ancestor)? { continue; }
        if let Some(workspace) = table(documents[ancestor].as_table(), "workspace", &manifest_path(ancestor))? {
            let rules = WorkspacePaths::read(ancestor, workspace)?;
            if rules.declares(dir) || (inherits && !rules.excludes(dir)) { return Ok(ancestor.to_owned()); }
            break;
        }
    }
    ensure!(!inherits, "{} inherits package.version but no workspace root was found; run from the workspace root",
        path.display());
    Ok(dir.to_owned())
}

/// Each selected workspace forms a release unit. Member manifests contribute
/// their own versions, or inherit the shared workspace version once.
fn collect_groups(root: &Path, starts: &[PathBuf], documents: &mut Documents)
    -> Result<BTreeMap<PathBuf, BTreeSet<PathBuf>>> {
    let mut groups = BTreeMap::<PathBuf, BTreeSet<PathBuf>>::new();
    for start in starts {
        let dir = normalize_relative(start)?;
        if !load_manifest(root, documents, &dir)? { continue; }
        let workspace = workspace_root(root, documents, &dir)?;
        groups.entry(workspace).or_default().insert(dir);
    }
    let mut owners = BTreeMap::new();
    for (dir, members) in &mut groups {
        members.insert(dir.clone());
        let path = manifest_path(dir);
        if let Some(workspace) = table(documents[dir].as_table(), "workspace", &path)? {
            let rules = WorkspacePaths::read(dir, workspace)?;
            for pattern in &rules.members {
                let mut dirs = BTreeSet::new();
                pattern.expand(root, &rules, &mut dirs)?;
                if let Some(literal) = pattern.literal()
                    && !rules.excludes(&literal) {
                    ensure!(dirs.contains(&literal) && load_manifest(root, documents, &literal)?,
                        "{}: workspace member {} has no real Cargo.toml",
                        path.display(), literal.display());
                }
                for member in dirs {
                    if load_manifest(root, documents, &member)? { members.insert(member); }
                }
            }
        }
        for member in members.iter() {
            if member != dir {
                let path = manifest_path(member);
                let doc = &documents[member];
                ensure!(table(doc.as_table(), "workspace", &path)?.is_none(),
                    "{} is itself a workspace root", path.display());
                let package = table(doc.as_table(), "package", &path)?
                    .with_context(|| format!("{}: workspace member has no package table", path.display()))?;
                if let Some(workspace) = explicit_workspace(package, member)? {
                    ensure!(&workspace == dir, "{} belongs to a different workspace", path.display());
                }
            }
            if let Some(other) = owners.insert(member.clone(), dir.clone()) {
                ensure!(&other == dir, "{} belongs to more than one selected workspace", manifest_path(member).display());
            }
        }
    }
    Ok(groups)
}

fn version_span(doc: &ImDocument<String>, value: &str, span: Option<Range<usize>>, path: &Path)
    -> Result<Range<usize>> {
    string_range(doc.raw(), span.context("Missing Cargo string span")?, value)
        .with_context(|| format!("Reading version in {}", path.display()))
}

fn primary(path: &Path, doc: &ImDocument<String>, item: &Item, label: &str) -> Result<Target> {
    let version = item.as_str().with_context(|| format!("{}: {label} must be a version string", path.display()))?;
    Version::parse(version).with_context(|| format!("{}: invalid {label} {version:?}", path.display()))?;
    let span = version_span(doc, version, item.span(), path)?;
    Target::new(path, doc.raw(), span, label, Role::Primary, Scheme::Semver)
}

fn collect_packages(documents: &Documents, groups: &BTreeMap<PathBuf, BTreeSet<PathBuf>>,
                    targets: &mut Vec<Target>) -> Result<Packages> {
    let mut packages = BTreeMap::new();
    for (dir, members) in groups {
        let path = manifest_path(dir);
        let doc = &documents[dir];
        let mut shared = None;
        if let Some(workspace) = table(doc.as_table(), "workspace", &path)?
            && let Some(package) = table(workspace, "package", &path)?
            && let Some(version) = package.get("version") {
            let target = primary(&path, doc, version, "workspace.package.version")?;
            shared = Some(target.current.clone());
            targets.push(target);
        }
        for member in members {
            let path = manifest_path(member);
            let doc = &documents[member];
            let Some(package) = table(doc.as_table(), "package", &path)? else { continue; };
            let Some(version) = package.get("version") else { continue; };
            let name = package.get("name").and_then(Item::as_str)
                .with_context(|| format!("{}: package.name must be a string", path.display()))?;
            let version = if inherited_version(package, &path)? {
                shared.clone().with_context(|| format!("{} inherits a missing workspace.package.version", path.display()))?
            } else {
                let target = primary(&path, doc, version, "package.version")?;
                let version = target.current.clone();
                targets.push(target);
                version
            };
            packages.insert(member.clone(), Package { name: name.into(), version });
        }
    }
    Ok(packages)
}

fn dependency_path(root: &Path, dir: &Path, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        normalize_relative(path.strip_prefix(root).ok()?).ok()
    } else {
        normalize_relative(&dir.join(path)).ok()
    }
}

// Rewrite only requirements naming the exact old version, preserving the
// operator and whitespace. Partial versions, wildcards and ranges stay intact.
fn requirement_version(requirement: &str, version: &str) -> Option<Range<usize>> {
    let trimmed = requirement.trim();
    let number = [">=", "<=", "^", "~", "=", ">", "<"].iter()
        .find_map(|operator| trimmed.strip_prefix(operator))
        .unwrap_or(trimmed).trim();
    if number != version { return None; }
    let end = requirement.trim_end().len();
    Some(end - version.len()..end)
}

fn dependency_section(root: &Path, dir: &Path, doc: &ImDocument<String>, section: &dyn TableLike,
                      prefix: &str, packages: &Packages, targets: &mut Vec<Target>) -> Result<()> {
    let path = manifest_path(dir);
    for (alias, item) in section.iter() {
        let Some(dep) = item.as_table_like() else { continue; };
        let Some(raw_path) = dep.get("path").and_then(Item::as_str) else { continue; };
        let Some(local) = dependency_path(root, dir, raw_path).and_then(|dir| packages.get(&dir)) else { continue; };
        let name = match dep.get("package") {
            Some(value) => value.as_str(),
            None => Some(alias),
        };
        if name != Some(local.name.as_str()) { continue; }
        let Some(item) = dep.get("version") else { continue; };
        let Some(requirement) = item.as_str() else { continue; };
        let Some(relative) = requirement_version(requirement, &local.version) else { continue; };
        let span = version_span(doc, requirement, item.span(), &path)?;
        targets.push(Target::new(&path, doc.raw(), span.start + relative.start..span.start + relative.end,
            format!("{prefix}.{alias}.version"), Role::Linked, Scheme::Semver)?);
    }
    Ok(())
}

fn dependencies(root: &Path, dir: &Path, doc: &ImDocument<String>, packages: &Packages,
                targets: &mut Vec<Target>) -> Result<()> {
    let path = manifest_path(dir);
    let sections = ["dependencies", "dev-dependencies", "build-dependencies", "dev_dependencies", "build_dependencies"];
    for section in sections {
        if let Some(deps) = table(doc.as_table(), section, &path)? {
            dependency_section(root, dir, doc, deps, section, packages, targets)?;
        }
    }
    if let Some(workspace) = table(doc.as_table(), "workspace", &path)?
        && let Some(deps) = table(workspace, "dependencies", &path)? {
        dependency_section(root, dir, doc, deps, "workspace.dependencies", packages, targets)?;
    }
    if let Some(platforms) = table(doc.as_table(), "target", &path)? {
        for (platform, item) in platforms.iter() {
            let Some(platform_table) = item.as_table_like() else { continue; };
            for section in sections {
                if let Some(deps) = table(platform_table, section, &path)? {
                    dependency_section(root, dir, doc, deps, &format!("target.{platform}.{section}"), packages, targets)?;
                }
            }
        }
    }
    Ok(())
}

fn lock_reference(doc: &ImDocument<String>, path: &Path, value: &Value,
                  local: &BTreeSet<(&str, &str)>, targets: &mut Vec<Target>) -> Result<()> {
    let Some(reference) = value.as_str() else { return Ok(()); };
    let mut words = reference.split_whitespace();
    let (Some(name), Some(version), None) = (words.next(), words.next(), words.next()) else { return Ok(()); };
    // A registry or git reference with the same name and version as a local
    // crate includes its source. A source-less reference to that local entry
    // remains safe to update, including in this collision case.
    if !local.contains(&(name, version)) { return Ok(()); }
    let span = version_span(doc, reference, value.span(), path)?;
    let end = span.start + reference.trim_end().len();
    targets.push(Target::new(path, doc.raw(), end - version.len()..end,
        format!("dependency {name}"), Role::Linked, Scheme::Semver)?);
    Ok(())
}

fn lockfile(root: &Path, dir: &Path, members: &BTreeSet<PathBuf>, packages: &Packages,
            targets: &mut Vec<Target>) -> Result<()> {
    let desired = members.iter().filter_map(|dir| packages.get(dir))
        .map(|pkg| (pkg.name.as_str(), pkg.version.as_str())).collect::<BTreeSet<_>>();
    if desired.is_empty() { return Ok(()); }
    let path = dir.join("Cargo.lock");
    let Some(doc) = document(root, &path)? else { return Ok(()); };
    let mut entries: Vec<&dyn TableLike> = Vec::new();
    if let Some(package) = table(doc.as_table(), "root", &path)? { entries.push(package); }
    if let Some(packages) = doc.get("package") {
        if let Some(array) = packages.as_array_of_tables() {
            entries.extend(array.iter().map(|entry| entry as &dyn TableLike));
        } else if let Some(array) = packages.as_array() {
            for item in array.iter() {
                entries.push(item.as_inline_table().with_context(|| format!("{}: invalid package entry",
                    path.display()))?);
            }
        } else { bail!("{}: invalid package entries", path.display()); }
    }
    let mut local = BTreeSet::new();
    for entry in &entries {
        if entry.get("source").is_some() { continue; }
        let Some(name) = entry.get("name").and_then(Item::as_str) else { continue; };
        let Some(item) = entry.get("version") else { continue; };
        let Some(version) = item.as_str() else { continue; };
        if !desired.contains(&(name, version)) { continue; }
        ensure!(local.insert((name, version)), "{}: duplicate local package {name} {version}", path.display());
        let range = version_span(&doc, version, item.span(), &path)?;
        targets.push(Target::new(&path, doc.raw(), range, format!("local package {name}"),
            Role::Linked, Scheme::Semver)?);
    }
    for entry in entries {
        if let Some(array) = entry.get("dependencies").and_then(Item::as_array) {
            for value in array.iter() { lock_reference(&doc, &path, value, &local, targets)?; }
        }
        if let Some(value) = entry.get("replace").and_then(Item::as_value) {
            lock_reference(&doc, &path, value, &local, targets)?;
        }
    }
    Ok(())
}

pub fn discover(root: &Path, starts: &[PathBuf]) -> Result<Vec<Target>> {
    let mut documents = Documents::new();
    let groups = collect_groups(root, starts, &mut documents)?;
    let mut targets = Vec::new();
    let packages = collect_packages(&documents, &groups, &mut targets)?;
    for (dir, members) in &groups {
        for member in members {
            dependencies(root, member, &documents[member], &packages, &mut targets)?;
        }
        lockfile(root, dir, members, &packages, &mut targets)?;
    }
    targets.sort_by(|a, b| (&a.path, a.range.start).cmp(&(&b.path, b.range.start)));
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            Self { _temp: temp, root }
        }

        fn write(&self, path: &str, source: &str) {
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }

        fn targets(&self, starts: &[&str]) -> Vec<Target> {
            discover(&self.root, &starts.iter().map(PathBuf::from).collect::<Vec<_>>()).unwrap()
        }

        fn updated(&self, targets: &[Target], path: &str) -> String {
            let mut source = fs::read_to_string(self.root.join(path)).unwrap();
            let mut edits = targets.iter().filter(|target| target.path == Path::new(path)).collect::<Vec<_>>();
            edits.sort_by_key(|target| std::cmp::Reverse(target.range.start));
            for target in edits {
                assert_eq!(&source[target.range.clone()], target.current);
                source.replace_range(target.range.clone(), "2.0.0");
            }
            source
        }
    }

    #[test]
    fn standalone_package_preserves_format_and_ignores_metadata() {
        let fixture = Fixture::new();
        let manifest = "[package]\r\nname = 'app'\r\nversion = '1.2.3' # release\r\n\r\n[package.metadata]\r\nversion = '1.2.3'\r\n";
        fixture.write("Cargo.toml", manifest);
        let targets = fixture.targets(&["", "./"]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].role, Role::Primary);
        assert_eq!(targets[0].label, "package.version");
        assert_eq!(fixture.updated(&targets, "Cargo.toml"),
            manifest.replacen("'1.2.3' # release", "'2.0.0' # release", 1));
        assert_eq!(fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap(), manifest);
    }

    #[test]
    fn workspace_members_inherit_once_and_respect_declared_globs() {
        let fixture = Fixture::new();
        fixture.write("Cargo.toml", r#"[workspace]
members = ["crates/*", "crates/alpha", "nested/**/crate"]
exclude = ["crates/excluded", "nested/ignore"]
[workspace.package]
version = "1.2.3"
[workspace.dependencies]
alpha = { path = "crates/alpha", version = "~1.2.3" }
"#);
        fixture.write("crates/alpha/Cargo.toml", r#"[package]
name = "alpha"
version.workspace = true
workspace = "../.."
"#);
        fixture.write("crates/beta/Cargo.toml", r#"[package]
name = "beta"
version = "1.2.3"
[dependencies]
alpha.workspace = true
"#);
        fixture.write("nested/sub/other/crate/Cargo.toml", r#"[package]
name = "nested"
version = { workspace = true }
"#);
        fixture.write("crates/not-a-crate/readme", "not a package");
        fixture.write("vendor/untouched/Cargo.toml", "[invalid");
        fixture.write("crates/excluded/Cargo.toml", "[invalid");
        fixture.write("nested/ignore/other/crate/Cargo.toml", "[invalid");

        // Starting at a declared member finds its workspace. Repeated starts
        // and overlapping member patterns never duplicate a version field.
        let targets = fixture.targets(&["crates/alpha", "crates/beta", "", "crates/alpha/./"]);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets.iter().filter(|target| target.role == Role::Primary).count(), 2);
        assert_eq!(targets.iter().filter(|target| target.role == Role::Linked).count(), 1);
        assert!(targets.iter().all(|target| target.current == "1.2.3"));
        let updated = fixture.updated(&targets, "Cargo.toml");
        assert!(updated.contains("version = \"2.0.0\""));
        assert!(updated.contains("version = \"~2.0.0\""));
        assert_eq!(fixture.updated(&targets, "crates/beta/Cargo.toml").matches("2.0.0").count(), 1);
        assert!(!fixture.updated(&targets, "crates/alpha/Cargo.toml").contains("2.0.0"));
    }

    #[test]
    fn local_dependency_requirements_are_matched_by_path_name_and_exact_version() {
        let fixture = Fixture::new();
        fixture.write("Cargo.toml", r#"[package]
name = "app"
version = "1.2.3"
[workspace]
members = ["core"]
exclude = ["vendor"]
[dependencies]
main = { package = "core", path = "core", version = "^ 1.2.3" }
registry = "1.2.3"
bad_path = { package = "core", path = "vendor", version = "^1.2.3" }
bad_name = { path = "core", version = "^1.2.3" }
partial = { package = "core", path = "core", version = "1.2" }
different = { package = "core", path = "core", version = "1.2.2" }
complex = { package = "core", path = "core", version = ">=1.2.3, <2.0.0" }
[dev-dependencies.renamed]
package = "core"
path = "./core"
version = ' ~1.2.3 '
[target.'cfg(unix)'.build-dependencies]
core = { path = "core", version = "=1.2.3" }
"#);
        fixture.write("core/Cargo.toml", r#"[package]
name = "core"
version = "1.2.3"
"#);
        fixture.write("vendor/Cargo.toml", r#"[package]
name = "core"
version = "1.2.3"
"#);
        let targets = fixture.targets(&[""]);
        assert_eq!(targets.len(), 5); // two package versions, three dependency references
        assert_eq!(targets.iter().filter(|target| target.role == Role::Linked).count(), 3);
        let updated = fixture.updated(&targets, "Cargo.toml");
        for field in ["version = \"^ 2.0.0\"", "version = ' ~2.0.0 '", "version = \"=2.0.0\""] {
            assert!(updated.contains(field), "{updated}");
        }
        for field in [
            "registry = \"1.2.3\"",
            "path = \"vendor\", version = \"^1.2.3\"",
            "bad_name = { path = \"core\", version = \"^1.2.3\" }",
            "version = \"1.2\"",
            "version = \"1.2.2\"",
            "version = \">=1.2.3, <2.0.0\"",
        ] { assert!(updated.contains(field), "{updated}"); }
    }

    #[test]
    fn cargo_lock_updates_only_source_free_workspace_packages_and_their_references() {
        let fixture = Fixture::new();
        fixture.write("Cargo.toml", r#"[workspace]
members = ["app", "core"]
[workspace.package]
version = "1.2.3"
"#);
        fixture.write("app/Cargo.toml", "[package]\nname = \"app\"\nversion.workspace = true\n");
        fixture.write("core/Cargo.toml", "[package]\nname = \"core\"\nversion.workspace = true\n");
        fixture.write("Cargo.lock", r#"version = 4

[[package]]
name = "app"
version = "1.2.3"
dependencies = [
 "core 1.2.3",
 "core 1.2.3 (registry+https://example.com/index)",
 "core 0.1.0",
 "external",
 "other 1.2.3",
]

[[package]]
name = "core"
version = "1.2.3"

[[package]]
name = "core"
version = "1.2.3"
source = "registry+https://example.com/index"

[[package]]
name = "core"
version = "0.1.0"
source = "git+https://example.com/core"

[[package]]
name = "external"
version = "1.2.3"
source = "registry+https://example.com/index"

[[package]]
name = "other"
version = "1.2.3"
"#);
        let targets = fixture.targets(&[""]);
        assert_eq!(targets.len(), 4);
        let updated = fixture.updated(&targets, "Cargo.lock");
        assert!(updated.starts_with("version = 4"));
        assert!(updated.contains("name = \"app\"\nversion = \"2.0.0\""));
        assert!(updated.contains("name = \"core\"\nversion = \"2.0.0\""));
        assert!(updated.contains("\"core 2.0.0\""));
        assert!(updated.contains("\"core 1.2.3 (registry+https://example.com/index)\""));
        assert!(updated.contains("\"core 0.1.0\""));
        assert!(updated.contains("\"other 1.2.3\""));
        assert!(updated.contains("name = \"core\"\nversion = \"1.2.3\"\nsource"));
        assert!(updated.contains("name = \"external\"\nversion = \"1.2.3\"\nsource"));
        assert!(updated.contains("name = \"other\"\nversion = \"1.2.3\""));
    }

    #[test]
    fn legacy_lockfile_root_is_linked_and_invalid_manifest_has_context() {
        let fixture = Fixture::new();
        fixture.write("Cargo.toml", "[package]\nname = \"app\"\nversion = \"1.2.3\"\n");
        fixture.write("Cargo.lock", "[root]\nname = \"app\"\nversion = \"1.2.3\"\n");
        let targets = fixture.targets(&[""]);
        assert_eq!(targets.len(), 2);
        assert!(fixture.updated(&targets, "Cargo.lock").contains("version = \"2.0.0\""));

        fixture.write("Cargo.toml", "[package]\nname = \"app\"\nversion.workspace = true\n");
        let err = discover(&fixture.root, &[PathBuf::new()]).unwrap_err();
        assert!(err.to_string().contains("no workspace root"));
        fixture.write("Cargo.toml", "[workspace\n");
        let err = discover(&fixture.root, &[PathBuf::new()]).unwrap_err();
        assert!(err.to_string().contains("Parsing Cargo.toml"));
        fixture.write("Cargo.toml", "[workspace]\nmembers = [\"missing\"]\n");
        let err = discover(&fixture.root, &[PathBuf::new()]).unwrap_err();
        assert!(err.to_string().contains("workspace member missing"));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_member_patterns_do_not_follow_symlinks() {
        let fixture = Fixture::new();
        let other = Fixture::new();
        fixture.write("Cargo.toml", "[workspace]\nmembers = [\"crates/**\"]\n");
        fixture.write("crates/local/Cargo.toml", "[package]\nname = \"local\"\nversion = \"1.2.3\"\n");
        other.write("Cargo.toml", "[package]\nname = \"external\"\nversion = \"1.2.3\"\n");
        std::os::unix::fs::symlink(&other.root, fixture.root.join("crates/alias")).unwrap();
        let targets = fixture.targets(&[""]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, Path::new("crates/local/Cargo.toml"));
    }
}
