//! Known PHP security issues that RPHP refuses to run with.
//!
//! An entry lists, per affected release branch, the first patch release that
//! contains the fix. Branches older than the supported minimum are rejected
//! anyway; branches newer than every listed one are assumed to be fixed.
//! `std` only, so the policy is unit-tested without spawning PHP.

use super::PhpVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advisory {
    pub ids: &'static [&'static str],
    pub name: &'static str,
    pub summary: &'static str,
    pub url: &'static str,
    /// First fixed `(major, minor, patch)` for each affected branch.
    pub fixed: &'static [(u32, u32, u32)],
}

/// Issues an RPHP-served PHP must not have. Keep newest first.
pub const ADVISORIES: &[Advisory] = &[Advisory {
    ids: &["CVE-2026-109618", "CVE-2026-109619"],
    name: "PHP2Shell",
    summary: "unauthenticated remote code execution through crafted multipart requests \
              and per-request state reused by later requests on the same worker; \
              affects php-cgi, which classic mode uses",
    url: "https://www.cisa.gov/news-events/alerts/2026/10/21/critical-vulnerabilities-php-require-immediate-patching",
    fixed: &[(8, 4, 27), (8, 5, 12)],
}];

impl Advisory {
    /// The first fixed release on `version`'s branch, if that branch is affected.
    pub fn fixed_in(&self, version: &PhpVersion) -> Option<(u32, u32, u32)> {
        self.fixed
            .iter()
            .copied()
            .find(|&(major, minor, _)| major == version.major && minor == version.minor)
    }

    /// True when `version` is on an affected branch and older than its fix.
    pub fn affects(&self, version: &PhpVersion) -> bool {
        self.fixed_in(version)
            .is_some_and(|(_, _, patch)| version.patch < patch)
    }
}

/// Every advisory that applies to `version`.
pub fn affecting(version: &PhpVersion) -> Vec<&'static Advisory> {
    ADVISORIES.iter().filter(|a| a.affects(version)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::php::parse_version_output;

    fn v(s: &str) -> PhpVersion {
        parse_version_output(&format!("PHP {s} (cgi-fcgi)")).unwrap()
    }

    #[test]
    fn php2shell_boundaries() {
        for vulnerable in [
            "8.4.0",
            "8.4.26",
            "8.5.0",
            "8.5.11",
            "8.4.26-1+ubuntu24.04.1",
        ] {
            assert_eq!(affecting(&v(vulnerable)).len(), 1, "{vulnerable}");
        }
        for fixed in ["8.4.27", "8.4.30", "8.5.12", "8.6.0", "9.0.0"] {
            assert!(affecting(&v(fixed)).is_empty(), "{fixed}");
        }
    }

    #[test]
    fn fixed_in_reports_the_branch_fix() {
        let a = &ADVISORIES[0];
        assert_eq!(a.fixed_in(&v("8.5.3")), Some((8, 5, 12)));
        assert_eq!(a.fixed_in(&v("8.6.1")), None);
    }

    #[test]
    fn every_entry_is_well_formed() {
        for a in ADVISORIES {
            assert!(!a.ids.is_empty() && !a.fixed.is_empty(), "{a:?}");
            assert!(a.url.starts_with("https://"), "{a:?}");
            assert!(a.ids.iter().all(|id| id.starts_with("CVE-")), "{a:?}");
        }
    }
}
