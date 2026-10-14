use super::*;

impl Manager {
    /// Called before Discord input is enabled. The previous systemd cgroup must be gone.
    pub async fn recover(&self) -> Result<()> {
        sqlx::query("UPDATE pending_requests SET state='expired',updated_at=now() WHERE state IN ('pending','responding')")
            .execute(&self.store.pool).await?;
        sqlx::query("UPDATE inbound_requests SET state='uncertain',updated_at=now() WHERE state IN ('received','sending')")
            .execute(&self.store.pool).await?;
        let sessions: Vec<Session> = sqlx::query_as("SELECT * FROM sessions WHERE repository_claimed OR status IN ('running','waiting','stopping','recovering','provisioning')")
            .fetch_all(&self.store.pool).await?;
        for s in sessions {
            sqlx::query("UPDATE sessions SET status='recovering',updated_at=now() WHERE id=$1")
                .bind(s.id)
                .execute(&self.store.pool)
                .await?;
            let result = self.recover_one(&s).await;
            if result.is_err() {
                // Keep the claim: lack of evidence must never authorize a second writer.
                sqlx::query("UPDATE sessions SET status='unavailable',last_outcome='unknown',last_error='Recovery could not verify conversation or process state; inspect with SSH',updated_at=now() WHERE id=$1")
                    .bind(s.id).execute(&self.store.pool).await?;
                if s.discord_thread_id.is_some() {
                    self.store.enqueue(s.id, &format!("recovery:{}:{}", self.generation, s.id),
                        "Recovery could not verify this session. The repository remains reserved. Inspect the service logs and Codex history over SSH; no work was replayed.", None).await?;
                }
            }
        }
        Ok(())
    }

    async fn recover_one(&self, session: &Session) -> Result<()> {
        let Some(thread) = &session.codex_thread_id else {
            // No turn can have been submitted without first persisting its Codex thread ID.
            sqlx::query("UPDATE sessions SET status='unavailable',repository_claimed=false,last_outcome='unknown',last_error='Setup was interrupted before task submission',updated_at=now() WHERE id=$1")
                .bind(session.id).execute(&self.store.pool).await?;
            return Ok(());
        };
        let repo: Repository = sqlx::query_as("SELECT id,name,path FROM repositories WHERE id=$1")
            .bind(session.repository_id)
            .fetch_one(&self.store.pool)
            .await?;
        ensure!(
            self.root.validate(&repo.name).await?.to_str()
                == Some(session.working_directory.as_str()),
            "Repository changed during downtime"
        );
        self.take_repository_lock(&repo, session.id)?;
        let resumed = self
            .client
            .resume_thread(thread, &session.working_directory)
            .await?;
        ensure!(
            resumed.thread.status["type"] == "idle",
            "Resumed thread is not idle"
        );
        self.client.clean_terminals(thread).await?;
        let history: Value = self
            .client
            .request(
                "thread/turns/list",
                json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"full"}),
            )
            .await?;
        let latest = history["data"].as_array().and_then(|v| v.first());
        let outcome = latest
            .filter(|v| {
                session.current_turn_id.is_some()
                    && v["id"].as_str() == session.current_turn_id.as_deref()
            })
            .and_then(|v| v["status"].as_str())
            .filter(|s| matches!(*s, "completed" | "failed" | "interrupted"))
            .unwrap_or("unknown");
        if let Some(turn) = latest
            && let Some(items) = turn["items"].as_array()
        {
            for item in items {
                if item["type"] == "agentMessage"
                    && let (Some(turn_id), Some(item_id), Some(text)) = (
                        turn["id"].as_str(),
                        item["id"].as_str(),
                        item["text"].as_str(),
                    )
                {
                    self.store
                        .enqueue(session.id, &format!("{turn_id}:{item_id}"), text, None)
                        .await?;
                }
            }
        }
        let mut tx = self.store.pool.begin().await?;
        sqlx::query("UPDATE sessions SET status='idle',repository_claimed=false,current_turn_id=NULL,last_outcome=$2,last_error=NULL,updated_at=now() WHERE id=$1")
            .bind(session.id).bind(outcome).execute(&mut *tx).await?;
        persistence::enqueue(&mut tx, session.id, &format!("recovered:{}:{}", self.generation, session.id),
            &format!("Service restarted. Latest stored turn: **{outcome}**. Pending approvals expired. No task was replayed; send a new message to continue."), None).await?;
        tx.commit().await?;
        self.drop_repository_lock(session);
        Ok(())
    }

    pub async fn maintain(self: Arc<Self>) -> Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! { _ = self.fatal.cancelled() => return Ok(()), _ = interval.tick() => {} }
            sqlx::query("SELECT 1").execute(&self.store.pool).await?;
            let expired: Vec<crate::persistence::PendingRequest> = sqlx::query_as("SELECT * FROM pending_requests WHERE state='pending' AND generation=$1 AND created_at < now() - interval '1 hour'")
                .bind(self.generation).fetch_all(&self.store.pool).await?;
            for request in expired {
                let gate = self.gate(request.session_id);
                let _guard = gate.lock().await;
                let changed = sqlx::query("UPDATE pending_requests SET state='expired',updated_at=now() WHERE id=$1 AND state='pending'")
                    .bind(request.id).execute(&self.store.pool).await?.rows_affected();
                if changed == 0 {
                    continue;
                }
                let id = serde_json::from_value(request.rpc_id)?;
                if let Some(response) = denied_response(&request.method) {
                    self.client.respond(&id, response).await?;
                }
                let params = &request.payload["params"];
                self.client
                    .interrupt(
                        params["threadId"].as_str().context("Missing thread")?,
                        &request.turn_id,
                    )
                    .await?;
                self.store
                    .enqueue(
                        request.session_id,
                        &format!("expired:{}", request.id),
                        "The unanswered request expired after one hour. The turn was interrupted.",
                        None,
                    )
                    .await?;
            }
        }
    }
}
