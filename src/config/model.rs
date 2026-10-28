//! Configuration model and validation.
//!
//! Validation uses `std` only; deserialisation lives in `config/mod.rs`.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

/// Upper bound for `worker.count`; protects against typos like `count = 40000`.
pub const MAX_WORKER_COUNT: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub php: PhpConfig,
    pub worker: WorkerConfig,
    pub admin: AdminConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Relative paths are resolved against the config file's directory.
    pub document_root: PathBuf,
    /// Front controller, relative to `document_root`.
    pub entry_point: String,
    /// Maximum accepted request body size; larger bodies get 413.
    pub max_body_bytes: u64,
    /// How long shutdown waits for in-flight connections.
    pub shutdown_timeout_ms: u64,
    /// How long a client may take to send request headers.
    pub header_read_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhpConfig {
    /// Executable name (searched on PATH) or path. The PHP 8.4 minimum is
    /// fixed by RPHP and intentionally not configurable.
    pub binary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerMode {
    Classic,
    Persistent,
}

impl fmt::Display for WorkerMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Classic => "classic",
            Self::Persistent => "persistent",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    pub mode: WorkerMode,
    /// Number of PHP processes.
    pub count: usize,
    /// Requests a PHP process serves before it is recycled. In classic mode
    /// this maps to php-cgi's `PHP_FCGI_MAX_REQUESTS`; request isolation is
    /// still provided by the CGI SAPI.
    pub max_requests: u64,
    /// Deadline for a whole request, including waiting for a worker.
    pub request_timeout_ms: u64,
    /// Requests allowed to wait for a free worker before new ones get 503.
    pub max_queue_size: usize,
    /// Recycle a worker after it has run this long (both modes).
    pub max_age_secs: Option<u64>,
    /// Persistent mode only: recycle a worker whose `memory_get_usage()`
    /// reaches this many MiB after a request.
    pub max_memory_mb: Option<u64>,
    /// Persistent mode only: the worker script (an `rphp/worker` loop),
    /// relative to the config file's directory.
    pub script: Option<PathBuf>,
}

/// Separate listener for `/health` and `/metrics`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// When true, `/health` and `/metrics` are served only on the admin
    /// listener and the application receives those paths.
    pub enabled: bool,
    pub host: String,
    pub port: u16,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".into(),
            port: 9180,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            document_root: PathBuf::from("public"),
            entry_point: "index.php".into(),
            max_body_bytes: 8 * 1024 * 1024,
            shutdown_timeout_ms: 10_000,
            header_read_timeout_ms: 30_000,
        }
    }
}

impl Default for PhpConfig {
    fn default() -> Self {
        Self {
            binary: "php-cgi".into(),
        }
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            mode: WorkerMode::Classic,
            count: 4,
            max_requests: 1000,
            request_timeout_ms: 30_000,
            max_queue_size: 256,
            max_age_secs: None,
            max_memory_mb: None,
            script: None,
        }
    }
}

/// Configuration that passed validation, with paths made absolute.
#[derive(Debug, Clone)]
pub struct ValidatedConfig {
    pub listen: SocketAddr,
    /// Admin listener address, when `admin.enabled`.
    pub admin_listen: Option<SocketAddr>,
    pub document_root: PathBuf,
    pub entry_point: PathBuf,
    /// Canonical worker script, in persistent mode.
    pub worker_script: Option<PathBuf>,
    /// Directory relative paths (including `php.binary`) are resolved against.
    pub base_dir: PathBuf,
    pub raw: Config,
}

impl ValidatedConfig {
    pub fn is_loopback(&self) -> bool {
        self.listen.ip().is_loopback()
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.raw.server.shutdown_timeout_ms)
    }

    pub fn header_read_timeout(&self) -> Duration {
        Duration::from_millis(self.raw.server.header_read_timeout_ms)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.raw.worker.request_timeout_ms)
    }

    pub fn max_age(&self) -> Option<Duration> {
        self.raw.worker.max_age_secs.map(Duration::from_secs)
    }

    pub fn max_memory_bytes(&self) -> Option<u64> {
        self.raw
            .worker
            .max_memory_mb
            .map(|mb| mb.saturating_mul(1024 * 1024))
    }
}

