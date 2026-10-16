//! Operator-owned GitHub Actions dispatch and independent deployment observation.
//! Production credentials belong to the remote workflow's protected environment.
mod controller;
#[cfg(test)]
mod tests;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
pub use controller::{Controller, run_service};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, io::Read, path::Path, process::Stdio};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    pub repositories: BTreeMap<String, Policy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub github_repository: String,
    pub workflow_id: i64,
    /// An operator-protected branch/tag containing trusted deployment code.
    pub workflow_ref: String,
    pub workflow_sha: String,
    pub deployment_timeout_seconds: u32,
    pub health_timeout_seconds: u32,
    pub health_checks: Vec<HealthCheck>,
    /// Operator attestation: a successful disposable deployment with this policy.
    pub disposable_test_run: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    pub url: String,
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResult {
    pub url: String,
    pub healthy: bool,
    pub revision: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Release {
    pub run_id: Uuid,
    pub github_repository: String,
    pub expected_sha: String,
    pub policy: Value,
    pub state: String,
    pub workflow_run_id: Option<i64>,
    pub workflow_observation: Value,
    pub health_results: Value,
    pub deployed_revision: Option<String>,
    pub deployment_deadline: DateTime<Utc>,
    pub health_deadline: Option<DateTime<Utc>>,
    pub verified_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl Policies {
    pub fn load(path: &Path) -> Result<Self> {
        crate::validation::trusted_path(path)?;
        let file = File::open(path).context("Open RELEASE_CONFIG")?;
        ensure!(file.metadata()?.is_file(), "Release config must be a file");
        let mut data = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
        ensure!(data.len() <= 1024 * 1024, "Release config is too large");
        let policies: Self = serde_json::from_slice(&data).context("Invalid RELEASE_CONFIG")?;
        for (name, policy) in &policies.repositories {
            crate::repositories::validate_name(name)?;
            policy.validate()?;
        }
        if !policies.repositories.is_empty() {
            crate::validation::trusted_path(Path::new("/usr/bin/gh"))?;
            crate::validation::trusted_path(Path::new("/usr/bin/curl"))?;
        }
        Ok(policies)
    }

    pub fn validate_gates(&self, integrations: &crate::integration::Policies) -> Result<()> {
        for (name, policy) in &self.repositories {
            let integration = integrations
                .repositories
                .get(name)
                .context("Release requires a trusted integration policy")?;
            ensure!(
                integration.release_gate == crate::integration::ReleaseGate::DispatchOnly
                    && integration.github_repository == policy.github_repository,
                "Release requires a matching dispatch_only integration gate"
            );
        }
        Ok(())
    }
}

pub(crate) fn is_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        crate::github::GitHubRepository::parse(&self.github_repository)?;
        ensure!(
            self.workflow_id > 0 && is_sha(&self.workflow_sha),
            "Pin a workflow ID and full commit SHA"
        );
        ensure!(
            !self.workflow_ref.is_empty()
                && self.workflow_ref.len() <= 255
                && self
                    .workflow_ref
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_/.".contains(&b))
                && !self.workflow_ref.contains("..")
                && !self.workflow_ref.contains("//")
                && !self.workflow_ref.starts_with(['-', '/', '.'])
                && !self.workflow_ref.ends_with(['/', '.']),
            "Use a simple protected workflow branch/tag name"
        );
        ensure!(
            (1..=7200).contains(&self.deployment_timeout_seconds)
                && (1..=3600).contains(&self.health_timeout_seconds),
            "Release timeouts are out of bounds"
        );
        ensure!(
            !self.health_checks.is_empty() && self.health_checks.len() <= 8,
            "Configure 1–8 independent revision health checks"
        );
        let mut urls = std::collections::HashSet::new();
        for check in &self.health_checks {
            let host = check
                .url
                .strip_prefix("https://")
                .and_then(|s| s.split('/').next());
            ensure!(
                host.is_some_and(|s| !s.is_empty() && !s.starts_with(':'))
                    && check.url.len() <= 2048
                    && !check.url.bytes().any(|b| b.is_ascii_control()
                        || b.is_ascii_whitespace()
                        || b"@#\\".contains(&b))
                    && urls.insert(&check.url),
                "Health URLs must be distinct HTTPS URLs without credentials or fragments"
            );
            ensure!(
                (1..=30).contains(&check.timeout_seconds),
                "Health check timeout must be 1–30 seconds"
            );
        }
        let prefix = format!(
            "https://github.com/{}/actions/runs/",
            self.github_repository
        );
        ensure!(
            self.disposable_test_run
                .strip_prefix(&prefix)
                .and_then(|id| id.parse::<u64>().ok())
                .is_some_and(|id| id > 0),
            "Record the successful disposable workflow run before enabling releases"
        );
        Ok(())
    }

    pub fn run_title(&self, release: &Release) -> String {
        format!("codex-release:{}:{}", release.run_id, release.expected_sha)
    }

    pub fn verify_run(&self, release: &Release, run: &Value) -> Result<()> {
        ensure!(
            run["id"].as_i64().is_some_and(|id| id > 0)
                && release.workflow_run_id.is_none_or(|id| run["id"] == id)
                && run["workflow_id"] == self.workflow_id
                && run["repository"]["full_name"] == self.github_repository
                && run["head_repository"]["full_name"] == self.github_repository
                && run["event"] == "workflow_dispatch"
                && run["run_attempt"] == 1
                && run["head_sha"] == self.workflow_sha
                && run["display_title"] == self.run_title(release),
            "Workflow identity, source, attempt or release correlation differs from the trusted dispatch"
        );
        Ok(())
    }

    pub fn verify_health(&self, revision: &str, results: &[HealthResult]) -> Result<()> {
        ensure!(
            is_sha(revision)
                && results.len() == self.health_checks.len()
                && !results.is_empty()
                && self
                    .health_checks
                    .iter()
                    .zip(results)
                    .all(|(check, result)| check.url == result.url
                        && result.healthy
                        && result.error.is_none()
                        && result.revision.as_deref() == Some(revision)),
            "Every configured health check must confirm the exact deployed revision"
        );
        Ok(())
    }
}

