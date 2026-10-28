//! Parsing of `php -v` / `php-cgi -v` output.
//!
//! This module only uses `std` so it can be unit-tested without spawning PHP.

use std::fmt;

/// Lowest supported `PHP_VERSION_ID` (PHP 8.4.0).
///
/// This is a project policy, not a configuration option: there is deliberately
/// no way to lower it from `rphp.toml`.
pub const MIN_PHP_VERSION_ID: u32 = 80400;

/// A PHP version as reported by the interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// Full version string as printed by PHP, e.g. `8.4.13` or `8.4.0RC1`.
    pub raw: String,
    /// Server API reported by the binary, e.g. `cli` or `cgi-fcgi`.
    pub sapi: Option<String>,
}

impl PhpVersion {
    /// Equivalent of PHP's `PHP_VERSION_ID` constant.
    pub fn version_id(&self) -> u32 {
        self.major * 10_000 + self.minor * 100 + self.patch
    }

    pub fn is_supported(&self) -> bool {
        self.version_id() >= MIN_PHP_VERSION_ID
    }

    /// True when the binary speaks FastCGI, which classic mode will use.
    pub fn is_fastcgi(&self) -> bool {
        self.sapi.as_deref() == Some("cgi-fcgi")
    }
}

impl fmt::Display for PhpVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.sapi {
            Some(sapi) => write!(f, "{} ({sapi})", self.raw),
            None => f.write_str(&self.raw),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionParseError(pub String);

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VersionParseError {}

/// Parse the output of `php -v`.
///
/// Startup warnings can precede the banner, so this scans for the first line
/// that looks like `PHP <digit>...` rather than assuming it is line one.
pub fn parse_version_output(output: &str) -> Result<PhpVersion, VersionParseError> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| {
            line.strip_prefix("PHP ")
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        })
        .ok_or_else(|| {
            VersionParseError(format!(
                "could not find a `PHP <version>` line in the output of `-v`: {:?}",
                truncate(output, 200)
            ))
        })?;

    let rest = &line["PHP ".len()..];
    let raw = rest
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();

    let mut parts = raw.splitn(3, '.');
    let major = leading_number(parts.next());
    let minor = leading_number(parts.next());
    let patch = leading_number(parts.next()).or(Some(0));

    let (Some(major), Some(minor), Some(patch)) = (major, minor, patch) else {
        return Err(VersionParseError(format!(
            "unrecognised PHP version string {raw:?}"
        )));
    };

    let sapi = rest
        .find('(')
        .and_then(|start| {
            rest[start + 1..]
                .find(')')
                .map(|end| &rest[start + 1..start + 1 + end])
        })
        .map(str::to_string)
        .filter(|s| !s.is_empty() && !s.starts_with("built:"));

    Ok(PhpVersion {
        major,
        minor,
        patch,
        raw,
        sapi,
    })
}

/// Parse the leading decimal digits of a version component (`"0RC1"` -> 0).
fn leading_number(part: Option<&str>) -> Option<u32> {
    let part = part?;
    let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_banner() {
        let out = "PHP 8.4.13 (cli) (built: Sep 30 2025 10:00:00) (NTS)\n\
                   Copyright (c) The PHP Group\nZend Engine v4.4.13";
        let v = parse_version_output(out).unwrap();
        assert_eq!((v.major, v.minor, v.patch), (8, 4, 13));
        assert_eq!(v.version_id(), 80413);
        assert_eq!(v.sapi.as_deref(), Some("cli"));
        assert!(v.is_supported());
        assert!(!v.is_fastcgi());
    }

    #[test]
    fn parses_cgi_banner() {
        let v = parse_version_output("PHP 8.5.0 (cgi-fcgi) (built: Nov 20 2025)").unwrap();
        assert_eq!(v.version_id(), 80500);
        assert!(v.is_fastcgi());
    }

    #[test]
    fn rejects_php_83() {
        let v = parse_version_output("PHP 8.3.6 (cli) (built: Jan 1 2024)").unwrap();
        assert_eq!(v.version_id(), 80306);
        assert!(!v.is_supported());
    }

    #[test]
    fn exact_minimum_is_supported() {
        let v = parse_version_output("PHP 8.4.0 (cli)").unwrap();
        assert_eq!(v.version_id(), MIN_PHP_VERSION_ID);
        assert!(v.is_supported());
    }

    #[test]
    fn handles_prerelease_and_distro_suffixes() {
        let v = parse_version_output("PHP 8.4.0RC1 (cli)").unwrap();
        assert_eq!(v.version_id(), 80400);
        assert_eq!(v.raw, "8.4.0RC1");

        let v = parse_version_output("PHP 8.4.5-1+ubuntu24.04.1+deb.sury.org+1 (cli)").unwrap();
        assert_eq!(v.version_id(), 80405);
    }

    #[test]
    fn skips_startup_warnings() {
        let out = "PHP Warning:  Module \"redis\" is already loaded in Unknown on line 0\n\
                   PHP 8.4.2 (cli) (built: x)";
        assert_eq!(parse_version_output(out).unwrap().version_id(), 80402);
    }

    #[test]
    fn missing_sapi_is_none() {
        let v = parse_version_output("PHP 8.4.1").unwrap();
        assert_eq!(v.sapi, None);
        assert_eq!(v.to_string(), "8.4.1");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_version_output("").is_err());
        assert!(parse_version_output("command not found").is_err());
        assert!(parse_version_output("PHP Warning: nope").is_err());
        assert!(parse_version_output("PHP 8").is_err());
    }
}
