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

    /// Probe the stand-in, retrying `ETXTBSY`: while this test writes the
    /// script, another test thread may fork, and the child holds the write
    /// descriptor until it execs. Real PHP binaries are not being written, so
    /// `php::probe` itself does not retry.
    async fn probe(&self) -> Result<php::PhpRuntime, ProbeError> {
        for _ in 0..100 {
            match php::probe(&self.path(), Path::new("/")).await {
                Err(ProbeError::Spawn { source, .. })
                    if source.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                result => return result,
            }
        }
        panic!("{} stayed busy", self.path());
    }
}

impl Drop for FakePhp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn accepts_php_84_fastcgi() {
    let fake = FakePhp::new("ok", "PHP 8.4.27 (cgi-fcgi) (built: Oct 21 2026)", 0);
    let rt = fake.probe().await.unwrap();
    assert_eq!(rt.version.version_id(), 80427);
    assert!(rt.version.is_fastcgi());
    assert!(rt.path.is_absolute());
}

#[tokio::test]
async fn rejects_php_83_with_actionable_message() {
    let fake = FakePhp::new("old", "PHP 8.3.6 (cli) (built: Sep  2 2026)", 0);
    let err = fake.probe().await.unwrap_err();
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
    let err = fake.probe().await.unwrap_err();
    assert!(matches!(err, ProbeError::Failed { .. }), "{err:?}");
}

#[tokio::test]
async fn reports_unparseable_output() {
    let fake = FakePhp::new("garbage", "hello world", 0);
    let err = fake.probe().await.unwrap_err();
    assert!(matches!(err, ProbeError::Parse { .. }), "{err:?}");
}

#[tokio::test]
async fn reports_missing_binary() {
    let err = php::probe("/nonexistent/php-cgi", Path::new("/"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Resolve(_)), "{err:?}");
}

#[tokio::test]
async fn rejects_php2shell_vulnerable_releases_with_upgrade_info() {
    for (tag, version, fix) in [("v84", "8.4.26", "8.4.27"), ("v85", "8.5.11", "8.5.12")] {
        let banner = format!("PHP {version} (cgi-fcgi) (built: Oct  1 2026)");
        let fake = FakePhp::new(tag, &banner, 0);
        let err = fake.probe().await.unwrap_err();
        assert!(matches!(err, ProbeError::Vulnerable { .. }), "{err:?}");
        let msg = err.to_string();
        for needle in ["CVE-2026-109618", "CVE-2026-109619", fix, "https://"] {
            assert!(msg.contains(needle), "missing {needle:?} in: {msg}");
        }
        assert!(!msg.contains("distribution build"), "{msg}");
    }
}

#[tokio::test]
async fn vulnerable_distro_build_gets_backport_note() {
    let fake = FakePhp::new(
        "distro",
        "PHP 8.4.5-1ubuntu0.2 (cgi-fcgi) (built: Oct  1 2026)",
        0,
    );
    let err = fake.probe().await.unwrap_err();
    assert!(err.to_string().contains("distribution build"), "{err}");
}

#[tokio::test]
async fn accepts_fixed_85_release() {
    let fake = FakePhp::new("v85ok", "PHP 8.5.12 (cgi-fcgi) (built: Oct 21 2026)", 0);
    assert!(fake.probe().await.is_ok());
}