pub(crate) struct Actions {
    github: crate::integration::github::GitHub,
}

impl Actions {
    pub fn new(repository: &str) -> Result<Self> {
        Ok(Self {
            github: crate::integration::github::GitHub::release_client(repository)?,
        })
    }

    /// Called before persisting dispatch intent. The tag/branch must be protected
    /// from changes by workers; run identity is checked again after dispatch.
    pub async fn preflight(&self, policy: &Policy) -> Result<()> {
        let workflow = self
            .github
            .api(
                "GET",
                &format!("/actions/workflows/{}", policy.workflow_id),
                None,
            )
            .await?;
        ensure!(
            workflow["id"] == policy.workflow_id && workflow["state"] == "active",
            "Configured release workflow is not active"
        );
        let commit = self
            .github
            .api(
                "GET",
                &format!(
                    "/commits/{}",
                    crate::integration::github::encode(&policy.workflow_ref)
                ),
                None,
            )
            .await?;
        ensure!(
            commit["sha"] == policy.workflow_sha,
            "Trusted workflow ref moved; release blocked"
        );
        let test_id = policy
            .disposable_test_run
            .rsplit('/')
            .next()
            .context("Missing disposable run")?;
        let tested = self
            .github
            .api("GET", &format!("/actions/runs/{test_id}"), None)
            .await?;
        ensure!(
            tested["workflow_id"] == policy.workflow_id
                && tested["head_sha"] == policy.workflow_sha
                && tested["repository"]["full_name"] == policy.github_repository
                && tested["event"] == "workflow_dispatch"
                && tested["status"] == "completed"
                && tested["conclusion"] == "success",
            "Disposable test must have succeeded using this exact trusted workflow revision"
        );
        Ok(())
    }

