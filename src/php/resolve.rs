//! Resolution of the configured PHP executable to an absolute path.
//!
//! `std` only, so it can be tested without a PHP installation.

use std::{
    env,
    ffi::OsStr,
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// A path (containing a separator) was given but nothing executable is there.
    NotFoundAtPath(PathBuf),
    /// A bare name was given and it is not on `PATH`.
    NotFoundOnPath(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFoundAtPath(path) => write!(
                f,
                "PHP binary {} does not exist or is not executable",
                path.display()
            ),
            Self::NotFoundOnPath(name) => write!(
                f,
                "PHP binary {name:?} was not found on PATH; install PHP 8.4+ \
                 (php-cgi) or set `[php] binary` to an absolute path"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Resolve `binary` the same way a shell would, without invoking a shell.
///
/// * Values containing a path separator are treated as paths, made absolute
///   against `base_dir` (normally the config file's directory).
/// * Bare names are searched for in `path_var` (normally `$PATH`).
pub fn resolve_binary(
    binary: &str,
    base_dir: &Path,
    path_var: Option<&OsStr>,
) -> Result<PathBuf, ResolveError> {
    let candidate = Path::new(binary);

    if candidate.components().count() > 1 || candidate.is_absolute() {
        let full = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            base_dir.join(candidate)
        };
        return if is_executable(&full) {
            Ok(full.canonicalize().unwrap_or(full))
        } else {
            Err(ResolveError::NotFoundAtPath(full))
        };
    }

    if let Some(path_var) = path_var {
        for dir in env::split_paths(path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let full = dir.join(binary);
            if is_executable(&full) {
                return Ok(full);
            }
        }
    }

    Err(ResolveError::NotFoundOnPath(binary.to_string()))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "rphp-resolve-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_file(path: &Path, mode: u32) {
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn finds_bare_name_on_path() {
        let dir = temp_dir("path");
        make_file(&dir.join("php-cgi"), 0o755);
        let path_var = env::join_paths([Path::new("/nonexistent"), &dir]).unwrap();
        let found = resolve_binary("php-cgi", Path::new("/"), Some(&path_var)).unwrap();
        assert_eq!(found, dir.join("php-cgi"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn skips_non_executable_files() {
        let dir = temp_dir("noexec");
        make_file(&dir.join("php-cgi"), 0o644);
        let path_var = env::join_paths([&dir]).unwrap();
        let err = resolve_binary("php-cgi", Path::new("/"), Some(&path_var)).unwrap_err();
        assert_eq!(err, ResolveError::NotFoundOnPath("php-cgi".into()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn relative_path_is_resolved_against_base_dir() {
        let dir = temp_dir("rel");
        fs::create_dir_all(dir.join("bin")).unwrap();
        make_file(&dir.join("bin/php"), 0o755);
        let found = resolve_binary("bin/php", &dir, None).unwrap();
        assert_eq!(found, dir.join("bin/php").canonicalize().unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_absolute_path_is_reported() {
        let err = resolve_binary("/definitely/not/php", Path::new("/"), None).unwrap_err();
        assert!(matches!(err, ResolveError::NotFoundAtPath(_)));
    }

    #[test]
    fn missing_bare_name_is_reported() {
        let err = resolve_binary("php-cgi", Path::new("/"), None).unwrap_err();
        assert!(err.to_string().contains("not found on PATH"));
    }
}
