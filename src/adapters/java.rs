use crate::model::{Role, Scheme, Target, normalize_relative, read_file};
use anyhow::{Context, Result, bail};
use roxmltree::Node;
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, ops::Range, path::{Path, PathBuf}};

fn literal(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._+-!".contains(c))
}

fn insert_target(targets: &mut BTreeMap<(PathBuf, usize, usize), Target>, target: Target) {
    let key = (target.path.clone(), target.range.start, target.range.end);
    if targets.get(&key).is_none_or(|old| old.role == Role::Linked) {
        targets.insert(key, target);
    }
}

pub fn discover(root: &Path, starts: &[PathBuf]) -> Result<Vec<Target>> {
    let mut targets = BTreeMap::new();
    discover_maven(root, starts, &mut targets)?;
    discover_gradle(root, starts, &mut targets)?;
    Ok(targets.into_values().collect())
}

type Targets = BTreeMap<(PathBuf, usize, usize), Target>;

#[derive(Clone, Debug)]
struct Value { text: String, range: Option<Range<usize>> }

fn named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|n| named(*n, name) && n.tag_name().namespace() == node.tag_name().namespace())
}

fn trimmed_range(source: &str, range: Range<usize>) -> Range<usize> {
    let raw = &source[range.clone()];
    let trimmed = raw.trim();
    let start = range.start + raw.len() - raw.trim_start().len();
    start..start + trimmed.len()
}

/// Edit only a contiguous, unescaped text node; XML namespaces, whitespace,
/// comments, and the rest of the original file remain untouched.
fn value(node: Node<'_, '_>, source: &str) -> Option<Value> {
    if node.children().any(|n| n.is_element()) { return None; }
    let text = node.children().filter_map(|n| n.is_text().then(|| n.text()).flatten()).collect::<String>();
    let text = text.trim().to_owned();
    let mut pieces = node.children().filter(|n| n.is_text() && n.text().is_some_and(|s| !s.trim().is_empty()));
    let range = pieces.next().and_then(|piece| {
        let range = trimmed_range(source, piece.range());
        (pieces.next().is_none() && source.get(range.clone()) == Some(&text)).then_some(range)
    });
    Some(Value { text, range })
}

fn field(node: Node<'_, '_>, name: &str, source: &str) -> Option<Value> {
    value(child(node, name)?, source)
}

#[derive(Clone)]
struct Reference { group: String, artifact: String, version: Value, kind: RefKind }
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefKind { Parent, Dependency, Other }

struct Pom {
    source: String,
    group: String,
    artifact: String,
    version: Option<Value>,
    parent: Option<Reference>,
    relative_parent: Option<String>,
    modules: Vec<String>,
    properties: BTreeMap<String, Value>,
    refs: Vec<Reference>,
}

impl Pom {
    fn parse(source: String, path: &Path) -> Result<Self> {
        let doc = roxmltree::Document::parse(&source).with_context(|| format!("Parsing Maven POM {}", path.display()))?;
        let project = doc.root_element();
        if !named(project, "project") { bail!("Expected Maven <project> in {}", path.display()); }
        let text = |node, name| field(node, name, &source).map(|v| v.text).unwrap_or_default();
        let parent_node = child(project, "parent");
        let parent = parent_node.and_then(|p| field(p, "version", &source).map(|version| Reference {
            group: text(p, "groupId"), artifact: text(p, "artifactId"), version, kind: RefKind::Parent,
        }));
        let group = child(project, "groupId").map(|n| value(n, &source).map(|v| v.text).unwrap_or_default())
            .or_else(|| parent.as_ref().map(|p| p.group.clone())).unwrap_or_default();
        let artifact = text(project, "artifactId");
        let version = field(project, "version", &source);
        let relative_parent = parent_node.and_then(|n| field(n, "relativePath", &source).map(|v| v.text));
        let modules = child(project, "modules").into_iter().flat_map(|n| n.children()).filter(|n| {
            named(*n, "module") && n.tag_name().namespace() == project.tag_name().namespace()
        }).filter_map(|n| value(n, &source).map(|v| v.text)).collect();
        let properties = child(project, "properties").into_iter().flat_map(|n| n.children())
            .filter(|n| n.is_element() && n.tag_name().namespace() == project.tag_name().namespace())
            .filter_map(|n| value(n, &source).map(|v| (n.tag_name().name().to_owned(), v))).collect();
        let mut refs = Vec::new();
        if let Some(parent) = &parent { refs.push(parent.clone()); }
        // Only dependencies in Maven dependency and dependencyManagement lists
        // are eligible for linking. Plugin/configuration metadata is not a GAV
        // declaration for a dependency of this project.
        for n in project.descendants().filter(|n| named(*n, "version") &&
            n.tag_name().namespace() == project.tag_name().namespace()) {
            let Some(container) = n.parent_element() else { continue; };
            if container == project || parent_node == Some(container) || named(container, "properties") { continue; }
            let Some(version) = value(n, &source) else { continue; };
            let is_dependency = named(container, "dependency") && container.parent_element().is_some_and(|deps| {
                named(deps, "dependencies") && deps.parent_element().is_some_and(|owner| {
                    owner == project || is_project_profile(owner, project) ||
                        (named(owner, "dependencyManagement") && owner.parent_element().is_some_and(|p| p == project || is_project_profile(p, project)))
                })
            });
            refs.push(Reference {
                group: text(container, "groupId"), artifact: text(container, "artifactId"),
                version, kind: if is_dependency { RefKind::Dependency } else { RefKind::Other },
            });
        }
        drop(doc);
        Ok(Self { source, group, artifact, version, parent, relative_parent, modules, properties, refs })
    }

