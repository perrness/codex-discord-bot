use super::{Actions, Policies, Policy};
use crate::persistence::Store;
use anyhow::{Context, Result, ensure};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn verify_service_identity(worker_uid: u32, service_uid: u32) -> Result<()> {
    ensure!(
        worker_uid != 0 && service_uid != 0 && worker_uid != service_uid,
        "The release controller must run as a non-root account distinct from CODEX_WORKER_UID"
    );
    Ok(())
}

/// Separate entry point: no Discord token, Codex config or worker process is
/// loaded. The protected service environment is the only dispatch credential.
pub async fn run_service() -> Result<()> {
    use crate::config::{required, retention};
    use std::{os::unix::fs::MetadataExt, path::Path};
    let worker_uid: u32 = required("CODEX_WORKER_UID")?
        .parse()
        .context("Invalid CODEX_WORKER_UID")?;
    let service_uid = rustix::process::geteuid().as_raw();
    verify_service_identity(worker_uid, service_uid)?;
    let service_home = required("HOME")?;
    let metadata = std::fs::metadata(&service_home).context("Read release service HOME")?;
    ensure!(
        Path::new(&service_home).is_absolute()
            && Path::new(&service_home).canonicalize()? == Path::new(&service_home)
            && metadata.is_dir()
            && metadata.uid() == service_uid
            && metadata.mode() & 0o077 == 0,
        "Release HOME must be canonical, owned by the release account and private (0700)"
    );
    let policies = Policies::load(Path::new(&required("RELEASE_CONFIG")?))?;
    let store = Store::connect(&required("DATABASE_URL")?).await?;
    let mut leader = store.pool.acquire().await?;
    let elected: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(7248306189302)")
        .fetch_one(&mut *leader)
        .await?;
    ensure!(elected, "Another release controller owns this database");
    store.recover_releases().await.context(
        "Release recovery failed; run the bot's migrate command before starting the controller",
    )?;
    let retention =
        retention(&std::env::var("DISCORD_TASK_RETENTION_HOURS").unwrap_or_else(|_| "48".into()))?;
    let controller = Arc::new(Controller::new(store, policies, retention));
    let mut worker = tokio::spawn(controller.clone().run_releases());
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = term.recv() => Ok(()),
        result = &mut worker => result.context("Release controller task failed")?,
        _ = async {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if sqlx::query("SELECT 1").execute(&mut *leader).await.is_err() { break; }
            }
        } => Err(anyhow::anyhow!("Lost release controller database lock connection")),
    };
    controller.stop.cancel();
    worker.abort();
    result
}

/// Runs in a separate Unix account, never in the process that spawns Codex.
pub struct Controller {
    store: Store,
    policies: Policies,
    retention: Duration,
    stop: CancellationToken,
}

impl Controller {
    pub fn new(store: Store, policies: Policies, retention: Duration) -> Self {
        Self {
            store,
            policies,
            retention,
            stop: CancellationToken::new(),
        }
    }