/// All validation problems found, so users can fix them in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<String>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid configuration:")?;
        for problem in &self.0 {
            writeln!(f, "  - {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

impl Config {
    pub fn validate(&self, base_dir: &Path) -> Result<ValidatedConfig, ValidationErrors> {
        let mut errors = Vec::new();
        let s = &self.server;
        let w = &self.worker;

        let ip = match s.host.parse::<IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => {
                errors.push(format!(
                    "server.host {:?} is not an IP address (use e.g. \"127.0.0.1\" or \"0.0.0.0\")",
                    s.host
                ));
                None
            }
        };
        if s.port == 0 {
            errors.push("server.port must be between 1 and 65535".into());
        }
        if s.max_body_bytes == 0 {
            errors.push("server.max_body_bytes must be greater than 0".into());
        }
        if s.shutdown_timeout_ms == 0 {
            errors.push("server.shutdown_timeout_ms must be greater than 0".into());
        }
        if s.header_read_timeout_ms == 0 {
            errors.push("server.header_read_timeout_ms must be greater than 0".into());
        }

        let a = &self.admin;
        let admin_ip = if a.enabled {
            let ip = a.host.parse::<IpAddr>().ok();
            if ip.is_none() {
                errors.push(format!(
                    "admin.host {:?} is not an IP address (use e.g. \"127.0.0.1\")",
                    a.host
                ));
            }
            if a.port == 0 {
                errors.push("admin.port must be between 1 and 65535".into());
            } else if a.port == s.port {
                errors.push(format!(
                    "admin.port {} is also server.port; the admin listener needs its own port",
                    a.port
                ));
            }
            ip
        } else {
            None
        };

        let document_root = check_document_root(&s.document_root, base_dir, &mut errors);
        let entry_point = document_root
            .as_deref()
            .and_then(|root| check_entry_point(root, &s.entry_point, &mut errors));

        if self.php.binary.trim().is_empty() {
            errors.push("php.binary must not be empty".into());
        }

        let worker_script = match (w.mode, &w.script) {
            (WorkerMode::Persistent, Some(script)) => {
                check_worker_script(script, base_dir, &mut errors)
            }
            (WorkerMode::Persistent, None) => {
                errors.push(
                    "worker.mode = \"persistent\" needs worker.script, the PHP file that runs \
                     the rphp/worker loop"
                        .into(),
                );
                None
            }
            (WorkerMode::Classic, Some(_)) => {
                errors.push("worker.script only applies to worker.mode = \"persistent\"".into());
                None
            }
            (WorkerMode::Classic, None) => None,
        };
        if w.mode == WorkerMode::Classic && w.max_memory_mb.is_some() {
            errors.push(
                "worker.max_memory_mb only applies to persistent mode; in classic mode use \
                 PHP's memory_limit"
                    .into(),
            );
        }
        if w.max_memory_mb == Some(0) {
            errors.push("worker.max_memory_mb must be greater than 0".into());
        }
        if w.max_age_secs == Some(0) {
            errors.push("worker.max_age_secs must be greater than 0".into());
        }
        if w.count == 0 || w.count > MAX_WORKER_COUNT {
            errors.push(format!(
                "worker.count must be between 1 and {MAX_WORKER_COUNT}, got {}",
                w.count
            ));
        }
        if w.max_requests == 0 {
            errors.push("worker.max_requests must be greater than 0".into());
        }
        if w.request_timeout_ms == 0 {
            errors.push("worker.request_timeout_ms must be greater than 0".into());
        }
        if w.max_queue_size == 0 {
            errors.push("worker.max_queue_size must be greater than 0".into());
        }

        match (errors.is_empty(), ip, document_root, entry_point) {
            (true, Some(ip), Some(document_root), Some(entry_point)) => Ok(ValidatedConfig {
                listen: SocketAddr::new(ip, s.port),
                admin_listen: admin_ip.map(|ip| SocketAddr::new(ip, a.port)),
                document_root,
                entry_point,
                worker_script,
                base_dir: base_dir.to_path_buf(),
                raw: self.clone(),
            }),
            _ => Err(ValidationErrors(errors)),
        }
    }
}

fn check_document_root(root: &Path, base_dir: &Path, errors: &mut Vec<String>) -> Option<PathBuf> {
    let full = if root.is_absolute() {
        root.to_path_buf()
    } else {
        base_dir.join(root)
    };
    match full.canonicalize() {
        Ok(path) if path.is_dir() => Some(path),
        Ok(path) => {
            errors.push(format!(
                "server.document_root {} is not a directory",
                path.display()
            ));
            None
        }
        Err(err) => {
            errors.push(format!(
                "server.document_root {} cannot be opened: {err}",
                full.display()
            ));
            None
        }
    }
}

/// The persistent worker script: an existing `.php` file, relative paths
/// resolved against the config file's directory.
fn check_worker_script(
    script: &Path,
    base_dir: &Path,
    errors: &mut Vec<String>,
) -> Option<PathBuf> {
    let full = if script.is_absolute() {
        script.to_path_buf()
    } else {
        base_dir.join(script)
    };
    if full.extension().and_then(|e| e.to_str()) != Some("php") {
        errors.push(format!(
            "worker.script {} must be a .php file",
            script.display()
        ));
        return None;
    }
    match full.canonicalize() {
        Ok(path) if path.is_file() => Some(path),
        Ok(path) => {
            errors.push(format!("worker.script {} is not a file", path.display()));
            None
        }
        Err(err) => {
            errors.push(format!(
                "worker.script {} cannot be opened: {err}",
                full.display()
            ));
            None
        }
    }
}

/// The entry point must be a relative `.php` path that stays inside the
/// document root, including after symlinks are resolved.
fn check_entry_point(root: &Path, entry: &str, errors: &mut Vec<String>) -> Option<PathBuf> {
    let rel = Path::new(entry);

    if entry.is_empty() {
        errors.push("server.entry_point must not be empty".into());
        return None;
    }
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        errors.push(format!(
            "server.entry_point {entry:?} must be a relative path inside document_root \
             (no leading '/', no '..')"
        ));
        return None;
    }
    if rel.extension().and_then(|e| e.to_str()) != Some("php") {
        errors.push(format!("server.entry_point {entry:?} must be a .php file"));
        return None;
    }

    let full = root.join(rel);
    match full.canonicalize() {
        Ok(path) if !path.starts_with(root) => {
            errors.push(format!(
                "server.entry_point {entry:?} resolves to {} which is outside document_root",
                path.display()
            ));
            None
        }
        Ok(path) if !path.is_file() => {
            errors.push(format!(
                "server.entry_point {} is not a file",
                path.display()
            ));
            None
        }
        Ok(path) => Some(path),
        Err(err) => {
            errors.push(format!(
                "server.entry_point {} cannot be opened: {err}",
                full.display()
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs};

    struct TempApp(PathBuf);

    impl TempApp {
        fn new(tag: &str) -> Self {
            let dir = env::temp_dir().join(format!(
                "rphp-config-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(dir.join("public")).unwrap();
            fs::write(dir.join("public/index.php"), "<?php echo 'hi';").unwrap();
            Self(dir)
        }
    }

    impl Drop for TempApp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn problems(cfg: &Config, base: &Path) -> Vec<String> {
        cfg.validate(base).unwrap_err().0
    }

    #[test]
    fn defaults_are_valid_with_public_index() {
        let app = TempApp::new("defaults");
        let v = Config::default().validate(&app.0).unwrap();
        assert_eq!(v.listen, "127.0.0.1:8080".parse().unwrap());
        assert!(v.is_loopback());
        assert!(v.entry_point.ends_with("public/index.php"));
        assert!(v.document_root.is_absolute());
    }

    #[test]
    fn collects_every_problem() {
        let app = TempApp::new("many");
        let mut cfg = Config::default();
        cfg.server.host = "localhost".into();
        cfg.server.port = 0;
        cfg.worker.count = 0;
        cfg.worker.max_queue_size = 0;
        let errs = problems(&cfg, &app.0);
        assert_eq!(errs.len(), 4, "{errs:#?}");
        assert!(errs[0].contains("not an IP address"));
    }

    #[test]
    fn persistent_mode_needs_an_existing_worker_script() {
        let app = TempApp::new("persistent");
        let mut cfg = Config::default();
        cfg.worker.mode = WorkerMode::Persistent;
        assert!(problems(&cfg, &app.0)[0].contains("needs worker.script"));

        cfg.worker.script = Some("missing.php".into());
        assert!(problems(&cfg, &app.0)[0].contains("cannot be opened"));

        fs::write(app.0.join("worker.php"), "<?php").unwrap();
        cfg.worker.script = Some("worker.php".into());
        cfg.worker.max_memory_mb = Some(128);
        let v = cfg.validate(&app.0).unwrap();
        assert!(v.worker_script.as_ref().unwrap().ends_with("worker.php"));
        assert_eq!(v.max_memory_bytes(), Some(128 * 1024 * 1024));
    }

    #[test]
    fn persistent_only_keys_are_rejected_in_classic_mode() {
        let app = TempApp::new("classic-keys");
        fs::write(app.0.join("worker.php"), "<?php").unwrap();
        let mut cfg = Config::default();
        cfg.worker.script = Some("worker.php".into());
        cfg.worker.max_memory_mb = Some(64);
        cfg.worker.max_age_secs = Some(0);
        let errs = problems(&cfg, &app.0);
        assert_eq!(errs.len(), 3, "{errs:#?}");
    }

    #[test]
    fn entry_point_cannot_escape_document_root() {
        let app = TempApp::new("escape");
        fs::write(app.0.join("secret.php"), "<?php").unwrap();
        for bad in ["../secret.php", "/etc/passwd.php", "sub/../../secret.php"] {
            let mut cfg = Config::default();
            cfg.server.entry_point = bad.into();
            let errs = problems(&cfg, &app.0);
            assert!(errs[0].contains("relative path inside"), "{bad}: {errs:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn entry_point_symlink_out_of_root_is_rejected() {
        let app = TempApp::new("symlink");
        fs::write(app.0.join("outside.php"), "<?php").unwrap();
        std::os::unix::fs::symlink(app.0.join("outside.php"), app.0.join("public/link.php"))
            .unwrap();
        let mut cfg = Config::default();
        cfg.server.entry_point = "link.php".into();
        assert!(problems(&cfg, &app.0)[0].contains("outside document_root"));
    }

    #[test]
    fn entry_point_must_be_php_and_exist() {
        let app = TempApp::new("entry");
        let mut cfg = Config::default();
        cfg.server.entry_point = "index.html".into();
        assert!(problems(&cfg, &app.0)[0].contains("must be a .php file"));

        cfg.server.entry_point = "missing.php".into();
        assert!(problems(&cfg, &app.0)[0].contains("cannot be opened"));
    }

    #[test]
    fn missing_document_root_is_reported() {
        let app = TempApp::new("noroot");
        let mut cfg = Config::default();
        cfg.server.document_root = "does-not-exist".into();
        assert!(problems(&cfg, &app.0)[0].contains("document_root"));
    }

    #[test]
    fn admin_listener_is_off_by_default_and_validated_when_on() {
        let app = TempApp::new("admin");
        assert_eq!(
            Config::default().validate(&app.0).unwrap().admin_listen,
            None
        );

        let mut cfg = Config::default();
        cfg.admin.enabled = true;
        let v = cfg.validate(&app.0).unwrap();
        assert_eq!(v.admin_listen, Some("127.0.0.1:9180".parse().unwrap()));

        cfg.admin.host = "localhost".into();
        cfg.admin.port = cfg.server.port;
        let errs = problems(&cfg, &app.0);
        assert_eq!(errs.len(), 2, "{errs:#?}");
        assert!(errs[0].contains("admin.host"));
        assert!(errs[1].contains("needs its own port"));

        // A disabled admin section is not validated.
        cfg.admin.enabled = false;
        assert!(cfg.validate(&app.0).is_ok());
    }

    #[test]
    fn non_loopback_host_is_valid_but_flagged() {
        let app = TempApp::new("public");
        let mut cfg = Config::default();
        cfg.server.host = "0.0.0.0".into();
        assert!(!cfg.validate(&app.0).unwrap().is_loopback());
    }
}
