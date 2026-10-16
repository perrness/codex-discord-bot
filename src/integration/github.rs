use super::{Integration, STATUS_CONTEXT};
use crate::github::GitHubRepository;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub(super) fn auth_environment(command: &mut Command) {
    for key in [
        "PATH",
        "HOME",
        "GH_CONFIG_DIR",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat");
}

async fn bounded(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(bytes);
        }
        ensure!(
            bytes.len() + count <= limit,
            "Integration CLI output exceeded limit"
        );
        bytes.extend_from_slice(&buffer[..count]);
    }
}

pub(super) async fn output(mut command: Command, input: Option<Value>) -> Result<Vec<u8>> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(45), async {
        let mut child = command.spawn().context("Start integration Git/gh CLI")?;
        let stdout = child.stdout.take().context("Missing stdout")?;
        let stderr = child.stderr.take().context("Missing stderr")?;
        let stdin = child.stdin.take();
        let (out, err, _, status) = tokio::try_join!(
            bounded(stdout, 2_000_000),
            bounded(stderr, 32_000),
            async {
                if let (Some(mut stdin), Some(input)) = (stdin, input) {
                    stdin.write_all(&serde_json::to_vec(&input)?).await?;
                    stdin.shutdown().await?;
                }
                Ok::<_, anyhow::Error>(())
            },
            async { Ok::<_, anyhow::Error>(child.wait().await?) }
        )?;
        ensure!(
            status.success(),
            "Integration Git/gh failed (exit {:?}): {}",
            status.code(),
            String::from_utf8_lossy(&err)
        );
        Ok(out)
    })
    .await
    .context("Integration Git/gh timed out; reconcile before retrying")?
}

pub(crate) struct GitHub {
    repository: GitHubRepository,
    executable: PathBuf,
}

impl GitHub {
    pub fn new(repository: &str) -> Result<Self> {
        Ok(Self {
            repository: GitHubRepository::parse(repository)?,
            executable: "gh".into(),
        })
    }
    pub fn remote(&self) -> String {
        format!("https://github.com/{}.git", self.repository.as_str())
    }

    pub async fn api(&self, method: &str, suffix: &str, body: Option<Value>) -> Result<Value> {
        let mut command = Command::new(&self.executable);
        command.env_clear().current_dir("/");
        auth_environment(&mut command);
        command
            .args([
                "api",
                "--hostname",
                "github.com",
                "--method",
                method,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2026-03-10",
            ])
            .arg(format!("repos/{}{}", self.repository.as_str(), suffix));
        if body.is_some() {
            command.args(["--input", "-"]);
        }
        serde_json::from_slice(&output(command, body).await?)
            .context("Invalid integration GitHub JSON")
    }

    pub async fn default_branch(&self) -> Result<String> {
        let repo = self.api("GET", "", None).await?;
        ensure!(
            repo["allow_merge_commit"] == true,
            "Repository must permit merge commits for exact-tree integration"
        );
        Ok(repo["default_branch"]
            .as_str()
            .context("Missing GitHub default branch")?
            .to_owned())
    }

    pub async fn protection(&self, branch: &str) -> Result<()> {
        // This endpoint includes strict, unlike the abbreviated branch response.
        let rules = self
            .api(
                "GET",
                &format!(
                    "/branches/{}/protection/required_status_checks",
                    encode(branch)
                ),
                None,
            )
            .await?;
        require_strict(&rules)?;
        let admins = self
            .api(
                "GET",
                &format!("/branches/{}/protection/enforce_admins", encode(branch)),
                None,
            )
            .await?;
        ensure!(
            admins["enabled"] == true,
            "Automatic integration requires branch protection to include administrators"
        );
        Ok(())
    }

    pub async fn branch_sha(&self, branch: &str) -> Result<String> {
        let value = self
            .api("GET", &format!("/git/ref/heads/{}", encode(branch)), None)
            .await?;
        Ok(value["object"]["sha"]
            .as_str()
            .context("Missing GitHub branch SHA")?
            .to_owned())
    }

