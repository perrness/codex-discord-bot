use super::*;
use crate::{
    queue::Run,
    validation::{self, Outcome},
};

impl Manager {
    pub(super) fn cancel_validation(&self, id: Uuid) -> bool {
        if let Some(cancel) = self.validations.lock().unwrap().get(&id) {
            cancel.cancel();
            true
        } else {
            false
        }
    }

    // The session gate and repository claim stay held across checks and repairs.
    pub(super) async fn validate_issue_turn(
        &self,
        session: &Session,
        turn: &str,
        run: &Run,
    ) -> Result<()> {
        let cancel = self.fatal.child_token();
        self.validations
            .lock()
            .unwrap()
            .insert(session.id, cancel.clone());
        let result = self
            .validate_issue_locked(session, turn, run, &cancel)
            .await;
        self.validations.lock().unwrap().remove(&session.id);
        result
    }

    async fn validate_issue_locked(
        &self,
        session: &Session,
        turn: &str,
        run: &Run,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let repo: Repository =
            sqlx::query_as("SELECT id,name,path,github_repository FROM repositories WHERE id=$1")
                .bind(run.repository_id)
                .fetch_one(&self.store.pool)
                .await?;
        let configured = self.config.validation.repositories.get(&repo.name);
        let (id, policy) = self
            .store
            .begin_issue_validation(run, turn, configured)
            .await?;
        let mut results = Vec::new();
        let setup = async {
            let policy = policy.as_ref().context(
                "No trusted build, test and lint policy is configured for this repository",
            )?;
            self.validate_session_directory(&repo, session).await?;
            let (worker, config) = (policy.clone(), self.config.clone());
            tokio::task::spawn_blocking(move || {
                worker.verify_worker(&[
                    &config.repos_root,
                    &config.state_dir,
                    &config.codex_home,
                    std::path::Path::new("/etc"),
                    std::path::Path::new("/home"),
                    std::path::Path::new("/root"),
                    std::path::Path::new("/run"),
                ])
            })
            .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let error = if let Err(error) = setup {
            Some(format!("Validation setup blocked: {error:#}"))
        } else {
            let policy = policy.as_ref().context("Validation policy missing")?;
            for (name, check) in policy.checks() {
                self.store
                    .activity(session.id, &format!("Running trusted {name} validation"))
                    .await?;
                let result = validation::run_check(
                    policy,
                    std::path::Path::new(&session.working_directory),
                    name,
                    check,
                    cancel,
                )
                .await;
                let stop = matches!(result.outcome, Outcome::Error | Outcome::Cancelled);
                results.push(result);
                // Persist each result, including bounded diagnostics, before starting another check.
                self.store.record_issue_checks(id, &results).await?;
                if stop {
                    break;
                }
            }
            None
        };
        let error = if cancel.is_cancelled() {
            Some("Validation cancelled; integration and repairs are blocked".to_owned())
        } else {
            error
        };
        let action = self
            .store
            .finish_issue_validation(run, id, error.as_deref())
            .await?;
        // End the cancellable validation phase before submitting another turn. A stop
        // acknowledged during the final transaction must also cancel its reserved repair.
        self.validations.lock().unwrap().remove(&session.id);
        if cancel.is_cancelled() {
            self.store
                .cancel_finished_validation(
                    run,
                    id,
                    action.as_ref().map(|(source, _)| source.as_str()),
                )
                .await?;
            return Ok(());
        }
        if let Some((source, prompt)) = action {
            // Same durable thread and worktree, with a reserved receipt and repair budget.
            // submit_turn fails closed on ambiguity; recovery never retries this request.
            self.submit_turn(&self.store.session(session.id).await?, &source, &prompt)
                .await?;
        }
        Ok(())
    }
}
