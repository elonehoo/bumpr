use serde_json::Value;
use std::{fs, path::Path, process::{Command, Output}};

fn run(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vershift")).arg("--path").arg(root).args(arguments).output().unwrap()
}
fn success(output: Output) -> String {
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}
fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
fn read(root: &Path, path: &str) -> String { fs::read_to_string(root.join(path)).unwrap() }
fn cargo(root: &Path, args: &[&str]) -> String {
    success(Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root).args(args).output().unwrap())
}
fn git(root: &Path, args: &[&str]) -> String {
    success(Command::new("git").arg("-C").arg(root).args(args).output().unwrap()).trim().into()
}
fn bare_git(root: &Path, args: &[&str]) -> String {
    success(Command::new("git").arg("--git-dir").arg(root).args(args).output().unwrap()).trim().into()
}
fn git_repo(root: &Path) {
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "Vershift Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    git(root, &["config", "tag.gpgSign", "false"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "core.hooksPath", ".git/hooks"]);
}

#[test]
fn cross_ecosystem_bump_updates_only_project_versions_and_cargo_remains_locked() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "Cargo.toml", "[package]\nname = 'mixed'\nversion = '1.2.3'\nedition = '2021'\n[workspace]\nmembers = ['crates/shared']\nresolver = '2'\n[dependencies]\nshared = { path = 'crates/shared', version = '=1.2.3' }\n");
    write(root, "src/lib.rs", "pub fn answer() -> u8 { 42 }\n");
    write(root, "crates/shared/Cargo.toml", "[package]\nname = 'shared'\nversion = '1.2.3'\nedition = '2021'\n");
    write(root, "crates/shared/src/lib.rs", "pub fn hello() {}\n");
    cargo(root, &["generate-lockfile", "--offline"]);
    write(root, "package.json", "{\n  \"name\": \"mixed\",\n  \"version\": \"1.2.3\",\n  \"dependencies\": {\"external\": \"1.2.3\"}\n}\n");
    write(root, "package-lock.json", "{\"name\":\"mixed\",\"version\":\"1.2.3\",\"lockfileVersion\":3,\"packages\":{\"\":{\"name\":\"mixed\",\"version\":\"1.2.3\",\"dependencies\":{\"external\":\"1.2.3\"}},\"node_modules/external\":{\"version\":\"1.2.3\"}}}\n");
    write(root, "pom.xml", "<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>com.example</groupId><artifactId>mixed</artifactId><version>1.2.3</version><dependencies><dependency><groupId>external</groupId><artifactId>lib</artifactId><version>1.2.3</version></dependency></dependencies></project>\n");
    write(root, "gradle.properties", "version=1.2.3\npluginVersion=1.2.3\n");
    write(root, "VERSION", "1.2.3\n");
    let files = ["Cargo.toml", "crates/shared/Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "pom.xml", "gradle.properties", "VERSION"];
    let before: Vec<_> = files.iter().map(|path| fs::read(root.join(path)).unwrap()).collect();
    let dry: Value = serde_json::from_str(&success(run(root, &["patch", "--dry-run", "--json"]))).unwrap();
    assert_eq!(dry["from"], "1.2.3");
    assert_eq!(dry["to"], "1.2.4");
    assert_eq!(dry["file_count"], files.len());
    for (path, content) in files.iter().zip(before) { assert_eq!(fs::read(root.join(path)).unwrap(), content); }

    let applied: Value = serde_json::from_str(&success(run(root, &["minor", "--yes", "--json"]))).unwrap();
    assert_eq!(applied["to"], "1.3.0");
    assert_eq!(applied["file_count"], files.len());
    assert!(read(root, "Cargo.toml").contains("version = '=1.3.0'"));
    let metadata: Value = serde_json::from_str(&cargo(root, &["metadata", "--locked", "--offline", "--format-version", "1"])).unwrap();
    assert!(metadata["packages"].as_array().unwrap().iter().all(|package| package["version"] == "1.3.0"));
    let package: Value = serde_json::from_str(&read(root, "package.json")).unwrap();
    assert_eq!(package["version"], "1.3.0");
    assert_eq!(package["dependencies"]["external"], "1.2.3");
    let lock: Value = serde_json::from_str(&read(root, "package-lock.json")).unwrap();
    assert_eq!(lock["version"], "1.3.0");
    assert_eq!(lock["packages"][""]["version"], "1.3.0");
    assert_eq!(lock["packages"]["node_modules/external"]["version"], "1.2.3");
    assert!(read(root, "pom.xml").contains("<artifactId>mixed</artifactId><version>1.3.0</version>"));
    assert!(read(root, "pom.xml").contains("<artifactId>lib</artifactId><version>1.2.3</version>"));
    assert_eq!(read(root, "gradle.properties"), "version=1.3.0\npluginVersion=1.2.3\n");
    assert_eq!(read(root, "VERSION"), "1.3.0\n");
}

