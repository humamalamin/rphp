use std::{path::Path, process::ExitCode, sync::Arc};

use rphp::{
    backend::{
        Php, PhpPool,
        cgi::Script,
        classic::{Classic, ClassicPool},
        persistent::{Persistent, PersistentPool},
        pool::PoolConfig,
    },
    cli::{self, Command},
    config::WorkerMode,
    logging::{self, LogFormat},
    metrics::Metrics,
    server::{self, Admin, App, ServerOptions},
};
use tokio::{net::TcpListener, sync::watch};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> ExitCode {
    let command = match cli::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    match command {
        Command::Help => {
            println!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("rphp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Check { config } => check(config.as_deref()).await,
        Command::Serve { config } => {
            match LogFormat::from_env() {
                Ok(format) => logging::init(format),
                Err(err) => {
                    eprintln!("error: {err}");
                    return ExitCode::from(2);
                }
            }
            match serve(config.as_deref()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    error!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

async fn check(config: Option<&Path>) -> ExitCode {
    match rphp::preflight(config).await {
        Ok(pre) => {
            println!("config:        {}", pre.source);
            println!("listen:        {}", pre.config.listen);
            println!("document_root: {}", pre.config.document_root.display());
            println!("entry_point:   {}", pre.config.entry_point.display());
            println!("php binary:    {}", pre.php.path.display());
            println!(
                "php version:   {} (PHP_VERSION_ID {})",
                pre.php.version,
                pre.php.version.version_id()
            );
            println!("worker mode:   {}", pre.config.raw.worker.mode);
            if let Some(script) = &pre.config.worker_script {
                println!("worker script: {}", script.display());
            }
            match pre.config.admin_listen {
                Some(addr) => println!("admin listen:  {addr} (/health, /metrics)"),
                None => println!("admin listen:  disabled (/health, /metrics on main listener)"),
            }
            for warning in rphp::warnings(&pre) {
                println!("warning:       {warning}");
            }
            println!("OK");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(config: Option<&Path>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Nothing is bound until configuration and PHP have been validated.
    let pre = rphp::preflight(config).await?;
    info!(source = %pre.source, "configuration loaded");
    info!(
        path = %pre.php.path.display(),
        version = %pre.php.version,
        version_id = pre.php.version.version_id(),
        "PHP interpreter validated"
    );
    for warning in rphp::warnings(&pre) {
        warn!("{warning}");
    }

    let raw = &pre.config.raw;
    let pool_config = PoolConfig {
        count: raw.worker.count,
        max_requests: raw.worker.max_requests,
        max_queue_size: raw.worker.max_queue_size,
        max_age: pre.config.max_age(),
    };
    let pool: PhpPool = match raw.worker.mode {
        WorkerMode::Classic => {
            let kind = Classic {
                binary: pre.php.path.clone(),
                working_dir: pre.config.document_root.clone(),
            };
            ClassicPool::start(kind, pool_config).await?.into()
        }
        WorkerMode::Persistent => {
            let script = pre
                .config
                .worker_script
                .clone()
                .expect("validated: persistent mode has a worker script");
            let kind = Persistent {
                binary: pre.php.path.clone(),
                working_dir: script
                    .parent()
                    .unwrap_or(&pre.config.base_dir)
                    .to_path_buf(),
                script,
                max_memory: pre.config.max_memory_bytes(),
            };
            PersistentPool::start(kind, pool_config).await?.into()
        }
    };
    let metrics = Arc::new(Metrics::new());
    let app = Arc::new(App {
        document_root: pre.config.document_root.clone(),
        metrics: Arc::clone(&metrics),
        builtin_endpoints: pre.config.admin_listen.is_none(),
        php: Some(Php {
            pool: pool.clone(),
            script: Script::new(
                pre.config.document_root.clone(),
                pre.config.entry_point.clone(),
            ),
            max_body_bytes: raw.server.max_body_bytes,
            request_timeout: pre.config.request_timeout(),
        }),
    });

    let listener = TcpListener::bind(pre.config.listen)
        .await
        .map_err(|err| format!("cannot listen on {}: {err}", pre.config.listen))?;
    let admin_listener = match pre.config.admin_listen {
        Some(addr) => Some(
            TcpListener::bind(addr)
                .await
                .map_err(|err| format!("cannot listen on {addr} (admin): {err}"))?,
        ),
        None => None,
    };
    info!(addr = %listener.local_addr()?, "RPHP HTTP server started");

    let options = ServerOptions {
        shutdown_timeout: pre.config.shutdown_timeout(),
        header_read_timeout: pre.config.header_read_timeout(),
    };
    // One signal stops both listeners.
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
    });
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|&stop| stop).await;
    };

    let admin_task = admin_listener.map(|listener| {
        if let Ok(addr) = listener.local_addr() {
            info!(%addr, "admin listener started (/health, /metrics)");
        }
        let admin = Arc::new(Admin {
            metrics,
            pool: Some(pool.clone()),
        });
        tokio::spawn(server::run(
            listener,
            admin,
            options.clone(),
            stopped(stop_rx.clone()),
        ))
    });
    let result = server::run(listener, app, options, stopped(stop_rx)).await;
    // HTTP is drained (or timed out) first, then PHP processes get SIGTERM
    // and the same grace period before SIGKILL.
    pool.shutdown(pre.config.shutdown_timeout()).await;
    if let Some(task) = admin_task {
        if let Ok(Err(err)) = task.await {
            warn!(error = %err, "admin listener stopped with an error");
        }
    }
    result?;
    info!("RPHP stopped");
    Ok(())
}

/// Resolves on Ctrl+C, or on SIGTERM (what systemd, Docker and Kubernetes send).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            error!(error = %err, "cannot listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                error!(error = %err, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("received Ctrl+C"),
        () = terminate => info!("received SIGTERM"),
    }
}
