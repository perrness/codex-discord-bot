//! Trusted integration policy and immutable Git candidates.
pub(crate) mod git;
pub(crate) mod github;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};
use uuid::Uuid;

pub const STATUS_CONTEXT: &str = "codex/integration";

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policies {
    pub repositories: BTreeMap<String, Policy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub github_repository: String,
    pub release_gate: ReleaseGate,
    pub protected_paths: Vec<String>,
    pub checks_timeout_seconds: u64,
}

/// Operator assertion about all deployment entry points, including external
/// webhooks. This cannot be inferred from GitHub Actions YAML alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseGate {
    DeploymentsDisabled,
    DispatchOnly,
}

impl Policies {
    pub fn load(path: &Path) -> Result<Self> {
        crate::validation::trusted_path(path)?;
        let file = File::open(path).context("Open INTEGRATION_CONFIG")?;
        ensure!(
            file.metadata()?.is_file(),
            "Integration config must be a file"
        );
        let mut data = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
        ensure!(data.len() <= 1024 * 1024, "Integration config is too large");
        let policies: Self = serde_json::from_slice(&data).context("Invalid INTEGRATION_CONFIG")?;
        for (name, policy) in &policies.repositories {
            crate::repositories::validate_name(name)?;
            policy.validate()?;
        }
        Ok(policies)
    }
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        crate::github::GitHubRepository::parse(&self.github_repository)?;
        ensure!(
            (1..=3600).contains(&self.checks_timeout_seconds),
            "Integration check timeout must be 1–3600 seconds"
        );
        for path in &self.protected_paths {
            ensure!(
                !path.is_empty()
                    && Path::new(path)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "Protected paths must be relative file or directory paths"
            );
        }
        Ok(())
    }

    pub fn protects(&self, path: &Path) -> bool {
        path.starts_with(".github") || self.protected_paths.iter().any(|p| path.starts_with(p))
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Integration {
    pub run_id: Uuid,
    pub repository_id: Uuid,
    pub validation_id: Uuid,
    pub policy: Value,
    pub validation_policy: Value,
    pub state: String,
    pub branch: String,
    pub worktree: String,
    pub default_branch: Option<String>,
    pub source_sha: Option<String>,
    pub base_sha: Option<String>,
    pub candidate_sha: Option<String>,
    pub candidate_tree: Option<String>,
    pub results: Value,
    pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub merge_request_id: Option<Uuid>,
    pub operation: Option<String>,
    pub merge_sha: Option<String>,
    pub retry_safe: bool,
}

impl Integration {
    pub fn candidate(&self) -> Result<&str> {
        self.candidate_sha
            .as_deref()
            .context("Integration candidate missing")
    }
    pub fn base(&self) -> Result<&str> {
        self.base_sha.as_deref().context("Integration base missing")
    }
    pub fn default_branch(&self) -> Result<&str> {
        self.default_branch
            .as_deref()
            .context("Default branch missing")
    }
    pub fn validated(&self) -> Result<()> {
        let results: Vec<crate::validation::CheckResult> =
            serde_json::from_value(self.results.clone())?;
        ensure!(
            self.validated_at.is_some()
                && crate::validation::decide(&results, 0, 0) == crate::validation::Decision::Passed,
            "Integration candidate has no successful validation"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_gate_is_explicit_and_workflows_are_always_protected() {
        let mut value = serde_json::json!({"github_repository":"owner/repo","protected_paths":["deploy"],"checks_timeout_seconds":60});
        assert!(serde_json::from_value::<Policy>(value.clone()).is_err());
        value["release_gate"] = "push".into();
        assert!(serde_json::from_value::<Policy>(value.clone()).is_err());
        value["release_gate"] = "dispatch_only".into();
        let p: Policy = serde_json::from_value(value).unwrap();
        p.validate().unwrap();
        assert!(p.protects(Path::new(".github/workflows/release.yml")));
        assert!(p.protects(Path::new("deploy/compose.yaml")));
        assert!(!p.protects(Path::new("deployment.rs")));
        let mut invalid = p;
        invalid.protected_paths = vec!["../deploy".into()];
        assert!(invalid.validate().is_err());
    }
}
