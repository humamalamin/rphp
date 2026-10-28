use std::{path::Path, process::ExitCode};

use rphp::{
    cli::{self, Command},
    server::{self, ServerOptions},
};
use tokio::net::TcpListener;
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
            init_tracing();
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

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
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

    let listener = TcpListener::bind(pre.config.listen)
        .await
        .map_err(|err| format!("cannot listen on {}: {err}", pre.config.listen))?;
    info!(addr = %listener.local_addr()?, "RPHP HTTP server started");

    let options = ServerOptions {
        shutdown_timeout: pre.config.shutdown_timeout(),
        header_read_timeout: pre.config.header_read_timeout(),
    };
    server::run(listener, options, shutdown_signal()).await?;
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
