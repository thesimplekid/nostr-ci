use anyhow::{anyhow, Context, Result};
use nostr::prelude::{nip04, nip44, Event, EventBuilder, Keys, Kind, Tag};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::loom::WorkerIdentity;
use crate::payments::BillingOutcome;

pub const JOB_REQUEST_KIND: u16 = 5100;
pub const JOB_RESULT_KIND: u16 = 6100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Success,
    Failure,
    Timeout,
    Invalid,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Timeout => "timeout",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerJobRequest {
    #[serde(default)]
    pub request_event_id: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub workflow: String,
    pub job: String,
    #[serde(default = "default_event")]
    pub event: String,
    #[serde(default)]
    pub event_payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerJobResponse {
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub elapsed_seconds: u64,
    pub log_tail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<JobBilling>,
    #[serde(default, skip_serializing)]
    pub result_errors: Vec<String>,
}

impl WorkerJobResponse {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: JobStatus::Invalid,
            exit_code: None,
            elapsed_seconds: 0,
            log_tail: message.into(),
            billing: None,
            result_errors: Vec::new(),
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            status: JobStatus::Failure,
            exit_code: None,
            elapsed_seconds: 0,
            log_tail: message.into(),
            billing: None,
            result_errors: Vec::new(),
        }
    }

    pub fn timeout(message: impl Into<String>, elapsed_seconds: u64) -> Self {
        Self {
            status: JobStatus::Timeout,
            exit_code: None,
            elapsed_seconds,
            log_tail: message.into(),
            billing: None,
            result_errors: Vec::new(),
        }
    }

    pub fn with_billing(&mut self, payment: JobBilling) {
        self.billing = Some(payment);
    }

    pub fn add_result_error(&mut self, message: impl Into<String>) {
        self.result_errors.push(message.into());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobBilling {
    pub duration: u64,
    pub billable_duration: u64,
    pub cost: u64,
    pub mint: String,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
}

impl JobBilling {
    pub fn from_outcome(
        outcome: BillingOutcome,
        mint: impl Into<String>,
        unit: impl Into<String>,
        change: Option<String>,
    ) -> Self {
        Self {
            duration: outcome.duration,
            billable_duration: outcome.billable_duration,
            cost: outcome.cost,
            mint: mint.into(),
            unit: unit.into(),
            change,
        }
    }
}

#[allow(
    dead_code,
    reason = "worker-side act execution settings are serialized to containers and covered by tests"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerExecutionSettings {
    pub act_path: String,
    pub work_dir: String,
    pub timeout_seconds: u64,
}

#[allow(
    dead_code,
    reason = "worker-side act command construction is covered by tests but not invoked by the manager binary"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[allow(
    dead_code,
    reason = "worker-side clone command construction is covered by tests but not invoked by the manager binary"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCloneCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[allow(
    dead_code,
    reason = "worker-side act command construction is covered by tests but not invoked by the manager binary"
)]
pub fn build_act_command(settings: &WorkerExecutionSettings, job: &WorkerJobRequest) -> ActCommand {
    ActCommand {
        program: settings.act_path.clone(),
        args: vec![
            job.event.clone(),
            "-W".to_string(),
            job.workflow.clone(),
            "-j".to_string(),
            job.job.clone(),
        ],
        env: vec![
            ("GITHUB_REF".to_string(), job.ref_.clone()),
            ("GITHUB_SHA".to_string(), job.ref_.clone()),
        ],
    }
}

#[allow(
    dead_code,
    reason = "worker-side clone command construction is covered by tests but not invoked by the manager binary"
)]
pub fn build_git_clone_command(job: &WorkerJobRequest, destination: &str) -> GitCloneCommand {
    GitCloneCommand {
        program: "git".to_string(),
        args: vec![
            "clone".to_string(),
            "--branch".to_string(),
            job.ref_.clone(),
            "--single-branch".to_string(),
            job.repo.clone(),
            destination.to_string(),
        ],
    }
}

pub fn parse_job_payload(payload: &str) -> Result<WorkerJobRequest> {
    let mut job: WorkerJobRequest =
        serde_json::from_str(payload).context("job payload must be valid JSON")?;
    validate_job(&job)?;
    job.request_event_id = job.request_event_id.trim().to_string();
    Ok(job)
}

pub fn decode_job_event(identity: &WorkerIdentity, event: &Event) -> Result<WorkerJobRequest> {
    if event.kind != Kind::Custom(JOB_REQUEST_KIND) {
        return Err(anyhow!("event kind must be {JOB_REQUEST_KIND}"));
    }
    if !event_is_addressed_to(event, &identity.pubkey) {
        return Err(anyhow!(
            "job is not addressed to worker {}",
            identity.pubkey
        ));
    }

    let plaintext =
        decrypt_job_content(&identity.keys, event).context("failed to decrypt job content")?;
    let mut job = parse_job_payload(&plaintext)?;
    job.request_event_id = event.id.to_hex();
    Ok(job)
}

pub fn event_is_addressed_to(event: &Event, pubkey_hex: &str) -> bool {
    event.tags.iter().any(|tag| {
        let values = tag.as_slice();
        values.len() >= 2 && values[0] == "p" && values[1] == pubkey_hex
    })
}

pub fn build_result_event(
    identity: &WorkerIdentity,
    request: &Event,
    response: &WorkerJobResponse,
) -> Result<Event> {
    let content = serde_json::to_string(response).context("failed to serialize result content")?;
    let request_id = request.id.to_hex();
    let requester = request.pubkey.to_hex();
    let mut tags = vec![
        tag(["e", request_id.as_str()])?,
        tag(["p", requester.as_str()])?,
        tag(["status", response.status.as_str()])?,
    ];
    if let Some(billing) = &response.billing {
        tags.push(tag(["duration", &billing.duration.to_string()])?);
        tags.push(tag([
            "billable_duration",
            &billing.billable_duration.to_string(),
        ])?);
        tags.push(tag(["cost", &billing.cost.to_string()])?);
        tags.push(tag(["mint", billing.mint.as_str()])?);
        tags.push(tag(["unit", billing.unit.as_str()])?);
        if let Some(change) = &billing.change {
            tags.push(tag(["change", change.as_str()])?);
        }
    }
    for error in &response.result_errors {
        tags.push(tag(["error", error.as_str()])?);
    }

    EventBuilder::new(Kind::Custom(JOB_RESULT_KIND), content)
        .tags(tags)
        .sign_with_keys(&identity.keys)
        .context("failed to sign job result event")
}

pub fn extract_payment_token(event: &Event) -> Result<String> {
    let payment_tags = event
        .tags
        .iter()
        .filter_map(|tag| {
            let values = tag.as_slice();
            (values.len() == 2 && values[0] == "payment").then(|| values[1].clone())
        })
        .collect::<Vec<_>>();

    match payment_tags.as_slice() {
        [token] if !token.trim().is_empty() => Ok(token.trim().to_string()),
        [] => Err(anyhow!("job request must include exactly one payment tag")),
        [_] => Err(anyhow!("payment tag token cannot be empty")),
        _ => Err(anyhow!("job request must include exactly one payment tag")),
    }
}

fn decrypt_job_content(keys: &Keys, event: &Event) -> Result<String> {
    nip44::decrypt(keys.secret_key(), &event.pubkey, event.content.as_bytes())
        .or_else(|_| nip04::decrypt(keys.secret_key(), &event.pubkey, event.content.clone()))
        .context("NIP-44 and NIP-04 decryption both failed")
}

fn validate_job(job: &WorkerJobRequest) -> Result<()> {
    if job.repo.trim().is_empty() {
        return Err(anyhow!("repo is required"));
    }
    if !is_ngit_repo_url(&job.repo) {
        return Err(anyhow!("repo must be an ngit nostr:// clone URL"));
    }
    if job.ref_.trim().is_empty() {
        return Err(anyhow!("ref is required"));
    }
    if job.workflow.trim().is_empty() {
        return Err(anyhow!("workflow is required"));
    }
    if job.job.trim().is_empty() {
        return Err(anyhow!("job is required"));
    }
    if job.event.trim().is_empty() {
        return Err(anyhow!("event is required"));
    }
    Ok(())
}

fn is_ngit_repo_url(repo: &str) -> bool {
    let trimmed = repo.trim();
    trimmed.starts_with("nostr://") && trimmed.len() > "nostr://".len()
}

fn default_event() -> String {
    "push".to_string()
}

fn tag<const N: usize>(values: [&str; N]) -> Result<Tag> {
    Tag::parse(values).context("failed to build Nostr tag")
}

#[cfg(test)]
mod tests {
    use nostr::prelude::nip04;

    use super::*;

    fn identity(slot: usize) -> WorkerIdentity {
        let keys = Keys::generate();
        WorkerIdentity {
            slot,
            pubkey: keys.public_key().to_hex(),
            keys,
        }
    }

    fn valid_payload() -> String {
        serde_json::json!({
            "repo": "nostr://_@danconwaydev.com/gitworkshop",
            "ref": "main",
            "workflow": ".github/workflows/ci.yml",
            "job": "test",
            "event": "push",
            "event_payload": {"after": "abc"}
        })
        .to_string()
    }

    #[test]
    fn parser_requires_explicit_workflow_and_job() {
        let missing_workflow = serde_json::json!({
            "repo": "nostr://_@danconwaydev.com/gitworkshop",
            "ref": "main",
            "job": "test"
        })
        .to_string();
        let missing_job = serde_json::json!({
            "repo": "nostr://_@danconwaydev.com/gitworkshop",
            "ref": "main",
            "workflow": ".github/workflows/ci.yml"
        })
        .to_string();

        assert!(parse_job_payload(&missing_workflow).is_err());
        assert!(parse_job_payload(&missing_job).is_err());
    }

    #[test]
    fn parser_requires_ngit_nostr_repo_url() {
        let github_repo = serde_json::json!({
            "repo": "https://github.com/org/repo.git",
            "ref": "main",
            "workflow": ".github/workflows/ci.yml",
            "job": "test"
        })
        .to_string();

        let err = parse_job_payload(&github_repo).unwrap_err();

        assert!(err.to_string().contains("ngit nostr://"));
    }

    #[test]
    fn git_clone_command_uses_ngit_nostr_remote_and_ref() {
        let job = parse_job_payload(&valid_payload()).unwrap();

        let command = build_git_clone_command(&job, "/tmp/work/repo");

        assert_eq!(command.program, "git");
        assert_eq!(
            command.args,
            vec![
                "clone".to_string(),
                "--branch".to_string(),
                "main".to_string(),
                "--single-branch".to_string(),
                "nostr://_@danconwaydev.com/gitworkshop".to_string(),
                "/tmp/work/repo".to_string(),
            ]
        );
    }

    #[test]
    fn act_command_uses_explicit_workflow_job_event_and_ref() {
        let job = parse_job_payload(&valid_payload()).unwrap();
        let settings = WorkerExecutionSettings {
            act_path: "/bin/act".to_string(),
            work_dir: "/tmp/work".to_string(),
            timeout_seconds: 60,
        };

        let command = build_act_command(&settings, &job);

        assert_eq!(command.program, "/bin/act");
        assert_eq!(
            command.args,
            vec![
                "push".to_string(),
                "-W".to_string(),
                ".github/workflows/ci.yml".to_string(),
                "-j".to_string(),
                "test".to_string(),
            ]
        );
        assert!(command
            .env
            .contains(&("GITHUB_REF".to_string(), "main".to_string())));
        assert!(command
            .env
            .contains(&("GITHUB_SHA".to_string(), "main".to_string())));
    }

    #[test]
    fn result_event_is_kind_6100_and_signed_by_slot_key() {
        let worker = identity(0);
        let requester = Keys::generate();
        let request = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), valid_payload())
            .tags([tag(["p", worker.pubkey.as_str()]).unwrap()])
            .sign_with_keys(&requester)
            .unwrap();
        let response = WorkerJobResponse {
            status: JobStatus::Success,
            exit_code: Some(0),
            elapsed_seconds: 42,
            log_tail: "ok".to_string(),
            billing: None,
            result_errors: Vec::new(),
        };

        let result = build_result_event(&worker, &request, &response).unwrap();

        assert_eq!(result.kind, Kind::Custom(JOB_RESULT_KIND));
        assert_eq!(result.pubkey, worker.keys.public_key());
        assert_eq!(
            result
                .tags
                .iter()
                .map(|tag| tag.as_slice())
                .collect::<Vec<_>>(),
            vec![
                vec!["e".to_string(), request.id.to_hex()],
                vec!["p".to_string(), requester.public_key().to_hex()],
                vec!["status".to_string(), "success".to_string()],
            ]
        );
    }

    #[test]
    fn extracts_exactly_one_payment_tag() {
        let worker = identity(0);
        let requester = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), valid_payload())
            .tags([
                tag(["p", worker.pubkey.as_str()]).unwrap(),
                tag(["payment", "cashu-token"]).unwrap(),
            ])
            .sign_with_keys(&requester)
            .unwrap();

        assert_eq!(extract_payment_token(&event).unwrap(), "cashu-token");

        let missing = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), valid_payload())
            .tags([tag(["p", worker.pubkey.as_str()]).unwrap()])
            .sign_with_keys(&requester)
            .unwrap();
        assert!(extract_payment_token(&missing).is_err());

        let duplicate = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), valid_payload())
            .tags([
                tag(["p", worker.pubkey.as_str()]).unwrap(),
                tag(["payment", "one"]).unwrap(),
                tag(["payment", "two"]).unwrap(),
            ])
            .sign_with_keys(&requester)
            .unwrap();
        assert!(extract_payment_token(&duplicate).is_err());
    }

    #[test]
    fn result_event_includes_billing_tags_and_optional_change() {
        let worker = identity(0);
        let requester = Keys::generate();
        let request = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), valid_payload())
            .tags([tag(["p", worker.pubkey.as_str()]).unwrap()])
            .sign_with_keys(&requester)
            .unwrap();
        let mut response = WorkerJobResponse {
            status: JobStatus::Success,
            exit_code: Some(0),
            elapsed_seconds: 3,
            log_tail: "ok".to_string(),
            billing: None,
            result_errors: Vec::new(),
        };
        response.with_billing(JobBilling {
            duration: 3,
            billable_duration: 5,
            cost: 50,
            mint: "https://mint.example".to_string(),
            unit: "sat".to_string(),
            change: Some("cashu-change".to_string()),
        });

        let result = build_result_event(&worker, &request, &response).unwrap();
        let tags: Vec<Vec<String>> = result
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();

        assert!(tags.contains(&vec!["duration".to_string(), "3".to_string()]));
        assert!(tags.contains(&vec!["billable_duration".to_string(), "5".to_string()]));
        assert!(tags.contains(&vec!["cost".to_string(), "50".to_string()]));
        assert!(tags.contains(&vec![
            "mint".to_string(),
            "https://mint.example".to_string()
        ]));
        assert!(tags.contains(&vec!["unit".to_string(), "sat".to_string()]));
        assert!(tags.contains(&vec!["change".to_string(), "cashu-change".to_string()]));
    }

    #[test]
    fn result_event_omits_change_tag_when_no_refund_is_due() {
        let worker = identity(0);
        let requester = Keys::generate();
        let request = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), valid_payload())
            .tags([tag(["p", worker.pubkey.as_str()]).unwrap()])
            .sign_with_keys(&requester)
            .unwrap();
        let mut response = WorkerJobResponse {
            status: JobStatus::Success,
            exit_code: Some(0),
            elapsed_seconds: 5,
            log_tail: "ok".to_string(),
            billing: None,
            result_errors: Vec::new(),
        };
        response.with_billing(JobBilling {
            duration: 5,
            billable_duration: 5,
            cost: 50,
            mint: "https://mint.example".to_string(),
            unit: "sat".to_string(),
            change: None,
        });

        let result = build_result_event(&worker, &request, &response).unwrap();
        let tags: Vec<Vec<String>> = result
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();

        assert!(tags.contains(&vec!["cost".to_string(), "50".to_string()]));
        assert!(!tags
            .iter()
            .any(|tag| tag.first().is_some_and(|name| name == "change")));
    }

    #[test]
    fn decodes_encrypted_job_for_addressed_worker() {
        let worker = identity(0);
        let requester = Keys::generate();
        let encrypted = nip04::encrypt(
            requester.secret_key(),
            &worker.keys.public_key(),
            valid_payload(),
        )
        .unwrap();
        let event = EventBuilder::new(Kind::Custom(JOB_REQUEST_KIND), encrypted)
            .tags([tag(["p", worker.pubkey.as_str()]).unwrap()])
            .sign_with_keys(&requester)
            .unwrap();

        let job = decode_job_event(&worker, &event).unwrap();

        assert_eq!(job.request_event_id, event.id.to_hex());
        assert_eq!(job.workflow, ".github/workflows/ci.yml");
        assert_eq!(job.job, "test");
    }
}
