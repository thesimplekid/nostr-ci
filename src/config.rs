use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerSoftware {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerPrice {
    pub mint_url: String,
    pub price_per_second: String,
    pub unit: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub relays: Vec<String>,
    pub worker_software: Vec<WorkerSoftware>,
    pub worker_prices: Vec<WorkerPrice>,
    pub worker_name: String,
    pub worker_description: String,
    pub worker_architecture: String,
    pub worker_default_shell: String,
    pub worker_geohash: Option<String>,
    pub worker_min_duration: u64,
    pub worker_max_duration: u64,
    pub worker_max_concurrent_jobs: u64,
    pub advertise_interval: Duration,
    pub worker_service_name: String,
    pub blossom_servers: Vec<String>,
    pub cashu_mints: Vec<String>,
    pub max_concurrent_jobs: usize,
    pub poll_interval: Duration,
    pub job_timeout: Duration,
    pub worker_http_port: u16,
    pub worker_act_path: String,
    pub worker_ngit_path: String,
    pub worker_git_remote_nostr_path: String,
    pub worker_work_dir: String,
    pub state_dir: PathBuf,
    pub cdk_cli_path: String,
    pub cdk_work_dir: PathBuf,
    pub cdk_engine: String,
    pub nixos_container_bin: PathBuf,
    pub http_addr: IpAddr,
    pub http_port: u16,
}

impl Config {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup<F>(get: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let relays = split_csv(
            get("NOSTR_RELAYS").context("NOSTR_RELAYS environment variable is required")?,
        );
        if relays.is_empty() {
            anyhow::bail!("NOSTR_RELAYS must include at least one relay URL");
        }

        let worker_software = parse_worker_software(
            &get("WORKER_SOFTWARE").context("WORKER_SOFTWARE environment variable is required")?,
        )?;
        let worker_prices = parse_worker_prices(
            &get("WORKER_PRICES").context("WORKER_PRICES environment variable is required")?,
        )?;

        let max_concurrent_jobs = parse_or_default(&get, "MAX_CONCURRENT", "7")?;
        let poll_interval_secs: u64 = parse_or_default(&get, "POLL_INTERVAL", "10")?;
        let job_timeout_secs: u64 = parse_or_default(&get, "JOB_TIMEOUT", "7200")?;
        let advertise_interval_secs: u64 = parse_or_default(&get, "ADVERTISE_INTERVAL", "300")?;

        let worker_min_duration: u64 = parse_or_default(&get, "WORKER_MIN_DURATION", "1")?;
        let worker_max_duration: u64 =
            parse_or_default(&get, "WORKER_MAX_DURATION", &job_timeout_secs.to_string())?;
        if worker_min_duration > worker_max_duration {
            anyhow::bail!("WORKER_MIN_DURATION must be less than or equal to WORKER_MAX_DURATION");
        }

        let state_dir: PathBuf = get("STATE_DIR")
            .unwrap_or_else(|| "/var/lib/runner-controller".to_string())
            .into();
        let cdk_work_dir = get("CDK_WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.join("cdk-cli"));

        let http_addr = parse_or_default(&get, "HTTP_ADDR", "127.0.0.1")?;
        let http_port = parse_or_default(&get, "HTTP_PORT", "8080")?;
        let worker_http_port = parse_or_default(&get, "WORKER_HTTP_PORT", "8081")?;

        Ok(Config {
            relays,
            worker_software,
            worker_prices,
            worker_name: get("WORKER_NAME").unwrap_or_else(|| "loom-worker".to_string()),
            worker_description: get("WORKER_DESCRIPTION").unwrap_or_default(),
            worker_architecture: get("WORKER_ARCHITECTURE")
                .unwrap_or_else(|| std::env::consts::ARCH.to_string()),
            worker_default_shell: get("WORKER_DEFAULT_SHELL")
                .unwrap_or_else(|| "/bin/bash".to_string()),
            worker_geohash: get("WORKER_GEOHASH").filter(|s| !s.trim().is_empty()),
            worker_min_duration,
            worker_max_duration,
            worker_max_concurrent_jobs: parse_or_default(&get, "WORKER_MAX_CONCURRENT_JOBS", "1")?,
            advertise_interval: Duration::from_secs(advertise_interval_secs),
            worker_service_name: get("WORKER_SERVICE_NAME")
                .unwrap_or_else(|| "hive-worker.service".to_string()),
            blossom_servers: get("BLOSSOM_SERVERS").map(split_csv).unwrap_or_default(),
            cashu_mints: get("CASHU_MINTS").map(split_csv).unwrap_or_default(),
            max_concurrent_jobs,
            poll_interval: Duration::from_secs(poll_interval_secs),
            job_timeout: Duration::from_secs(job_timeout_secs),
            worker_http_port,
            worker_act_path: get("WORKER_ACT_PATH").unwrap_or_else(|| "act".to_string()),
            worker_ngit_path: get("WORKER_NGIT_PATH")
                .unwrap_or_else(|| "/usr/local/bin/ngit".to_string()),
            worker_git_remote_nostr_path: get("WORKER_GIT_REMOTE_NOSTR_PATH")
                .unwrap_or_else(|| "/usr/local/bin/git-remote-nostr".to_string()),
            worker_work_dir: get("WORKER_WORK_DIR")
                .unwrap_or_else(|| "/var/lib/loom-worker/work".to_string()),
            state_dir,
            cdk_cli_path: get("CDK_CLI_PATH").unwrap_or_else(|| "cdk-cli".to_string()),
            cdk_work_dir,
            cdk_engine: get("CDK_ENGINE").unwrap_or_else(|| "redb".to_string()),
            nixos_container_bin: get("NIXOS_CONTAINER_BIN")
                .unwrap_or_else(|| "nixos-container".to_string())
                .into(),
            http_addr,
            http_port,
        })
    }
}

fn parse_or_default<T, F>(get: &F, key: &str, default: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
    F: Fn(&str) -> Option<String>,
{
    get(key)
        .unwrap_or_else(|| default.to_string())
        .parse()
        .with_context(|| format!("{key} must be a valid value"))
}

fn split_csv(value: String) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_worker_software(value: &str) -> Result<Vec<WorkerSoftware>> {
    split_csv(value.to_string())
        .into_iter()
        .map(|entry| {
            let parts: Vec<&str> = entry.splitn(3, ':').collect();
            if parts.len() != 3 {
                return Err(anyhow!(
                    "WORKER_SOFTWARE entries must use name:version:/absolute/path"
                ));
            }
            if !parts[2].starts_with('/') {
                return Err(anyhow!("WORKER_SOFTWARE path must be absolute"));
            }
            Ok(WorkerSoftware {
                name: non_empty(parts[0], "WORKER_SOFTWARE name")?,
                version: non_empty(parts[1], "WORKER_SOFTWARE version")?,
                path: non_empty(parts[2], "WORKER_SOFTWARE path")?,
            })
        })
        .collect::<Result<Vec<_>>>()
        .and_then(|items| {
            if items.is_empty() {
                Err(anyhow!("WORKER_SOFTWARE must include at least one entry"))
            } else {
                Ok(items)
            }
        })
}

fn parse_worker_prices(value: &str) -> Result<Vec<WorkerPrice>> {
    split_csv(value.to_string())
        .into_iter()
        .map(|entry| {
            let parts = split_three_from_right(&entry).ok_or_else(|| {
                anyhow!("WORKER_PRICES entries must use mint_url:price_per_second:unit")
            })?;
            let mint_url = non_empty(parts.0, "WORKER_PRICES mint_url")?;
            if !mint_url.contains("://") {
                return Err(anyhow!("WORKER_PRICES mint_url must include a URL scheme"));
            }
            let price_per_second = non_empty(parts.1, "WORKER_PRICES price_per_second")?;
            let parsed_price = price_per_second
                .parse::<u64>()
                .context("WORKER_PRICES price_per_second must be a positive integer")?;
            if parsed_price == 0 {
                return Err(anyhow!(
                    "WORKER_PRICES price_per_second must be a positive integer"
                ));
            }
            Ok(WorkerPrice {
                mint_url,
                price_per_second,
                unit: non_empty(parts.2, "WORKER_PRICES unit")?,
            })
        })
        .collect::<Result<Vec<_>>>()
        .and_then(|items| {
            if items.is_empty() {
                return Err(anyhow!("WORKER_PRICES must include at least one entry"));
            }
            Ok(items)
        })
}

fn non_empty(value: &str, name: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(anyhow!("{name} cannot be empty"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn split_three_from_right(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.rsplitn(3, ':');
    let third = parts.next()?;
    let second = parts.next()?;
    let first = parts.next()?;
    Some((first, second, third))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config_from(entries: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn parses_loom_config() {
        let config = config_from(&[
            ("NOSTR_RELAYS", "wss://relay.example,wss://relay2.example"),
            ("WORKER_SOFTWARE", "nix:2.24:/run/current-system/sw/bin/nix"),
            ("WORKER_PRICES", "https://mint.example:10:sat"),
            ("WORKER_MIN_DURATION", "5"),
            ("WORKER_MAX_DURATION", "120"),
            ("WORKER_GEOHASH", "u09tun"),
        ])
        .unwrap();

        assert_eq!(
            config.relays,
            vec!["wss://relay.example", "wss://relay2.example"]
        );
        assert_eq!(config.worker_software[0].name, "nix");
        assert_eq!(
            config.worker_software[0].path,
            "/run/current-system/sw/bin/nix"
        );
        assert_eq!(config.worker_prices[0].unit, "sat");
        assert_eq!(config.worker_min_duration, 5);
        assert_eq!(config.worker_max_duration, 120);
        assert_eq!(config.worker_geohash.as_deref(), Some("u09tun"));
        assert_eq!(config.cdk_cli_path, "cdk-cli");
        assert_eq!(
            config.cdk_work_dir,
            PathBuf::from("/var/lib/runner-controller/cdk-cli")
        );
        assert_eq!(config.cdk_engine, "redb");
        assert_eq!(config.nixos_container_bin, PathBuf::from("nixos-container"));
        assert_eq!(config.http_addr, IpAddr::from([127, 0, 0, 1]));
        assert_eq!(config.worker_ngit_path, "/usr/local/bin/ngit");
        assert_eq!(
            config.worker_git_remote_nostr_path,
            "/usr/local/bin/git-remote-nostr"
        );
    }

    #[test]
    fn rejects_invalid_software_format() {
        let err = config_from(&[
            ("NOSTR_RELAYS", "wss://relay.example"),
            ("WORKER_SOFTWARE", "nix:2.24:relative/path"),
            ("WORKER_PRICES", "https://mint.example:10:sat"),
        ])
        .unwrap_err();

        assert!(err.to_string().contains("path must be absolute"));
    }

    #[test]
    fn rejects_invalid_price_format() {
        let err = config_from(&[
            ("NOSTR_RELAYS", "wss://relay.example"),
            ("WORKER_SOFTWARE", "nix:2.24:/bin/nix"),
            ("WORKER_PRICES", "https://mint.example:10"),
        ])
        .unwrap_err();

        assert!(err.to_string().contains("mint_url must include"));
    }

    #[test]
    fn parses_cdk_cli_overrides() {
        let config = config_from(&[
            ("NOSTR_RELAYS", "wss://relay.example"),
            ("WORKER_SOFTWARE", "nix:2.24:/bin/nix"),
            ("WORKER_PRICES", "https://mint.example:10:sat"),
            ("STATE_DIR", "/tmp/runner-controller"),
            ("CDK_CLI_PATH", "/opt/cdk-cli"),
            ("CDK_WORK_DIR", "/srv/cdk-wallet"),
            ("CDK_ENGINE", "sqlite"),
            ("HTTP_ADDR", "0.0.0.0"),
            (
                "NIXOS_CONTAINER_BIN",
                "/run/current-system/sw/bin/nixos-container",
            ),
        ])
        .unwrap();

        assert_eq!(config.cdk_cli_path, "/opt/cdk-cli");
        assert_eq!(config.cdk_work_dir, PathBuf::from("/srv/cdk-wallet"));
        assert_eq!(config.cdk_engine, "sqlite");
        assert_eq!(
            config.nixos_container_bin,
            PathBuf::from("/run/current-system/sw/bin/nixos-container")
        );
        assert_eq!(config.http_addr, IpAddr::from([0, 0, 0, 0]));
    }

    #[test]
    fn rejects_non_integer_or_zero_prices() {
        for price in ["0", "1.5"] {
            let err = config_from(&[
                ("NOSTR_RELAYS", "wss://relay.example"),
                ("WORKER_SOFTWARE", "nix:2.24:/bin/nix"),
                (
                    "WORKER_PRICES",
                    &format!("https://mint.example:{price}:sat"),
                ),
            ])
            .unwrap_err();

            assert!(err.to_string().contains("positive integer"));
        }
    }
}
