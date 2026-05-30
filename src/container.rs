use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::jobs::{WorkerJobRequest, WorkerJobResponse};
use crate::loom::WorkerRuntimeConfig;

const NSPAWN_CONFIG_TEMPLATE: &str = r#"[Exec]
SystemCallFilter=add_key keyctl bpf
Capability=all

[Files]
Bind=/sys/fs/bpf
BindReadOnly=/sys/module
BindReadOnly=/lib/modules
BindReadOnly=/run/secrets
BindReadOnly=/run/agenix
"#;

const NGIT_INSTALL_URL: &str = "https://ngit.dev/install.sh";
pub const WORKER_HTTP_TIMEOUT_MESSAGE: &str = "Timed out waiting for worker HTTP API";

pub struct ContainerManager {
    nixos_container_bin: PathBuf,
    container_template: PathBuf,
    state_dir: PathBuf,
}

impl ContainerManager {
    pub fn new(nixos_container_bin: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            nixos_container_bin,
            container_template: PathBuf::from("/etc/nixos/ci-container-template.nix"),
            state_dir,
        }
    }

    /// Run nixos-container command and return output
    async fn run_container_cmd(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.nixos_container_bin)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("Failed to execute nixos-container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("nixos-container {} failed: {}", args.join(" "), stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Run a command inside a container
    async fn run_in_container(&self, name: &str, cmd: &[&str]) -> Result<String> {
        let mut args = vec!["run", name, "--"];
        args.extend(cmd);

        let output = Command::new(&self.nixos_container_bin)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("Failed to execute command in container")?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Run a command inside a container and require a successful exit status.
    async fn run_in_container_checked(&self, name: &str, cmd: &[&str]) -> Result<String> {
        let mut args = vec!["run", name, "--"];
        args.extend(cmd);

        let output = Command::new(&self.nixos_container_bin)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("Failed to execute command in container")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("container command failed: {}", stderr);
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Check if container can be reached
    async fn container_is_reachable(&self, name: &str) -> bool {
        let result = Command::new(&self.nixos_container_bin)
            .args(["run", name, "--", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        matches!(result, Ok(status) if status.success())
    }

    /// List all pool containers ({prefix}-r{digits})
    pub async fn list(&self) -> Result<Vec<String>> {
        let output = self.run_container_cmd(&["list"]).await?;

        let containers: Vec<String> = output
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|name| {
                // match {prefix}-r{digits}
                if let Some((prefix, slot)) = name.split_once("-r") {
                    return prefix.len() == 5
                        && prefix.chars().all(|c| c.is_ascii_hexdigit())
                        && !slot.is_empty()
                        && slot.chars().all(|c| c.is_ascii_digit());
                }

                false
            })
            .collect();

        Ok(containers)
    }

    /// List all worker containers ({prefix}-r* style)
    pub async fn list_all(&self) -> Result<Vec<String>> {
        let output = self.run_container_cmd(&["list"]).await?;

        let containers: Vec<String> = output
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|name| {
                // match {prefix}-r{digits}
                if let Some((prefix, slot)) = name.split_once("-r") {
                    return prefix.len() == 5
                        && prefix.chars().all(|c| c.is_ascii_hexdigit())
                        && !slot.is_empty()
                        && slot.chars().all(|c| c.is_ascii_digit());
                }

                if let Some(job_id) = name.strip_prefix('j') {
                    return !job_id.is_empty() && job_id.chars().all(|c| c.is_ascii_digit());
                }

                false
            })
            .collect();

        Ok(containers)
    }

    /// Get a free subnet octet in the 100-199 range
    pub async fn get_free_subnet(&self) -> Result<u8> {
        let containers = self.list_all().await?;
        let mut used_subnets = HashSet::new();

        for container in containers {
            if let Ok(ip) = self.run_container_cmd(&["show-ip", &container]).await {
                // Parse IP like "192.168.150.11" to get subnet octet (150)
                if let Some(octet) = ip.trim().split('.').nth(2) {
                    if let Ok(n) = octet.parse::<u8>() {
                        used_subnets.insert(n);
                    }
                }
            }
        }

        for octet in 100..=199 {
            if !used_subnets.contains(&octet) {
                return Ok(octet);
            }
        }

        // Fallback - shouldn't happen with the configured worker pool size.
        Ok(100)
    }

    /// Convert pool slot index to container name (r + slot number)
    pub fn slot_to_container_name(slot: usize) -> String {
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
            .unwrap_or_else(|_| "runner".to_string());
        let hash = Self::hash_string(&hostname);
        let short_id = format!("{:x}", hash).chars().take(5).collect::<String>();

        // Container names like "hash-r0"
        format!("{}-r{}", short_id, slot)
    }
    fn hash_string(s: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }

    /// Write nspawn configuration for Docker support
    fn write_nspawn_config(&self, name: &str) -> Result<()> {
        let nspawn_dir = Path::new("/etc/systemd/nspawn");
        std::fs::create_dir_all(nspawn_dir)?;

        let config_path = nspawn_dir.join(format!("{}.nspawn", name));
        std::fs::write(&config_path, NSPAWN_CONFIG_TEMPLATE)
            .with_context(|| format!("Failed to write nspawn config: {:?}", config_path))?;

        Ok(())
    }

    /// Create and start a container for a pool slot.
    pub async fn spawn_pool_container(
        &self,
        slot: usize,
        worker_config: &WorkerRuntimeConfig,
    ) -> Result<String> {
        let name = Self::slot_to_container_name(slot);
        let subnet = self.get_free_subnet().await?;

        info!(
            name = %name,
            slot,
            subnet,
            "Spawning pool container"
        );

        // Clean up any existing container with same name
        if self.list_all().await?.contains(&name) {
            warn!(name = %name, "Cleaning up existing container first");
            self.cleanup_container(&name).await?;
        }

        // Clean up leftover artifacts
        self.cleanup_artifacts(&name).await;

        // Write nspawn config for Docker support
        self.write_nspawn_config(&name)?;

        // Create container
        let local_addr = format!("192.168.{}.11", subnet);
        let host_addr = format!("192.168.{}.10", subnet);

        let create_result = self
            .run_container_cmd(&[
                "create",
                &name,
                "--config-file",
                self.container_template.to_str().unwrap(),
                "--local-address",
                &local_addr,
                "--host-address",
                &host_addr,
            ])
            .await;

        if let Err(e) = create_result {
            // Cleanup on failure
            self.cleanup_artifacts(&name).await;
            return Err(e);
        }

        // Write execution-only runtime config into the container filesystem before starting.
        let container_root = PathBuf::from(format!("/var/lib/nixos-containers/{}", name));
        let worker_dir = container_root.join("var/lib/loom-worker");
        let container_config_path = worker_dir.join("config.json");

        std::fs::create_dir_all(&worker_dir)?;

        let config_json = serde_json::to_vec_pretty(worker_config)
            .context("Failed to serialize worker runtime config")?;
        let mut config_handle = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&container_config_path)
            .context("Failed to open worker config file in container")?;
        config_handle
            .write_all(&config_json)
            .context("Failed to write worker config into container")?;
        std::fs::set_permissions(
            &container_config_path,
            std::fs::Permissions::from_mode(0o600),
        )
        .context("Failed to set worker config permissions in container")?;

        // Start container
        if let Err(e) = self.run_container_cmd(&["start", &name]).await {
            warn!(name = %name, error = %e, "Failed to start container, cleaning up");
            self.cleanup_container(&name).await?;
            return Err(e);
        }

        if let Err(e) = self.ensure_ngit_installed(&name, worker_config).await {
            warn!(name = %name, error = %e, "Failed to install ngit in container, cleaning up");
            self.cleanup_container(&name).await?;
            return Err(e);
        }

        info!(name = %name, slot, "Pool container started");
        Ok(name)
    }

    async fn ensure_ngit_installed(
        &self,
        name: &str,
        worker_config: &WorkerRuntimeConfig,
    ) -> Result<()> {
        if self.ngit_available(name, worker_config).await {
            debug!(name = %name, "ngit already available in container");
            return Ok(());
        }

        info!(name = %name, "Installing ngit in container");
        let install_script = format!(
            "set -euo pipefail\n\
             tmpdir=$(mktemp -d)\n\
             trap 'rm -rf \"$tmpdir\"' EXIT\n\
             curl -Ls {install_url} -o \"$tmpdir/install-ngit.sh\"\n\
             bash \"$tmpdir/install-ngit.sh\"\n\
             mkdir -p /usr/local/bin\n\
             for tool in ngit git-remote-nostr; do\n\
               for dir in /root/.local/bin /root/.cargo/bin /usr/local/bin; do\n\
                 if [ -x \"$dir/$tool\" ]; then\n\
                   cp \"$dir/$tool\" \"/usr/local/bin/$tool\"\n\
                   chmod 0755 \"/usr/local/bin/$tool\"\n\
                   break\n\
                 fi\n\
               done\n\
             done\n",
            install_url = shell_quote(NGIT_INSTALL_URL),
        );
        self.run_in_container_checked(name, &["bash", "-lc", &install_script])
            .await?;

        if !self.ngit_available(name, worker_config).await {
            anyhow::bail!("ngit installer completed but ngit or git-remote-nostr is unavailable");
        }

        Ok(())
    }

    async fn ngit_available(&self, name: &str, worker_config: &WorkerRuntimeConfig) -> bool {
        let check = format!(
            "command -v {} >/dev/null && command -v {} >/dev/null",
            shell_quote(&worker_config.ngit_path),
            shell_quote(&worker_config.git_remote_nostr_path),
        );
        Command::new(&self.nixos_container_bin)
            .args(["run", name, "--", "bash", "-lc", &check])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success())
    }

    pub async fn dispatch_job(
        &self,
        name: &str,
        port: u16,
        job: &WorkerJobRequest,
        timeout: Duration,
    ) -> Result<WorkerJobResponse> {
        let ip = self
            .run_container_cmd(&["show-ip", name])
            .await
            .with_context(|| format!("Failed to get IP for container {name}"))?;
        let host = ip.trim();
        dispatch_http_job(host, port, job, timeout).await
    }

    /// Check if the worker service inside container has completed or failed.
    pub async fn is_worker_completed(&self, name: &str, service_name: &str) -> Result<bool> {
        // First check if container is reachable
        if !self.container_is_reachable(name).await {
            debug!(name = %name, "Container not reachable, considering completed");
            return Ok(true);
        }

        // Check worker service status
        let status = self
            .run_in_container(name, &["systemctl", "is-active", service_name])
            .await
            .unwrap_or_else(|_| "unknown".to_string());

        let status = status.trim();

        match status {
            "active" | "activating" | "reloading" => Ok(false),
            "failed" => {
                debug!(name = %name, service = %service_name, "Worker service failed");
                Ok(true)
            }
            "inactive" => {
                // Check if service ever ran
                let result = self
                    .run_in_container(
                        name,
                        &["systemctl", "show", service_name, "--property=Result"],
                    )
                    .await
                    .unwrap_or_default();

                if result.contains("success") || result.contains("exit-code") {
                    debug!(name = %name, service = %service_name, "Worker service completed");
                    Ok(true)
                } else {
                    // Service hasn't run yet
                    Ok(false)
                }
            }
            _ => {
                debug!(name = %name, status, "Unknown service status");
                Ok(false)
            }
        }
    }

    /// Stop a container
    pub async fn stop(&self, name: &str) -> Result<()> {
        debug!(name = %name, "Stopping container");

        // Stop systemd service first
        let _ = Command::new("systemctl")
            .args(["stop", &format!("container@{}.service", name)])
            .status()
            .await;

        // Then nixos-container stop
        let _ = self.run_container_cmd(&["stop", name]).await;

        Ok(())
    }

    /// Destroy a container
    pub async fn destroy(&self, name: &str) -> Result<()> {
        debug!(name = %name, "Destroying container");
        let _ = self.run_container_cmd(&["destroy", name]).await;
        Ok(())
    }

    /// Clean up leftover artifacts (network interface, profiles, etc.)
    async fn cleanup_artifacts(&self, name: &str) {
        // Remove nspawn config
        let nspawn_config = PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name));
        let _ = std::fs::remove_file(&nspawn_config);

        // Remove nspawn unix-export socket directory (prevents "Mount point exists already" error)
        let unix_export = PathBuf::from(format!("/run/systemd/nspawn/unix-export/{}", name));
        let _ = std::fs::remove_dir_all(&unix_export);

        // Remove container profiles
        let profile_dir = PathBuf::from(format!("/nix/var/nix/profiles/per-container/{}", name));
        let _ = std::fs::remove_dir_all(&profile_dir);

        // Remove container root
        let container_root = PathBuf::from(format!("/var/lib/nixos-containers/{}", name));
        let _ = std::fs::remove_dir_all(&container_root);

        // Remove network interface (shell out to ip)
        let _ = Command::new("ip")
            .args(["link", "delete", &format!("ve-{}", name)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        // Remove state files
        let _ = std::fs::remove_file(self.state_dir.join(format!("{}.token", name)));
    }

    /// Full cleanup of a container
    pub async fn cleanup_container(&self, name: &str) -> Result<()> {
        info!(name = %name, "Cleaning up container");

        self.stop(name).await?;
        self.destroy(name).await?;
        self.cleanup_artifacts(name).await;

        info!(name = %name, "Container cleaned up");
        Ok(())
    }
}