    fn parent_path(&self, file: &Path) -> Option<PathBuf> {
        self.parent.as_ref()?;
        let path = self.relative_parent.as_deref().unwrap_or("../pom.xml");
        if path.is_empty() || path.contains('$') { return None; }
        let path = normalize_relative(&file.parent()?.join(path)).ok()?;
        if path.extension().is_some_and(|s| s == "xml") { Some(path) } else { Some(path.join("pom.xml")) }
    }
}

fn is_project_profile(node: Node<'_, '_>, project: Node<'_, '_>) -> bool {
    named(node, "profile") && node.parent_element().is_some_and(|p| named(p, "profiles") && p.parent_element() == Some(project))
}

type Poms = BTreeMap<PathBuf, Pom>;
type Parents = BTreeMap<PathBuf, PathBuf>;

fn load_modules(root: &Path, initial: Vec<PathBuf>, poms: &mut Poms, scope: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut queue = VecDeque::from(initial);
    while let Some(path) = queue.pop_front() {
        if !scope.insert(path.clone()) { continue; }
        if !poms.contains_key(&path) {
            let Some(source) = read_file(root, &path)? else { continue; };
            poms.insert(path.clone(), Pom::parse(source, &path)?);
        }
        for module in &poms[&path].modules {
            if module.is_empty() || module.contains('$') { continue; }
            let Ok(path) = normalize_relative(&path.parent().unwrap_or(Path::new("")).join(module)) else { continue; };
            queue.push_back(if path.extension().is_some_and(|s| s == "xml") { path } else { path.join("pom.xml") });
        }
    }
    Ok(())
}

/// Relative parent paths are only candidates: verify the actual Maven identity
/// and then the version before treating a POM as part of this project.
fn load_parents(root: &Path, poms: &mut Poms) -> Result<Parents> {
    let mut queue = VecDeque::from_iter(poms.keys().cloned());
    let mut parents = Parents::new();
    let mut seen = BTreeSet::new();
    while let Some(file) = queue.pop_front() {
        if !seen.insert(file.clone()) { continue; }
        let pom = &poms[&file];
        let Some(path) = pom.parent_path(&file) else { continue; };
        if path == file { continue; }
        let parent = pom.parent.as_ref().unwrap();
        if parent.group.is_empty() || parent.artifact.is_empty() || parent.group.contains('$') || parent.artifact.contains('$') { continue; }
        if !poms.contains_key(&path) {
            let Some(source) = read_file(root, &path)? else { continue; };
            let candidate = Pom::parse(source, &path)?;
            if parent.group != candidate.group || parent.artifact != candidate.artifact { continue; }
            poms.insert(path.clone(), candidate);
        }
        let parent = poms[&file].parent.as_ref().unwrap();
        let candidate = &poms[&path];
        if parent.group == candidate.group && parent.artifact == candidate.artifact {
            queue.push_back(path.clone());
            parents.insert(file, path);
        }
    }
    // Matching paths and coordinates are not enough if the POM refers to an
    // installed, different release. Resolve exact property expressions first.
    loop {
        let invalid: Vec<_> = parents.iter().filter(|(file, parent)| {
            let reference = &poms[*file].parent.as_ref().unwrap().version.text;
            let actual = effective_version(poms, &parents, parent);
            actual.is_none() || resolve(poms, &parents, file, reference).map(|(v, _)| v) != actual
        }).map(|(file, _)| file.clone()).collect();
        if invalid.is_empty() { break; }
        for file in invalid { parents.remove(&file); }
    }
    Ok(parents)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Binding { path: PathBuf, property: Option<String> }
impl Binding {
    fn value<'a>(&self, poms: &'a Poms) -> Option<&'a Value> {
        let pom = poms.get(&self.path)?;
        match &self.property { Some(key) => pom.properties.get(key), None => pom.version.as_ref() }
    }
}

fn property(s: &str) -> Option<&str> {
    let key = s.strip_prefix("${")?.strip_suffix('}')?;
    (!key.is_empty() && !key.contains(['{', '}', '$'])).then_some(key)
}

fn property_binding(poms: &Poms, parents: &Parents, path: &Path, key: &str) -> Option<Binding> {
    let mut current = path;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current.to_path_buf()) { return None; }
        if poms.get(current)?.properties.contains_key(key) {
            return Some(Binding { path: current.to_path_buf(), property: Some(key.into()) });
        }
        current = parents.get(current)?.as_path();
    }
}

fn resolve(poms: &Poms, parents: &Parents, path: &Path, expr: &str) -> Option<(String, Option<Binding>)> {
    let mut text = expr.to_owned();
    let mut binding = None;
    let mut seen = BTreeSet::new();
    while let Some(key) = property(&text) {
        let b = property_binding(poms, parents, path, key)?;
        if !seen.insert(b.clone()) { return None; }
        text = b.value(poms)?.text.clone();
        binding = Some(b);
    }
    literal(&text).then_some((text, binding))
}

fn effective_version(poms: &Poms, parents: &Parents, path: &Path) -> Option<String> {
    let pom = poms.get(path)?;
    let version = pom.version.as_ref().or_else(|| pom.parent.as_ref().map(|p| &p.version))?;
    resolve(poms, parents, path, &version.text).map(|(v, _)| v)
}

fn project_binding(poms: &Poms, parents: &Parents, path: &Path) -> Option<Binding> {
    let mut path = path;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(path.to_path_buf()) { return None; }
        let pom = poms.get(path)?;
        if let Some(version) = &pom.version {
            let (_, binding) = resolve(poms, parents, path, &version.text)?;
            let binding = binding.unwrap_or(Binding { path: path.to_path_buf(), property: None });
            binding.value(poms)?.range.as_ref()?;
            return Some(binding);
        }
        path = parents.get(path)?;
    }
}

