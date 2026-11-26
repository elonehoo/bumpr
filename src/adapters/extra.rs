//! Other common project formats. Dynamic version expressions are not evaluated.
use crate::model::{Role, Scheme, Target, read_file, string_range};
use anyhow::{Context, Result};
use regex::Regex;
use std::{fs, ops::Range, path::{Path, PathBuf}};
use toml_edit::{ImDocument, Item};

fn literal(source: &str, range: Range<usize>) -> Option<Range<usize>> {
    let raw = source.get(range.clone())?;
    let value = raw.trim();
    crate::version::validate_literal(value).ok()?;
    let start = range.start + raw.len() - raw.trim_start().len();
    Some(start..start + value.len())
}

fn toml_field<'a>(doc: &'a Item, keys: &[&str]) -> Option<(Range<usize>, &'a str)> {
    let mut item = doc;
    for key in keys { item = item.get(key)?; }
    let value = item.as_value()?;
    Some((value.span()?, value.as_str()?))
}

fn python(root: &Path, dir: &Path, out: &mut Vec<Target>) -> Result<()> {
    let path = dir.join("pyproject.toml");
    if let Some(source) = read_file(root, &path)? {
        let doc = ImDocument::parse(source.as_str()).with_context(|| format!("Parsing {}", path.display()))?;
        for (keys, label) in [(&["project", "version"][..], "Python project.version"),
            (&["tool", "poetry", "version"][..], "Poetry version")] {
            if let Some((span, version)) = toml_field(doc.as_item(), keys) {
                let range = string_range(&source, span, version)?;
                out.push(Target::new(&path, &source, range, label, Role::Primary, Scheme::Flexible)?);
            }
        }
    }
    let path = dir.join("setup.cfg");
    if let Some(source) = read_file(root, &path)? {
        let mut metadata = false;
        let mut offset = 0;
        let re = Regex::new(r"(?i)^\s*version\s*=\s*([^\s;#]+)\s*(?:[;#].*)?$")?;
        for line in source.split_inclusive('\n') {
            let trimmed = line.trim();
            if trimmed.starts_with('[') { metadata = trimmed.eq_ignore_ascii_case("[metadata]"); }
            if metadata && let Some(value) = re.captures(line.trim_end_matches(['\r', '\n'])).and_then(|c| c.get(1))
                && let Some(range) = literal(&source, offset + value.start()..offset + value.end()) {
                out.push(Target::new(&path, &source, range, "Python setup.cfg metadata.version", Role::Primary, Scheme::Flexible)?);
            }
            offset += line.len();
        }
    }
    Ok(())
}

