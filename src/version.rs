use anyhow::{Context, Result, bail, ensure};
use semver::{BuildMetadata, Prerelease, Version};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bump { Patch, Minor, Major, Prepatch, Preminor, Premajor, Prerelease, Stable, Snapshot }

impl Bump {
    pub const ALL: [Self; 9] = [Self::Patch, Self::Minor, Self::Major, Self::Prepatch,
        Self::Preminor, Self::Premajor, Self::Prerelease, Self::Stable, Self::Snapshot];

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "patch" => Some(Self::Patch), "minor" => Some(Self::Minor), "major" => Some(Self::Major),
            "prepatch" => Some(Self::Prepatch), "preminor" => Some(Self::Preminor),
            "premajor" => Some(Self::Premajor), "prerelease" => Some(Self::Prerelease),
            "release" | "stable" => Some(Self::Stable), "snapshot" => Some(Self::Snapshot), _ => None,
        }
    }
}

impl fmt::Display for Bump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Patch => "patch", Self::Minor => "minor", Self::Major => "major",
            Self::Prepatch => "prepatch", Self::Preminor => "preminor", Self::Premajor => "premajor",
            Self::Prerelease => "prerelease", Self::Stable => "stable", Self::Snapshot => "snapshot",
        })
    }
}

/// Version literals are safe to insert in the supported text formats. More
/// unusual numbering conventions can use an explicit value rather than a bump.
pub fn validate_literal(value: &str) -> Result<()> {
    ensure!(!value.is_empty() && value.len() <= 128 && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_+!".contains(&b)),
        "Invalid version {value:?}; use up to 128 characters from letters, numbers, . - _ + !");
    Ok(())
}

/// Accept the short numeric versions commonly found in Maven/Gradle and other
/// projects. Automatic operations return a three-component semantic version.
fn parse_for_bump(current: &str) -> Result<Version> {
    if let Ok(version) = Version::parse(current) { return Ok(version); }
    let numeric_end = current.find(['-', '+']).unwrap_or(current.len());
    let (numbers, suffix) = current.split_at(numeric_end);
    let parts: Vec<&str> = numbers.split('.').collect();
    if (1..=3).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        let mut nums = [0_u64; 3];
        for (i, part) in parts.iter().enumerate() { nums[i] = part.parse()?; }
        return Version::parse(&format!("{}.{}.{}{suffix}", nums[0], nums[1], nums[2]))
            .with_context(|| format!("Cannot increment {current:?}; provide an explicit new version"));
    }
    bail!("Cannot increment {current:?} as a semantic version; provide an explicit new version")
}

fn increment(value: u64) -> Result<u64> { value.checked_add(1).context("Version component overflow") }

fn inferred_preid(version: &Version) -> String {
    let pre = version.pre.as_str();
    let first = pre.split('.').next().unwrap_or_default();
    if first.is_empty() || first.bytes().all(|b| b.is_ascii_digit()) { "beta".into() } else { first.into() }
}

fn pre_start(preid: &str) -> Result<Prerelease> {
    Prerelease::new(&format!("{preid}.0")).context("Invalid prerelease identifier")
}

fn next_pre(version: &Version, preid: &str, explicit: bool) -> Result<Prerelease> {
    let pre = version.pre.as_str();
    if pre.is_empty() || (explicit && pre != preid && !pre.starts_with(&format!("{preid}."))) {
        return pre_start(preid);
    }
    let mut parts: Vec<String> = pre.split('.').map(str::to_owned).collect();
    if let Some(index) = parts.iter().rposition(|part| part.bytes().all(|b| b.is_ascii_digit())) {
        parts[index] = increment(parts[index].parse().context("Prerelease counter overflow")?)?.to_string();
    } else {
        parts.push("0".into());
    }
    Ok(Prerelease::new(&parts.join("."))?)
}

