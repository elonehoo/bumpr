use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use std::{collections::BTreeMap, io::IsTerminal, path::PathBuf, process::ExitCode};
use bumpr::{adapters::custom, discovery, git::{self, GitOptions, GitPlan},
    model::{Role, Target}, plan::{Plan, valid_for_targets}, version::{self, Bump}};

/// Bump project versions across languages without touching unrelated dependencies.
#[derive(Debug, Parser)]
#[command(name = "bumpr", version, about, after_help = "Examples:\n  bumpr                         Interactive version picker\n  bumpr patch --dry-run         Preview a patch bump\n  bumpr minor --tag --yes       Update, commit and tag\n  bumpr 2.0.0 --push --yes       Update, commit, tag and push\n  bumpr --current --recursive   Inspect nested projects")]
struct Cli {
    /// patch, minor, major, prepatch, preminor, premajor, prerelease, stable,
    /// snapshot, or an explicit new version. Omit for an interactive picker.
    #[arg(value_name = "BUMP_OR_VERSION")]
    change: Option<String>,
    /// Project directory (defaults to the current working directory).
    #[arg(short = 'C', long, value_name = "DIR", default_value = ".")]
    path: PathBuf,
    /// Also discover nested projects, excluding dependency/build directories.
    #[arg(long)]
    recursive: bool,
    /// Choose the current version to update when independent versions coexist.
    #[arg(long, value_name = "VERSION")]
    from: Option<String>,
    /// Prerelease identifier, such as alpha, beta or rc.
    #[arg(long, value_name = "ID")]
    preid: Option<String>,
    /// Show discovered version fields instead of changing anything.
    #[arg(long, conflicts_with = "change")]
    current: bool,
    /// Show planned edits and Git actions without writing files or refs.
    #[arg(long)]
    dry_run: bool,
    /// Skip interactive confirmation; defaults to patch when no bump is given.
    #[arg(short, long)]
    yes: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
    /// Custom rule configuration (default: bumpr.toml or .bumpr.toml).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Disable built-in detection; use only custom file rules or Git tags.
    #[arg(long)]
    no_auto: bool,
    /// Additional version file or glob (use with --pattern).
    #[arg(long, requires = "pattern", value_name = "GLOB")]
    file: Option<String>,
    /// Regex with a named `version` capture (or one capture group).
    #[arg(long, requires = "file", value_name = "REGEX")]
    pattern: Option<String>,
    /// Commit the changed version files.
    #[arg(long)]
    commit: bool,
    /// Commit and create a version tag.
    #[arg(long)]
    tag: bool,
    /// Commit, tag and push atomically to the selected Git remote.
    #[arg(long, visible_alias = "release")]
    push: bool,
    /// Git remote used by --push.
    #[arg(long, default_value = "origin")]
    remote: String,
    /// Prefix for tag creation and tag-based version discovery.
    #[arg(long, default_value = "v")]
    tag_prefix: String,
    /// Git commit/tag message template: {version}, {oldVersion}, {tag}.
    #[arg(long, default_value = "chore: release {tag}")]
    message: String,
}

fn pick_current(cli: &Cli, groups: &BTreeMap<String, Vec<PathBuf>>, tag: Option<&str>, interactive: bool) -> Result<Option<String>> {
    if let Some(from) = &cli.from {
        version::validate_literal(from)?;
        ensure!(groups.is_empty() || groups.contains_key(from),
            "Current version {from:?} is not declared in this project; detected: {}", groups.keys().cloned().collect::<Vec<_>>().join(", "));
        return Ok(Some(from.clone()));
    }
    if groups.is_empty() {
        return tag.map(str::to_owned).map(Some).context("No project version found. Add a custom file rule, or use --from with --tag for a tag-versioned project.");
    }
    if groups.len() == 1 { return Ok(groups.keys().next().cloned()); }
    if !interactive || cli.yes {
        bail!("Multiple project versions found: {}. Choose one with --from VERSION (see --current).", groups.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    let keys: Vec<_> = groups.keys().cloned().collect();
    let labels: Vec<String> = keys.iter().map(|version| {
        let paths = &groups[version];
        format!("{version}  ({})", paths.iter().take(3).map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
    }).collect();
    let theme = ColorfulTheme::default();
    Ok(Select::with_theme(&theme).with_prompt("Which current version should be updated?")
        .items(&labels).default(0).interact_opt()?.map(|i| keys[i].clone()))
}

fn pick_new(cli: &Cli, from: &str, targets: &[Target], interactive: bool) -> Result<Option<String>> {
    if let Some(argument) = &cli.change {
        return Ok(Some(version::resolve(from, argument, cli.preid.as_deref())?));
    }
    if cli.yes || (!interactive && cli.dry_run) {
        return Ok(Some(version::bumped(from, Bump::Patch, cli.preid.as_deref())?));
    }
    ensure!(interactive, "Provide a bump (patch, minor, major, ...) or an explicit version in noninteractive mode");
    let suggestions: Vec<_> = Bump::ALL.into_iter().filter_map(|bump| {
        let value = version::bumped(from, bump, cli.preid.as_deref()).ok()?;
        (value != from && valid_for_targets(&value, targets, from).is_ok()).then_some((bump, value))
    }).collect();
    let mut labels: Vec<_> = suggestions.iter().map(|(bump, value)| format!("{bump:<11} {value}")).collect();
    labels.push("custom      Enter a version".into());
    let theme = ColorfulTheme::default();
    let Some(choice) = Select::with_theme(&theme).with_prompt(format!("Select a new version (current: {from})"))
        .items(&labels).default(0).interact_opt()? else { return Ok(None); };
    if choice < suggestions.len() { return Ok(Some(suggestions[choice].1.clone())); }
    let value = Input::<String>::with_theme(&theme).with_prompt("New version")
        .validate_with(|value: &String| valid_for_targets(value, targets, from).map_err(|error| format!("{error:#}")))
        .interact_text()?;
    Ok(Some(value))
}

fn show_current(cli: &Cli, groups: &BTreeMap<String, Vec<PathBuf>>, targets: &[Target], tag: Option<&str>) -> Result<()> {
    ensure!(!groups.is_empty() || tag.is_some(), "No project version found; add a custom version rule or a version tag");
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"versions": groups, "tag_version": tag, "fields": targets}))?);
    } else {
        if let Some(tag) = tag { println!("Git tag version: {tag}"); }
        for target in targets {
            println!("{}  {}  {}  ({})", target.current, target.path.display(), target.label,
                if target.role == Role::Primary { "version" } else { "linked" });
        }
        if groups.len() > 1 { println!("Multiple versions detected; use --from VERSION to choose a release group."); }
    }
    Ok(())
}