pub async fn dispatch_http_job(
    host: &str,
    port: u16,
    job: &WorkerJobRequest,
    timeout: Duration,
) -> Result<WorkerJobResponse> {
    let body = serde_json::to_vec(job).context("Failed to serialize worker job request")?;
    let request = build_http_job_request(host, port, &body);
    let response = tokio::time::timeout(timeout, async {
        let mut stream = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("Failed to connect to worker HTTP API at {host}:{port}"))?;
        stream
            .write_all(&request)
            .await
            .context("Failed to write worker HTTP request")?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .context("Failed to read worker HTTP response")?;
        Ok::<Vec<u8>, anyhow::Error>(response)
    })
    .await
    .context(WORKER_HTTP_TIMEOUT_MESSAGE)??;

    parse_http_job_response(&response)
}

fn build_http_job_request(host: &str, port: u16, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "POST /jobs HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    request
}

fn parse_http_job_response(response: &[u8]) -> Result<WorkerJobResponse> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("Worker HTTP response missing header terminator")?;
    let headers = std::str::from_utf8(&response[..separator])
        .context("Worker HTTP response headers were not UTF-8")?;
    let status_line = headers
        .lines()
        .next()
        .context("Worker HTTP response missing status line")?;
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .context("Worker HTTP response missing status code")?
        .parse::<u16>()
        .context("Worker HTTP response status code was invalid")?;
    if !(200..300).contains(&status_code) {
        anyhow::bail!("Worker HTTP API returned status {status_code}");
    }

    serde_json::from_slice(&response[separator + 4..])
        .context("Worker HTTP response body was not valid result JSON")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::jobs::{JobStatus, WorkerJobRequest};

    #[test]
    fn builds_worker_jobs_http_request() {
        let job = WorkerJobRequest {
            request_event_id: "event-1".to_string(),
            repo: "nostr://_@danconwaydev.com/gitworkshop".to_string(),
            ref_: "main".to_string(),
            workflow: ".github/workflows/ci.yml".to_string(),
            job: "test".to_string(),
            event: "push".to_string(),
            event_payload: json!({}),
        };
        let body = serde_json::to_vec(&job).unwrap();

        let request =
            String::from_utf8(build_http_job_request("192.168.100.11", 8081, &body)).unwrap();

        assert!(request.starts_with("POST /jobs HTTP/1.1\r\n"));
        assert!(request.contains("Host: 192.168.100.11:8081\r\n"));
        assert!(request.contains("Content-Type: application/json\r\n"));
        assert!(request.contains("\"request_event_id\":\"event-1\""));
        assert!(request.contains("\"workflow\":\".github/workflows/ci.yml\""));
        assert!(request.contains("\"job\":\"test\""));
    }

    #[test]
    fn parses_worker_jobs_http_response() {
        let body = json!({
            "status": "success",
            "exit_code": 0,
            "elapsed_seconds": 3,
            "log_tail": "ok"
        })
        .to_string();
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );

        let response = parse_http_job_response(raw.as_bytes()).unwrap();

        assert_eq!(response.status, JobStatus::Success);
        assert_eq!(response.exit_code, Some(0));
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("ngit"), "'ngit'");
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }
}
