//! Node manifests and the project-owned portions of text lockfiles.
use crate::model::{Role, Scheme, Target, normalize_relative, read_file, string_range};
use anyhow::{Context, Result, bail, ensure};
use glob::{MatchOptions, Pattern};
use jsonc_parser::{ParseOptions, ast::{Object, StringLit, Value}, parse_to_ast};
use semver::Version;
use std::{collections::{BTreeMap, BTreeSet}, fs, io::ErrorKind, path::{Path, PathBuf}};

const DEPENDENCIES: &[&str] = &["dependencies", "devDependencies", "optionalDependencies", "peerDependencies"];
const MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: true,
};

struct Package {
    dir: PathBuf,
    source: String,
    name: Option<String>,
    version: Option<Target>,
}

struct Local<'a> {
    dir: &'a Path,
    version: &'a str,
}

// Keep dependency edits pending until all locks have been inspected. An
// explicit nonlocal resolution must suppress both the manifest and its lock
// mirrors, even if overlapping discovery starts encounter the same package.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Reference {
    dir: PathBuf,
    name: String,
    specifier: String,
}

impl Reference {
    fn new(dir: &Path, name: &str, specifier: &str) -> Self {
        Self { dir: dir.to_owned(), name: name.into(), specifier: specifier.into() }
    }
}

#[derive(Default)]
struct NodeTargets {
    fields: Vec<Target>,
    dependencies: Vec<(Reference, Target)>,
    nonlocal: BTreeSet<Reference>,
}

impl NodeTargets {
    fn finish(mut self) -> Vec<Target> {
        self.fields.extend(self.dependencies.into_iter().filter_map(|(reference, target)| {
            (!self.nonlocal.contains(&reference)).then_some(target)
        }));
        self.fields
    }
}

fn json_object<'a>(source: &'a str, path: &Path) -> Result<Object<'a>> {
    // bun.lock uses comments and trailing commas. The other JSON extensions
    // are unnecessary, and accepting missing commas could hide a broken file.
    let options = ParseOptions {
        allow_missing_commas: false,
        allow_loose_object_property_names: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
        ..Default::default()
    };
    let json = source.strip_prefix('\u{feff}').unwrap_or(source);
    match parse_to_ast(json, &Default::default(), &options)
        .with_context(|| format!("Parsing {}", path.display()))?.value {
        Some(Value::Object(object)) => Ok(object),
        _ => bail!("Expected a JSON object in {}", path.display()),
    }
}

fn field<'a, 'b>(object: &'b Object<'a>, key: &str) -> Result<Option<&'b Value<'a>>> {
    let mut properties = object.properties.iter().filter(|p| p.name.as_str() == key);
    let result = properties.next().map(|p| &p.value);
    ensure!(properties.next().is_none(), "Duplicate JSON property: {key}");
    Ok(result)
}

fn string_field<'a, 'b>(object: &'b Object<'a>, key: &str) -> Result<Option<&'b StringLit<'a>>> {
    field(object, key)?.map(|value| value.as_string_lit().with_context(|| {
        format!("Expected a string for {key}")
    })).transpose()
}

fn object_field<'a, 'b>(object: &'b Object<'a>, key: &str) -> Result<Option<&'b Object<'a>>> {
    field(object, key)?.map(|value| value.as_object().with_context(|| {
        format!("Expected an object for {key}")
    })).transpose()
}

fn literal_span(source: &str, literal: &StringLit<'_>) -> Result<std::ops::Range<usize>> {
    let bom = if source.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
    string_range(source, literal.range.start+bom..literal.range.end+bom, &literal.value)
}

fn version_target(path: &Path, source: &str, object: &Object<'_>, role: Role, label: &str) -> Result<Option<Target>> {
    string_field(object, "version")?.map(|literal| {
        Target::new(path, source, literal_span(source, literal)?, label, role, Scheme::Semver)
    }).transpose()
}

fn load_package(root: &Path, dir: &Path) -> Result<Option<Package>> {
    let path = dir.join("package.json");
    let Some(source) = read_file(root, &path)? else { return Ok(None) };
    let object = json_object(&source, &path)?;
    let name = string_field(&object, "name")?.map(|name| name.value.to_string());
    let version = version_target(&path, &source, &object, Role::Primary, "package.json version")?;
    Ok(Some(Package { dir: dir.to_owned(), source, name, version }))
}

fn manifest_patterns(package: Option<&Package>) -> Result<Vec<String>> {
    let Some(package) = package else { return Ok(Vec::new()) };
    let object = json_object(&package.source, &package.dir.join("package.json"))?;
    let Some(workspaces) = field(&object, "workspaces")? else { return Ok(Vec::new()) };
    let patterns = match workspaces {
        Value::Array(array) => array,
        Value::Object(object) => field(object, "packages")?
            .and_then(Value::as_array).context("Expected workspaces.packages to be an array")?,
        _ => bail!("Expected workspaces to be an array or an object with packages"),
    };
    patterns.elements.iter().map(|value| {
        Ok(value.as_string_lit().context("Workspace patterns must be strings")?.value.to_string())
    }).collect()
}

fn workspace_patterns(root: &Path, start: &Path, package: Option<&Package>) -> Result<Vec<String>> {
    let path = start.join("pnpm-workspace.yaml");
    let Some(source) = read_file(root, &path)? else { return manifest_patterns(package) };
    // pnpm's own workspace manifest determines membership when it exists.
    #[derive(serde::Deserialize, Default)]
    struct PnpmWorkspace {
        #[serde(default)]
        packages: Vec<String>,
    }
    let config: Option<PnpmWorkspace> = serde_yaml_ng::from_str(&source)
        .with_context(|| format!("Parsing {}", path.display()))?;
    Ok(config.unwrap_or_default().packages)
}

