//! Log output: human-readable text (default) or one JSON object per line.
//!
//! `RPHP_LOG_FORMAT=json|text` picks the format, `RUST_LOG` the verbosity
//! (default `info`). Events inside a request carry its `request_id`.

use std::fmt;

use tracing_subscriber::EnvFilter;

pub const FORMAT_VAR: &str = "RPHP_LOG_FORMAT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownFormat(String);

impl fmt::Display for UnknownFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{FORMAT_VAR}={:?} is not a log format (use \"text\" or \"json\")",
            self.0
        )
    }
}

impl std::error::Error for UnknownFormat {}

impl LogFormat {
    /// Parse the value of `RPHP_LOG_FORMAT`; unset or empty means text.
    pub fn parse(value: Option<&str>) -> Result<Self, UnknownFormat> {
        match value.map(str::trim) {
            None | Some("") => Ok(Self::Text),
            Some(v) if v.eq_ignore_ascii_case("text") => Ok(Self::Text),
            Some(v) if v.eq_ignore_ascii_case("json") => Ok(Self::Json),
            Some(v) => Err(UnknownFormat(v.to_owned())),
        }
    }

    pub fn from_env() -> Result<Self, UnknownFormat> {
        let value = std::env::var(FORMAT_VAR).ok();
        Self::parse(value.as_deref())
    }
}

/// Install the global subscriber.
pub fn init(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        // Fields at the top level, plus the current span (the request ID);
        // the list of all enclosing spans adds nothing here.
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .init(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_formats() {
        assert_eq!(LogFormat::parse(None), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("")), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("text")), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("JSON")), Ok(LogFormat::Json));
        assert_eq!(LogFormat::parse(Some(" json ")), Ok(LogFormat::Json));
        let err = LogFormat::parse(Some("yaml")).unwrap_err();
        assert!(err.to_string().contains("RPHP_LOG_FORMAT=\"yaml\""));
    }
}
