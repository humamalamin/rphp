//! Loading `rphp.toml`.

mod model;

pub use model::{
    AdminConfig, Config, MAX_WORKER_COUNT, PhpConfig, ServerConfig, ValidatedConfig,
    ValidationErrors, WorkerConfig, WorkerMode,
};

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// Config file looked up in the current directory when `--config` is absent.
pub const DEFAULT_CONFIG_FILE: &str = "rphp.toml";

/// Where the effective configuration came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    File(PathBuf),
    Defaults,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "{}", path.display()),
            Self::Defaults => f.write_str("built-in defaults (no rphp.toml found)"),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
    Invalid {
        source: ConfigSource,
        errors: ValidationErrors,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, message } => {
                write!(f, "cannot parse {}: {message}", path.display())
            }
            Self::Invalid { source, errors } => write!(f, "{source}: {errors}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Load and validate configuration.
///
/// With an explicit path the file must exist. Without one, `./rphp.toml` is
/// used when present and built-in defaults otherwise. Relative paths inside the
/// file are resolved against the file's directory, not the process CWD.
pub fn load(explicit: Option<&Path>) -> Result<(ValidatedConfig, ConfigSource), ConfigError> {
    let cwd = std::env::current_dir().map_err(|source| ConfigError::Read {
        path: PathBuf::from("."),
        source,
    })?;

    let path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => {
            let default = cwd.join(DEFAULT_CONFIG_FILE);
            default.is_file().then_some(default)
        }
    };

    let (config, source, base_dir) = match path {
        Some(path) => {
            let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            let config = parse(&text).map_err(|message| ConfigError::Parse {
                path: path.clone(),
                message,
            })?;
            let abs = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            let base_dir = abs
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cwd.clone());
            (config, ConfigSource::File(abs), base_dir)
        }
        None => (Config::default(), ConfigSource::Defaults, cwd),
    };

    let validated = config
        .validate(&base_dir)
        .map_err(|errors| ConfigError::Invalid {
            source: source.clone(),
            errors,
        })?;
    Ok((validated, source))
}

/// Parse TOML text into a [`Config`], adding hints for common mistakes.
pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str::<Config>(text).map_err(|err| {
        let mut message = err.to_string();
        if message.contains("min_version") {
            message.push_str(
                "\nhint: `php.min_version` was removed. The PHP 8.4 minimum is fixed by RPHP \
                 and cannot be lowered; delete this key.",
            );
        }
        message
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let cfg = parse("[server]\nport = 9000\n[worker]\ncount = 2\n").unwrap();
        assert_eq!(cfg.server.port, 9000);
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.worker.count, 2);
        assert_eq!(cfg.worker.mode, WorkerMode::Classic);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = parse("[server]\nprot = 9000\n").unwrap_err();
        assert!(err.contains("prot"), "{err}");
    }

    #[test]
    fn min_version_gets_a_hint() {
        let err = parse("[php]\nmin_version = \"8.2\"\n").unwrap_err();
        assert!(err.contains("cannot be lowered"), "{err}");
    }

    #[test]
    fn invalid_mode_is_rejected() {
        assert!(parse("[worker]\nmode = \"turbo\"\n").is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../rphp.example.toml");
        assert_eq!(parse(text).unwrap(), Config::default());
    }
}