fn ignored_name(name: &str) -> bool {
    ["node_modules", ".git", ".hg", ".svn"].iter().any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn ignored_path(path: &Path) -> bool {
    path.iter().any(|name| name.to_str().is_some_and(ignored_name))
}

// Expand comma-separated brace alternatives before handing path components
// to glob::Pattern (which supports *, ?, [] and **).
fn brace_patterns(pattern: &str) -> Result<Vec<String>> {
    let mut pending = vec![pattern.to_owned()];
    let mut expanded = Vec::new();
    while let Some(pattern) = pending.pop() {
        let Some(open) = pattern.find('{') else {
            expanded.push(pattern);
            continue;
        };
        let mut depth = 0;
        let mut start = open + 1;
        let mut choices = Vec::new();
        let mut close = None;
        for (offset, character) in pattern[start..].char_indices() {
            let i = open + 1 + offset;
            match character {
                '{' => depth += 1,
                '}' if depth == 0 => {
                    choices.push(&pattern[start..i]);
                    close = Some(i);
                    break;
                }
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    choices.push(&pattern[start..i]);
                    start = i + 1;
                }
                _ => (),
            }
        }
        let close = close.context("Unclosed workspace glob brace")?;
        ensure!(choices.len() >= 2, "Unsupported workspace glob brace: {pattern}");
        for choice in choices {
            pending.push(format!("{}{}{}", &pattern[..open], choice, &pattern[close+1..]));
        }
        ensure!(pending.len() + expanded.len() <= 4096, "Too many workspace glob alternatives");
    }
    Ok(expanded)
}

fn match_directories(root: &Path, start: &Path, pattern: &str) -> Result<BTreeSet<PathBuf>> {
    let relative = normalize_relative(Path::new(pattern))
        .with_context(|| format!("Invalid workspace glob: {pattern}"))?;
    let components: Vec<_> = relative.iter().map(|part| {
        Pattern::new(part.to_str().context("Non-Unicode workspace glob")?)
            .with_context(|| format!("Invalid workspace glob: {pattern}"))
    }).collect::<Result<_>>()?;
    let mut matches = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut pending = vec![(start.to_owned(), 0)];
    while let Some((dir, index)) = pending.pop() {
        if !visited.insert((dir.clone(), index)) { continue; }
        let Some(component) = components.get(index) else {
            matches.insert(dir);
            continue;
        };
        let segment = component.as_str();
        let recursive = segment == "**";
        if recursive { pending.push((dir.clone(), index + 1)); }
        if !segment.contains(['*', '?', '[']) {
            if ignored_name(segment) { continue; }
            let child = dir.join(segment);
            match fs::symlink_metadata(root.join(&child)) {
                Ok(metadata) if metadata.is_dir() => pending.push((child, index + 1)),
                Ok(_) => (),
                Err(error) if error.kind() == ErrorKind::NotFound => (),
                Err(error) => return Err(error).with_context(|| format!("Accessing {}", child.display())),
            }
            continue;
        }
        // Walk only the directories selected by the glob. Never descend into
        // installed dependencies, repository metadata, or symlinked directories.
        for entry in fs::read_dir(root.join(&dir)).with_context(|| format!("Reading {}", dir.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else { continue };
            if ignored_name(name_str) || !entry.file_type()?.is_dir() { continue; }
            let selected = if recursive {
                !name_str.starts_with('.')
            } else {
                component.matches_with(name_str, MATCH_OPTIONS)
            };
            if selected { pending.push((dir.join(name), if recursive { index } else { index + 1 })); }
        }
    }
    Ok(matches)
}

fn workspace_directories(root: &Path, start: &Path, patterns: &[String]) -> Result<BTreeSet<PathBuf>> {
    let mut included = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for pattern in patterns {
        ensure!(!["@(", "!(", "+(", "*(", "?("].iter().any(|operator| pattern.contains(operator)),
            "Extended workspace glob is not supported: {pattern}");
        let (pattern, out) = match pattern.strip_prefix('!') {
            Some(pattern) => (pattern, &mut excluded),
            None => (pattern.as_str(), &mut included),
        };
        ensure!(!pattern.is_empty(), "Empty workspace glob");
        for pattern in brace_patterns(pattern)? {
            out.extend(match_directories(root, start, &pattern)?);
        }
    }
    included.retain(|dir| !excluded.contains(dir));
    included.insert(start.to_owned());
    Ok(included)
}

fn local_versions(packages: &[Package]) -> Result<BTreeMap<&str, Local<'_>>> {
    let mut locals = BTreeMap::new();
    let mut names = BTreeSet::new();
    for package in packages {
        let Some(name) = package.name.as_deref() else { continue };
        ensure!(names.insert(name), "Duplicate workspace package name: {name}");
        let Some(version) = package.version.as_ref() else { continue };
        if Version::parse(&version.current).is_ok() {
            locals.insert(name, Local { dir: &package.dir, version: &version.current });
        }
    }
    Ok(locals)
}

/// A whole-version dependency is unambiguous; preserve its operator and any
/// workspace protocol. Do not adjust compound ranges, aliases, paths or tags.
fn local_suffix(specifier: &str, version: &str) -> Option<usize> {
    let range = specifier.strip_prefix("workspace:").unwrap_or(specifier);
    if ["", "^", "~", "="].iter().any(|prefix| range.strip_prefix(prefix) == Some(version)) {
        Some(specifier.len() - version.len())
    } else { None }
}