    pub async fn fresh(&self, integration: &Integration) -> Result<()> {
        ensure!(
            self.default_branch().await? == integration.default_branch()?,
            "Default branch changed; validation is stale"
        );
        ensure!(
            self.branch_sha(integration.default_branch()?).await? == integration.base()?,
            "Default branch advanced; validation is stale"
        );
        self.protection(integration.default_branch()?).await
    }

    pub async fn find_pr(&self, integration: &Integration) -> Result<Option<Value>> {
        let owner = self.repository.as_str().split('/').next().unwrap();
        let prs = self
            .api(
                "GET",
                &format!(
                    "/pulls?state=all&head={}&per_page=100",
                    encode(&format!("{owner}:{}", integration.branch))
                ),
                None,
            )
            .await?;
        let prs = prs.as_array().context("Invalid PR list")?;
        ensure!(
            prs.len() <= 1,
            "Ambiguous integration PRs; inspect manually"
        );
        if let Some(pr) = prs.first() {
            self.identity(pr, integration)?;
            return self
                .pr(pr["number"].as_i64().context("Missing PR number")?)
                .await
                .map(Some);
        }
        Ok(None)
    }

    pub async fn pr(&self, number: i64) -> Result<Value> {
        self.api("GET", &format!("/pulls/{number}"), None).await
    }

    pub async fn recovery_pr(&self, i: &Integration) -> Result<Option<Value>> {
        match i.pr_number {
            Some(number) => self.pr(number).await.map(Some),
            None => self.find_pr(i).await,
        }
    }

    pub fn identity(&self, pr: &Value, i: &Integration) -> Result<()> {
        ensure!(
            pr["head"]["repo"]["full_name"].as_str() == Some(self.repository.as_str())
                && i.pr_number
                    .is_none_or(|number| pr["number"].as_i64() == Some(number))
                && pr["base"]["repo"]["full_name"].as_str() == Some(self.repository.as_str())
                && pr["head"]["ref"].as_str() == Some(i.branch.as_str())
                && pr["head"]["sha"].as_str() == Some(i.candidate()?)
                && pr["base"]["ref"].as_str() == Some(i.default_branch()?),
            "PR identity or head changed; validation is stale"
        );
        Ok(())
    }

    pub async fn create_pr(&self, i: &Integration, issue: i64) -> Result<Value> {
        self.api("POST", "/pulls", Some(json!({
            "title": format!("Integrate issue #{issue}"), "head": i.branch,
            "base": i.default_branch()?, "maintainer_can_modify": false,
            "body": format!("Issue #{issue}\n\nIntegration run: {}\nValidated candidate: {}\nBase: {}\n\nRelease remains gated and is tracked separately.", i.run_id, i.candidate()?, i.base()?)
        }))).await
    }

    pub async fn create_or_reconcile_pr(&self, i: &Integration, issue: i64) -> Result<Value> {
        match self.create_pr(i, issue).await {
            Ok(pr) => Ok(pr),
            Err(error) => self
                .find_pr(i)
                .await?
                .context(format!("PR creation uncertain: {error:#}")),
        }
    }

    pub async fn status(&self, i: &Integration) -> Result<()> {
        i.validated()?;
        self.api("POST", &format!("/statuses/{}", i.candidate()?), Some(json!({"state":"success","context":STATUS_CONTEXT,"description":"Trusted build, test and lint passed for this integration candidate"}))).await?;
        Ok(())
    }

    pub async fn merge(&self, i: &Integration) -> Result<Value> {
        i.validated()?;
        self.api(
            "PUT",
            &format!("/pulls/{}/merge-async", i.pr_number.context("Missing PR")?),
            Some(merge_body(i.candidate()?)),
        )
        .await
    }

