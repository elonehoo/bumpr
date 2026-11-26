//! Optional Git operations. A normal bump edits files only; no remote is
//! contacted unless the user explicitly requests a push.
use crate::plan::Plan;
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use std::{ffi::OsStr, path::{Path, PathBuf}, process::{Command, Output}};

#[derive(Clone, Debug)]
pub struct GitOptions {
    pub commit: bool,
    pub tag: bool,
    pub push: bool,
    pub remote: String,
    pub tag_prefix: String,
    pub message: String,
}

impl GitOptions {
    pub fn enabled(&self) -> bool { self.commit || self.tag || self.push }
    pub fn tag_name(&self, version: &str) -> String {
        if self.tag_prefix == "v" && version.starts_with('v') { version.into() }
        else { format!("{}{}", self.tag_prefix, version) }
    }
    pub fn commit_message(&self, plan: &Plan) -> String {
        self.message.replace("{oldVersion}", &plan.from).replace("{version}", &plan.to)
            .replace("{tag}", &self.tag_name(&plan.to))
    }
}

fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn run<I, S>(root: &Path, args: I) -> Result<Output>
where I: IntoIterator<Item = S>, S: AsRef<OsStr> {
    command(root).args(args).output().context("Running Git (is it installed?)")
}

fn successful(output: Output, what: &str) -> Result<String> {
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("{what} failed ({}): {}{}", output.status, details.trim(),
            if details.trim().is_empty() { stdout.trim() } else { "" });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn checked(root: &Path, args: &[&str], what: &str) -> Result<String> { successful(run(root, args)?, what) }

/// Tag-versioned projects (for example Go modules and many Swift packages)
/// can use the highest reachable semantic-version tag as their version source.
pub fn current_version(root: &Path, prefix: &str) -> Result<Option<String>> {
    let Ok(output) = run(root, ["rev-parse", "--is-inside-work-tree"]) else { return Ok(None); };
    if !output.status.success() { return Ok(None); }
    let output = run(root, ["tag", "--merged", "HEAD"])?;
    if !output.status.success() { return Ok(None); }
    let tags = String::from_utf8_lossy(&output.stdout);
    Ok(tags.lines().filter_map(|tag| tag.strip_prefix(prefix))
        .filter_map(|value| Version::parse(value).ok()).max().map(|version| version.to_string()))
}

pub struct GitPlan {
    root: PathBuf,
    branch: String,
    tag: Option<String>,
    options: GitOptions,
    message: String,
}

impl GitPlan {
    /// Check the repository and refs before touching version files. Requiring a
    /// clean Git tree for commit/tag/push prevents unrelated changes entering a
    /// release; local-only updates do not impose that restriction.
    pub fn preflight(root: &Path, plan: &Plan, options: GitOptions) -> Result<Self> {
        let repo = checked(root, &["rev-parse", "--show-toplevel"], "Finding the Git repository")?;
        let repo_root = PathBuf::from(repo).canonicalize().context("Resolving the Git repository root")?;
        ensure!(root.starts_with(&repo_root), "The selected project is outside the Git repository");
        let status = checked(&repo_root, &["status", "--porcelain=v1", "--untracked-files=normal"], "Checking Git status")?;
        ensure!(status.is_empty(), "Git automation requires a clean working tree. Commit or stash unrelated changes, or omit Git flags for a local-only bump.");
        checked(root, &["rev-parse", "--verify", "HEAD"], "Checking the initial Git commit")?;
        let branch = checked(root, &["symbolic-ref", "--short", "HEAD"], "Finding the current branch")
            .context("Git automation requires an attached branch")?;
        if options.commit && !plan.is_empty() {
            let mut cmd = command(root);
            let output = cmd.args(["ls-files", "--error-unmatch", "--"]).args(plan.paths()).output()?;
            successful(output, "Checking that version files are tracked by Git")?;
        }
        let tag = if options.tag {
            let tag = options.tag_name(&plan.to);
            let reference = format!("refs/tags/{tag}");
            checked(root, &["check-ref-format", &reference], "Validating the new Git tag")?;
            let output = run(root, ["show-ref", "--verify", "--quiet", &reference])?;
            ensure!(!output.status.success(), "Git tag {tag} already exists");
            ensure!(matches!(output.status.code(), Some(1)), "Could not check whether Git tag {tag} exists");
            Some(tag)
        } else { None };
        if options.push {
            ensure!(!options.remote.is_empty() && !options.remote.starts_with('-') && !options.remote.contains(char::is_whitespace),
                "Invalid Git remote name");
            checked(root, &["remote", "get-url", &options.remote], "Checking the Git remote")?;
            let branch_ref = format!("refs/heads/{branch}");
            let tag_ref = tag.as_ref().map(|tag| format!("refs/tags/{tag}"));
            let mut args = vec!["ls-remote", &options.remote, branch_ref.as_str()];
            if let Some(reference) = &tag_ref { args.push(reference); }
            let refs = checked(root, &args, "Checking the remote branch and tag")?;
            for line in refs.lines() {
                let Some((sha, reference)) = line.split_once('\t') else { continue; };
                if Some(reference) == tag_ref.as_deref() { bail!("Git tag {} already exists on {}", tag.as_deref().unwrap_or_default(), options.remote); }
                if reference == branch_ref {
                    let output = run(root, ["merge-base", "--is-ancestor", sha, "HEAD"])?;
                    ensure!(output.status.success(), "The remote branch is ahead or has diverged; fetch and synchronize before releasing");
                }
            }
        }
        let message = options.commit_message(plan);
        ensure!(!message.trim().is_empty(), "The release commit message cannot be empty");
        Ok(Self { root: root.into(), branch, tag, options, message })
    }

    pub fn execute(&self, plan: &Plan) -> Result<()> {
        if self.options.commit && !plan.is_empty() {
            let mut cmd = command(&self.root);
            let output = cmd.args(["commit", "--only", "-m", &self.message, "--"]).args(plan.paths()).output()?;
            successful(output, "Creating the version commit").context("Version files were updated; Git did not complete the commit")?;
        }
        if let Some(tag) = &self.tag {
            checked(&self.root, &["tag", "-a", tag, "-m", &self.message], "Creating the Git tag")
                .context("Version changes remain local; Git did not complete tag creation")?;
        }
        if self.options.push {
            let branch_ref = format!("HEAD:refs/heads/{}", self.branch);
            let mut args = vec!["push", "--atomic", self.options.remote.as_str(), branch_ref.as_str()];
            let tag_ref = self.tag.as_ref().map(|tag| format!("refs/tags/{tag}:refs/tags/{tag}"));
            if let Some(reference) = &tag_ref { args.push(reference); }
            checked(&self.root, &args, "Pushing the version commit and tag")
                .context("The local commit and tag remain; Git did not complete the push")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_reachable_tags_without_version_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        checked(&root, &["init", "-b", "main"], "init").unwrap();
        checked(&root, &["-c", "user.name=Example", "-c", "user.email=example@example.invalid", "commit", "--allow-empty", "-m", "init"], "commit").unwrap();
        for tag in ["v0.1.0", "v0.2.0", "v0.3.0-beta.1", "other"] { checked(&root, &["tag", tag], "tag").unwrap(); }
        assert_eq!(current_version(&root, "v").unwrap().as_deref(), Some("0.3.0-beta.1"));
    }
}
