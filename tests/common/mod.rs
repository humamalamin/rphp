//! Helpers shared by the end-to-end tests: a running server around a PHP
//! pool, a minimal HTTP/1.1 client, and process checks for the "no orphaned
//! PHP processes" exit gate.
#![allow(dead_code)] // each test binary uses a different subset

use std::{net::SocketAddr, sync::Arc, time::Duration};

use rphp::{
    backend::PhpPool,
    server::{self, App, ServerOptions},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

/// Resolve `binary` on PATH if it is PHP 8.4+ with the given SAPI
/// (`cgi-fcgi` or `cli`), or `None` to skip the test. With
/// `RPHP_REQUIRE_PHP=1` (CI), an unusable interpreter fails instead.
pub async fn require_php(binary: &str, sapi: &str) -> Option<std::path::PathBuf> {
    let required = std::env::var_os("RPHP_REQUIRE_PHP").is_some_and(|v| v == "1");
    match rphp::php::probe(binary, std::path::Path::new("/")).await {
        Ok(rt) if rt.version.sapi.as_deref() == Some(sapi) => Some(rt.path),
        Ok(rt) if required => panic!("{binary} is not the {sapi} SAPI: {}", rt.version),
        Err(err) if required => panic!("RPHP_REQUIRE_PHP=1 but {binary} is unusable: {err}"),
        _ => {
            eprintln!("skipping: no PHP 8.4+ {binary} on PATH (set RPHP_REQUIRE_PHP=1 to fail)");
            None
        }
    }
}

pub struct Harness {
    pub addr: SocketAddr,
    pub pool: PhpPool,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl Harness {
    /// Serve `app` (whose PHP backend is `pool`) on an ephemeral port.
    pub async fn start(app: App, pool: PhpPool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = tokio::spawn(server::run(
            listener,
            Arc::new(app),
            ServerOptions::default(),
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            addr,
            pool,
            stop: Some(stop),
            handle: Some(handle),
        }
    }

    /// Ordered shutdown (HTTP, then PHP), then the exit-gate check: no
    /// php-cgi process the pool knew about may survive.
    pub async fn shutdown(mut self) {
        let pids = self.pool.worker_pids();
        let _ = self.stop.take().unwrap().send(());
        self.handle.take().unwrap().await.unwrap().unwrap();
        self.pool.shutdown(Duration::from_secs(5)).await;
        assert_no_survivors(&pids);
        assert!(self.pool.worker_pids().is_empty());
    }

    pub async fn send(&self, raw: Vec<u8>) -> Res {
        send_to(self.addr, raw).await
    }

    pub async fn get(&self, target: &str) -> Res {
        self.request("GET", target, &[], b"").await
    }

    pub async fn request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Res {
        let mut raw =
            format!("{method} {target} HTTP/1.1\r\nHost: fixture.test\r\nConnection: close\r\n");
        for (name, value) in headers {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        raw.push_str("\r\n");
        let mut raw = raw.into_bytes();
        raw.extend_from_slice(body);
        self.send(raw).await
    }
}

pub async fn send_to(addr: SocketAddr, raw: Vec<u8>) -> Res {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&raw).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut buf))
        .await
        .expect("response in time")
        .unwrap();
    Res::parse(&buf)
}

/// GET without borrowing the harness, for requests run in spawned tasks.
pub async fn get_at(addr: SocketAddr, target: &str) -> Res {
    let raw = format!("GET {target} HTTP/1.1\r\nHost: fixture.test\r\nConnection: close\r\n\r\n");
    send_to(addr, raw.into_bytes()).await
}

#[derive(Debug)]
pub struct Res {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Res {
    fn parse(raw: &[u8]) -> Self {
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(raw)));
        let head = std::str::from_utf8(&raw[..end]).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap()[9..12].parse().unwrap();
        let headers: Vec<(String, String)> = lines
            .map(|l| {
                let (n, v) = l.split_once(':').unwrap();
                (n.trim().to_ascii_lowercase(), v.trim().to_string())
            })
            .collect();
        let mut body = raw[end + 4..].to_vec();
        if headers
            .iter()
            .any(|(n, v)| n == "transfer-encoding" && v.contains("chunked"))
        {
            body = dechunk(&body);
        }
        Self {
            status,
            headers,
            body,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("invalid JSON ({e}): {}", self.text()))
    }
}

pub fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size line");
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..nl]).unwrap().trim(), 16).unwrap();
        data = &data[nl + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

pub fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

pub fn assert_no_survivors(pids: &[u32]) {
    for &pid in pids {
        assert!(!process_exists(pid), "php-cgi {pid} survived shutdown");
    }
}

pub fn kill9(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -9 {pid}");
}

pub async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