fn resolve_coordinate(poms: &Poms, parents: &Parents, file: &Path, text: &str) -> Option<String> {
    let pom = poms.get(file)?;
    let text = match text {
        "${project.groupId}" | "${pom.groupId}" => &pom.group,
        "${project.artifactId}" | "${pom.artifactId}" => &pom.artifact,
        other => other,
    };
    resolve(poms, parents, file, text).map(|(v, _)| v)
}

fn ref_version(poms: &Poms, parents: &Parents, file: &Path, reference: &Reference) -> Option<(String, Option<Binding>)> {
    match reference.version.text.as_str() {
        "${project.version}" | "${pom.version}" => effective_version(poms, parents, file).map(|v| (v, None)),
        value => resolve(poms, parents, file, value),
    }
}

fn linked_project<'a>(
    poms: &Poms, parents: &'a Parents, scope: &'a BTreeSet<PathBuf>, active: &BTreeSet<Binding>,
    file: &Path, reference: &Reference,
) -> Option<&'a Path> {
    let current = ref_version(poms, parents, file, reference)?.0;
    let path = match reference.kind {
        RefKind::Parent => parents.get(file)?.as_path(),
        RefKind::Dependency => {
            let group = resolve_coordinate(poms, parents, file, &reference.group)?;
            let artifact = resolve_coordinate(poms, parents, file, &reference.artifact)?;
            let mut matches = scope.iter().filter(|path| poms.get(*path).is_some_and(|pom| {
                resolve_coordinate(poms, parents, path, &pom.group).as_deref() == Some(&group) &&
                    resolve_coordinate(poms, parents, path, &pom.artifact).as_deref() == Some(&artifact) &&
                    effective_version(poms, parents, path).as_deref() == Some(&current)
            }));
            let first = matches.next()?;
            if matches.next().is_some() { return None; }
            first
        }
        RefKind::Other => return None,
    };
    active.contains(&project_binding(poms, parents, path)?).then_some(path)
}

/// An exact property may be shared with unrelated dependencies/parents/plugins.
/// Never change it when that would implicitly alter an unrelated version.
fn trace_properties(poms: &Poms, parents: &Parents, path: &Path, text: &str, seen: &mut BTreeSet<Binding>) {
    let mut rest = text;
    while let Some((_, suffix)) = rest.split_once("${") {
        let Some((key, after)) = suffix.split_once('}') else { break; };
        rest = after;
        if let Some(binding) = property_binding(poms, parents, path, key)
            && seen.insert(binding.clone()) && let Some(value) = binding.value(poms) {
            trace_properties(poms, parents, path, &value.text, seen);
        }
    }
}

fn maven_target(poms: &Poms, binding: &Binding, role: Role) -> Result<Target> {
    let value = binding.value(poms).context("Missing Maven version declaration")?;
    let label = match &binding.property { Some(p) => format!("Maven property {p}"), None => "Maven project version".into() };
    Target::new(binding.path.clone(), &poms[&binding.path].source,
        value.range.clone().context("Cannot rewrite an escaped or noncontiguous Maven version")?,
        label, role, Scheme::Flexible)
}

fn discover_maven(root: &Path, starts: &[PathBuf], targets: &mut Targets) -> Result<()> {
    let mut poms = Poms::new();
    let mut scope = BTreeSet::new();
    let initial = starts.iter().map(|path| normalize_relative(&path.join("pom.xml"))).collect::<Result<Vec<_>>>()?;
    load_modules(root, initial, &mut poms, &mut scope)?;
    let selected = scope.clone();
    // When a selected module inherits its version, the owning local POM is
    // the source of truth. Include its declared modules for linked references
    // without promoting independent sibling versions to primary targets.
    let (parents, active) = loop {
        let parents = load_parents(root, &mut poms)?;
        let active: BTreeSet<_> = selected.iter().filter_map(|file| project_binding(&poms, &parents, file)).collect();
        let owners: Vec<_> = active.iter().filter(|b| !scope.contains(&b.path)).map(|b| b.path.clone()).collect();
        if owners.is_empty() { break (parents, active); }
        load_modules(root, owners, &mut poms, &mut scope)?;
    };
    let mut forbidden = BTreeSet::new();
    for path in &scope {
        let Some(pom) = poms.get(path) else { continue; };
        // Changing a version property must not rename a project or artifact
        // or modify module/parent paths that happen to use the same property.
        for text in [&pom.group, &pom.artifact].into_iter().chain(&pom.modules).chain(&pom.relative_parent) {
            trace_properties(&poms, &parents, path, text, &mut forbidden);
        }
        for reference in &pom.refs {
            for text in [&reference.group, &reference.artifact] {
                trace_properties(&poms, &parents, path, text, &mut forbidden);
            }
            if linked_project(&poms, &parents, &scope, &active, path, reference).is_none() {
                trace_properties(&poms, &parents, path, &reference.version.text, &mut forbidden);
            }
        }
    }
    for binding in &active {
        if forbidden.contains(binding) {
            bail!("Maven version property in {} also controls an unrelated version", binding.path.display());
        }
        insert_target(targets, maven_target(&poms, binding, Role::Primary)?);
    }
    for path in &scope {
        let Some(pom) = poms.get(path) else { continue; };
        for reference in &pom.refs {
            if linked_project(&poms, &parents, &scope, &active, path, reference).is_none() { continue; }
            let (_, binding) = ref_version(&poms, &parents, path, reference).unwrap();
            if let Some(binding) = binding {
                if active.contains(&binding) { continue; }
                if forbidden.contains(&binding) {
                    bail!("Maven linked property in {} also controls an unrelated version", binding.path.display());
                }
                insert_target(targets, maven_target(&poms, &binding, Role::Linked)?);
            } else if literal(&reference.version.text) {
                let range = reference.version.range.clone().context("Cannot rewrite an escaped or noncontiguous Maven reference")?;
                let label = if reference.kind == RefKind::Parent { "Maven local parent version".into() }
                    else { format!("Maven dependency {}:{}", reference.group, reference.artifact) };
                insert_target(targets, Target::new(path.clone(), &pom.source, range, label, Role::Linked, Scheme::Flexible)?);
            }
        }
    }
    Ok(())
}

