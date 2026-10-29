//! `ssd` — surveillance-rs daemon. SPEC §1.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use ss_core::Store;
use ss_db::SqliteStore;
use ss_engine::{Engine, EngineConfig};
use ss_server::{auth, router, AppState};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "ssd", version, about = "surveillance-rs daemon")]
struct Cli {
    /// Directory holding `ss.db` and `recordings/`.
    #[arg(long, env = "SS_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,
    /// Address to serve HTTP on.
    #[arg(long, env = "SS_LISTEN", default_value = "0.0.0.0:9900")]
    listen: SocketAddr,
    /// Password for the initial `admin` user (used only when there are no users).
    #[arg(long, env = "SS_ADMIN_PASSWORD")]
    admin_password: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let recordings = cli.data_dir.join("recordings");
    std::fs::create_dir_all(&recordings)
        .with_context(|| format!("creating {}", recordings.display()))?;
    let store: Arc<dyn Store> =
        Arc::new(SqliteStore::open(&cli.data_dir.join("ss.db")).context("opening ss.db")?);

    if let Some(password) = auth::bootstrap_admin(store.as_ref(), cli.admin_password.as_deref())? {
        tracing::warn!(username = "admin", password = %password, "created initial admin user; change this password after first login");
    }

    let engine = Engine::new(
        store.clone(),
        EngineConfig {
            storage_root: recordings,
        },
    );
    engine.start().await.context("starting engine")?;

    // Expired sessions are also ignored on lookup; this just keeps the table small.
    let purge_store = store.clone();
    let purger = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match purge_store.purge_expired_sessions(chrono::Utc::now()) {
                Ok(n) if n > 0 => tracing::info!(removed = n, "purged expired sessions"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "session purge failed"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    tracing::info!(listen = %cli.listen, data_dir = %cli.data_dir.display(), "ssd listening");

    let state = AppState::new(engine.clone(), store);
    let app = router(state.clone());
    // Open SSE/MJPEG bodies end once shutdown is requested, so the server can drain.
    let drain = state.shutdown_requested();
    let mut serve = Box::pin(
        axum::serve(listener, app)
            .with_graceful_shutdown(drain)
            .into_future(),
    );
    let served: anyhow::Result<()> = tokio::select! {
        res = &mut serve => res.context("http server"),
        () = shutdown_signal() => {
            state.request_shutdown();
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut serve).await {
                Ok(res) => res.context("http server"),
                Err(_) => {
                    tracing::warn!("http server did not drain within {:?}; dropping open connections", SHUTDOWN_GRACE);
                    Ok(())
                }
            }
        }
    };

    // Always stop the workers, even if the HTTP side failed.
    purger.abort();
    tracing::info!("http stopped; shutting down engine");
    engine.shutdown().await;
    served
}

/// Maximum time the HTTP server gets to drain after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Resolves on Ctrl-C or (on Unix) SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!(error = %e, "failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