pub fn bumped(current: &str, bump: Bump, preid: Option<&str>) -> Result<String> {
    let prefixed = current.strip_prefix('v').filter(|rest| rest.as_bytes().first().is_some_and(u8::is_ascii_digit));
    let mut version = parse_for_bump(prefixed.unwrap_or(current))?;
    let had_pre = !version.pre.is_empty();
    let id = preid.map(str::to_owned).unwrap_or_else(|| inferred_preid(&version));
    if let Some(value) = preid {
        ensure!(!value.is_empty(), "Prerelease identifier cannot be empty");
        Prerelease::new(value).context("Invalid prerelease identifier")?;
    }
    version.build = BuildMetadata::EMPTY;
    match bump {
        Bump::Patch | Bump::Stable => {
            if bump == Bump::Patch && !had_pre { version.patch = increment(version.patch)?; }
            version.pre = Prerelease::EMPTY;
        }
        Bump::Minor => {
            if !had_pre || version.patch != 0 { version.minor = increment(version.minor)?; }
            version.patch = 0;
            version.pre = Prerelease::EMPTY;
        }
        Bump::Major => {
            if !had_pre || version.minor != 0 || version.patch != 0 { version.major = increment(version.major)?; }
            version.minor = 0; version.patch = 0; version.pre = Prerelease::EMPTY;
        }
        Bump::Prepatch | Bump::Snapshot => {
            version.patch = increment(version.patch)?;
            version.pre = if bump == Bump::Snapshot { Prerelease::new("SNAPSHOT")? } else { pre_start(&id)? };
        }
        Bump::Preminor => {
            version.minor = increment(version.minor)?; version.patch = 0; version.pre = pre_start(&id)?;
        }
        Bump::Premajor => {
            version.major = increment(version.major)?; version.minor = 0; version.patch = 0;
            version.pre = pre_start(&id)?;
        }
        Bump::Prerelease => {
            if !had_pre { version.patch = increment(version.patch)?; }
            version.pre = next_pre(&version, &id, preid.is_some())?;
        }
    }
    Ok(format!("{}{version}", if prefixed.is_some() { "v" } else { "" }))
}

pub fn resolve(current: &str, argument: &str, preid: Option<&str>) -> Result<String> {
    match Bump::parse(argument) {
        Some(bump) => bumped(current, bump, preid),
        None => { validate_literal(argument)?; Ok(argument.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_bumps() {
        for (old, operation, expected) in [
            ("0.0.5", "patch", "0.0.6"), ("0.0.5", "minor", "0.1.0"), ("0.0.5", "major", "1.0.0"),
            ("1.2.3", "prepatch", "1.2.4-beta.0"), ("1.2.3-beta.4", "prerelease", "1.2.3-beta.5"),
            ("1.2.3-rc.0", "patch", "1.2.3"), ("2.0.0-rc.1", "major", "2.0.0"),
            ("1.2.0-rc.1", "minor", "1.2.0"), ("1.2", "patch", "1.2.1"),
            ("1.2-SNAPSHOT", "stable", "1.2.0"), ("1.2.3", "snapshot", "1.2.4-SNAPSHOT"),
            ("v1.2.3", "patch", "v1.2.4"),
        ] { assert_eq!(resolve(old, operation, None).unwrap(), expected, "{old} {operation}"); }
        assert_eq!(resolve("1.0.0-beta.5", "prerelease", Some("rc")).unwrap(), "1.0.0-rc.0");
        assert_eq!(resolve("1.0.0", "preminor", Some("alpha")).unwrap(), "1.1.0-alpha.0");
    }
    #[test]
    fn arbitrary_explicit_numbering_and_overflows() {
        assert_eq!(resolve("2026.01", "2026.02", None).unwrap(), "2026.02");
        assert_eq!(resolve("unusual", "1.2.3.4", None).unwrap(), "1.2.3.4");
        assert!(resolve("1.2.3.4", "patch", None).is_err());
        assert!(resolve("1.0.0", "bad\nvalue", None).is_err());
        assert!(resolve("18446744073709551615.0.0", "major", None).is_err());
    }
}