    pub async fn dispatch(&self, release: &Release, policy: &Policy) -> Result<i64> {
        let response = self.github.api("POST", &format!("/actions/workflows/{}/dispatches", policy.workflow_id),
            Some(json!({"ref":policy.workflow_ref,"inputs":{"revision":release.expected_sha,"release_id":release.run_id.to_string()}}))).await?;
        response["workflow_run_id"]
            .as_i64()
            .filter(|id| *id > 0)
            .context("Dispatch response has no workflow run ID; reconcile without redispatching")
    }

    pub async fn observe(&self, release: &Release, policy: &Policy) -> Result<Option<Value>> {
        if let Some(id) = release.workflow_run_id {
            let run = self
                .github
                .api("GET", &format!("/actions/runs/{id}"), None)
                .await?;
            policy.verify_run(release, &run)?;
            return Ok(Some(run));
        }
        // Lost dispatch responses are correlated by an unguessable durable run
        // UUID AND exact revision, never by "latest run" or the moving main SHA.
        let mut found = None;
        for page in 1..=100 {
            let response = self.github.api("GET", &format!("/actions/workflows/{}/runs?event=workflow_dispatch&per_page=100&page={page}", policy.workflow_id), None).await?;
            let runs = response["workflow_runs"]
                .as_array()
                .context("Invalid workflow run list")?;
            for run in runs {
                if run["display_title"] == policy.run_title(release) {
                    policy.verify_run(release, run)?;
                    ensure!(
                        found.is_none(),
                        "Multiple workflows claim the release; inspect manually"
                    );
                    found = Some(run.clone());
                }
            }
            if runs.len() < 100 {
                return Ok(found);
            }
        }
        anyhow::bail!("Release run discovery exceeded pagination limit")
    }
}

pub async fn check_health(check: &HealthCheck) -> HealthResult {
    let result = fetch_health(check).await;
    match result {
        Ok(value) => HealthResult {
            url: check.url.clone(),
            healthy: value["healthy"] == true,
            revision: value["revision"].as_str().map(str::to_owned),
            error: None,
        },
        Err(error) => HealthResult {
            url: check.url.clone(),
            healthy: false,
            revision: None,
            error: Some(format!("{error:#}")),
        },
    }
}

async fn fetch_health(check: &HealthCheck) -> Result<Value> {
    let mut command = Command::new("/usr/bin/curl");
    command
        .env_clear()
        .current_dir("/")
        .args([
            "--disable",
            "--silent",
            "--fail",
            "--noproxy",
            "*",
            "--proto",
            "=https,http",
            "--connect-timeout",
            "5",
            "--max-filesize",
            "16384",
            "--header",
            "Cache-Control: no-cache",
            "--max-time",
            &check.timeout_seconds.to_string(),
            "--write-out",
            "\n%{http_code}",
            "--url",
            &check.url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // No curl config, credentials, proxy environment, redirects, shell or
    // worktree executable. The policy loader requires HTTPS; HTTP is used only
    // by the disposable loopback test through this internal implementation.
    tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(check.timeout_seconds) + 1),
        async {
            let mut child = command.spawn().context("Start release health check")?;
            let mut output = Vec::new();
            child
                .stdout
                .take()
                .context("Missing health stdout")?
                .take(16385)
                .read_to_end(&mut output)
                .await?;
            ensure!(output.len() <= 16384, "Health response is too large");
            ensure!(
                child.wait().await?.success(),
                "Health endpoint request failed"
            );
            let split = output
                .iter()
                .rposition(|b| *b == b'\n')
                .context("Missing health HTTP status")?;
            let status = std::str::from_utf8(&output[split + 1..])?.parse::<u16>()?;
            ensure!(
                (200..300).contains(&status),
                "Health endpoint must return a successful HTTP status"
            );
            serde_json::from_slice(&output[..split]).context("Invalid health JSON")
        },
    )
    .await
    .context("Health endpoint timed out")?
}