fn show_plan(plan: &Plan, git: &GitOptions, dry_run: bool, json: bool) -> Result<()> {
    if json {
        let tag = git.tag.then(|| git.tag_name(&plan.to));
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "from": plan.from, "to": plan.to, "file_count": plan.file_count(), "changes": plan.changes,
            "dry_run": dry_run, "git": {"commit": git.commit && !plan.is_empty(), "tag": tag,
                "push": git.push, "remote": git.push.then_some(&git.remote), "message": git.commit_message(plan)}
        }))?);
        return Ok(());
    }
    println!("\n  {} -> {}", plan.from, plan.to);
    for change in &plan.changes {
        println!("  {}  [{}]", change.path.display(), change.label);
    }
    println!("  {} file(s), {} version field(s)", plan.file_count(), plan.changes.len());
    if git.commit && !plan.is_empty() { println!("  Git commit: {}", git.commit_message(plan)); }
    if git.tag { println!("  Git tag: {}", git.tag_name(&plan.to)); }
    if git.push { println!("  Git push: {} (branch and tag)", git.remote); }
    if dry_run { println!("  Dry run: no files or Git refs changed."); }
    Ok(())
}

fn execute(cli: Cli) -> Result<()> {
    let root = cli.path.canonicalize().with_context(|| format!("Finding project {}", cli.path.display()))?;
    ensure!(root.is_dir(), "Project path is not a directory: {}", root.display());
    let mut config = custom::load(&root, cli.config.as_deref())?;
    config.recursive |= cli.recursive;
    if cli.no_auto { config.auto = false; }
    if let (Some(file), Some(pattern)) = (&cli.file, &cli.pattern) {
        config.files.push(custom::Rule::explicit(file.clone(), pattern.clone()));
    }
    let targets = discovery::discover(&root, &config)?;
    let groups = discovery::versions(&targets);
    let tag_version = if groups.is_empty() { git::current_version(&root, &cli.tag_prefix)? } else { None };
    if cli.current { return show_current(&cli, &groups, &targets, tag_version.as_deref()); }
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let Some(from) = pick_current(&cli, &groups, tag_version.as_deref(), interactive)? else { return Ok(()); };
    let Some(to) = pick_new(&cli, &from, &targets, interactive)? else { return Ok(()); };
    let plan = Plan::build(&root, &targets, &from, &to)?;
    let options = GitOptions { commit: cli.commit || cli.tag || cli.push, tag: cli.tag || cli.push,
        push: cli.push, remote: cli.remote.clone(), tag_prefix: cli.tag_prefix.clone(), message: cli.message.clone() };
    if groups.is_empty() && plan.is_empty() && !options.tag && !cli.dry_run {
        bail!("This project stores its version in Git tags; use --tag (or --push) to create the next version tag");
    }
    if cli.dry_run { return show_plan(&plan, &options, true, cli.json); }
    if !cli.json { show_plan(&plan, &options, false, false)?; }
    if plan.is_empty() && !options.tag {
        if cli.json { show_plan(&plan, &options, false, true)?; } else { println!("  Nothing to change."); }
        return Ok(());
    }
    if interactive && !cli.yes {
        let theme = ColorfulTheme::default();
        if !Confirm::with_theme(&theme).with_prompt("Apply these changes?").default(!options.push).interact()? {
            if !cli.json { println!("  Canceled; no changes made."); }
            return Ok(());
        }
    }
    let git = if options.enabled() { Some(GitPlan::preflight(&root, &plan, options.clone())?) } else { None };
    plan.apply()?;
    if let Some(git) = git { git.execute(&plan)?; }
    if cli.json { show_plan(&plan, &options, false, true)?; } else { println!("  Done."); }
    Ok(())
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => { eprintln!("Error: {error:#}"); ExitCode::FAILURE }
    }
}