fn properties_version(source: &str) -> Option<Range<usize>> {
    let mut offset = 0;
    let mut continuation = false;
    let mut version = None;
    for line in source.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        let raw = content.trim_start();
        let continued = content.bytes().rev().take_while(|b| *b == b'\\').count() % 2 != 0;
        if !continuation && !raw.starts_with(['#', '!']) {
            if let Some(rest) = raw.strip_prefix("version")
                && (rest.is_empty() || rest.starts_with(['=', ':']) || rest.starts_with(char::is_whitespace)) {
                let rest = rest.trim_start();
                let rest = rest.strip_prefix(['=', ':']).unwrap_or(rest).trim();
                let start = offset + (rest.as_ptr() as usize - line.as_ptr() as usize);
                version = (!continued && literal(rest)).then_some(start..start + rest.len());
            }
            continuation = continued;
        } else if continuation {
            continuation = continued;
        }
        offset += line.len();
    }
    version
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind<'a> { Word(&'a str), String(&'a str), Symbol(u8), Newline, Opaque }
#[derive(Clone, Debug)]
struct Token<'a> { kind: TokenKind<'a>, start: usize, end: usize }

fn block_comment_end(bytes: &[u8], mut i: usize) -> usize {
    let mut depth = 1;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"/*") { depth += 1; i += 2; }
        else if bytes[i..].starts_with(b"*/") {
            depth -= 1; i += 2;
            if depth == 0 { break; }
        } else { i += 1; }
    }
    i
}

fn interpolation_end(bytes: &[u8], mut i: usize, recursion: usize) -> usize {
    if recursion >= 64 { return bytes.len(); }
    let mut depth = 1;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
        } else if bytes[i..].starts_with(b"/*") { i = block_comment_end(bytes, i + 2); }
        else {
            match bytes[i] {
                b'\'' | b'"' => i = quote_end(bytes, i, recursion + 1).0,
                b'{' => { depth += 1; i += 1; }
                b'}' => {
                    depth -= 1; i += 1;
                    if depth == 0 { return i; }
                }
                _ => i += 1,
            }
        }
    }
    i
}

/// Recognize only ordinary, unescaped single-line string literals. Other
/// strings remain opaque so text inside them is not mistaken for Gradle DSL.
fn quote_end(bytes: &[u8], start: usize, recursion: usize) -> (usize, bool) {
    if recursion >= 64 { return (bytes.len(), false); }
    let quote = bytes[start];
    let triple = bytes.get(start..start + 3).is_some_and(|s| s == [quote; 3]);
    let width = if triple { 3 } else { 1 };
    let mut simple = !triple;
    let mut i = start + width;
    while i < bytes.len() {
        if bytes.get(i..i + width).is_some_and(|s| s.iter().all(|b| *b == quote)) {
            return (i + width, simple);
        }
        if bytes[i] == b'\\' {
            simple = false;
            i = (i + 2).min(bytes.len());
        } else if quote == b'"' && bytes[i..].starts_with(b"${") {
            simple = false;
            i = interpolation_end(bytes, i + 2, recursion + 1);
        } else {
            if matches!(bytes[i], b'\r' | b'\n') || (quote == b'"' && bytes[i] == b'$') { simple = false; }
            i += 1;
        }
    }
    (i, false)
}

fn lex_gradle(source: &str, kotlin: bool) -> Vec<Token<'_>> {
    use TokenKind::*;
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let kind = match bytes[i] {
            b'\r' | b'\n' => {
                i += 1;
                if bytes[start] == b'\r' && bytes.get(i) == Some(&b'\n') { i += 1; }
                Newline
            }
            b if b.is_ascii_whitespace() => { i += 1; continue; }
            b'/' if bytes[i..].starts_with(b"//") => {
                while i < bytes.len() && !matches!(bytes[i], b'\r' | b'\n') { i += 1; }
                continue;
            }
            b'/' if bytes[i..].starts_with(b"/*") => {
                i = block_comment_end(bytes, i + 2);
                if !bytes[start..i].iter().any(|b| matches!(b, b'\r' | b'\n')) { continue; }
                Newline
            }
            b'\'' | b'"' => {
                let (end, simple) = quote_end(bytes, start, 0);
                i = end;
                if simple && (!kotlin || bytes[start] == b'"') { String(&source[start + 1..i - 1]) } else { Opaque }
            }
            b'$' if !kotlin && bytes[i..].starts_with(b"$/") => {
                i += 2;
                while i < bytes.len() && !bytes[i..].starts_with(b"/$") { i += 1; }
                i = (i + 2).min(bytes.len());
                Opaque
            }
            b'/' if !kotlin => {
                // Groovy slashy strings may contain multiline DSL-looking text.
                // Division is ambiguous lexically; err on the side of skipping
                // a potential string rather than editing its contents.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' { i = (i + 2).min(bytes.len()); }
                    else if bytes[i..].starts_with(b"${") { i = interpolation_end(bytes, i + 2, 0); }
                    else if bytes[i] == b'/' { i += 1; break; }
                    else { i += 1; }
                }
                Opaque
            }
            b if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || b >= 128 => {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$') || bytes[i] >= 128) { i += 1; }
                Word(&source[start..i])
            }
            b => { i += 1; Symbol(b) }
        };
        tokens.push(Token { kind, start, end: i });
    }
    tokens
}

