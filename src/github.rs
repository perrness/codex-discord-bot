//! Read-only GitHub discovery. Issue bodies are discarded by gh before parsing.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepository(String);

impl GitHubRepository {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('/').collect();
        ensure!(parts.len() == 2, "Use a GitHub owner/repository mapping");
        ensure!(
            parts[0].len() <= 39 && parts[1].len() <= 100,
            "Invalid GitHub mapping"
        );
        for (i, part) in parts.iter().enumerate() {
            ensure!(
                !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && (i == 1
                        || (part.as_bytes()[0].is_ascii_alphanumeric()
                            && !part.ends_with('-')
                            && !part.contains("--")))
                    && part.bytes().all(|c| c.is_ascii_alphanumeric()
                        || c == b'-'
                        || (i == 1 && (c == b'_' || c == b'.'))),
                "Invalid GitHub mapping"
            );
        }
        Ok(Self(value.to_owned()))
    }

    pub fn from_remote(remote: &str) -> Result<Self> {
        let path = remote.trim().strip_prefix("https://github.com/")
            .or_else(|| remote.trim().strip_prefix("git@github.com:"))
            .or_else(|| remote.trim().strip_prefix("ssh://git@github.com/"))
            .context("Origin must be an HTTPS or SSH github.com remote; configure /repo add github:owner/repository")?;
        Self::parse(path.strip_suffix(".git").unwrap_or(path))
    }
}

pub async fn origin(path: &Path) -> Result<GitHubRepository> {
    let mut command = crate::repositories::git(path);
    command.args(["config", "--local", "--get", "remote.origin.url"]);
    let output = run(command, Duration::from_secs(10)).await?;
    GitHubRepository::from_remote(
        std::str::from_utf8(&output).context("Invalid Git remote encoding")?,
    )
}

// Bound both memory use and elapsed time; dropping a child kills timed-out commands.
async fn run(mut command: Command, timeout: Duration) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(timeout, async {
        let mut child = command.spawn().context("Cannot start Git/GitHub CLI; install gh and authenticate as the service account")?;
        let mut bytes = Vec::new();
        child.stdout.take().context("Missing CLI output")?.take(2_000_001).read_to_end(&mut bytes).await?;
        ensure!(bytes.len() <= 2_000_000, "GitHub CLI output exceeded safety limit");
        let status = child.wait().await?;
        ensure!(status.success(), "Git/GitHub CLI failed (exit {:?}); check gh auth status, repository access and API rate limits; retry later", status.code());
        Ok(bytes)
    }).await.context("Git/GitHub discovery timed out; retry later")?
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    state: String,
    labels: Vec<Label>,
    assignees: Vec<serde_json::Value>,
    pull_request: Option<serde_json::Value>,
}
#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

impl Issue {
    fn ready(&self) -> bool {
        self.number > 0
            && self.state == "open"
            && self.pull_request.is_none()
            && self.assignees.is_empty()
            && self.labels.iter().any(|l| l.name == "codex-ready")
            && !self.labels.iter().any(|l| l.name == "codex-claimed")
    }
}

pub async fn list(repository: &GitHubRepository) -> Result<Vec<Issue>> {
    list_with(repository, "gh").await
}

async fn list_with(repository: &GitHubRepository, executable: &str) -> Result<Vec<Issue>> {
    tokio::time::timeout(Duration::from_secs(45), async {
        let mut issues = Vec::new();
        for page in 1..=100 {
            let mut command = Command::new(executable);
            command
                .env("GH_PROMPT_DISABLED", "1")
                .env("GH_PAGER", "cat")
                .args([
                    "api",
                    "--hostname",
                    "github.com",
                    "--method",
                    "GET",
                    "-H",
                    "Accept: application/vnd.github+json",
                    "--jq",
                    "map({number,title,state,labels,assignees,pull_request})",
                ])
                .arg(format!(
                    "repos/{}/issues?state=open&labels=codex-ready&per_page=100&page={page}",
                    repository.0
                ));
            let bytes = run(command, Duration::from_secs(15)).await?;
            let batch: Vec<Issue> =
                serde_json::from_slice(&bytes).context("Invalid GitHub issue JSON")?;
            let finished = batch.len() < 100;
            for issue in batch.into_iter().filter(Issue::ready) {
                if !issues.iter().any(|i: &Issue| i.number == issue.number) {
                    issues.push(issue);
                }
            }
            if finished {
                return Ok(issues);
            }
        }
        bail!("GitHub pagination limit reached; backlog was not returned partially")
    })
    .await
    .context("GitHub backlog timed out; retry later")?
}