#[test]
fn independent_projects_require_an_explicit_group_in_noninteractive_mode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write(root, "frontend/package.json", "{\"name\":\"frontend\",\"version\":\"1.0.0\"}");
    write(root, "backend/pom.xml", "<project><modelVersion>4.0.0</modelVersion><groupId>a</groupId><artifactId>b</artifactId><version>2.0.0</version></project>");
    write(root, "node_modules/foreign/package.json", "{\"version\":\"1.0.0\"}");
    let current: Value = serde_json::from_str(&success(run(root, &["--recursive", "--current", "--json"]))).unwrap();
    assert_eq!(current["versions"].as_object().unwrap().len(), 2);
    assert!(!run(root, &["patch", "--recursive", "--yes"]).status.success());
    success(run(root, &["minor", "--recursive", "--from", "1.0.0", "--yes"]));
    assert!(read(root, "frontend/package.json").contains("1.1.0"));
    assert!(read(root, "backend/pom.xml").contains("2.0.0"));
    assert!(read(root, "node_modules/foreign/package.json").contains("1.0.0"));
}

#[test]
fn arbitrary_version_files_and_custom_globs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project [with spaces]");
    fs::create_dir(&root).unwrap();
    write(&root, "vershift.toml", r#"auto = false
[[files]]
path = "include/**/*.h"
pattern = '(?m)^#define APP_VERSION "(?P<version>[^"\n]+)"$'
[[files]]
path = "docs/about.txt"
pattern = '(?m)^Version: (?P<version>\S+)$'
primary = false
"#);
    write(&root, "include/version.h", "#define APP_VERSION \"2026.04\"\n#define OTHER_VERSION \"2026.04\"\n");
    write(&root, "include/deep/version.h", "#define APP_VERSION \"2026.04\"\n");
    write(&root, "docs/about.txt", "Version: 2026.04\nUnrelated: 2026.04\n");
    success(run(&root, &["2026.05", "--yes"]));
    assert_eq!(read(&root, "include/version.h"), "#define APP_VERSION \"2026.05\"\n#define OTHER_VERSION \"2026.04\"\n");
    assert!(read(&root, "include/deep/version.h").contains("2026.05"));
    assert_eq!(read(&root, "docs/about.txt"), "Version: 2026.05\nUnrelated: 2026.04\n");
}

#[test]
fn git_release_is_opt_in_and_pushes_only_the_new_commit_and_tag() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let remote = temp.path().join("remote.git");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare"]);
    git_repo(&root);
    write(&root, "VERSION", "0.1.0\n");
    git(&root, &["add", "VERSION"]);
    git(&root, &["commit", "-m", "initial"]);
    git(&root, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&root, &["push", "-u", "origin", "main"]);
    let original_head = git(&root, &["rev-parse", "HEAD"]);
    success(run(&root, &["patch", "--push", "--dry-run", "--json"]));
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), original_head);
    assert!(git(&root, &["tag"]).is_empty());
    success(run(&root, &["patch", "--push", "--yes"]));
    assert_eq!(read(&root, "VERSION"), "0.1.1\n");
    assert_eq!(git(&root, &["log", "-1", "--format=%s"]), "chore: release v0.1.1");
    assert_eq!(git(&root, &["tag"]), "v0.1.1");
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), bare_git(&remote, &["rev-parse", "refs/heads/main"]));
    assert_eq!(git(&root, &["rev-parse", "v0.1.1^{}"]), bare_git(&remote, &["rev-parse", "v0.1.1^{}"]));
    // Dirty Git state does not prevent an ordinary local update, but it must
    // not become an accidental release commit.
    write(&root, "unrelated.txt", "user changes");
    assert!(!run(&root, &["patch", "--tag", "--yes"]).status.success());
    assert_eq!(read(&root, "VERSION"), "0.1.1\n");
    success(run(&root, &["patch", "--yes"]));
    assert_eq!(read(&root, "VERSION"), "0.1.2\n");
    assert_eq!(read(&root, "unrelated.txt"), "user changes");
    assert_eq!(git(&root, &["tag"]), "v0.1.1");
}

#[test]
fn tag_only_projects_do_not_need_a_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    git_repo(root);
    write(root, "main.go", "package main\nfunc main() {}\n");
    git(root, &["add", "main.go"]);
    git(root, &["commit", "-m", "initial"]);
    git(root, &["tag", "v1.0.0"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    success(run(root, &["patch", "--tag", "--yes"]));
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(root, &["rev-parse", "v1.0.1^{}"]), head);
}

#[cfg(unix)]
#[test]
fn explicit_symlinks_cannot_edit_files_outside_the_project() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside.txt");
    let root = temp.path().join("project");
    fs::create_dir(&root).unwrap();
    fs::write(&outside, "1.0.0\n").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("linked.txt")).unwrap();
    assert!(!run(&root, &["patch", "--no-auto", "--file", "linked.txt", "--pattern", r"(\S+)", "--yes"]).status.success());
    assert_eq!(fs::read_to_string(outside).unwrap(), "1.0.0\n");
}