fn dependency_targets(path: &Path, source: &str, object: &Object<'_>, fields: &[&str],
                      dir: &Path, locals: &BTreeMap<&str, Local<'_>>, targets: &mut NodeTargets) -> Result<()> {
    for &section in fields {
        let Some(deps) = object_field(object, section)? else { continue };
        let mut seen = BTreeSet::new();
        for property in &deps.properties {
            let name = property.name.as_str();
            let Some(local) = locals.get(name) else { continue };
            if local.dir == dir { continue; }
            ensure!(seen.insert(name), "Duplicate dependency {name} in {section}");
            let Some(literal) = property.value.as_string_lit() else { continue };
            let Some(offset) = local_suffix(&literal.value, local.version) else { continue };
            let span = literal_span(source, literal)?;
            let target = Target::new(path, source, span.start+offset..span.end,
                format!("{section}.{name}"), Role::Linked, Scheme::Semver)?;
            targets.dependencies.push((Reference::new(dir, name, &literal.value), target));
        }
    }
    Ok(())
}

fn lock_key(path: &Path) -> Result<String> {
    // Lockfiles use forward slashes on every platform.
    Ok(path.iter().map(|name| name.to_str().context("Non-Unicode workspace path"))
        .collect::<Result<Vec<_>>>()?.join("/"))
}

// The canonical lock formats use relative link paths. Nonliteral paths
// (for example URI-encoded paths) cannot establish a conflicting identity.
fn link_matches(base: &Path, link: &str, local: &Path) -> Option<bool> {
    if link.is_empty() || Path::new(link).is_absolute() { return None; }
    if normalize_relative(&base.join(link)).is_ok_and(|dir| dir == local) {
        return Some(true);
    }
    if link.contains(['%', '\\']) { return None; }
    Some(false)
}

fn remote_resolution(value: &str) -> bool {
    ["https://", "http://", "git+", "git://", "ssh://", "git:", "github:", "gitlab:",
        "bitbucket:", "registry.npmjs.org/"].iter().any(|prefix| value.starts_with(prefix))
}

// npm's closest installed entry wins, including a nested node_modules copy
// that shadows the workspace link at the root. Missing resolution metadata
// is not sufficient to conclude that a dependency came from a registry.
fn npm_nonlocal_resolution(start: &Path, importer: &Path, name: &str,
                           local: &Local<'_>, entries: &Object<'_>) -> Result<bool> {
    if normalize_relative(Path::new(name)).is_err() { return Ok(false); }
    let mut directory = importer.strip_prefix(start)?;
    loop {
        let key = lock_key(&directory.join("node_modules").join(name))?;
        if let Some(entry) = object_field(entries, &key)? {
            let resolved = string_field(entry, "resolved")?.map(|v| v.value.as_ref());
            let link = field(entry, "link")?.and_then(Value::as_boolean_lit).is_some_and(|v| v.value);
            if link {
                return Ok(resolved.and_then(|value| {
                    link_matches(start, value.strip_prefix("file:").unwrap_or(value), local.dir)
                }) == Some(false));
            }
            if let Some(resolved) = resolved {
                if remote_resolution(resolved) { return Ok(true); }
                if let Some(path) = resolved.strip_prefix("file:") {
                    return Ok(link_matches(start, path, local.dir) == Some(false));
                }
            }
            if string_field(entry, "version")?.is_some_and(|v| remote_resolution(&v.value)) {
                return Ok(true);
            }
            // npm can omit registry URLs while retaining the artifact hash.
            // With no link/path metadata, this proves an installed archive.
            return Ok(resolved.is_none() && string_field(entry, "integrity")?.is_some());
        }
        let Some(parent) = directory.parent() else { return Ok(false) };
        directory = parent;
    }
}

fn npm_nonlocal_refs(start: &Path, package: &Package, locked: &Object<'_>, entries: &Object<'_>,
                     locals: &BTreeMap<&str, Local<'_>>, targets: &mut NodeTargets) -> Result<()> {
    let manifest = json_object(&package.source, &package.dir.join("package.json"))?;
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        let Some(deps) = object_field(&manifest, section)? else { continue };
        let Some(locked_deps) = object_field(locked, section)? else { continue };
        for property in &deps.properties {
            let name = property.name.as_str();
            let Some(local) = locals.get(name) else { continue };
            if local.dir == package.dir { continue; }
            let Some(literal) = property.value.as_string_lit() else { continue };
            let specifier = literal.value.as_ref();
            if local_suffix(specifier, local.version).is_none() { continue; }
            if string_field(locked_deps, name)?.is_none_or(|v| v.value != specifier) { continue; }
            if npm_nonlocal_resolution(start, &package.dir, name, local, entries)? {
                targets.nonlocal.insert(Reference::new(&package.dir, name, specifier));
            }
        }
    }
    Ok(())
}

fn lock_targets(root: &Path, start: &Path, filename: &str, packages: &[Package],
                locals: &BTreeMap<&str, Local<'_>>, targets: &mut NodeTargets) -> Result<()> {
    let path = start.join(filename);
    let Some(source) = read_file(root, &path)? else { return Ok(()) };
    let object = json_object(&source, &path)?;
    let bun = filename == "bun.lock";
    if !bun && packages.iter().any(|p| p.dir == start && p.version.is_some()) {
        targets.fields.extend(version_target(&path, &source, &object, Role::Linked, "lockfile version")?);
    }
    let section = if bun { "workspaces" } else { "packages" };
    if let Some(entries) = object_field(&object, section)? {
        for package in packages {
            let key = lock_key(package.dir.strip_prefix(start)?)?;
            // npm's node_modules entries resolve installed packages and must
            // not be mistaken for a workspace's own version declaration.
            let Some(entry) = object_field(entries, &key)? else { continue };
            if package.version.is_some() {
                targets.fields.extend(version_target(&path, &source, entry, Role::Linked,
                    &format!("{section}[{key:?}].version"))?);
            }
            if !bun { npm_nonlocal_refs(start, package, entry, entries, locals, targets)?; }
            dependency_targets(&path, &source, entry, DEPENDENCIES, &package.dir, locals, targets)?;
        }
    }
    if !bun {
        // npm v2 may also mirror workspace metadata in the legacy dependency
        // tree. Only a file: reference to an actual member proves it is local.
        if let Some(entries) = object_field(&object, "dependencies")? {
            for (&name, local) in locals {
                if local.dir == start { continue; }
                let Some(entry) = object_field(entries, name)? else { continue };
                let Some(version) = string_field(entry, "version")? else { continue };
                let Some(relative) = version.value.strip_prefix("file:") else { continue };
                let Ok(relative) = normalize_relative(&start.join(relative)) else { continue };
                if relative == local.dir {
                    dependency_targets(&path, &source, entry, &["requires"], local.dir, locals, targets)?;
                }
            }
        }
    }
    Ok(())
}