pub fn render(repository: &GitHubRepository, issues: &[Issue]) -> String {
    if issues.is_empty() {
        return "No unclaimed codex-ready issues.".into();
    }
    let mut text = format!("Backlog for {} ({} ready)\n", repository.0, issues.len());
    let mut shown = 0;
    for issue in issues {
        // Untrusted titles cannot inject Discord Markdown, mentions or links.
        let title: String = issue
            .title
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || " -.,:!?()/".contains(c) {
                    c
                } else {
                    ' '
                }
            })
            .take(90)
            .collect();
        let line = format!(
            "• #{} {} — ready\nhttps://github.com/{}/issues/{}\n",
            issue.number, title, repository.0, issue.number
        );
        if text.encode_utf16().count() + line.encode_utf16().count() > 1300 {
            break;
        }
        text.push_str(&line);
        shown += 1;
    }
    if shown < issues.len() {
        text.push_str(&format!("Showing {shown}/{}. Full backlog: https://github.com/{}/issues?q=is%3Aissue+is%3Aopen+label%3Acodex-ready+no%3Aassignee+-label%3Acodex-claimed", issues.len(), repository.0));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_mapping_and_remote() {
        for remote in [
            "https://github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
        ] {
            assert_eq!(
                GitHubRepository::from_remote(remote).unwrap().0,
                "owner/repo"
            );
        }
        for remote in [
            "https://github.com.evil/owner/repo",
            "https://user@github.com/owner/repo",
            "file:///owner/repo",
            "https://github.com/a/b/extra",
        ] {
            assert!(GitHubRepository::from_remote(remote).is_err());
        }
        assert!(GitHubRepository::parse("owner/.github").is_ok());
        for value in ["a/../b", "-a/b", "a/b?x", "a/b#x", "a/..", "a/b\n"] {
            assert!(GitHubRepository::parse(value).is_err());
        }
    }
    #[test]
    fn filters_untrusted_api_results() {
        let base = serde_json::json!({"number":1,"title":"@everyone **task**","state":"open","labels":[{"name":"codex-ready"}],"assignees":[]});
        assert!(
            serde_json::from_value::<Issue>(base.clone())
                .unwrap()
                .ready()
        );
        for (key, value) in [
            ("state", serde_json::json!("closed")),
            ("pull_request", serde_json::json!({})),
            ("assignees", serde_json::json!([{"login":"worker"}])),
            ("labels", serde_json::json!([])),
            (
                "labels",
                serde_json::json!([{"name":"codex-ready"},{"name":"codex-claimed"}]),
            ),
        ] {
            let mut item = base.clone();
            item[key] = value;
            assert!(!serde_json::from_value::<Issue>(item).unwrap().ready());
        }
        let output = render(
            &GitHubRepository::parse("a/b").unwrap(),
            &[serde_json::from_value(base).unwrap()],
        );
        assert!(!output.contains('@'));
        assert!(!output.contains('*'));
    }
    #[tokio::test]
    async fn reads_origin_without_executing_git_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::repositories::RepositoryRoot::open(temp.path()).unwrap();
        let path = root.create("project").await.unwrap();
        assert!(origin(&path).await.is_err());
        let status = crate::repositories::git(&path)
            .args([
                "config",
                "remote.origin.url",
                "git@github.com:owner/project.git",
            ])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(origin(&path).await.unwrap().as_str(), "owner/project");
    }

    #[test]
    fn long_backlog_preserves_footer_and_discord_limit() {
        let repo =
            GitHubRepository::parse(&format!("{}/{}", "a".repeat(39), "b".repeat(100))).unwrap();
        let issues: Vec<Issue> = (1..100).map(|number| serde_json::from_value(serde_json::json!({"number":number,"title":"💥".repeat(100),"state":"open","labels":[{"name":"codex-ready"}],"assignees":[]})).unwrap()).collect();
        let output = render(&repo, &issues);
        assert!(output.encode_utf16().count() <= 1800);
        assert!(output.contains("Showing"));
        assert!(output.ends_with("-label%3Acodex-claimed"));
    }

    #[tokio::test]
    async fn follows_pages_and_rejects_invalid_json() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("gh");
        let item = serde_json::json!({"number":1,"title":"task","state":"open","labels":[{"name":"codex-ready"}],"assignees":[]});
        let first = serde_json::to_string(&vec![item; 100]).unwrap();
        std::fs::write(temp.path().join("page"), first).unwrap();
        std::fs::write(&executable, format!("#!/bin/sh\ncase \"$*\" in *page=1) cat '{}' ;; *page=2) echo '[]' ;; *) exit 9 ;; esac\n", temp.path().join("page").display())).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let repo = GitHubRepository::parse("owner/repo").unwrap();
        assert_eq!(
            list_with(&repo, executable.to_str().unwrap())
                .await
                .unwrap()
                .len(),
            1
        );
        std::fs::write(&executable, "#!/bin/sh\necho invalid\n").unwrap();
        assert!(
            list_with(&repo, executable.to_str().unwrap())
                .await
                .unwrap_err()
                .to_string()
                .contains("JSON")
        );
        std::fs::write(&executable, "#!/bin/sh\nexit 4\n").unwrap();
        assert!(
            list_with(&repo, executable.to_str().unwrap())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn handles_cli_failures_and_timeout() {
        let mut failure = Command::new("sh");
        failure.args(["-c", "exit 4"]);
        assert!(
            run(failure, Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string()
                .contains("auth")
        );
        let mut slow = Command::new("sleep");
        slow.arg("2");
        assert!(
            run(slow, Duration::from_millis(10))
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        let missing = Command::new("/nonexistent/gh");
        assert!(run(missing, Duration::from_secs(1)).await.is_err());
    }
}
