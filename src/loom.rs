use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use nostr::prelude::{
    Event, EventBuilder, Keys, Kind, Metadata, Nip19Profile, PublicKey, RelayUrl, Tag, ToBech32,
};
use nostr_sdk::prelude::{Client, Filter, RelayPoolNotification, Timestamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::config::{Config, WorkerPrice, WorkerSoftware};
use crate::jobs::{build_result_event, WorkerJobResponse, JOB_REQUEST_KIND};

pub const WORKER_ADVERTISEMENT_KIND: u16 = 10100;

#[derive(Debug, Clone)]
pub struct WorkerIdentity {
    pub slot: usize,
    pub keys: Keys,
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRuntimeConfig {
    pub relays: Vec<String>,
    pub software: Vec<WorkerSoftware>,
    pub prices: Vec<WorkerPrice>,
    pub min_duration: u64,
    pub max_duration: u64,
    pub act_path: String,
    pub ngit_path: String,
    pub git_remote_nostr_path: String,
    pub work_dir: String,
    pub timeout_seconds: u64,
    pub http_port: u16,
    pub default_shell: String,
    pub blossom_servers: Vec<String>,
    pub cashu_mints: Vec<String>,
}

impl WorkerRuntimeConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            relays: config.relays.clone(),
            software: config.worker_software.clone(),
            prices: config.worker_prices.clone(),
            min_duration: config.worker_min_duration,
            max_duration: config.worker_max_duration,
            act_path: config.worker_act_path.clone(),
            ngit_path: config.worker_ngit_path.clone(),
            git_remote_nostr_path: config.worker_git_remote_nostr_path.clone(),
            work_dir: config.worker_work_dir.clone(),
            timeout_seconds: config.job_timeout.as_secs(),
            http_port: config.worker_http_port,
            default_shell: config.worker_default_shell.clone(),
            blossom_servers: config.blossom_servers.clone(),
            cashu_mints: config.cashu_mints.clone(),
        }
    }
}

pub struct WorkerKeyStore {
    key_dir: PathBuf,
}

impl WorkerKeyStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            key_dir: state_dir.join("keys"),
        }
    }

    pub fn load_or_create(&self, slot: usize) -> Result<WorkerIdentity> {
        std::fs::create_dir_all(&self.key_dir)
            .with_context(|| format!("Failed to create key directory: {:?}", self.key_dir))?;
        std::fs::set_permissions(&self.key_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!(
                    "Failed to set key directory permissions: {:?}",
                    self.key_dir
                )
            })?;

        let path = self.key_dir.join(format!("slot-{slot}.nsec"));
        let nsec = if path.exists() {
            std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read worker key: {:?}", path))?
                .trim()
                .to_string()
        } else {
            let keys = Keys::generate();
            let nsec = keys
                .secret_key()
                .to_bech32()
                .context("Failed to encode generated worker key as nsec")?;
            write_secret_file(&path, &nsec)?;
            nsec
        };

        let keys = Keys::parse(&nsec)
            .with_context(|| format!("Failed to parse worker key for slot {slot}"))?;
        let pubkey = keys.public_key().to_hex();

        Ok(WorkerIdentity { slot, keys, pubkey })
    }

    pub fn load_or_create_all(&self, slots: usize) -> Result<Vec<WorkerIdentity>> {
        (0..slots).map(|slot| self.load_or_create(slot)).collect()
    }
}

pub fn write_secret_file(path: &Path, content: &str) -> Result<()> {
    let mut handle = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Failed to open secret file: {:?}", path))?;
    handle
        .write_all(content.as_bytes())
        .with_context(|| format!("Failed to write secret file: {:?}", path))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to set secret file permissions: {:?}", path))?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdvertisementContent {
    pub name: String,
    pub description: String,
    pub max_concurrent_jobs: u64,
    pub current_queue_depth: u64,
}

pub fn worker_display_name(config: &Config, container_name: &str) -> String {
    format!("{} {}", config.worker_name, container_name)
}

pub fn build_advertisement_event(
    config: &Config,
    identity: &WorkerIdentity,
    container_name: &str,
) -> Result<Event> {
    let content = AdvertisementContent {
        name: worker_display_name(config, container_name),
        description: config.worker_description.clone(),
        max_concurrent_jobs: config.worker_max_concurrent_jobs,
        current_queue_depth: 0,
    };

    let mut tags = Vec::new();
    for software in &config.worker_software {
        tags.push(tag([
            "S",
            software.name.as_str(),
            software.version.as_str(),
            software.path.as_str(),
        ])?);
    }
    tags.push(tag(["A", config.worker_architecture.as_str()])?);
    for price in &config.worker_prices {
        tags.push(tag([
            "price",
            price.mint_url.as_str(),
            price.price_per_second.as_str(),
            price.unit.as_str(),
        ])?);
    }
    tags.push(tag(["metric", "second"])?);
    tags.push(tag([
        "min_duration",
        &config.worker_min_duration.to_string(),
    ])?);
    tags.push(tag([
        "max_duration",
        &config.worker_max_duration.to_string(),
    ])?);
    tags.push(tag([
        "default_shell",
        config.worker_default_shell.as_str(),
    ])?);
    if let Some(geohash) = &config.worker_geohash {
        tags.push(tag(["g", geohash.as_str()])?);
    }
    for relay in &config.relays {
        tags.push(tag(["relay", relay.as_str()])?);
    }

    let content = serde_json::to_string(&content)?;
    EventBuilder::new(Kind::Custom(WORKER_ADVERTISEMENT_KIND), content)
        .tags(tags)
        .sign_with_keys(&identity.keys)
        .context("Failed to sign worker advertisement")
}

