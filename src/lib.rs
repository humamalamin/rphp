//! RPHP: a Rust-powered PHP application server.
//!
//! Current scope (roadmap phase 2): configuration, PHP discovery and version
//! enforcement, the HTTP server, static files, and classic-mode execution
//! over FastCGI to a pool of `php-cgi` processes.

#[cfg(not(unix))]
compile_error!("RPHP currently supports Unix-like systems only (Linux first; see SPEC.md).");

pub mod backend;
pub mod body;
pub mod cli;
pub mod config;
pub mod fastcgi;
pub mod php;
pub mod server;
pub mod static_files;

use config::{ConfigSource, ValidatedConfig};
use php::PhpRuntime;

/// Everything that must be true before RPHP accepts traffic.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub config: ValidatedConfig,
    pub source: ConfigSource,
    pub php: PhpRuntime,
}

/// Load configuration, then resolve and version-check PHP.
pub async fn preflight(
    config_path: Option<&std::path::Path>,
) -> Result<Preflight, Box<dyn std::error::Error + Send + Sync>> {
    let (config, source) = config::load(config_path)?;
    let php = php::probe(&config.raw.php.binary, &config.base_dir).await?;
    // Classic mode (the only mode so far) speaks FastCGI.
    if !php.version.is_fastcgi() {
        return Err(format!(
            "{} reports SAPI {:?}, but classic mode needs the FastCGI SAPI \
             (cgi-fcgi). Install php-cgi (e.g. the php8.4-cgi package) and set \
             `[php] binary = \"php-cgi\"`.",
            php.path.display(),
            php.version.sapi.as_deref().unwrap_or("unknown")
        )
        .into());
    }
    Ok(Preflight {
        config,
        source,
        php,
    })
}

/// Non-fatal findings worth telling the operator about.
pub fn warnings(pre: &Preflight) -> Vec<String> {
    let mut warnings = Vec::new();
    if !pre.config.is_loopback() {
        warnings.push(format!(
            "listening on {} which is not a loopback address; the server is reachable from the network",
            pre.config.listen
        ));
    }
    warnings
}