fn kind<'a>(tokens: &[Token<'a>], i: usize) -> Option<TokenKind<'a>> { tokens.get(i).map(|t| t.kind) }
fn skip_lines(tokens: &[Token<'_>], mut i: usize) -> usize {
    while kind(tokens, i) == Some(TokenKind::Newline) { i += 1; }
    i
}

fn continuation_token(token: TokenKind<'_>) -> bool {
    matches!(token, TokenKind::Word("as" | "is" | "in" | "instanceof") | TokenKind::Symbol(b'=' | b'+' | b'-' | b'*' | b'/' | b'.' | b'?' | b':' | b',' | b'\\' | b'&' | b'|' | b'!' | b'<' | b'>'))
}

fn statement_end(tokens: &[Token<'_>], i: usize) -> bool {
    match kind(tokens, i) {
        None | Some(TokenKind::Symbol(b';' | b'}')) => true,
        Some(TokenKind::Newline) => !kind(tokens, skip_lines(tokens, i)).is_some_and(continuation_token),
        _ => false,
    }
}

/// Return only statement starts at project/script scope. Nested closures,
/// function arguments, plugins, dependencies and task configurations are
/// deliberately not interpreted as project version assignments.
fn statement_starts(tokens: &[Token<'_>]) -> Vec<usize> {
    use TokenKind::*;
    let mut result = Vec::new();
    let (mut braces, mut parens, mut brackets) = (0usize, 0usize, 0usize);
    let mut boundary = true;
    let mut previous = None;
    let mut statement = None;
    for (i, t) in tokens.iter().enumerate() {
        let top = braces == 0 && parens == 0 && brackets == 0;
        if top && boundary && !matches!(t.kind, Newline | Symbol(b';')) {
            result.push(i);
            statement = Some(i);
        }
        match t.kind {
            Symbol(b'{') => braces += 1,
            Symbol(b'}') => braces = braces.saturating_sub(1),
            Symbol(b'(') => parens += 1,
            Symbol(b')') => parens = parens.saturating_sub(1),
            Symbol(b'[') => brackets += 1,
            Symbol(b']') => brackets = brackets.saturating_sub(1),
            _ => (),
        }
        match t.kind {
            Newline => {
                if top {
                    boundary = !previous.is_some_and(|p| continuation_token(p) ||
                        matches!(p, Word("def" | "val" | "var" | "return" | "throw"))) &&
                        !statement.is_some_and(|start| control_header(&tokens[start..i]));
                    if boundary { statement = None; }
                }
            }
            Symbol(b';') => { boundary = true; previous = Some(t.kind); statement = None; }
            _ => { boundary = false; previous = Some(t.kind); }
        }
    }
    result
}

// A bare if/for/while header continues into its following body, even if
// there are line breaks and the body has no braces.
fn control_header(tokens: &[Token<'_>]) -> bool {
    use TokenKind::*;
    let meaningful: Vec<_> = tokens.iter().map(|t| t.kind).filter(|t| *t != Newline).collect();
    let mut i = 0;
    if meaningful.first() == Some(&Word("else")) { i += 1; }
    if meaningful.len() == i { return i != 0; }
    if matches!(meaningful.get(i), Some(Word("do" | "try" | "finally"))) {
        return meaningful.len() == i + 1;
    }
    if !matches!(meaningful.get(i), Some(Word("if" | "while" | "for" | "switch" | "when" | "catch" | "synchronized")))
        || meaningful.get(i + 1) != Some(&Symbol(b'(')) { return false; }
    let mut depth = 0usize;
    for (pos, token) in meaningful.iter().enumerate().skip(i + 1) {
        match token {
            Symbol(b'(') => depth += 1,
            Symbol(b')') => {
                depth -= 1;
                if depth == 0 { return pos + 1 == meaningful.len(); }
            }
            _ => (),
        }
    }
    true
}

fn gradle_version(source: &str, kotlin: bool) -> Option<Range<usize>> {
    use TokenKind::*;
    let tokens = lex_gradle(source, kotlin);
    let mut version = None;
    for start in statement_starts(&tokens) {
        let raw = match (kind(&tokens, start), kind(&tokens, start + 1), kind(&tokens, start + 2)) {
            (Some(Word("version")), _, _) => start + 1,
            (Some(Word("project")), Some(Symbol(b'.')), Some(Word("version"))) => start + 3,
            _ => continue,
        };
        let next = skip_lines(&tokens, raw);
        let assignment = kind(&tokens, next) == Some(Symbol(b'=')) && kind(&tokens, next + 1) != Some(Symbol(b'='));
        let direct = !kotlin && next == raw && !matches!(kind(&tokens, next), None | Some(Newline | Symbol(b'.' | b';' | b'=' | b'}')));
        if !assignment && !direct { continue; }
        let i = if assignment { skip_lines(&tokens, next + 1) } else { next };
        let paren = !kotlin && !assignment && kind(&tokens, i) == Some(Symbol(b'('));
        let i = if paren { skip_lines(&tokens, i + 1) } else { i };
        let mut end = i + 1;
        if paren {
            end = skip_lines(&tokens, end);
            if kind(&tokens, end) != Some(Symbol(b')')) { version = None; continue; }
            end += 1;
        }
        // A later nonliteral assignment supersedes an earlier literal one.
        version = match kind(&tokens, i) {
            Some(String(text)) if literal(text) && statement_end(&tokens, end) =>
                Some(tokens[i].start + 1..tokens[i].end - 1),
            _ => None,
        };
    }
    version
}

fn project_name(text: &str) -> Option<String> {
    let text = text.strip_prefix(':').unwrap_or(text);
    (!text.is_empty() && text.split(':').all(|p| !p.is_empty() && !matches!(p, "." | "..") &&
        !p.contains(['/', '\\', '$']))).then(|| text.to_owned())
}

/// Parse a complete list of literal include arguments, with either Groovy
/// command syntax or the parenthesized Kotlin/Groovy syntax.
fn arguments<'a>(tokens: &[Token<'a>], mut i: usize, require_parens: bool) -> Option<(Vec<&'a str>, usize)> {
    use TokenKind::*;
    let parens = kind(tokens, i) == Some(Symbol(b'('));
    if require_parens && !parens { return None; }
    if parens { i = skip_lines(tokens, i + 1); }
    let mut args = Vec::new();
    loop {
        if parens && kind(tokens, i) == Some(Symbol(b')')) { return Some((args, i + 1)); }
        let Some(String(value)) = kind(tokens, i) else { return None; };
        args.push(value);
        i += 1;
        if parens { i = skip_lines(tokens, i); }
        match kind(tokens, i) {
            Some(Symbol(b',')) => { i = skip_lines(tokens, i + 1); }
            Some(Symbol(b')')) if parens => return Some((args, i + 1)),
            _ if !parens && statement_end(tokens, i) => return Some((args, i)),
            _ => return None,
        }
    }
}

fn settings_projects(dir: &Path, source: &str, kotlin: bool) -> Vec<PathBuf> {
    use TokenKind::*;
    let tokens = lex_gradle(source, kotlin);
    let mut names = BTreeSet::new();
    let mut overrides: BTreeMap<std::string::String, Option<PathBuf>> = BTreeMap::new();
    let mut custom_build_files = BTreeSet::new();
    for start in statement_starts(&tokens) {
        if kind(&tokens, start) == Some(Word("include")) {
            if let Some((args, end)) = arguments(&tokens, skip_lines(&tokens, start + 1), kotlin)
                && statement_end(&tokens, end) {
                for name in args.into_iter().filter_map(project_name) {
                    names.insert(name.clone());
                    for (i, _) in name.match_indices(':') { names.insert(name[..i].to_owned()); }
                }
            }
        } else if kind(&tokens, start) == Some(Word("project")) {
            let Some((args, end)) = arguments(&tokens, start + 1, true) else { continue; };
            let [name] = args.as_slice() else { continue; };
            let Some(name) = project_name(name) else { continue; };
            if kind(&tokens, end) != Some(Symbol(b'.')) || kind(&tokens, end + 2) != Some(Symbol(b'=')) { continue; }
            match kind(&tokens, end + 1) {
                Some(Word("projectDir")) => {
                    // A dynamic override cannot safely be guessed; avoid
                    // accidentally updating an unrelated default-directory project.
                    let directory = settings_project_dir(&tokens, skip_lines(&tokens, end + 3))
                        .filter(|path| !path.contains('$'))
                        .and_then(|path| normalize_relative(&dir.join(path)).ok());
                    overrides.insert(name, directory);
                }
                Some(Word("buildFileName")) => { custom_build_files.insert(name); }
                _ => (),
            }
        }
    }
    let mut paths: BTreeMap<std::string::String, Option<PathBuf>> = BTreeMap::new();
    for name in names {
        let path = if let Some(override_path) = overrides.get(&name) { override_path.clone() }
            else if let Some((parent, last)) = name.rsplit_once(':') {
                paths.get(parent).and_then(|p| p.as_ref()).map(|p| p.join(last))
            } else { Some(dir.join(&name)) };
        paths.insert(name, path);
    }
    paths.into_iter().filter(|(name, _)| !custom_build_files.contains(name)).filter_map(|(_, path)| path).collect()
}

fn settings_project_dir<'a>(tokens: &[Token<'a>], mut i: usize) -> Option<&'a str> {
    use TokenKind::*;
    if kind(tokens, i) == Some(Word("file")) {
        let (args, end) = arguments(tokens, i + 1, true)?;
        let [path] = args.as_slice() else { return None; };
        return statement_end(tokens, end).then_some(*path);
    }
    // Common Groovy "new File(rootDir, ...)" and Kotlin "File(settingsDir,
    // ...)" forms, restricted to a literal path relative to this settings file.
    if kind(tokens, i) == Some(Word("new")) { i = skip_lines(tokens, i + 1); }
    if kind(tokens, i) != Some(Word("File")) || kind(tokens, i + 1) != Some(Symbol(b'(')) { return None; }
    i = skip_lines(tokens, i + 2);
    if !matches!(kind(tokens, i), Some(Word("rootDir" | "settingsDir"))) { return None; }
    i = skip_lines(tokens, i + 1);
    if kind(tokens, i) != Some(Symbol(b',')) { return None; }
    i = skip_lines(tokens, i + 1);
    let Some(String(path)) = kind(tokens, i) else { return None; };
    i = skip_lines(tokens, i + 1);
    (kind(tokens, i) == Some(Symbol(b')')) && statement_end(tokens, i + 1)).then_some(path)
}

