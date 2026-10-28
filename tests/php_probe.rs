//! PHP discovery tests using stand-in executables, so they run without PHP
//! installed and can simulate versions the machine does not have.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use rphp::php::{self, ProbeError};

struct FakePhp {
    dir: PathBuf,
}

impl FakePhp {
    /// Create an executable that prints `banner` for `-v` and exits with `code`.
    fn new(tag: &str, banner: &str, code: i32) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rphp-probe-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let script = format!("#!/bin/sh\nprintf '%s\\n' '{banner}'\nexit {code}\n");
        let path = dir.join("php-cgi");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn path(&self) -> String {
        self.dir.join("php-cgi").to_string_lossy().into_owned()
    }
}

impl Drop for FakePhp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn accepts_php_84_fastcgi() {
    let fake = FakePhp::new("ok", "PHP 8.4.13 (cgi-fcgi) (built: Oct  1 2026)", 0);
    let rt = php::probe(&fake.path(), Path::new("/")).await.unwrap();
    assert_eq!(rt.version.version_id(), 80413);
    assert!(rt.version.is_fastcgi());
    assert!(rt.path.is_absolute());
}

#[tokio::test]
async fn rejects_php_83_with_actionable_message() {
    let fake = FakePhp::new("old", "PHP 8.3.6 (cli) (built: Sep  2 2026)", 0);
    let err = php::probe(&fake.path(), Path::new("/")).await.unwrap_err();
    assert!(matches!(err, ProbeError::Unsupported { .. }), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("80306") && msg.contains("PHP 8.4 or newer"),
        "{msg}"
    );
}

#[tokio::test]
async fn reports_non_zero_exit() {
    let fake = FakePhp::new("fail", "boom", 3);
    let err = php::probe(&fake.path(), Path::new("/")).await.unwrap_err();
    assert!(matches!(err, ProbeError::Failed { .. }), "{err:?}");
}

#[tokio::test]
async fn reports_unparseable_output() {
    let fake = FakePhp::new("garbage", "hello world", 0);
    let err = php::probe(&fake.path(), Path::new("/")).await.unwrap_err();
    assert!(matches!(err, ProbeError::Parse { .. }), "{err:?}");
}

#[tokio::test]
async fn reports_missing_binary() {
    let err = php::probe("/nonexistent/php-cgi", Path::new("/"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Resolve(_)), "{err:?}");
}