pub fn build_worker_nprofile(relays: &[String], identity: &WorkerIdentity) -> Result<String> {
    let relay_urls = relays
        .iter()
        .map(|relay| {
            RelayUrl::parse(relay)
                .with_context(|| format!("Invalid relay URL for worker nprofile: {relay}"))
        })
        .collect::<Result<Vec<_>>>()?;

    Nip19Profile::new(identity.keys.public_key(), relay_urls)
        .to_bech32()
        .context("Failed to encode worker nprofile")
}

fn tag<const N: usize>(values: [&str; N]) -> Result<Tag> {
    Tag::parse(values).context("Failed to build Nostr tag")
}

pub struct NostrPublisher {
    client: Client,
    relays: Vec<String>,
}

impl NostrPublisher {
    pub async fn new(relays: Vec<String>) -> Result<Self> {
        let client = Client::default();
        for relay in &relays {
            client
                .add_read_relay(relay)
                .await
                .with_context(|| format!("Failed to add Nostr read relay {relay}"))?;
            client
                .add_write_relay(relay)
                .await
                .with_context(|| format!("Failed to add Nostr relay {relay}"))?;
        }
        client.connect().await;
        client.wait_for_connection(Duration::from_secs(5)).await;

        Ok(Self { client, relays })
    }

    pub fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.client.notifications()
    }

    pub async fn subscribe_job_requests(&self, workers: &[WorkerIdentity]) -> Result<()> {
        let pubkeys = workers
            .iter()
            .map(|worker| {
                PublicKey::from_hex(&worker.pubkey)
                    .with_context(|| format!("Invalid worker pubkey for slot {}", worker.slot))
            })
            .collect::<Result<Vec<_>>>()?;
        let filter = Filter::new()
            .kind(Kind::Custom(JOB_REQUEST_KIND))
            .pubkeys(pubkeys)
            .since(Timestamp::now());

        self.client
            .subscribe(filter, None)
            .await
            .context("Failed to subscribe for worker job requests")?;
        info!(
            workers = workers.len(),
            "Subscribed for worker job requests"
        );
        Ok(())
    }

    pub async fn publish_worker(
        &self,
        config: &Config,
        identity: &WorkerIdentity,
        container_name: &str,
    ) -> Result<()> {
        let metadata = Metadata::new()
            .name(worker_display_name(config, container_name))
            .display_name(worker_display_name(config, container_name))
            .about(config.worker_description.clone());
        let metadata_event = EventBuilder::metadata(&metadata)
            .sign_with_keys(&identity.keys)
            .context("Failed to sign worker profile metadata")?;
        let nprofile = build_worker_nprofile(&self.relays, identity)?;

        self.client
            .send_event(&metadata_event)
            .await
            .context("Failed to publish worker profile metadata")?;

        let advertisement = build_advertisement_event(config, identity, container_name)?;
        self.client
            .send_event(&advertisement)
            .await
            .context("Failed to publish worker advertisement")?;

        info!(
            slot = identity.slot,
            pubkey = %identity.pubkey,
            nprofile = %nprofile,
            metadata_event_id = %metadata_event.id,
            advertisement_event_id = %advertisement.id,
            relays = ?self.relays,
            "Published worker advertisement"
        );
        Ok(())
    }

    pub async fn publish_result(
        &self,
        identity: &WorkerIdentity,
        request: &Event,
        response: &WorkerJobResponse,
    ) -> Result<Event> {
        let result = build_result_event(identity, request, response)?;
        self.client
            .send_event(&result)
            .await
            .context("Failed to publish worker job result")?;
        info!(
            slot = identity.slot,
            request_event_id = %request.id,
            result_event_id = %result.id,
            status = response.status.as_str(),
            "Published worker job result"
        );
        Ok(result)
    }

    pub async fn shutdown(&self) {
        debug!("Disconnecting Nostr publisher");
        self.client.disconnect().await;
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::container::ContainerManager;
    use nostr::prelude::FromBech32;

    fn test_config() -> Config {
        Config {
            relays: vec!["wss://relay.example".to_string()],
            worker_software: vec![WorkerSoftware {
                name: "nix".to_string(),
                version: "2.24".to_string(),
                path: "/run/current-system/sw/bin/nix".to_string(),
            }],
            worker_prices: vec![WorkerPrice {
                mint_url: "https://mint.example".to_string(),
                price_per_second: "10".to_string(),
                unit: "sat".to_string(),
            }],
            worker_name: "worker".to_string(),
            worker_description: "test worker".to_string(),
            worker_architecture: "x86_64".to_string(),
            worker_default_shell: "/bin/bash".to_string(),
            worker_geohash: Some("u09tun".to_string()),
            worker_min_duration: 5,
            worker_max_duration: 120,
            worker_max_concurrent_jobs: 1,
            advertise_interval: Duration::from_secs(300),
            worker_service_name: "hive-worker.service".to_string(),
            blossom_servers: Vec::new(),
            cashu_mints: Vec::new(),
            max_concurrent_jobs: 1,
            poll_interval: Duration::from_secs(10),
            job_timeout: Duration::from_secs(7200),
            worker_http_port: 8081,
            worker_act_path: "/bin/act".to_string(),
            worker_ngit_path: "/usr/local/bin/ngit".to_string(),
            worker_git_remote_nostr_path: "/usr/local/bin/git-remote-nostr".to_string(),
            worker_work_dir: "/tmp/work".to_string(),
            state_dir: PathBuf::from("/tmp/runner-controller-test"),
            cdk_cli_path: "cdk-cli".to_string(),
            cdk_work_dir: PathBuf::from("/tmp/runner-controller-test/cdk-cli"),
            cdk_engine: "redb".to_string(),
            nixos_container_bin: PathBuf::from("nixos-container"),
            http_addr: "127.0.0.1".parse().unwrap(),
            http_port: 8080,
        }
    }

    #[test]
    fn persists_slot_key_and_derives_stable_pubkey() {
        let tempdir = tempfile::tempdir().unwrap();
        let store = WorkerKeyStore::new(tempdir.path());

        let first = store.load_or_create(0).unwrap();
        let second = store.load_or_create(0).unwrap();
        let metadata = std::fs::metadata(tempdir.path().join("keys/slot-0.nsec")).unwrap();

        assert_eq!(first.pubkey, second.pubkey);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn builds_worker_advertisement_event() {
        let config = test_config();
        let identity = WorkerIdentity {
            slot: 0,
            keys: Keys::generate(),
            pubkey: "unused".to_string(),
        };

        let event = build_advertisement_event(&config, &identity, "abcde-r0").unwrap();
        let kind: u16 = event.kind.into();
        let tags: Vec<Vec<String>> = event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();
        let content: AdvertisementContent = serde_json::from_str(&event.content).unwrap();

        assert_eq!(kind, WORKER_ADVERTISEMENT_KIND);
        assert_eq!(content.name, "worker abcde-r0");
        assert!(tags.contains(&vec![
            "S".to_string(),
            "nix".to_string(),
            "2.24".to_string(),
            "/run/current-system/sw/bin/nix".to_string(),
        ]));
        assert!(tags.contains(&vec!["A".to_string(), "x86_64".to_string()]));
        assert!(tags.contains(&vec![
            "price".to_string(),
            "https://mint.example".to_string(),
            "10".to_string(),
            "sat".to_string(),
        ]));
        assert!(tags.contains(&vec!["metric".to_string(), "second".to_string()]));
        assert!(tags.contains(&vec!["min_duration".to_string(), "5".to_string()]));
        assert!(tags.contains(&vec!["max_duration".to_string(), "120".to_string()]));
        assert!(tags.contains(&vec!["default_shell".to_string(), "/bin/bash".to_string()]));
        assert!(tags.contains(&vec!["g".to_string(), "u09tun".to_string()]));
        assert!(tags.contains(&vec![
            "relay".to_string(),
            "wss://relay.example".to_string()
        ]));
    }

    #[test]
    fn builds_worker_nprofile_with_relay_hints() {
        let config = test_config();
        let keys = Keys::generate();
        let identity = WorkerIdentity {
            slot: 0,
            pubkey: keys.public_key().to_hex(),
            keys,
        };

        let nprofile = build_worker_nprofile(&config.relays, &identity).unwrap();
        let decoded = Nip19Profile::from_bech32(&nprofile).unwrap();

        assert!(nprofile.starts_with("nprofile1"));
        assert_eq!(decoded.public_key, identity.keys.public_key());
        assert_eq!(decoded.relays.len(), 1);
        assert_eq!(decoded.relays[0].as_str(), "wss://relay.example");
    }

    #[test]
    fn worker_runtime_config_does_not_contain_nsec() {
        let config = test_config();
        let runtime_config = WorkerRuntimeConfig::from_config(&config);
        let json = serde_json::to_string(&runtime_config).unwrap();

        assert!(!json.contains("nsec"));
        assert!(json.contains("\"act_path\":\"/bin/act\""));
        assert!(json.contains("\"ngit_path\":\"/usr/local/bin/ngit\""));
        assert!(json.contains("\"git_remote_nostr_path\":\"/usr/local/bin/git-remote-nostr\""));
        assert!(json.contains("\"http_port\":8081"));
    }

    #[test]
    fn container_names_remain_stable() {
        std::env::set_var("HOSTNAME", "test-host");
        assert_eq!(ContainerManager::slot_to_container_name(0), "aa098-r0");
    }
}
