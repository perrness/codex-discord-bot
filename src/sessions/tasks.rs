use super::*;

impl Manager {
    pub async fn task_for_user(
        &self,
        user: u64,
        guild: u64,
        channel: u64,
        id: Option<Uuid>,
    ) -> Result<Session> {
        self.authorize(user, Some(guild))?;
        let session = match id {
            Some(id) => self.store.session(id).await?,
            None => self.store.by_discord(channel).await?.context(
                "Use this command in a task channel or supply its session ID from /sessions",
            )?,
        };
        ensure!(session.guild_id == guild.to_string(), "Wrong guild");
        Ok(session)
    }

    /// A successful turn alone cannot establish validation, integration or deployment success.
    /// Interactive tasks finish only on an authorized user's explicit confirmation.
    pub async fn finish_task(
        &self,
        user: u64,
        guild: u64,
        id: Uuid,
        dismiss: bool,
    ) -> Result<String> {
        self.authorize(user, Some(guild))?;
        let gate = self.gate(id);
        let _guard = gate.lock().await;
        let session = self.store.session(id).await?;
        ensure!(session.guild_id == guild.to_string(), "Wrong guild");
        let legacy = session.channel_kind == "thread";
        let failed_setup = !legacy
            && session.channel_state == "missing"
            && session.channel_id().is_none()
            && session.codex_thread_id.is_none();
        if legacy {
            ensure!(
                dismiss,
                "Use /dismiss to hide a legacy thread from /sessions"
            );
            ensure!(
                session.channel_state == "legacy",
                "Legacy thread is already dismissed or unavailable; check /status"
            );
        } else {
            ensure!(
                session.channel_state == "active" || (dismiss && failed_setup),
                "Task is not active; use /reconnect if its channel is missing"
            );
        }
        ensure!(
            !session.repository_claimed
                && session.current_turn_id.is_none()
                && matches!(session.status.as_str(), "idle" | "unavailable"),
            "Stop the running task and wait for recovery before closing it"
        );
        ensure!(
            !self.store.unresolved(id).await?,
            "Resolve pending approvals and uncertain input before closing this task"
        );
        ensure!(
            dismiss || session.last_outcome.as_deref() == Some("completed"),
            "The last turn did not succeed; continue the task or explicitly /dismiss it"
        );
        if !dismiss {
            self.store.require_issue_release(id).await?;
        } else {
            let integration: Option<crate::integration::Integration> = sqlx::query_as("SELECT i.* FROM issue_integrations i JOIN issue_runs r ON r.id=i.run_id WHERE r.session_id=$1")
                .bind(id).fetch_optional(&self.store.pool).await?;
            if let Some(i) = integration {
                ensure!(
                    i.merge_sha.is_none(),
                    "This issue was integrated; independent release verification is required before advancing the queue"
                );
                if i.operation.is_some() || !i.retry_safe {
                    self.reconcile_integration(&i).await?;
                }
                let i = self
                    .store
                    .integration(i.run_id)
                    .await?
                    .context("Missing integration")?;
                ensure!(
                    i.retry_safe && i.merge_sha.is_none(),
                    "Integration is open or uncertain. Inspect GitHub and close the unmerged PR before dismissing"
                );
            }
        }
        if failed_setup {
            let mut tx = self.store.pool.begin().await?;
            sqlx::query("UPDATE sessions SET channel_state='dismissed',completed_at=now(),delete_after=NULL,updated_at=now() WHERE id=$1")
                .bind(id).execute(&mut *tx).await?;
            sqlx::query("UPDATE issue_runs SET state='cancelled',validation_id=NULL,updated_at=now() WHERE session_id=$1")
                .bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok("Failed setup dismissed. No Codex work was submitted; use /codex to start a new task.".into());
        }
        if legacy {
            ensure!(
                !self.store.undelivered(id).await?,
                "Wait for queued messages to be delivered before dismissing this thread"
            );
            sqlx::query("UPDATE sessions SET channel_state='dismissed',completed_at=now(),delete_after=NULL,updated_at=now() WHERE id=$1")
                .bind(id).execute(&self.store.pool).await?;
            return Ok("Legacy thread hidden from /sessions. Its history is retained; reply in the thread to reopen it.".into());
        }
        let state = if dismiss { "dismissed" } else { "completed" };
        let mut tx = self.store.pool.begin().await?;
        let issue: Option<crate::queue::Run> =
            sqlx::query_as("SELECT * FROM issue_runs WHERE session_id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let is_issue = issue.is_some();
        if let Some(run) = issue {
            if !dismiss {
                // /complete can close presentation again after /reconnect, but
                // cannot manufacture deployment evidence or a release receipt.
                ensure!(
                    run.state == "released",
                    "Release is not independently verified"
                );
            }
            sqlx::query("UPDATE issue_runs SET state=$2,validation_id=CASE WHEN $2='cancelled' THEN NULL ELSE validation_id END,last_error=CASE WHEN $2='released' THEN NULL ELSE last_error END,updated_at=now() WHERE id=$1")
                .bind(run.id).bind(if dismiss { "cancelled" } else { "released" }).execute(&mut *tx).await?;
            // A verified release receipt is the only successful unlock. Safely
            // blocked integrations already relinquished their slot separately.
            sqlx::query("DELETE FROM issue_release_locks WHERE run_id=$1 AND run_id IN (SELECT id FROM confirmed_issue_releases)")
                .bind(run.id).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE sessions SET channel_state=$2,completed_at=now(),delete_after=now()+make_interval(secs => $3::double precision),updated_at=now() WHERE id=$1")
            .bind(id).bind(state).bind(self.config.channel_retention.as_secs_f64()).execute(&mut *tx).await?;
        let next = if is_issue {
            "Issue worktrees are preserved on failure; clean worktrees are eligible for cleanup after release confirmation."
        } else {
            "Reply before then to reopen it."
        };
        persistence::enqueue(&mut tx, id, &format!("task-{state}:{id}:{}", session.status_revision),
            &format!("Task **{state}**. This channel will move to COMPLETED and be deleted after {} hours. {next}", self.config.channel_retention.as_secs() / 3600), None).await?;
        tx.commit().await?;
        Ok(format!(
            "Task {state}. Channel move and cleanup are scheduled."
        ))
    }

    pub async fn stop_session(&self, user: u64, guild: u64, id: Uuid) -> Result<String> {
        self.authorize(user, Some(guild))?;
        let session = self.store.session(id).await?;
        ensure!(session.guild_id == guild.to_string(), "Wrong guild");
        if self.cancel_validation(id) {
            return Ok("Validation or integration cancellation requested. Waiting for the current operation to settle.".into());
        }
        let gate = self.gate(id);
        let _guard = gate.lock().await;
        let s = self.store.session(id).await?;
        ensure!(s.guild_id == guild.to_string(), "Wrong guild");
        if let (Some(thread), Some(turn)) = (&s.codex_thread_id, &s.current_turn_id) {
            if self.client.interrupt(thread, turn).await.is_err() {
                self.fail_closed("interrupt could not be confirmed");
                bail!("Interrupt failed; restarting the service to stop its processes");
            }
            sqlx::query("UPDATE sessions SET status='stopping',updated_at=now() WHERE id=$1 AND repository_claimed")
                .bind(id).execute(&self.store.pool).await?;
            return Ok("Interruption requested. Waiting for Codex to stop.".into());
        }
        ensure!(
            !s.repository_claimed,
            "Session is starting or recovering; wait for its status update"
        );
        Ok("There is no running turn in this session.".into())
    }

    pub async fn reconnect_channel(&self, user: u64, guild: u64, id: Uuid) -> Result<String> {
        self.authorize(user, Some(guild))?;
        let gate = self.gate(id);
        let _guard = gate.lock().await;
        let session = self.store.session(id).await?;
        ensure!(session.guild_id == guild.to_string(), "Wrong guild");
        ensure!(
            session.channel_kind == "text",
            "Existing threads retain their original routing"
        );
        // An explicit recovery command may replace a deleted channel, but never replays a turn.
        let mut existing = None;
        if let Some(channel) = session.channel_id() {
            match self.task_channel(&session).await {
                Ok(found) => {
                    if matches!(
                        session.channel_state.as_str(),
                        "missing" | "provisioning" | "deleted"
                    ) {
                        self.bind_task_channel(&session, &found).await?;
                    }
                    existing = Some(serenity::all::ChannelId::new(channel.parse()?));
                }
                Err(error)
                    if crate::discord::channels::discord_error_code(&error) == Some(10003) => {}
                Err(error) => {
                    self.channel_failure(&session, &error).await?;
                    let diagnostic = self.discord_diagnostic(&error);
                    return Err(error.context(diagnostic));
                }
            }
        }
        let channel = if let Some(channel) = existing {
            channel
        } else {
            sqlx::query("UPDATE sessions SET discord_channel_id=NULL,status_message_id=NULL,last_seen_message_id=NULL,channel_state='provisioning',completed_at=NULL,delete_after=NULL,discord_retry_at=now() WHERE id=$1")
            .bind(id).execute(&self.store.pool).await?;
            let repo: Repository = sqlx::query_as(
                "SELECT id,name,path,github_repository FROM repositories WHERE id=$1",
            )
            .bind(session.repository_id)
            .fetch_one(&self.store.pool)
            .await?;
            self.provision_channel(&self.store.session(id).await?, &repo.name)
                .await?
        };
        if let (Some(thread), Some(turn)) = (&session.codex_thread_id, &session.current_turn_id) {
            let mut live = self.live.lock().unwrap();
            if let Some(old) = session.channel_id().and_then(|c| c.parse().ok()) {
                live.remove(&old);
            }
            live.insert(channel.get(), (thread.clone(), turn.clone()));
        }
        let fresh_setup = session.codex_thread_id.is_none()
            && !session.repository_claimed
            && session.current_turn_id.is_none()
            && matches!(session.status.as_str(), "idle" | "unavailable");
        if fresh_setup {
            // No turn can have been sent without a persisted conversation ID. An explicit new
            // message may start this setup, but /reconnect itself never submits or replays work.
            let mut tx = self.store.pool.begin().await?;
            sqlx::query("UPDATE sessions SET status='idle',last_error=NULL WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE inbound_requests SET state='rejected',updated_at=now() WHERE session_id=$1 AND state IN ('received','uncertain')")
                .bind(id).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        let current = self.store.session(id).await?;
        if let Err(error) = self.sync_status(&current).await {
            self.channel_failure(&current, &error).await?;
            let diagnostic = self.discord_diagnostic(&error);
            return Err(error.context(diagnostic));
        }
        sqlx::query("UPDATE sessions SET discord_error=NULL,discord_failures=0 WHERE id=$1")
            .bind(id)
            .execute(&self.store.pool)
            .await?;
        if fresh_setup {
            return Ok(format!(
                "Reconnected in <#{channel}>. No Codex work was submitted. Send a new task message there to begin, or /dismiss this setup."
            ));
        }
        Ok(format!(
            "Reconnected in <#{channel}>. No Codex work was replayed."
        ))
    }
}
