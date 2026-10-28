//! Minimal command-line parsing (std only, no extra dependency).

use std::{fmt, path::PathBuf};

pub const USAGE: &str = "\
Usage: rphp <command> [options]

Commands:
  serve     Validate configuration and PHP, then start the HTTP server
  check     Validate configuration and PHP, print a report, and exit
  version   Print the RPHP version
  help      Print this message

Options:
  -c, --config <path>   Configuration file (default: ./rphp.toml if present)

Logging is controlled with RUST_LOG (default: info).";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve { config: Option<PathBuf> },
    Check { config: Option<PathBuf> },
    Version,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n\n{USAGE}", self.0)
    }
}

impl std::error::Error for UsageError {}

/// Parse arguments, excluding the program name.
pub fn parse<I, S>(args: I) -> Result<Command, UsageError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);

    let Some(command) = args.next() else {
        return Err(UsageError("missing command".into()));
    };

    let mut config = None;
    while let Some(arg) = args.next() {
        let value = if arg == "-c" || arg == "--config" {
            args.next()
                .ok_or_else(|| UsageError(format!("{arg} requires a path")))?
        } else if let Some(value) = arg.strip_prefix("--config=") {
            value.to_string()
        } else {
            return Err(UsageError(format!("unexpected argument {arg:?}")));
        };
        if value.is_empty() {
            return Err(UsageError("--config path must not be empty".into()));
        }
        if config.replace(PathBuf::from(value)).is_some() {
            return Err(UsageError("--config given more than once".into()));
        }
    }

    match command.as_str() {
        "serve" => Ok(Command::Serve { config }),
        "check" => Ok(Command::Check { config }),
        "version" | "--version" | "-V" if config.is_none() => Ok(Command::Version),
        "help" | "--help" | "-h" if config.is_none() => Ok(Command::Help),
        "version" | "--version" | "-V" | "help" | "--help" | "-h" => {
            Err(UsageError(format!("{command} does not take --config")))
        }
        other => Err(UsageError(format!("unknown command {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, UsageError> {
        parse(args.iter().copied())
    }

    #[test]
    fn parses_commands() {
        assert_eq!(p(&["serve"]), Ok(Command::Serve { config: None }));
        assert_eq!(p(&["check"]), Ok(Command::Check { config: None }));
        assert_eq!(p(&["version"]), Ok(Command::Version));
        assert_eq!(p(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn parses_config_forms() {
        let want = Ok(Command::Serve {
            config: Some("a.toml".into()),
        });
        assert_eq!(p(&["serve", "--config", "a.toml"]), want);
        assert_eq!(p(&["serve", "-c", "a.toml"]), want);
        assert_eq!(p(&["serve", "--config=a.toml"]), want);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&[]).is_err());
        assert!(p(&["start"]).is_err());
        assert!(p(&["serve", "--config"]).is_err());
        assert!(p(&["serve", "--config="]).is_err());
        assert!(p(&["serve", "-c", "a", "-c", "b"]).is_err());
        assert!(p(&["serve", "--port", "1"]).is_err());
        assert!(p(&["version", "-c", "a"]).is_err());
    }
}
