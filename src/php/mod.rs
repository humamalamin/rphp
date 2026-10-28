//! Discovery and validation of the PHP interpreter.

mod resolve;
mod version;

pub use resolve::{ResolveError, resolve_binary};
pub use version::{MIN_PHP_VERSION_ID, PhpVersion, VersionParseError, parse_version_output};

use std::{fmt, path::PathBuf, process::Stdio, time::Duration};

use tokio::process::Command;

/// How long `<php> -v` may take before we give up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// A resolved and version-checked PHP interpreter.
#[derive(Debug, Clone)]
pub struct PhpRuntime {
    pub path: PathBuf,
    pub version: PhpVersion,
}

#[derive(Debug)]
pub enum ProbeError {
    Resolve(ResolveError),
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    Timeout {
        path: PathBuf,
    },
    Failed {
        path: PathBuf,
        status: String,
        stderr: String,
    },
    Parse {
        path: PathBuf,
        source: VersionParseError,
    },
    Unsupported {
        path: PathBuf,
        version: PhpVersion,
    },
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(err) => err.fmt(f),
            Self::Spawn { path, source } => {
                write!(f, "failed to start {}: {source}", path.display())
            }
            Self::Timeout { path } => write!(
                f,
                "{} -v did not finish within {}s",
                path.display(),
                PROBE_TIMEOUT.as_secs()
            ),
            Self::Failed {
                path,
                status,
                stderr,
            } => write!(
                f,
                "{} -v exited with {status}: {}",
                path.display(),
                stderr.trim()
            ),
            Self::Parse { path, source } => {
                write!(
                    f,
                    "could not read the version of {}: {source}",
                    path.display()
                )
            }
            Self::Unsupported { path, version } => write!(
                f,
                "{} is PHP {version} (PHP_VERSION_ID {}), but RPHP requires PHP 8.4 or newer \
                 (PHP_VERSION_ID >= {MIN_PHP_VERSION_ID}). Install PHP 8.4+ or point \
                 `[php] binary` at a newer interpreter.",
                path.display(),
                version.version_id()
            ),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Resolve `binary`, run `<binary> -v` and enforce the PHP 8.4 minimum.
///
/// The binary is started directly (no shell), with stdin closed and a timeout,
/// and is killed if the probe is abandoned.
pub async fn probe(binary: &str, base_dir: &std::path::Path) -> Result<PhpRuntime, ProbeError> {
    let path_var = std::env::var_os("PATH");
    let path =
        resolve_binary(binary, base_dir, path_var.as_deref()).map_err(ProbeError::Resolve)?;

    let mut command = Command::new(&path);
    command
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = match tokio::time::timeout(PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => return Err(ProbeError::Spawn { path, source }),
        Err(_) => return Err(ProbeError::Timeout { path }),
    };

    if !output.status.success() {
        return Err(ProbeError::Failed {
            path,
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = match parse_version_output(&stdout) {
        Ok(version) => version,
        Err(source) => return Err(ProbeError::Parse { path, source }),
    };

    if !version.is_supported() {
        return Err(ProbeError::Unsupported { path, version });
    }

    Ok(PhpRuntime { path, version })
}