    pub async fn verify_merge(&self, i: &Integration, pr: &Value) -> Result<String> {
        self.identity(pr, i)?;
        i.validated()?;
        ensure!(pr["merged"] == true, "PR is not confirmed merged");
        let sha = pr["merge_commit_sha"]
            .as_str()
            .context("Missing merge SHA")?;
        let commit = self
            .api("GET", &format!("/git/commits/{}", encode(sha)), None)
            .await?;
        verify_commit(i, &commit)?;
        Ok(sha.to_owned())
    }
}

fn merge_body(sha: &str) -> Value {
    json!({"sha":sha,"merge_method":"merge","merge_action":"direct_merge","bypass_rules":false})
}

fn require_strict(rules: &Value) -> Result<()> {
    let required = rules["contexts"]
        .as_array()
        .is_some_and(|v| v.iter().any(|c| c == STATUS_CONTEXT))
        || rules["checks"]
            .as_array()
            .is_some_and(|v| v.iter().any(|c| c["context"] == STATUS_CONTEXT));
    ensure!(
        rules["strict"] == true && required,
        "Automatic integration requires strict up-to-date branch protection and the required codex/integration status"
    );
    Ok(())
}

fn verify_commit(i: &Integration, commit: &Value) -> Result<()> {
    let parents = commit["parents"]
        .as_array()
        .context("Missing merge parents")?;
    ensure!(
        commit["tree"]["sha"].as_str() == i.candidate_tree.as_deref()
            && i.candidate_tree.is_some()
            && parents.len() == 2
            && parents[0]["sha"].as_str() == Some(i.base()?)
            && parents[1]["sha"].as_str() == Some(i.candidate()?),
        "Merged commit differs from the validated candidate/base; release blocked"
    );
    Ok(())
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, GitHub, Integration) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let i = Integration {
            run_id: uuid::Uuid::new_v4(), repository_id: uuid::Uuid::new_v4(), validation_id: uuid::Uuid::new_v4(),
            policy: json!({}), validation_policy: json!({}), state: "merging".into(),
            branch: "codex/integration-test".into(), worktree: "/unused".into(),
            default_branch: Some("main".into()), source_sha: Some("a".repeat(40)),
            base_sha: Some("b".repeat(40)), candidate_sha: Some("c".repeat(40)), candidate_tree: Some("d".repeat(40)),
            results: json!(["build","test","lint"].map(|name| json!({"name":name,"outcome":"passed","exit_code":0,"duration_ms":1,"stdout":"","stderr":""}))),
            validated_at: Some(chrono::Utc::now()), pr_number: Some(7), pr_url: None,
            merge_request_id: None, operation: Some("merge".into()), merge_sha: None, retry_safe: false,
        };
        let pr = json!({"number":7,"state":"open","merged":false,
            "head":{"repo":{"full_name":"owner/repo"},"ref":i.branch,"sha":i.candidate_sha},
            "base":{"repo":{"full_name":"owner/repo"},"ref":"main","sha":i.base_sha}});
        let mut merged = pr.clone();
        merged["merged"] = true.into();
        merged["state"] = "closed".into();
        merged["merge_commit_sha"] = "e".repeat(40).into();
        for (name, data) in [
            (
                "repository",
                json!({"default_branch":"main","allow_merge_commit":true}),
            ),
            (
                "protection",
                json!({"strict":true,"contexts":[STATUS_CONTEXT]}),
            ),
            ("admins", json!({"enabled":true})),
            ("base", json!({"object":{"sha":i.base_sha}})),
            ("head", json!({"object":{"sha":i.candidate_sha}})),
            ("list", json!([])),
            ("created-list", json!([pr])),
            ("pr", pr.clone()),
            ("created-pr", pr),
            ("merged-pr", merged),
            (
                "commit",
                json!({"tree":{"sha":i.candidate_tree},"parents":[{"sha":i.base_sha},{"sha":i.candidate_sha}]}),
            ),
        ] {
            std::fs::write(dir.path().join(format!("{name}.json")), data.to_string()).unwrap();
        }
        let executable = dir.path().join("gh");
        std::fs::write(&executable, r#"#!/bin/sh
root=$(dirname "$0")
for arg in "$@"; do case "$arg" in repos/*) endpoint=$arg ;; esac; done
printf '%s %s\n' "$5" "$endpoint" >> "$root/requests"
if [ "$5" != GET ]; then cat > "$root/last-input.json"; fi
case "$endpoint" in
repos/owner/repo) cat "$root/repository.json" ;;
*/required_status_checks) cat "$root/protection.json" ;;
*/enforce_admins) cat "$root/admins.json" ;;
*/git/ref/heads/main) cat "$root/base.json" ;;
*/git/ref/heads/*) cat "$root/head.json" ;;
*/pulls\?*) cat "$root/list.json" ;;
*/pulls) cp "$root/created-list.json" "$root/list.json"; cp "$root/created-pr.json" "$root/pr.json"; exit 1 ;;
*/pulls/7/merge-async) cp "$root/merged-pr.json" "$root/pr.json"; exit 1 ;;
*/pulls/7) cat "$root/pr.json" ;;
*/git/commits/*) cat "$root/commit.json" ;;
*) exit 9 ;;
esac
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut gh = GitHub::new("owner/repo").unwrap();
        gh.executable = executable;
        (dir, gh, i)
    }

    #[tokio::test]
    async fn lost_create_and_merge_responses_are_reconciled_without_repeating_mutations() {
        let (dir, gh, i) = fixture();
        let pr = gh.create_or_reconcile_pr(&i, 6).await.unwrap();
        assert_eq!(pr["number"], 7);
        assert!(gh.merge(&i).await.is_err());
        // Restart/retry reconciliation only reads the durable PR identity.
        for _ in 0..2 {
            let pr = gh.recovery_pr(&i).await.unwrap().unwrap();
            assert_eq!(gh.verify_merge(&i, &pr).await.unwrap(), "e".repeat(40));
        }
        let calls = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert_eq!(calls.lines().filter(|l| l.starts_with("POST ")).count(), 1);
        assert_eq!(calls.lines().filter(|l| l.starts_with("PUT ")).count(), 1);
        let body: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("last-input.json")).unwrap())
                .unwrap();
        assert_eq!(body["sha"], i.candidate().unwrap());
        assert_eq!(body["bypass_rules"], false);
    }

    #[tokio::test]
    async fn restart_observes_absent_open_closed_and_merged_prs_using_only_reads() {
        let (dir, gh, mut i) = fixture();
        i.pr_number = None;
        for operation in ["push", "create_pr", "merge"] {
            i.operation = Some(operation.into());
            assert!(gh.recovery_pr(&i).await.unwrap().is_none());
        }
        std::fs::copy(
            dir.path().join("created-list.json"),
            dir.path().join("list.json"),
        )
        .unwrap();
        let pr = gh.recovery_pr(&i).await.unwrap().unwrap();
        gh.identity(&pr, &i).unwrap();
        assert_eq!(pr["state"], "open");
        i.pr_number = Some(7);
        let mut closed = pr;
        closed["state"] = "closed".into();
        std::fs::write(dir.path().join("pr.json"), closed.to_string()).unwrap();
        assert_eq!(gh.recovery_pr(&i).await.unwrap().unwrap()["merged"], false);
        std::fs::copy(
            dir.path().join("merged-pr.json"),
            dir.path().join("pr.json"),
        )
        .unwrap();
        for _ in 0..2 {
            let pr = gh.recovery_pr(&i).await.unwrap().unwrap();
            assert_eq!(gh.verify_merge(&i, &pr).await.unwrap(), "e".repeat(40));
        }
        std::fs::remove_file(dir.path().join("pr.json")).unwrap();
        assert!(gh.recovery_pr(&i).await.is_err());
        let calls = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert!(calls.lines().all(|line| line.starts_with("GET ")));
    }

    #[tokio::test]
    async fn remote_advancement_and_changed_protection_invalidate_the_candidate() {
        let (dir, gh, i) = fixture();
        gh.fresh(&i).await.unwrap();
        std::fs::write(
            dir.path().join("base.json"),
            json!({"object":{"sha":"f".repeat(40)}}).to_string(),
        )
        .unwrap();
        assert!(
            gh.fresh(&i)
                .await
                .unwrap_err()
                .to_string()
                .contains("advanced")
        );
        std::fs::write(
            dir.path().join("base.json"),
            json!({"object":{"sha":i.base_sha}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("admins.json"),
            json!({"enabled":false}).to_string(),
        )
        .unwrap();
        assert!(gh.fresh(&i).await.is_err());
        let calls = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert!(calls.lines().all(|l| l.starts_with("GET ")));
    }

    #[tokio::test]
    async fn unvalidated_or_different_merge_trees_parents_and_heads_never_authorize_release() {
        let (dir, gh, mut i) = fixture();
        let mut pr: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("merged-pr.json")).unwrap())
                .unwrap();
        gh.verify_merge(&i, &pr).await.unwrap();
        pr["head"]["sha"] = "f".repeat(40).into();
        assert!(gh.verify_merge(&i, &pr).await.is_err());
        pr["head"]["sha"] = i.candidate().unwrap().into();
        for commit in [
            json!({"tree":{"sha":"wrong"},"parents":[{"sha":i.base_sha},{"sha":i.candidate_sha}]}),
            json!({"tree":{"sha":i.candidate_tree},"parents":[{"sha":"advanced-base"},{"sha":i.candidate_sha}]}),
            json!({"tree":{"sha":i.candidate_tree},"parents":[{"sha":i.base_sha}]}),
        ] {
            std::fs::write(dir.path().join("commit.json"), commit.to_string()).unwrap();
            assert!(gh.verify_merge(&i, &pr).await.is_err());
        }
        i.results = json!([]);
        assert!(gh.merge(&i).await.is_err());
        assert!(gh.status(&i).await.is_err());
        let calls = std::fs::read_to_string(dir.path().join("requests")).unwrap();
        assert!(calls.lines().all(|l| l.starts_with("GET ")));
    }

    #[test]
    fn strict_checks_close_the_base_advancement_race_and_merge_never_bypasses() {
        for rules in [
            json!({}),
            json!({"strict":false,"contexts":[STATUS_CONTEXT]}),
            json!({"strict":true,"contexts":["other"]}),
        ] {
            assert!(require_strict(&rules).is_err());
        }
        require_strict(&json!({"strict":true,"contexts":[STATUS_CONTEXT]})).unwrap();
        require_strict(&json!({"strict":true,"checks":[{"context":STATUS_CONTEXT}]})).unwrap();
        assert_eq!(
            merge_body("head"),
            json!({"sha":"head","merge_method":"merge","merge_action":"direct_merge","bypass_rules":false})
        );
        assert_eq!(encode("release/main#?"), "release%2Fmain%23%3F");
    }

    #[tokio::test]
    async fn cli_uses_structured_input_and_fails_on_invalid_output() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("gh");
        std::fs::write(&executable, "#!/bin/sh\ncat\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut gh = GitHub::new("owner/repo").unwrap();
        gh.executable = executable.clone();
        let body = json!({"body":"literal `data` and $(data)\nsecond line"});
        assert_eq!(
            gh.api("POST", "/pulls", Some(body.clone())).await.unwrap(),
            body
        );
        std::fs::write(&executable, "#!/bin/sh\necho invalid\n").unwrap();
        assert!(gh.api("GET", "", None).await.is_err());
    }
}