    pub async fn poll_issue_release(&self, run: Uuid, allow_dispatch: bool) -> Result<()> {
        let session: Uuid = sqlx::query_scalar("SELECT session_id FROM issue_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&self.store.pool)
            .await?;
        let result = self.advance_release(run, session, allow_dispatch).await;
        if let Err(error) = result {
            let intent = self.store.release(run).await?;
            let uncertain = intent
                .as_ref()
                .is_some_and(|r| !matches!(r.state.as_str(), "failed" | "succeeded"));
            self.store.block_release(run, &format!("Release blocked: {error:#}. Subsequent releases remain blocked; no deployment or rollback is retried."), uncertain).await?;
        }
        Ok(())
    }

    async fn advance_release(&self, run: Uuid, session: Uuid, allow_dispatch: bool) -> Result<()> {
        let release = match self.store.release(run).await? {
            Some(release) => release,
            None => {
                ensure!(
                    allow_dispatch,
                    "No release dispatch intent; use /queue release after inspection"
                );
                let name: String = sqlx::query_scalar("SELECT p.name FROM repositories p JOIN issue_runs r ON r.repository_id=p.id WHERE r.id=$1")
                    .bind(run).fetch_one(&self.store.pool).await?;
                let policy = self.policies.repositories.get(&name)
                    .context("No trusted RELEASE_CONFIG for this repository; no unattended deployment is permitted")?;
                policy.validate()?;
                let actions = Actions::new(&policy.github_repository)?;
                actions.preflight(policy).await?;
                ensure!(!self.stop.is_cancelled(), "Release controller is stopping");
                let release = self.store.begin_issue_release(run, policy).await?;
                // Once intent exists, even a rejected/lost response follows the
                // observation path. No error handler may call dispatch again.
                ensure!(
                    !self.stop.is_cancelled(),
                    "Release dispatch interrupted before submission"
                );
                let id = actions.dispatch(&release, policy).await?;
                self.store.release_run_id(run, id).await?;
                self.store.release(run).await?.context("Missing release")?
            }
        };
        if matches!(release.state.as_str(), "succeeded" | "failed") {
            return Ok(());
        }
        let deadline = release
            .health_deadline
            .unwrap_or(release.deployment_deadline);
        if chrono::Utc::now() >= deadline {
            return self.store.block_release(run, "Deployment or revision health verification timed out. Inspect /queue status and the workflow. Subsequent releases remain blocked; no automatic rollback is configured.", false).await;
        }
        let policy: Policy = serde_json::from_value(release.policy.clone())?;
        policy.validate()?;
        let actions = Actions::new(&release.github_repository)?;
        let observation =
            tokio::time::timeout(Duration::from_secs(45), actions.observe(&release, &policy))
                .await
                .context("Release observation timed out")??;
        let Some(observation) = observation else {
            return self.store.block_release(run, "Release dispatch is not yet observable. Its outcome is uncertain; the controller will only look for the existing correlated workflow, never dispatch again.", true).await;
        };
        self.store
            .observe_release_workflow(run, &observation)
            .await?;
        if observation["status"] != "completed" {
            self.store
                .activity(
                    session,
                    "Waiting for the configured release workflow to finish",
                )
                .await?;
            return Ok(());
        }
        if observation["conclusion"] != "success" {
            return self.store.block_release(run, "Release workflow did not succeed. Subsequent releases are blocked. Inspect the recorded workflow; rollback requires an operator's tested procedure, including its database migration constraints.", false).await;
        }
        self.store
            .activity(session, "Verifying deployment health and exact revision")
            .await?;
        let results = futures_util::future::join_all(
            policy
                .health_checks
                .iter()
                .map(crate::releases::check_health),
        )
        .await;
        self.store.record_release_health(run, &results).await?;
        if policy
            .verify_health(&release.expected_sha, &results)
            .is_ok()
        {
            self.store.finish_issue_release(run, self.retention).await?;
        }
        Ok(())
    }

    pub async fn run_releases(self: Arc<Self>) -> Result<()> {
        let mut active = std::collections::HashSet::new();
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = self.stop.cancelled() => return Ok(()),
                Some(result) = workers.join_next() => {
                    let run = result??;
                    active.remove(&run);
                    continue;
                },
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
            let pending: Vec<Uuid> = sqlx::query_scalar("SELECT r.id FROM issue_runs r LEFT JOIN issue_releases d ON d.run_id=r.id
                WHERE (r.state='releasing' AND r.release_requested AND d.run_id IS NULL)
                    OR (r.state IN ('releasing','blocked') AND d.state IN ('dispatching','deploying','verifying','uncertain'))")
                .fetch_all(&self.store.pool).await?;
            for run in pending {
                if active.insert(run) {
                    let manager = self.clone();
                    workers.spawn(async move {
                        manager.poll_issue_release(run, true).await?;
                        Ok::<_, anyhow::Error>(run)
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_credentials_require_a_separate_non_root_identity() {
        assert!(verify_service_identity(1001, 1001).is_err());
        assert!(verify_service_identity(1001, 0).is_err());
        assert!(verify_service_identity(0, 1002).is_err());
        verify_service_identity(1001, 1002).unwrap();
    }
}
