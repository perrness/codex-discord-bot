use super::*;
use crate::queue::{Run, State};

impl Manager {
    pub async fn queue_command(
        &self,
        action: &str,
        name: &str,
        user: u64,
        guild: u64,
        issue: Option<i64>,
    ) -> Result<String> {
        self.authorize(user, Some(guild))?;
        let repo = self.store.repository(name).await?;
        match action {
            "start" => {
                let path = self.root.validate(name).await?;
                ensure!(path.to_str() == Some(&repo.path), "Repository path changed");
                let mapping = match &repo.github_repository {
                    Some(value) => crate::github::GitHubRepository::parse(value)?,
                    None => crate::github::origin(&path).await?,
                };
                let issues = crate::github::list(&mapping).await?;
                // Create paused first; enable dispatch only after the complete discovery succeeds.
                sqlx::query("INSERT INTO issue_queues(repository_id,requester_id,guild_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                    .bind(repo.id).bind(user.to_string()).bind(guild.to_string()).execute(&self.store.pool).await?;
                for issue in issues {
                    self.store
                        .enqueue_issue(repo.id, mapping.as_str(), i64::try_from(issue.number)?)
                        .await?;
                }
                sqlx::query("UPDATE issue_queues SET paused=false,requester_id=$2,guild_id=$3,updated_at=now() WHERE repository_id=$1")
                    .bind(repo.id).bind(user.to_string()).bind(guild.to_string()).execute(&self.store.pool).await?;
                Ok("Queue started. Ready issues were persisted; one issue runs at a time. Use /queue start again to discover new issues.".into())
            }
            "pause" | "resume" => {
                let count = sqlx::query(
                    "UPDATE issue_queues SET paused=$2,updated_at=now() WHERE repository_id=$1",
                )
                .bind(repo.id)
                .bind(action == "pause")
                .execute(&self.store.pool)
                .await?
                .rows_affected();
                ensure!(count == 1, "Use /queue start first");
                Ok(if action == "pause" {
                    "Queue paused. Active work continues."
                } else {
                    "Queue resumed. Blocked runs still require explicit review and retry."
                }
                .into())
            }
            "retry" => {
                self.store
                    .retry_issue(repo.id, issue.context("Supply an issue number")?)
                    .await?;
                Ok("New attempt queued. Dispatch follows the queue's pause setting.".into())
            }
            "status" => {
                let paused: Option<bool> =
                    sqlx::query_scalar("SELECT paused FROM issue_queues WHERE repository_id=$1")
                        .bind(repo.id)
                        .fetch_optional(&self.store.pool)
                        .await?;
                let runs: Vec<Run> = sqlx::query_as("SELECT * FROM issue_runs WHERE repository_id=$1 ORDER BY updated_at DESC LIMIT 12").bind(repo.id).fetch_all(&self.store.pool).await?;
                let mut text = format!(
                    "Queue: {}",
                    match paused {
                        Some(true) => "paused",
                        Some(false) => "running",
                        None => "not started",
                    }
                );
                for run in runs {
                    text.push_str(&format!(
                        "\n#{} attempt {}: {}{}",
                        run.issue_number,
                        run.attempt,
                        run.state,
                        run.session_id
                            .map(|id| format!(" · session `{id}`"))
                            .unwrap_or_default()
                    ));
                }
                Ok(text)
            }
            _ => bail!("Unknown queue action"),
        }
    }

    pub async fn run_issue_queue(self: Arc<Self>) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.fatal.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
            if !self.discord_ready.load(Ordering::Relaxed) {
                continue;
            }
            // An idle completed turn is not evidence of release. Keep its worker slot
            // until the operator explicitly completes/dismisses the existing task.
            sqlx::query("UPDATE issue_runs r SET state=CASE WHEN s.channel_state='completed' THEN 'released' WHEN s.channel_state='dismissed' THEN 'cancelled' WHEN s.last_outcome IN ('failed','interrupted','unknown') THEN 'blocked' ELSE r.state END,updated_at=now() FROM sessions s WHERE r.session_id=s.id AND (r.state='implementing' OR (r.state='blocked' AND s.channel_state IN ('completed','dismissed'))) AND NOT s.repository_claimed")
                .execute(&self.store.pool).await?;
            for repo in self.store.repositories().await? {
                let Some(run) = self.store.claim_issue(repo.id).await? else {
                    continue;
                };
                let (user, guild): (String, String) = sqlx::query_as(
                    "SELECT requester_id,guild_id FROM issue_queues WHERE repository_id=$1",
                )
                .bind(repo.id)
                .fetch_one(&self.store.pool)
                .await?;
                let task = format!(
                    "Implement GitHub issue https://github.com/{}/issues/{}. Inspect its requirements, implement the change and validate it. Report the result and any remaining integration or release steps.",
                    run.github_repository, run.issue_number
                );
                let result = self
                    .create_task(
                        user.parse()?,
                        guild.parse()?,
                        &format!("issue:{}", run.id),
                        &repo.name,
                        &task,
                    )
                    .await;
                let (next, error) = match result {
                    Ok(_) => (State::Implementing, None),
                    Err(e) => (
                        State::Blocked,
                        Some(format!("Dispatch needs review: {e:#}")),
                    ),
                };
                self.store
                    .transition_issue(&run, State::Preparing, next, error.as_deref())
                    .await?;
            }
        }
    }
}
