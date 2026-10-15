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
        ensure!(
            session.channel_kind == "text",
            "Legacy threads remain available and are not automatically moved or deleted"
        );
        ensure!(
            session.channel_state == "active",
            "Task is not active; use /reconnect if its channel is missing"
        );
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
        let state = if dismiss { "dismissed" } else { "completed" };
        let mut tx = self.store.pool.begin().await?;
        sqlx::query("UPDATE sessions SET channel_state=$2,completed_at=now(),delete_after=now()+make_interval(secs => $3::double precision),updated_at=now() WHERE id=$1")
            .bind(id).bind(state).bind(self.config.channel_retention.as_secs_f64()).execute(&mut *tx).await?;
        persistence::enqueue(&mut tx, id, &format!("task-{state}:{id}:{}", session.status_revision),
            &format!("Task **{state}**. This channel will move to COMPLETED and be deleted after {} hours. Reply before then to reopen it.", self.config.channel_retention.as_secs() / 3600), None).await?;
        tx.commit().await?;
        Ok(format!(
            "Task {state}. Channel move and cleanup are scheduled."
        ))
    }

    pub async fn stop_session(&self, user: u64, guild: u64, id: Uuid) -> Result<String> {
        self.authorize(user, Some(guild))?;
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
        if let Some(channel) = session.channel_id() {
            match self.task_channel(&session).await {
                Ok(_) => return Ok(format!("Task channel: <#{channel}>")),
                Err(error)
                    if crate::discord::channels::discord_error_code(&error) == Some(10003) => {}
                Err(error) => return Err(error),
            }
        }
        sqlx::query("UPDATE sessions SET discord_channel_id=NULL,status_message_id=NULL,channel_state='provisioning',completed_at=NULL,delete_after=NULL,discord_retry_at=now(),discord_failures=0,discord_error=NULL WHERE id=$1")
            .bind(id).execute(&self.store.pool).await?;
        let repo: Repository = sqlx::query_as("SELECT id,name,path FROM repositories WHERE id=$1")
            .bind(session.repository_id)
            .fetch_one(&self.store.pool)
            .await?;
        let channel = self
            .provision_channel(&self.store.session(id).await?, &repo.name)
            .await?;
        if let (Some(thread), Some(turn)) = (&session.codex_thread_id, &session.current_turn_id) {
            let mut live = self.live.lock().unwrap();
            if let Some(old) = session.channel_id().and_then(|c| c.parse().ok()) {
                live.remove(&old);
            }
            live.insert(channel.get(), (thread.clone(), turn.clone()));
        }
        self.sync_status(&self.store.session(id).await?).await?;
        sqlx::query("DELETE FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NULL AND (dedupe_key LIKE 'task-completed:%' OR dedupe_key LIKE 'task-dismissed:%')")
            .bind(id).execute(&self.store.pool).await?;
        // Redisplay still-live prompts and their complete previews, without replaying any response.
        sqlx::query("UPDATE discord_outbox o SET delivered_at=NULL,discord_message_id=NULL,controls_closed=false,attempts=0,next_attempt_at=now() WHERE session_id=$1 AND EXISTS(SELECT 1 FROM pending_requests p WHERE p.session_id=$1 AND p.state='pending' AND p.generation=$2 AND o.dedupe_key LIKE 'request:' || p.id::text || ':%')")
            .bind(id).bind(self.generation).execute(&self.store.pool).await?;
        Ok(format!(
            "Reconnected in <#{channel}>. No Codex work was replayed."
        ))
    }
}
