use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::watch;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod container;
mod http;
mod jobs;
mod listener;
mod loom;
mod payments;
mod state;

use config::Config;
use container::ContainerManager;
use http::{AdvertisementSettings, AppState};
use listener::PoolController;
use loom::{NostrPublisher, WorkerKeyStore};
use state::StateDb;

#[tokio::main]
async fn main() -> Result<()> {
    install_rustls_crypto_provider();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("runner-controller (Loom worker pool mode) starting");

    let start_time = Instant::now();

    // Load configuration
    let config = Config::from_env()?;
    tracing::info!(
        pool_size = config.max_concurrent_jobs,
        poll_interval = ?config.poll_interval,
        advertise_interval = ?config.advertise_interval,
        relays = ?config.relays,
        worker_service = %config.worker_service_name,
        http_port = config.http_port,
        "Configuration loaded"
    );

    // Initialize state database
    let state_db = Arc::new(StateDb::open(&config.state_dir)?);
    tracing::info!(state_dir = ?config.state_dir, "State database opened");

    // Load or create stable per-slot worker identities.
    let key_store = WorkerKeyStore::new(&config.state_dir);
    let workers = key_store.load_or_create_all(config.max_concurrent_jobs)?;
    tracing::info!(count = workers.len(), "Worker identities loaded");

    // Initialize Nostr publisher.
    let publisher = Arc::new(NostrPublisher::new(config.relays.clone()).await?);
    tracing::info!("Nostr publisher initialized");

    // Initialize container manager
    let containers = Arc::new(ContainerManager::new(
        config.nixos_container_bin.clone(),
        config.state_dir.clone(),
    ));
    tracing::info!("Container manager initialized");

    // Set up shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Start HTTP server
    let http_state = AppState {
        state_db: Arc::clone(&state_db),
        start_time,
        pool_size: config.max_concurrent_jobs,
        poll_interval_seconds: config.poll_interval.as_secs(),
        job_timeout_seconds: config.job_timeout.as_secs(),
        advertisement: AdvertisementSettings {
            relays: config.relays.clone(),
            software: config.worker_software.clone(),
            prices: config.worker_prices.clone(),
            min_duration: config.worker_min_duration,
            max_duration: config.worker_max_duration,
            advertise_interval_seconds: config.advertise_interval.as_secs(),
        },
    };
    let http_addr: SocketAddr = ([0, 0, 0, 0], config.http_port).into();
    let http_shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(http::run_server(http_addr, http_state, http_shutdown_rx));

    // Create pool controller
    let mut controller = PoolController::new(
        config.clone(),
        workers,
        Arc::clone(&publisher),
        Arc::clone(&containers),
        Arc::clone(&state_db),
        shutdown_rx,
    );

    // Spawn signal handler
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to create SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to create SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM");
            }
            _ = sigint.recv() => {
                tracing::info!("Received SIGINT");
            }
        }

        let _ = shutdown_tx_clone.send(true);
    });

    // Run the main loop
    let result = controller.run().await;

    // Graceful shutdown
    controller.shutdown().await?;

    tracing::info!("runner-controller stopped");

    result
}

fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