/// Return a literal YAML scalar's inner span only when it is written verbatim
/// (plain, single-quoted or double-quoted), with optional trailing comments.
fn yaml_literal_span(value: &str, start: usize, expected: &str) -> Option<std::ops::Range<usize>> {
    for quote in ["", "'", "\""] {
        let Some(rest) = value.strip_prefix(quote).and_then(|v| v.strip_prefix(expected))
            .and_then(|v| v.strip_prefix(quote)) else { continue };
        let trimmed = rest.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || (trimmed.len() < rest.len() && trimmed.starts_with('#')) {
            let start = start + quote.len();
            return Some(start..start+expected.len());
        }
    }
    None
}

// serde_yaml_ng verifies the actual dependency graph below, while this small
// scanner recovers spans in pnpm's canonical two-space block mappings. It
// deliberately does not interpret anchors, flow mappings or multiline values.
fn pnpm_specifier_spans(path: &Path, source: &str, wanted: &BTreeMap<Vec<String>, (Reference, usize)>,
                       targets: &mut NodeTargets) -> Result<()> {
    let pair = regex::Regex::new(r#"^('(?:[^']|'')*'|"(?:[^"\\]|\\.)*"|[^:'"][^:]*):(?:[ \t]+(.*)|$)$"#)?;
    let mut parents: Vec<String> = Vec::new();
    let mut next_offset = 0;
    for raw_line in source.split_inclusive('\n') {
        let line_offset = next_offset;
        next_offset += raw_line.len();
        let line = raw_line.trim_end_matches(['\r', '\n']);
        let bom = if line_offset == 0 && line.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let line = &line[bom..];
        let text = line.trim_start_matches(' ');
        let indent = line.len() - text.len();
        if text.is_empty() || text.starts_with('#') { continue; }
        let level = indent / 2;
        parents.truncate(level);
        if indent % 2 != 0 || level != parents.len() || level > 4 { continue; }
        let Some(captures) = pair.captures(text) else { continue };
        let raw_key = captures.get(1).unwrap().as_str().trim_end();
        let key = if raw_key.starts_with(['\'', '"']) {
            let Ok(key) = serde_yaml_ng::from_str::<String>(raw_key) else { continue };
            key
        } else { raw_key.to_owned() };
        if level == 0 && key != "importers" { continue; }
        let value = captures.get(2);
        if level == 4 && key == "specifier" {
            let Some((reference, suffix)) = wanted.get(&parents) else { continue };
            let Some(value) = value else { continue };
            let Some(span) = yaml_literal_span(value.as_str(), line_offset+bom+indent+value.start(), &reference.specifier) else { continue };
            let target = Target::new(path, source, span.start+suffix..span.end,
                format!("importers.{}.{}.{}.specifier", parents[1], parents[2], parents[3]),
                Role::Linked, Scheme::Semver)?;
            targets.dependencies.push((reference.clone(), target));
        }
        if level < 4 && value.is_none_or(|v| v.as_str().is_empty() || v.as_str().starts_with('#')) {
            parents.push(key);
        }
    }
    Ok(())
}

fn pnpm_nonlocal_resolution(start: &Path, importer: &Path, version: &str, local: &Path) -> bool {
    // A plain version (possibly followed by a peer context) references a
    // registry snapshot, not a workspace link.
    if Version::parse(version.split('(').next().unwrap_or(version)).is_ok() || remote_resolution(version) {
        return true;
    }
    // Injected workspace dependencies may use file: paths; do not classify
    // them as external when either relative base could identify the member.
    let Some(file) = version.strip_prefix("file:") else { return false };
    if file.contains(['(', ')']) { return false; }
    link_matches(start, file, local) == Some(false) && link_matches(importer, file, local) == Some(false)
}

fn pnpm_lock_targets(root: &Path, start: &Path, packages: &[Package],
                     locals: &BTreeMap<&str, Local<'_>>, targets: &mut NodeTargets) -> Result<()> {
    use serde_yaml_ng::Value as Yaml;
    if packages.len() < 2 || locals.is_empty() { return Ok(()); }
    let path = start.join("pnpm-lock.yaml");
    let Some(source) = read_file(root, &path)? else { return Ok(()) };
    // v8 writes lockfile format 6.0; v9 writes format 9.0. Other and
    // multi-document formats may have differing resolution semantics.
    let mut documents = serde_yaml_ng::Deserializer::from_str(&source);
    let Some(document) = documents.next() else { return Ok(()) };
    let lock: Yaml = serde::Deserialize::deserialize(document)
        .with_context(|| format!("Parsing {}", path.display()))?;
    if documents.next().is_some() || !matches!(lock.get("lockfileVersion").and_then(Yaml::as_str), Some("6.0" | "9.0")) {
        return Ok(());
    }
    let Some(importers) = lock.get("importers").and_then(Yaml::as_mapping) else { return Ok(()) };
    let mut wanted = BTreeMap::new();
    for package in packages {
        let key = lock_key(package.dir.strip_prefix(start)?)?;
        let key = if key.is_empty() { ".".to_owned() } else { key };
        let Some(importer) = importers.get(key.as_str()) else { continue };
        let object = json_object(&package.source, &package.dir.join("package.json"))?;
        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            let Some(deps) = object_field(&object, section)? else { continue };
            let Some(locked_deps) = importer.get(section) else { continue };
            for property in &deps.properties {
                let name = property.name.as_str();
                let Some(local) = locals.get(name) else { continue };
                if local.dir == package.dir { continue; }
                let Some(literal) = property.value.as_string_lit() else { continue };
                let specifier = literal.value.as_ref();
                let Some(suffix) = local_suffix(specifier, local.version) else { continue };
                let Some(locked) = locked_deps.get(name) else { continue };
                if locked.get("specifier").and_then(Yaml::as_str) != Some(specifier) { continue; }
                let Some(version) = locked.get("version").and_then(Yaml::as_str) else { continue };
                let reference = Reference::new(&package.dir, name, specifier);
                // Exact specifier and link identity prove a local reference;
                // definite registry or nonlocal resolution vetoes the edit.
                let Some(link) = version.strip_prefix("link:") else {
                    if pnpm_nonlocal_resolution(start, &package.dir, version, local.dir) {
                        targets.nonlocal.insert(reference);
                    }
                    continue;
                };
                match link_matches(&package.dir, link, local.dir) {
                    Some(true) => (),
                    Some(false) => { targets.nonlocal.insert(reference); continue; }
                    None => continue,
                }
                wanted.insert(vec!["importers".into(), key.clone(), section.into(), name.into()],
                    (reference, suffix));
            }
        }
    }
    if !wanted.is_empty() { pnpm_specifier_spans(&path, &source, &wanted, targets)?; }
    Ok(())
}

