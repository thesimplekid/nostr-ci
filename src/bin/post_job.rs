use std::time::Duration;

use anyhow::{Context, Result};
use nostr::prelude::{nip44, EventBuilder, FromBech32, Keys, Kind, Nip19Profile, PublicKey, Tag};
use nostr_sdk::prelude::Client;
use serde_json::Value;

const JOB_REQUEST_KIND: u16 = 5100;

#[tokio::main]
async fn main() -> Result<()> {
    install_rustls_crypto_provider();

    let worker_target = worker_target()?;
    let mut relays = env_relays();
    if relays.is_empty() || !relay_env_only() {
        relays.extend(worker_target.relays);
    }
    relays.sort();
    relays.dedup();
    if relays.is_empty() {
        anyhow::bail!("NOSTR_RELAYS is required when the worker target has no relay hints");
    }

    let requester_keys = Keys::parse(&required_env("REQUESTER_NSEC")?)
        .context("REQUESTER_NSEC must be a valid nsec or hex secret key")?;
    let worker_pubkey = worker_target.pubkey;
    let payment_token = required_env("PAYMENT_TOKEN")?;

    let payload = serde_json::json!({
        "repo": required_env("JOB_REPO")?,
        "ref": env_or("JOB_REF", "main"),
        "workflow": required_env("JOB_WORKFLOW")?,
        "job": required_env("JOB_NAME")?,
        "event": env_or("JOB_EVENT", "push"),
        "event_payload": event_payload()?,
    });
    let plaintext =
        serde_json::to_string(&payload).context("failed to serialize job request payload")?;
    let encrypted = nip44::encrypt(
        requester_keys.secret_key(),
        &worker_pubkey,
        plaintext.as_bytes(),
        nip44::Version::V2,
    )
    .context("failed to encrypt job request content")?;

    let event = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), encrypted)
        .tags([
            Tag::parse(["p", worker_pubkey.to_hex().as_str()])
                .context("failed to build worker p tag")?,
            Tag::parse(["payment", payment_token.as_str()])
                .context("failed to build payment tag")?,
        ])
        .sign_with_keys(&requester_keys)
        .context("failed to sign job request event")?;

    let client = Client::default();
    for relay in &relays {
        client
            .add_relay(relay)
            .await
            .with_context(|| format!("failed to add relay {relay}"))?;
    }
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    let output = client
        .send_event(&event)
        .await
        .context("failed to publish job request event")?;

    println!("published job request {}", event.id);
    println!("{output:?}");
    client.disconnect().await;
    Ok(())
}

struct WorkerTarget {
    pubkey: PublicKey,
    relays: Vec<String>,
}

fn worker_target() -> Result<WorkerTarget> {
    if let Some(value) = std::env::args().nth(1) {
        return parse_worker_target(&value);
    }
    if let Ok(value) = std::env::var("WORKER_NPROFILE") {
        return parse_worker_target(&value);
    }
    parse_worker_target(&required_env("WORKER_PUBKEY")?)
}

fn parse_worker_target(value: &str) -> Result<WorkerTarget> {
    if value.starts_with("nprofile1") {
        let profile = Nip19Profile::from_bech32(value)
            .context("worker nprofile must be a valid NIP-19 profile")?;
        return Ok(WorkerTarget {
            pubkey: profile.public_key,
            relays: profile
                .relays
                .into_iter()
                .map(|relay| relay.to_string())
                .collect(),
        });
    }

    Ok(WorkerTarget {
        pubkey: PublicKey::from_hex(value).context(
            "WORKER_PUBKEY must be a hex public key, or pass WORKER_NPROFILE/an nprofile argument",
        )?,
        relays: Vec::new(),
    })
}

fn env_relays() -> Vec<String> {
    std::env::var("NOSTR_RELAYS")
        .map(split_csv)
        .unwrap_or_default()
}

fn relay_env_only() -> bool {
    std::env::var("NOSTR_RELAYS_ONLY")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn required_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required"))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn split_csv(value: String) -> Vec<String> {
    value
        .split(',')
        .map(|relay| relay.trim().to_string())
        .filter(|relay| !relay.is_empty())
        .collect()
}

fn event_payload() -> Result<Value> {
    match std::env::var("JOB_EVENT_PAYLOAD") {
        Ok(value) => serde_json::from_str(&value).context("JOB_EVENT_PAYLOAD must be valid JSON"),
        Err(_) => Ok(Value::Object(Default::default())),
    }
}

fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