fn discover_gradle(root: &Path, starts: &[PathBuf], targets: &mut Targets) -> Result<()> {
    let mut dirs: BTreeSet<_> = starts.iter().map(|path| normalize_relative(path)).collect::<Result<_>>()?;
    for start in starts {
        for (file, kotlin) in [("settings.gradle", false), ("settings.gradle.kts", true)] {
            if let Some(source) = read_file(root, &normalize_relative(&start.join(file))?)? {
                dirs.extend(settings_projects(start, &source, kotlin));
                break;
            }
        }
    }
    for dir in dirs {
        let path = dir.join("gradle.properties");
        if let Some(source) = read_file(root, &path)? && let Some(range) = properties_version(&source) {
            insert_target(targets, Target::new(path, &source, range, "Gradle version property", Role::Primary, Scheme::Flexible)?);
        }
        for (file, kotlin) in [("build.gradle", false), ("build.gradle.kts", true)] {
            let path = dir.join(file);
            if let Some(source) = read_file(root, &path)? {
                if let Some(range) = gradle_version(&source, kotlin) {
                    insert_target(targets, Target::new(path, &source, range, "Gradle project version", Role::Primary, Scheme::Flexible)?);
                }
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, file: &str, source: &str) {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn scan(root: &Path) -> Vec<Target> {
        discover(&root.canonicalize().unwrap(), &[PathBuf::new()]).unwrap()
    }

    fn edits(root: &Path, targets: &[Target], version: &str) -> BTreeMap<PathBuf, String> {
        let mut files = BTreeMap::new();
        for t in targets.iter().rev() {
            let content = files.entry(t.path.clone()).or_insert_with(|| fs::read_to_string(root.join(&t.path)).unwrap());
            assert_eq!(&content[t.range.clone()], t.current);
            content.replace_range(t.range.clone(), version);
        }
        files
    }

    #[test]
    fn maven_literal_preserves_xml_and_unrelated_versions() {
        let dir = tempfile::tempdir().unwrap();
        let pom = r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent><groupId>foreign</groupId><artifactId>parent</artifactId><version>1.2.3-SNAPSHOT</version><relativePath/></parent>
  <groupId>example</groupId><artifactId>example</artifactId>
  <version>  1.2.3-SNAPSHOT  </version><!-- keep -->
  <properties><unrelated.version>1.2.3-SNAPSHOT</unrelated.version></properties>
  <dependencies><dependency><groupId>foreign</groupId><artifactId>example</artifactId><version>1.2.3-SNAPSHOT</version></dependency></dependencies>
  <build><plugins><plugin><artifactId>plugin</artifactId><version>1.2.3-SNAPSHOT</version></plugin></plugins></build>
</project>"#;
        write(dir.path(), "pom.xml", pom);
        let targets = scan(dir.path());
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].current, "1.2.3-SNAPSHOT");
        assert_eq!(targets[0].role, Role::Primary);
        assert_eq!(targets[0].scheme, Scheme::Flexible);
        assert_eq!(edits(dir.path(), &targets, "2.0.0")[Path::new("pom.xml")], pom.replace("<version>  1.2.3-SNAPSHOT", "<version>  2.0.0"));
    }

    #[test]
    fn maven_modules_link_only_matching_versions_and_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let parent = r#"<project><groupId>example</groupId><artifactId>root</artifactId><version>1.0.0</version>
          <modules><module>lib</module><module>app</module><module>external</module></modules>
          <properties><lib.version>1.0.0</lib.version><unrelated.version>1.0.0</unrelated.version></properties>
          <dependencyManagement><dependencies><dependency><groupId>${project.groupId}</groupId><artifactId>lib</artifactId><version>${lib.version}</version></dependency></dependencies></dependencyManagement></project>"#;
        let lib = r#"<project><parent><groupId>example</groupId><artifactId>root</artifactId><version>1.0.0</version></parent><artifactId>lib</artifactId></project>"#;
        let app = r#"<project><parent><groupId>example</groupId><artifactId>root</artifactId><version>1.0.0</version></parent><artifactId>app</artifactId><version>1.0.0</version>
          <dependencies>
            <dependency><groupId>example</groupId><artifactId>lib</artifactId><version>1.0.0</version></dependency>
            <dependency><groupId>foreign</groupId><artifactId>lib</artifactId><version>1.0.0</version></dependency>
            <dependency><groupId>example</groupId><artifactId>lib</artifactId><version>0.9.0</version></dependency>
          </dependencies></project>"#;
        let external = r#"<project><parent><groupId>foreign</groupId><artifactId>root</artifactId><version>1.0.0</version></parent><artifactId>external</artifactId></project>"#;
        write(dir.path(), "pom.xml", parent);
        write(dir.path(), "lib/pom.xml", lib);
        write(dir.path(), "app/pom.xml", app);
        write(dir.path(), "external/pom.xml", external);
        write(dir.path(), "not-a-module/pom.xml", "<project><version>1.0.0</version></project>");
        let targets = scan(dir.path());
        assert_eq!(targets.iter().filter(|t| t.role == Role::Primary).count(), 2);
        assert_eq!(targets.iter().filter(|t| t.role == Role::Linked).count(), 4);
        let files = edits(dir.path(), &targets, "2.0.0");
        assert_eq!(files[Path::new("pom.xml")], parent.replace("<version>1.0.0", "<version>2.0.0").replace("<lib.version>1.0.0", "<lib.version>2.0.0"));
        assert_eq!(files[Path::new("lib/pom.xml")], lib.replace("1.0.0", "2.0.0"));
        assert_eq!(files[Path::new("app/pom.xml")], app.replacen("1.0.0", "2.0.0", 3));
        assert!(!files.contains_key(Path::new("external/pom.xml")));
        assert!(!files.contains_key(Path::new("not-a-module/pom.xml")));
    }

    #[test]
    fn maven_revision_property_and_inherited_version() {
        let dir = tempfile::tempdir().unwrap();
        let parent = r#"<project><groupId>example</groupId><artifactId>root</artifactId><version>${revision}</version>
          <properties><revision>1.2.3-SNAPSHOT</revision><junit.version>1.2.3-SNAPSHOT</junit.version></properties>
          <modules><module>lib</module></modules></project>"#;
        let lib = r#"<project><parent><groupId>example</groupId><artifactId>root</artifactId><version>${revision}</version></parent><artifactId>lib</artifactId>
          <dependencies><dependency><groupId>example</groupId><artifactId>root</artifactId><version>${project.version}</version></dependency>
          <dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>${junit.version}</version></dependency></dependencies></project>"#;
        write(dir.path(), "pom.xml", parent);
        write(dir.path(), "lib/pom.xml", lib);
        let targets = scan(dir.path());
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "Maven property revision");
        assert_eq!(targets[0].current, "1.2.3-SNAPSHOT");
        assert_eq!(edits(dir.path(), &targets, "1.2.4")[Path::new("pom.xml")], parent.replace("<revision>1.2.3-SNAPSHOT", "<revision>1.2.4"));
    }

    #[test]
    fn maven_selected_inheriting_module_keeps_local_siblings_linked() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pom.xml", "<project><groupId>x</groupId><artifactId>root</artifactId><version>1.0.0</version><modules><module>a</module><module>b</module></modules></project>");
        let parent = "<parent><groupId>x</groupId><artifactId>root</artifactId><version>1.0.0</version></parent>";
        write(dir.path(), "a/pom.xml", &format!("<project>{parent}<artifactId>a</artifactId></project>"));
        write(dir.path(), "b/pom.xml", &format!("<project>{parent}<artifactId>b</artifactId><version>3.0.0</version></project>"));
        let targets = discover(&dir.path().canonicalize().unwrap(), &[PathBuf::from("a")]).unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets.iter().filter(|t| t.role == Role::Primary).count(), 1);
        assert!(targets.iter().all(|t| t.current == "1.0.0"));
    }

    #[test]
    fn maven_shared_external_property_is_not_changed() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pom.xml", r#"<project><artifactId>x</artifactId><version>${revision}</version>
          <properties><revision>1.0.0</revision><dep.version>${revision}</dep.version></properties>
          <dependencies><dependency><groupId>unrelated</groupId><artifactId>y</artifactId><version>${dep.version}</version></dependency></dependencies></project>"#);
        let result = discover(&dir.path().canonicalize().unwrap(), &[PathBuf::new()]);
        assert!(result.unwrap_err().to_string().contains("unrelated version"));
    }

    #[test]
    fn gradle_groovy_ignores_other_version_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"// version = '9.9.9'
