//! RPHP: a Rust-powered PHP application server.
//!
//! Current scope (roadmap phase 1): configuration, PHP discovery and version
//! enforcement, and the HTTP shell. PHP execution is not implemented yet.

pub mod cli;
pub mod config;
pub mod php;
pub mod server;

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
    if !pre.php.version.is_fastcgi() {
        warnings.push(format!(
            "{} reports SAPI {:?}; classic mode will need the FastCGI SAPI (php-cgi). \
             This becomes an error once PHP execution lands (roadmap phase 2).",
            pre.php.path.display(),
            pre.php.version.sapi.as_deref().unwrap_or("unknown")
        ));
    }
    warnings
}
