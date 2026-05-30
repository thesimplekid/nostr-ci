use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use nostr::prelude::{Event, Kind};
use nostr_sdk::prelude::RelayPoolNotification;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::container::ContainerManager;
use crate::jobs::{
    decode_job_event, event_is_addressed_to, JobStatus, WorkerJobResponse, JOB_REQUEST_KIND,
};
use crate::loom::{NostrPublisher, WorkerIdentity, WorkerRuntimeConfig};
use crate::state::{ContainerState, StateDb};

pub struct PoolController {
    config: Config,
    workers: Vec<WorkerIdentity>,
    publisher: Arc<NostrPublisher>,
    containers: Arc<ContainerManager>,
    state_db: Arc<StateDb>,
    shutdown_rx: watch::Receiver<bool>,
    last_advertised: Option<Instant>,
}

impl PoolController {
    pub fn new(
        config: Config,
        workers: Vec<WorkerIdentity>,
        publisher: Arc<NostrPublisher>,
        containers: Arc<ContainerManager>,
        state_db: Arc<StateDb>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            config,
            workers,
            publisher,
            containers,
            state_db,
            shutdown_rx,
            last_advertised: None,
        }
    }

    /// Reconcile state on startup - clean up old containers and stale state.
    pub async fn reconcile_on_startup(&self) -> Result<()> {
        info!("Reconciling worker pool on startup");

        // Get all containers (both old j* and current r* style).
        let all_containers = self.containers.list_all().await?;

        // Clean up any old-style j* containers (migration from job-based to pool-based).
        for name in all_containers.iter().filter(|n| n.starts_with('j')) {
            info!(name = %name, "Cleaning up old-style job container");
            self.cleanup_container_full(name).await?;
        }

        let pool_containers = self.containers.list().await?;
        for name in &pool_containers {
            match self
                .containers
                .is_worker_completed(name, &self.config.worker_service_name)
                .await
            {
                Ok(true) => {
                    info!(name = %name, "Cleaning up completed worker container from previous run");
                    self.cleanup_container_full(name).await?;
                }
                Ok(false) => {
                    info!(name = %name, "Worker container is still active");
                }
                Err(e) => {
                    warn!(name = %name, error = %e, "Failed to check container, cleaning up");
                    self.cleanup_container_full(name).await?;
                }
            }
        }

        // Clean up stale state entries (containers in DB but not in nixos-container list).
        let db_containers = self.state_db.list_containers()?;
        let active_set: HashSet<&str> = pool_containers.iter().map(|s| s.as_str()).collect();

        for (name, _) in db_containers {
            if !active_set.contains(name.as_str()) {
                info!(name = %name, "Removing stale state entry");
                self.state_db.remove_container(&name)?;
            }
        }

        Ok(())
    }

    /// Full cleanup: destroy container and remove state.
    async fn cleanup_container_full(&self, name: &str) -> Result<()> {
        self.containers.cleanup_container(name).await?;
        self.state_db.remove_container(name)?;
        Ok(())
    }

    /// Spawn a container for a pool slot.
    async fn spawn_pool_container(&self, slot: usize) -> Result<String> {
        let identity = &self.workers[slot];
        let runtime_config = WorkerRuntimeConfig::from_config(&self.config);

        let name = self
            .containers
            .spawn_pool_container(slot, &runtime_config)
            .await?;

        let state = ContainerState::new(slot, identity.pubkey.clone());
        self.state_db.put_container(&name, &state)?;

        if let Err(e) = self.publish_slot(slot).await {
            warn!(slot, name = %name, error = %e, "Failed to publish worker advertisement after spawn");
        }

        Ok(name)
    }

    /// Respawn a container in a pool slot (cleanup old, spawn new).
    async fn respawn_pool_container(&self, name: &str, slot: usize) -> Result<()> {
        self.cleanup_container_full(name).await?;
        self.spawn_pool_container(slot).await?;
        Ok(())
    }

    async fn handle_job_event(&self, event: &Event) -> Result<()> {
        if event.kind != Kind::Custom(JOB_REQUEST_KIND) {
            return Ok(());
        }

        let Some(slot) = self.slot_for_event(event) else {
            debug!(event_id = %event.id, "Ignoring job not addressed to this worker pool");
            return Ok(());
        };
        let identity = &self.workers[slot];

        let job = match decode_job_event(identity, event) {
            Ok(job) => job,
            Err(e) => {
                warn!(slot, event_id = %event.id, error = %e, "Rejecting malformed worker job");
                let response = WorkerJobResponse::invalid(e.to_string());
                if let Err(publish_error) = self
                    .publisher
                    .publish_result(identity, event, &response)
                    .await
                {
                    warn!(slot, event_id = %event.id, error = %publish_error, "Failed to publish invalid job result");
                }
                return Ok(());
            }
        };

        let name = ContainerManager::slot_to_container_name(slot);
        let response = match self
            .containers
            .dispatch_job(
                &name,
                self.config.worker_http_port,
                &job,
                self.config.job_timeout,
            )
            .await
        {
            Ok(response) => response,
            Err(e) => {
                warn!(slot, name = %name, event_id = %event.id, error = %e, "Worker dispatch failed");
                WorkerJobResponse::failure(e.to_string())
            }
        };

        if matches!(response.status, JobStatus::Failure | JobStatus::Timeout) {
            warn!(
                slot,
                name = %name,
                event_id = %event.id,
                status = response.status.as_str(),
                exit_code = ?response.exit_code,
                "Worker returned unsuccessful job result"
            );
        }

        if let Err(e) = self
            .publisher
            .publish_result(identity, event, &response)
            .await
        {
            warn!(slot, event_id = %event.id, error = %e, "Failed to publish worker job result");
        }

        Ok(())
    }

    fn slot_for_event(&self, event: &Event) -> Option<usize> {
        self.workers
            .iter()
            .find(|worker| event_is_addressed_to(event, &worker.pubkey))
            .map(|worker| worker.slot)
    }

    async fn publish_slot(&self, slot: usize) -> Result<()> {
        let identity = &self.workers[slot];
        let name = ContainerManager::slot_to_container_name(slot);
        self.publisher
            .publish_worker(&self.config, identity, &name)
            .await?;

        if let Some(mut state) = self.state_db.get_container(&name)? {
            state.worker_pubkey = identity.pubkey.clone();
            state.mark_advertised();
            self.state_db.put_container(&name, &state)?;
        }

        Ok(())
    }

    async fn publish_all_advertisements(&mut self) {
        for slot in 0..self.config.max_concurrent_jobs {
            if let Err(e) = self.publish_slot(slot).await {
                warn!(slot, error = %e, "Failed to publish worker advertisement");
            }
        }
        self.last_advertised = Some(Instant::now());
    }

    fn advertisements_due(&self) -> bool {
        self.last_advertised
            .map(|last| last.elapsed() >= self.config.advertise_interval)
            .unwrap_or(true)
    }

    /// Maintain the warm pool - ensure all slots have running worker containers.
    async fn maintain_pool(&self) -> Result<()> {
        let current_containers: HashSet<String> =
            self.containers.list().await?.into_iter().collect();

        for slot in 0..self.config.max_concurrent_jobs {
            let name = ContainerManager::slot_to_container_name(slot);
            let identity = &self.workers[slot];

            if !current_containers.contains(&name) {
                info!(slot, "Spawning container for empty worker pool slot");
                match self.spawn_pool_container(slot).await {
                    Ok(spawned_name) => {
                        info!(slot, name = %spawned_name, "Worker container spawned successfully");
                    }
                    Err(e) => {
                        warn!(slot, error = %e, "Failed to spawn worker container");
                    }
                }
                continue;
            }

            match self
                .containers
                .is_worker_completed(&name, &self.config.worker_service_name)
                .await
            {
                Ok(true) => {
                    info!(slot, name = %name, "Worker service completed, respawning container");
                    if let Err(e) = self.respawn_pool_container(&name, slot).await {
                        warn!(slot, name = %name, error = %e, "Failed to respawn worker container");
                    }
                }
                Ok(false) => {
                    if let Some(mut state) = self.state_db.get_container(&name)? {
                        if state.worker_pubkey != identity.pubkey {
                            state.worker_pubkey = identity.pubkey.clone();
                            self.state_db.put_container(&name, &state)?;
                        }
                        debug!(
                            slot,
                            name = %name,
                            running_secs = state.running_seconds(),
                            "Worker container healthy"
                        );
                    } else {
                        warn!(slot, name = %name, "Orphaned worker container still active, recovering state");
                        let state = ContainerState::new(slot, identity.pubkey.clone());
                        if let Err(e) = self.state_db.put_container(&name, &state) {
                            warn!(slot, name = %name, error = %e, "Failed to recover state for orphaned container");
                        }
                    }
                }
                Err(e) => {
                    warn!(slot, name = %name, error = %e, "Failed to check worker status, respawning");
                    if let Err(e) = self.respawn_pool_container(&name, slot).await {
                        warn!(slot, name = %name, error = %e, "Failed to respawn container after check failure");
                    }
                }
            }
        }

        // Clean up stale state entries (in DB but container no longer exists).
        let latest_containers: HashSet<String> =
            self.containers.list().await?.into_iter().collect();
        let db_containers = self.state_db.list_containers()?;
        for (name, _) in db_containers {
            if !latest_containers.contains(&name) {
                info!(name = %name, "Removing stale state entry (container no longer exists)");
                self.state_db.remove_container(&name)?;
            }
        }

        Ok(())
    }

    /// Main run loop.
    pub async fn run(&mut self) -> Result<()> {
        info!(
            poll_interval = ?self.config.poll_interval,
            advertise_interval = ?self.config.advertise_interval,
            pool_size = self.config.max_concurrent_jobs,
            worker_service = %self.config.worker_service_name,
            "Worker pool controller starting"
        );

        self.reconcile_on_startup().await?;
        self.publish_all_advertisements().await;
        if let Err(e) = self.publisher.subscribe_job_requests(&self.workers).await {
            warn!(error = %e, "Failed to subscribe for worker job requests");
        }
        let mut nostr_notifications = self.publisher.notifications();

        loop {
            if *self.shutdown_rx.borrow() {
                info!("Shutdown signal received");
                break;
            }

            if let Err(e) = self.maintain_pool().await {
                warn!(error = %e, "Error maintaining worker pool");
            }

            if self.advertisements_due() {
                self.publish_all_advertisements().await;
            }

            tokio::select! {
                _ = tokio::time::sleep(self.config.poll_interval) => {}
                notification = nostr_notifications.recv() => {
                    match notification {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            if let Err(e) = self.handle_job_event(&event).await {
                                warn!(event_id = %event.id, error = %e, "Failed to handle worker job event");
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(skipped, "Lagged while reading Nostr job notifications");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("Nostr notification stream closed");
                        }
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        info!("Shutdown signal received during sleep");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Graceful shutdown - stop managing the pool without killing active workers.
    pub async fn shutdown(&self) -> Result<()> {
        self.publisher.shutdown().await;
        info!("Controller shutdown complete; worker containers left running for recovery");
        Ok(())
    }
}