plugins {
  id 'example' version '1.2.3'
}
def notes = '''
version '9.9.9'
'''
def example = /
version = '9.9.9'
/
dependencies { implementation 'foreign:library:1.2.3' }
tasks.register('example') { version = '9.9.9' }
version '1.2.3-SNAPSHOT' // keep
"#;
        write(dir.path(), "build.gradle", script);
        let targets = scan(dir.path());
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].current, "1.2.3-SNAPSHOT");
        assert_eq!(edits(dir.path(), &targets, "2.0.0")[Path::new("build.gradle")], script.replace("1.2.3-SNAPSHOT", "2.0.0"));
    }

    #[test]
    fn gradle_kotlin_literal_and_dynamic_assignment() {
        let script = r#"/* version = "9.9.9" */
plugins {
    id("sample") version "4.0.0"
}
val text = "value ${x("version = 9.9.9")}"
project.version = "1.0.0"
"#;
        let range = gradle_version(script, true).unwrap();
        assert_eq!(&script[range], "1.0.0");
        assert!(gradle_version("version = \"1.0.0\" + suffix", true).is_none());
        assert!(gradle_version("version = \"1.0.0\"\n  + suffix", true).is_none());
        assert!(gradle_version("version = \"1.0.0\"\nversion = providers.gradleProperty(\"v\").get()", true).is_none());
        assert!(gradle_version("val version = \"1.0.0\"", true).is_none());
        assert!(gradle_version("version = \"$dynamic\"", true).is_none());
        assert!(gradle_version("if (enabled)\n\n  version = \"3.0.0\"", true).is_none());
        let script = "if (enabled)\n  version = \"3.0.0\"\nversion = \"4.0.0\"";
        assert_eq!(&script[gradle_version(script, true).unwrap()], "4.0.0");
    }

    #[test]
    fn gradle_properties_and_settings_projects() {
        let dir = tempfile::tempdir().unwrap();
        let props = "  # version=8.0.0\r\nversionName=9.0.0\r\nversion:  1.0.0  \r\n";
        write(dir.path(), "gradle.properties", props);
        write(dir.path(), "settings.gradle.kts", r#"include(":app", ":lib:inner", ":unsafe")
project(":app").projectDir = file("modules/app")
project(":unsafe").projectDir = file(somePath)
"#);
        write(dir.path(), "modules/app/build.gradle.kts", "version = \"1.0.0\"");
        write(dir.path(), "lib/inner/build.gradle", "version = '1.0.0'");
        write(dir.path(), "app/build.gradle", "version = '8.0.0'");
        write(dir.path(), "unsafe/build.gradle", "version = '8.0.0'");
        write(dir.path(), "other/build.gradle", "version = '8.0.0'");
        let targets = scan(dir.path());
        assert_eq!(targets.len(), 3);
        assert!(targets.iter().all(|t| t.current == "1.0.0"));
        let files = edits(dir.path(), &targets, "2.0.0");
        assert_eq!(files[Path::new("gradle.properties")], props.replace("1.0.0", "2.0.0"));
        assert_eq!(files[Path::new("modules/app/build.gradle.kts")], "version = \"2.0.0\"");
        assert_eq!(files[Path::new("lib/inner/build.gradle")], "version = '2.0.0'");
        assert!(!files.contains_key(Path::new("app/build.gradle")));
        assert!(!files.contains_key(Path::new("unsafe/build.gradle")));
    }

    #[test]
    fn gradle_groovy_settings_file_override() {
        let script = "include ':app', ':lib'\nproject(':app').projectDir = new File(rootDir, 'src/app')\nproject(':lib').buildFileName = 'custom.gradle'\nproject(':lib').projectDir = new File(settingsDir, 'src/lib')";
        assert_eq!(settings_projects(Path::new(""), script, false), vec![PathBuf::from("src/app")]);
    }

    #[test]
    fn gradle_properties_continuations_and_last_assignment() {
        let source = "version=1.0.0\nfoo=continued\\\nversion=bad\nversion = 2.0.0";
        assert_eq!(&source[properties_version(source).unwrap()], "2.0.0");
        assert!(properties_version("version=1.0.0\nversion=2.\\\n0.0").is_none());
    }
}