fn dotnet(root: &Path, dir: &Path, out: &mut Vec<Target>) -> Result<()> {
    let mut paths = vec![dir.join("Directory.Build.props")];
    for entry in fs::read_dir(root.join(dir)).with_context(|| format!("Listing {}", dir.display()))? {
        let entry = entry?;
        let path = dir.join(entry.file_name());
        if matches!(path.extension().and_then(|e| e.to_str()), Some("csproj" | "fsproj" | "vbproj")) {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let Some(source) = read_file(root, &path)? else { continue; };
        let doc = roxmltree::Document::parse(&source).with_context(|| format!("Parsing {}", path.display()))?;
        let project = doc.root_element();
        if project.tag_name().name() != "Project" { continue; }
        let properties: Vec<_> = project.children().filter(|n| n.is_element() && n.tag_name().name() == "PropertyGroup")
            .flat_map(|group| group.children().filter(|n| n.is_element())).collect();
        let has_version = properties.iter().any(|node| matches!(node.tag_name().name(), "Version" | "PackageVersion"));
        for node in properties {
            let name = node.tag_name().name();
            let scheme = match name {
                "Version" | "PackageVersion" => Scheme::Flexible,
                "VersionPrefix" if !has_version => Scheme::Numeric,
                _ => continue,
            };
            let mut children = node.children().filter(|n| !n.is_comment());
            let Some(text) = children.next().filter(|n| n.is_text()) else { continue; };
            if children.next().is_some() { continue; }
            if let Some(range) = literal(&source, text.range()) {
                out.push(Target::new(&path, &source, range, format!(".NET {name}"), Role::Primary, scheme)?);
            }
        }
    }
    Ok(())
}

fn composer(root: &Path, dir: &Path, out: &mut Vec<Target>) -> Result<()> {
    let path = dir.join("composer.json");
    let Some(source) = read_file(root, &path)? else { return Ok(()); };
    // Validate JSON before using the ranged AST for a format-preserving edit.
    let _: serde_json::Value = serde_json::from_str(&source).with_context(|| format!("Parsing {}", path.display()))?;
    let ast = jsonc_parser::parse_to_ast(&source, &Default::default(), &Default::default())
        .map_err(|e| anyhow::anyhow!("Parsing {}: {e}", path.display()))?;
    if let Some(value) = ast.value.as_ref().and_then(|v| v.as_object()).and_then(|obj| obj.get_string("version")) {
        let range = string_range(&source, value.range.start..value.range.end, &value.value)?;
        out.push(Target::new(&path, &source, range, "Composer version", Role::Primary, Scheme::Flexible)?);
    }
    Ok(())
}

fn text_fields(root: &Path, dir: &Path, out: &mut Vec<Target>) -> Result<()> {
    let dart = Regex::new(r#"(?m)^version[ \t]*:[ \t]*["']?([0-9][A-Za-z0-9.+-]*)["']?[ \t]*(?:#[^\r\n]*)?\r?$"#)?;
    let cmake = Regex::new(r"(?im)^[ \t]*project\s*\(\s*[^\s()]+\s+VERSION\s+([0-9]+(?:\.[0-9]+){0,3})(?:\s|\))")?;
    for (name, regex, label, scheme) in [("pubspec.yaml", &dart, "Dart/Flutter version", Scheme::Semver),
        ("CMakeLists.txt", &cmake, "CMake project VERSION", Scheme::Numeric)] {
        let path = dir.join(name);
        if let Some(source) = read_file(root, &path)? {
            for capture in regex.captures_iter(&source) {
                let value = capture.get(1).expect("version capture");
                out.push(Target::new(&path, &source, value.range(), label, Role::Primary, scheme)?);
            }
        }
    }
    let gemspec = Regex::new(r#"(?m)^[ \t]*[A-Za-z_][A-Za-z0-9_]*\.version[ \t]*=[ \t]*["']([0-9][A-Za-z0-9.!_+-]*)["'][ \t]*(?:#[^\r\n]*)?\r?$"#)?;
    for entry in fs::read_dir(root.join(dir))? {
        let path = dir.join(entry?.file_name());
        if path.extension().and_then(|e| e.to_str()) != Some("gemspec") { continue; }
        if let Some(source) = read_file(root, &path)? {
            for capture in gemspec.captures_iter(&source) {
                out.push(Target::new(&path, &source, capture.get(1).unwrap().range(), "Ruby gemspec version", Role::Primary, Scheme::Flexible)?);
            }
        }
    }
    for name in ["VERSION", "VERSION.txt", "version.txt", ".version"] {
        let path = dir.join(name);
        if let Some(source) = read_file(root, &path)? {
            let version = source.trim();
            let numeric = version.strip_prefix('v').unwrap_or(version).as_bytes().first().is_some_and(u8::is_ascii_digit);
            if numeric && let Some(range) = literal(&source, 0..source.len()) {
                out.push(Target::new(&path, &source, range, "Version file", Role::Primary, Scheme::Flexible)?);
            }
        }
    }
    Ok(())
}

pub fn discover(root: &Path, starts: &[PathBuf]) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    for dir in starts {
        python(root, dir, &mut targets)?;
        dotnet(root, dir, &mut targets)?;
        composer(root, dir, &mut targets)?;
        text_fields(root, dir, &mut targets)?;
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_common_formats_without_dependency_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for (file, content) in [
            ("pyproject.toml", "[project]\nname='example'\nversion='1.2.3'\ndependencies=['a==1.2.3']\n"),
            ("setup.cfg", "[metadata]\nversion = 1.2.3 # comment\n[options]\nversion = 9.9.9\n"),
            ("app.csproj", "<Project><PropertyGroup><Version> 1.2.3 </Version><AssemblyVersion>1.2.3.0</AssemblyVersion></PropertyGroup><ItemGroup><PackageReference Include=\"a\" Version=\"1.2.3\"/></ItemGroup></Project>"),
            ("composer.json", "{\"name\":\"a/b\",\"version\":\"1.2.3\",\"require\":{\"c/d\":\"1.2.3\"}}"),
            ("pubspec.yaml", "version: 1.2.3 # comment\ndependencies:\n  x: 1.2.3\n"),
            ("app.gemspec", "Gem::Specification.new do |s|\n  s.version = '1.2.3'\n  s.add_dependency 'a', '1.2.3'\nend\n"),
            ("CMakeLists.txt", "project(app VERSION 1.2.3 LANGUAGES C)\n"),
            ("VERSION", "1.2.3\n"),
        ] { fs::write(root.join(file), content).unwrap(); }
        let targets = discover(&root, &[PathBuf::new()]).unwrap();
        assert_eq!(targets.len(), 8);
        assert!(targets.iter().all(|t| t.current == "1.2.3"));
        assert!(targets.iter().any(|t| t.scheme == Scheme::Numeric));
    }
}