pub fn discover(root: &Path, starts: &[PathBuf]) -> Result<Vec<Target>> {
    let starts: BTreeSet<_> = starts.iter().map(|start| normalize_relative(start)).collect::<Result<_>>()?;
    let mut targets = NodeTargets::default();
    for start in starts {
        if ignored_path(&start) { continue; }
        let root_package = load_package(root, &start)?;
        let patterns = workspace_patterns(root, &start, root_package.as_ref())?;
        let directories = workspace_directories(root, &start, &patterns)?;
        let mut packages: Vec<_> = root_package.into_iter().collect();
        for directory in directories {
            if directory == start { continue; }
            packages.extend(load_package(root, &directory)?);
        }
        if packages.is_empty() { continue; }
        let locals = local_versions(&packages)?;
        for package in &packages {
            targets.fields.extend(package.version.clone());
            let path = package.dir.join("package.json");
            let object = json_object(&package.source, &path)?;
            dependency_targets(&path, &package.source, &object, DEPENDENCIES, &package.dir, &locals, &mut targets)?;
        }
        for filename in ["package-lock.json", "npm-shrinkwrap.json", "bun.lock"] {
            lock_targets(root, &start, filename, &packages, &locals, &mut targets)?;
        }
        pnpm_lock_targets(root, &start, &packages, &locals, &mut targets)?;
    }
    let mut targets = targets.finish();
    targets.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)).then(a.range.end.cmp(&b.range.end)));
    targets.dedup_by(|a, b| a.path == b.path && a.range == b.range);
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    fn project(files: &[(&str, &str)]) -> TempDir {
        let dir = tempdir().unwrap();
        for &(name, content) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        dir
    }

    fn discover_at(dir: &TempDir, starts: &[&str]) -> Vec<Target> {
        discover(&dir.path().canonicalize().unwrap(),
            &starts.iter().map(PathBuf::from).collect::<Vec<_>>()).unwrap()
    }

    fn bumped(dir: &TempDir, targets: &[Target], path: &str) -> String {
        let mut content = fs::read_to_string(dir.path().join(path)).unwrap();
        let mut edits: Vec<_> = targets.iter().filter(|t| t.path == Path::new(path)).collect();
        edits.sort_by_key(|t| std::cmp::Reverse(t.range.start));
        for target in edits {
            assert_eq!(&content[target.range.clone()], target.current);
            content.replace_range(target.range.clone(), "2.0.0");
        }
        content
    }

    #[test]
    fn npm_root_and_lock_preserve_bytes() {
        let manifest = "\u{feff}{\r\n  \"name\": \"root\", \"description\": \"版本\",\r\n  \"version\" : \"1.2.3\",\r\n  \"dependencies\": {\"external\":\"1.2.3\"},\r\n  \"config\":{\"version\":\"1.2.3\"}\r\n}\r\n";
        let lock = r#"{"name":"root","version":"1.2.3","lockfileVersion":2,"packages":{"":{"version":"1.2.3","dependencies":{"external":"1.2.3"}},"node_modules/external":{"version":"1.2.3"}},"dependencies":{"external":{"version":"1.2.3"}}}"#;
        let shrinkwrap = r#"{"name":"root","version":"1.2.3","lockfileVersion":1,"dependencies":{"external":{"version":"1.2.3"}}}"#;
        let dir = project(&[("package.json", manifest), ("package-lock.json", lock), ("npm-shrinkwrap.json", shrinkwrap),
            ("node_modules/external/package.json", r#"{"name":"external","version":"1.2.3"}"#)]);
        let targets = discover_at(&dir, &["", "node_modules/external"]);
        assert_eq!(targets.len(), 4);
        assert_eq!(targets.iter().filter(|t| t.role == Role::Primary).count(), 1);
        assert_eq!(bumped(&dir, &targets, "package.json"), manifest.replacen("\"version\" : \"1.2.3\"", "\"version\" : \"2.0.0\"", 1));
        assert_eq!(bumped(&dir, &targets, "package-lock.json"), lock.replacen("\"version\":\"1.2.3\"", "\"version\":\"2.0.0\"", 2));
        assert_eq!(bumped(&dir, &targets, "npm-shrinkwrap.json"), shrinkwrap.replacen("\"version\":\"1.2.3\"", "\"version\":\"2.0.0\"", 1));
        assert_eq!(fs::read_to_string(dir.path().join("package.json")).unwrap(), manifest);
    }

    #[test]
    fn workspace_only_updates_known_members_and_simple_local_ranges() {
        let root = r#"{"name":"root","version":"1.2.3","workspaces":["packages/*","!packages/skip"],"dependencies":{"@scope/a":"^1.2.3","external":"1.2.3","skip":"^1.2.3","b":">=1.2.3"}}"#;
        let a = r#"{"name":"@scope/a","version":"1.2.3","dependencies":{"b":"workspace:~1.2.3","skip":"1.2.3"},"devDependencies":{"b":"workspace:*"},"peerDependencies":{"b":"^1.2.3 || ^2.0.0"}}"#;
        let b = r#"{"name":"b","version":"1.2.3","optionalDependencies":{"@scope/a":"=1.2.3"},"dependencies":{"root":"1.2.3"}}"#;
        let dir = project(&[
            ("package.json", root), ("packages/a/package.json", a), ("packages/b/package.json", b),
            ("packages/skip/package.json", r#"{"name":"skip","version":"1.2.3"}"#),
            ("packages/a/node_modules/nested/package.json", r#"{"name":"nested","version":"1.2.3"}"#),
            ("unrelated/package.json", r#"{"name":"external","version":"1.2.3"}"#),
        ]);
        let targets = discover_at(&dir, &["", "packages/a", ""]);
        assert_eq!(targets.len(), 7);
        assert_eq!(targets.iter().filter(|t| t.role == Role::Primary).count(), 3);
        assert_eq!(bumped(&dir, &targets, "package.json"),
            root.replacen("\"version\":\"1.2.3\"", "\"version\":\"2.0.0\"", 1).replace("\"@scope/a\":\"^1.2.3\"", "\"@scope/a\":\"^2.0.0\""));
        assert_eq!(bumped(&dir, &targets, "packages/a/package.json"),
            a.replacen("\"version\":\"1.2.3\"", "\"version\":\"2.0.0\"", 1).replace("workspace:~1.2.3", "workspace:~2.0.0"));
        assert_eq!(bumped(&dir, &targets, "packages/b/package.json"), b.replace("1.2.3", "2.0.0"));
    }

    #[test]
    fn workspace_locks_leave_installed_packages_alone() {
        let lock = r#"{"version":"1.0.0","packages":{"":{"version":"1.0.0","dependencies":{"a":"^1.0.0"}},"packages/a":{"version":"1.0.0","dependencies":{"b":"1.0.0"}},"packages/b":{"version":"1.0.0"},"node_modules/a":{"resolved":"packages/a","link":true},"packages/a/node_modules/b":{"version":"1.0.0"},"node_modules/external":{"version":"1.0.0","dependencies":{"a":"^1.0.0"}}},"dependencies":{"a":{"version":"file:packages/a","requires":{"b":"1.0.0"}},"b":{"version":"1.0.0"},"external":{"version":"1.0.0","requires":{"a":"^1.0.0"}}}}"#;
        let dir = project(&[
            ("package.json", r#"{"name":"root","version":"1.0.0","workspaces":["packages/*"],"dependencies":{"a":"^1.0.0"}}"#),
            ("packages/a/package.json", r#"{"name":"a","version":"1.0.0","dependencies":{"b":"1.0.0"}}"#),
            ("packages/b/package.json", r#"{"name":"b","version":"1.0.0"}"#),
            ("package-lock.json", lock),
        ]);
        let targets = discover_at(&dir, &[""]);
        let expected = lock.replacen("\"version\":\"1.0.0\"", "\"version\":\"2.0.0\"", 4)
            .replacen("\"a\":\"^1.0.0\"", "\"a\":\"^2.0.0\"", 1)
            .replace("\"b\":\"1.0.0\"", "\"b\":\"2.0.0\"");
        assert_eq!(bumped(&dir, &targets, "package-lock.json"), expected);
    }

    #[test]
    fn npm_resolution_evidence_vetoes_nonlocal_manifest_and_lock_ranges() {
        let root = r#"{"version":"1.0.0","workspaces":["packages/*"],"dependencies":{"a":"^1.0.0","b":"^1.0.0","c":"^1.0.0","d":"^1.0.0","e":"^1.0.0","f":"^1.0.0"}}"#;
        let member = r#"{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0"}}"#;
        let lock = r#"{"version":"1.0.0","lockfileVersion":2,"packages":{"":{"version":"1.0.0","dependencies":{"a":"^1.0.0","b":"^1.0.0","c":"^1.0.0","d":"^1.0.0","e":"^1.0.0","f":"^1.0.0"}},"packages/a":{"version":"1.0.0","dependencies":{"b":"^1.0.0"}},"packages/b":{"version":"1.0.0"},"packages/c":{"version":"1.0.0"},"packages/d":{"version":"1.0.0"},"packages/e":{"version":"1.0.0"},"packages/f":{"version":"1.0.0"},"node_modules/a":{"link":true,"resolved":"packages/a"},"node_modules/b":{"link":true,"resolved":"packages/b"},"packages/a/node_modules/b":{"version":"1.0.0","resolved":"https://registry.example.test/b-1.0.0.tgz"},"node_modules/c":{"version":"1.0.0","resolved":"https://registry.example.test/c-1.0.0.tgz"},"node_modules/d":{"link":true,"resolved":"packages/b"},"node_modules/e":{"version":"1.0.0","integrity":"sha512-test"},"node_modules/f":{"version":"1.0.0"}},"dependencies":{"a":{"version":"file:packages/a","requires":{"b":"^1.0.0"}}}}"#;
        let dir = project(&[
            ("package.json", root), ("package-lock.json", lock), ("packages/a/package.json", member),
            ("packages/b/package.json", r#"{"name":"b","version":"1.0.0"}"#),
            ("packages/c/package.json", r#"{"name":"c","version":"1.0.0"}"#),
            ("packages/d/package.json", r#"{"name":"d","version":"1.0.0"}"#),
            ("packages/e/package.json", r#"{"name":"e","version":"1.0.0"}"#),
            ("packages/f/package.json", r#"{"name":"f","version":"1.0.0"}"#),
        ]);
        let targets = discover_at(&dir, &[""]);
        let ranges = |text: String| text.replacen("\"a\":\"^1.0.0\"", "\"a\":\"^2.0.0\"", 1)
            .replacen("\"b\":\"^1.0.0\"", "\"b\":\"^2.0.0\"", 1)
            .replacen("\"f\":\"^1.0.0\"", "\"f\":\"^2.0.0\"", 1);
        assert_eq!(bumped(&dir, &targets, "package.json"),
            ranges(root.replacen("\"version\":\"1.0.0\"", "\"version\":\"2.0.0\"", 1)));
        assert_eq!(bumped(&dir, &targets, "packages/a/package.json"),
            member.replacen("\"version\":\"1.0.0\"", "\"version\":\"2.0.0\"", 1));
        assert_eq!(bumped(&dir, &targets, "package-lock.json"),
            ranges(lock.replacen("\"version\":\"1.0.0\"", "\"version\":\"2.0.0\"", 8)));
    }

    #[test]
    fn yarn_object_workspace_braces() {
        let dir = project(&[
            ("package.json", r#"{"private":true,"workspaces":{"packages":["{libs,apps}/*"],"nohoist":["**"]}}"#),
            ("libs/a/package.json", r#"{"version":"1.0.0"}"#),
            ("apps/b/package.json", r#"{"version":"1.0.0"}"#),
            ("other/c/package.json", r#"{"version":"1.0.0"}"#),
        ]);
        let targets = discover_at(&dir, &[""]);
        assert_eq!(targets.iter().map(|t| t.path.as_path()).collect::<Vec<_>>(),
            vec![Path::new("apps/b/package.json"), Path::new("libs/a/package.json")]);
    }

    #[test]
    fn pnpm_patterns_are_authoritative_and_respect_exclusions() {
        let dir = project(&[
            ("package.json", r#"{"version":"1.0.0","workspaces":["other/*","packages/*"]}"#),
            ("pnpm-workspace.yaml", "packages:\n  - './packages/*'\n  - modules/**\n  - '!packages/skip'\n  - '!**/test/**'\ncatalog:\n  example: 1.0.0\n"),
            ("packages/a/package.json", r#"{"name":"a","version":"1.0.0","dependencies":{"b":"workspace:1.0.0"}}"#),
            ("modules/deep/b/package.json", r#"{"name":"b","version":"1.0.0"}"#),
            ("packages/skip/package.json", r#"{"version":"1.0.0"}"#),
            ("packages/.hidden/package.json", r#"{"version":"1.0.0"}"#),
            ("modules/deep/test/fixture/package.json", r#"{"version":"1.0.0"}"#),
            ("modules/node_modules/pkg/package.json", r#"{"version":"1.0.0"}"#),
            ("other/pkg/package.json", r#"{"version":"1.0.0"}"#),
        ]);
        let targets = discover_at(&dir, &[""]);
        assert_eq!(targets.len(), 4);
        assert_eq!(targets.iter().filter(|t| t.role == Role::Primary).count(), 3);
        assert_eq!(bumped(&dir, &targets, "packages/a/package.json"),
            r#"{"name":"a","version":"2.0.0","dependencies":{"b":"workspace:2.0.0"}}"#);
    }

    #[test]
    fn pnpm_lock_updates_only_exact_proven_local_specifiers() {
        let manifest = r#"{"version":"1.0.0","dependencies":{"@scope/a":"^1.0.0","b":"^1.0.0","c":"^1.0.0","d":"^1.0.0","e":"workspace:*","f":"^1.0.0","external":"^1.0.0"}}"#;
        let fixture = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      '@scope/a':
        specifier: '^1.0.0' # keep comment
        version: link:packages/a
      b:
        specifier: ^1.0.0
        version: 1.0.0
      c:
        specifier: ^1.0.0
        version: link:packages/b
      d:
        specifier: ~1.0.0
        version: link:packages/d
      e:
        specifier: workspace:*
        version: link:packages/e
      f:
        specifier: ^1.0.0
        version: file:packages/f
      external:
        specifier: ^1.0.0
        version: 1.0.0

  packages/a:
    devDependencies:
      b:
        specifier: "workspace:~1.0.0"
        version: link:../b
    optionalDependencies:
      c:
        specifier: =1.0.0
        version: link:../c

  packages/other:
    dependencies:
      '@scope/a':
        specifier: '^1.0.0'
        version: link:../a

packages:
  '@scope/a@1.0.0':
    resolution: {integrity: sha512-example}
    dependencies:
      b: 1.0.0
"#;
        for version in ["6.0", "9.0"] {
            let mut lock = fixture.replacen("9.0", version, 1);
            if version == "9.0" { lock = lock.replace('\n', "\r\n"); }
            let dir = project(&[
                ("package.json", manifest),
                ("pnpm-workspace.yaml", "packages:\n  - packages/*\n  - '!packages/other'\n"),
                ("packages/a/package.json", r#"{"name":"@scope/a","version":"1.0.0","devDependencies":{"b":"workspace:~1.0.0"},"optionalDependencies":{"c":"=1.0.0"}}"#),
                ("packages/b/package.json", r#"{"name":"b","version":"1.0.0"}"#),
                ("packages/c/package.json", r#"{"name":"c","version":"1.0.0"}"#),
                ("packages/d/package.json", r#"{"name":"d","version":"1.0.0"}"#),
                ("packages/e/package.json", r#"{"name":"e","version":"1.0.0"}"#),
                ("packages/f/package.json", r#"{"name":"f","version":"1.0.0"}"#),
                ("packages/other/package.json", r#"{"name":"other","version":"1.0.0","dependencies":{"@scope/a":"^1.0.0"}}"#),
                ("pnpm-lock.yaml", &lock),
            ]);
            let targets = discover_at(&dir, &[""]);
            assert_eq!(targets.iter().filter(|t| t.path == Path::new("pnpm-lock.yaml")).count(), 3);
            let expected = lock.replace("specifier: '^1.0.0' # keep comment", "specifier: '^2.0.0' # keep comment")
                .replace("specifier: \"workspace:~1.0.0\"", "specifier: \"workspace:~2.0.0\"")
                .replace("specifier: =1.0.0", "specifier: =2.0.0");
            assert_eq!(bumped(&dir, &targets, "pnpm-lock.yaml"), expected);
            // b is a registry package and c points to a different member.
            // The stale d entry and ambiguous injected f entry prove neither.
            assert_eq!(bumped(&dir, &targets, "package.json"), manifest
                .replacen("\"version\":\"1.0.0\"", "\"version\":\"2.0.0\"", 1)
                .replace("\"@scope/a\":\"^1.0.0\"", "\"@scope/a\":\"^2.0.0\"")
                .replace("\"d\":\"^1.0.0\"", "\"d\":\"^2.0.0\"")
                .replace("\"f\":\"^1.0.0\"", "\"f\":\"^2.0.0\""));
        }
    }

    #[test]
    fn pnpm_lock_skips_noncanonical_and_unknown_formats() {
        let block = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a:\n        specifier: ^1.0.0\n        version: link:packages/a\n";
        for lock in [
            block.replace("9.0", "13.0"),
            format!("{block}---\nlockfileVersion: '9.0'\nimporters: {{}}\n"),
            "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      a: {specifier: ^1.0.0, version: link:packages/a}\n".to_string(),
        ] {
            let dir = project(&[
                ("package.json", r#"{"version":"1.0.0","dependencies":{"a":"^1.0.0"}}"#),
                ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
                ("packages/a/package.json", r#"{"name":"a","version":"1.0.0"}"#),
                ("pnpm-lock.yaml", &lock),
            ]);
            let targets = discover_at(&dir, &[""]);
            assert_eq!(bumped(&dir, &targets, "pnpm-lock.yaml"), lock);
        }
    }

    #[test]
    fn bun_text_lock_preserves_comments_and_package_resolution() {
        let lock = r#"{
  // Versions of our workspace packages
  "lockfileVersion": 1,
  "workspaces": {
    "": {"version":"1.0.0","dependencies":{"a":"~1.0.0"},},
    "packages/a": {"name":"a","version":"1.0.0"},
  },
  "packages": {
    "a": ["a@workspace:packages/a"],
    "external": ["external@1.0.0", "", {}, "sha512-test"],
  },
}"#;
        let dir = project(&[
            ("package.json", r#"{"version":"1.0.0","workspaces":["packages/*"],"dependencies":{"a":"~1.0.0"}}"#),
            ("packages/a/package.json", r#"{"name":"a","version":"1.0.0"}"#),
            ("bun.lock", lock),
        ]);
        let targets = discover_at(&dir, &[""]);
        assert_eq!(targets.len(), 6);
        assert_eq!(bumped(&dir, &targets, "bun.lock"),
            lock.replace("\"version\":\"1.0.0\"", "\"version\":\"2.0.0\"").replace("~1.0.0", "~2.0.0"));
    }

    #[test]
    fn separate_starts_are_not_one_workspace() {
        let b = r#"{"name":"b","version":"1.0.0","dependencies":{"a":"1.0.0"}}"#;
        let dir = project(&[
            ("a/package.json", r#"{"name":"a","version":"1.0.0"}"#),
            ("b/package.json", b),
        ]);
        let targets = discover_at(&dir, &["a", "b"]);
        assert_eq!(targets.len(), 2);
        assert_eq!(bumped(&dir, &targets, "b/package.json"), b.replacen("1.0.0", "2.0.0", 1));
    }

    #[test]
    fn duplicate_version_is_rejected() {
        let dir = project(&[("package.json", r#"{"version":"1.0.0","version":"2.0.0"}"#)]);
        assert!(discover(&dir.path().canonicalize().unwrap(), &[PathBuf::new()]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_globs_do_not_follow_symlinks() {
        let outside = project(&[("package.json", r#"{"version":"1.0.0"}"#)]);
        let dir = project(&[("package.json", r#"{"version":"1.0.0","workspaces":["**"]}"#)]);
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
        let targets = discover_at(&dir, &[""]);
        assert_eq!(targets.len(), 1);
    }
}
