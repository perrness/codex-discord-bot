use super::render;
use crate::{persistence::OutboxMessage, sessions::Manager};
use anyhow::Result;
use futures_util::{StreamExt, TryStreamExt, stream};
use serenity::all::*;
use std::{sync::Arc, time::Duration};

impl Manager {
    pub async fn deliver(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! { _ = self.fatal.cancelled() => return Ok(()), _ = tick.tick() => {} }
            let messages = self.store.outbox().await?;
            stream::iter(messages.into_iter().map(|m| self.deliver_one(m)))
                .buffer_unordered(4)
                .try_collect::<Vec<_>>()
                .await?;
            // Disable stale controls, including prompts delivered before a service restart.
            let stale: Vec<OutboxMessage> = sqlx::query_as("SELECT o.* FROM discord_outbox o JOIN pending_requests p ON p.id=o.request_id WHERE o.delivered_at IS NOT NULL AND NOT o.controls_closed AND p.state <> 'pending' LIMIT 20")
                .fetch_all(&self.store.pool).await?;
            for message in stale {
                let s = self.store.session(message.session_id).await?;
                if let (Some(channel), Some(message_id)) =
                    (s.channel_id(), message.discord_message_id)
                {
                    if matches!(s.channel_state.as_str(), "missing" | "deleted") {
                        sqlx::query("UPDATE discord_outbox SET controls_closed=true WHERE id=$1")
                            .bind(message.id)
                            .execute(&self.store.pool)
                            .await?;
                        continue;
                    }
                    let result = tokio::time::timeout(
                        Duration::from_secs(5),
                        ChannelId::new(channel.parse()?).edit_message(
                            &self.http,
                            MessageId::new(message_id.parse()?),
                            EditMessage::new().components(Vec::new()),
                        ),
                    )
                    .await;
                    let closed = match result {
                        Ok(Ok(_)) => true,
                        Ok(Err(error)) => matches!(
                            super::channels::discord_error_code(&error.into()),
                            Some(10003 | 10008)
                        ),
                        Err(_) => false,
                    };
                    if closed {
                        sqlx::query("UPDATE discord_outbox SET controls_closed=true WHERE id=$1")
                            .bind(message.id)
                            .execute(&self.store.pool)
                            .await?;
                    }
                }
            }
        }
    }

    async fn deliver_one(&self, message: OutboxMessage) -> Result<()> {
        let gate = self.gate(message.session_id);
        let _guard = gate.lock().await;
        // Reopening can supersede a completion notice while this delivery waits for the gate.
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM discord_outbox WHERE id=$1 AND delivered_at IS NULL)",
        )
        .bind(message.id)
        .fetch_one(&self.store.pool)
        .await?;
        if !pending {
            return Ok(());
        }
        let session = self.store.session(message.session_id).await?;
        let Some(thread) = session.channel_id() else {
            return Ok(());
        };
        if matches!(session.channel_state.as_str(), "missing" | "deleted")
            || (session.terminal_task()
                && session.status_revision != session.status_delivered_revision)
        {
            return Ok(());
        }
        let channel = ChannelId::new(thread.parse()?);
        let mut content = message.content.clone();
        let mut mentions = CreateAllowedMentions::new();
        if message.notify_requester {
            let requester = UserId::new(session.requester_id.parse()?);
            content = format!("<@{requester}>\n{content}");
            mentions = mentions.users([requester]);
        }
        let mut builder = CreateMessage::new()
            .content(content)
            .allowed_mentions(mentions)
            .nonce(render::message_nonce("outbox", channel.get(), message.id))
            .enforce_nonce(true);
        if let Some(text) = &message.attachment {
            builder =
                builder.add_file(CreateAttachment::bytes(text.as_bytes(), "codex-output.txt"));
        }
        if let Some(id) = message.request_id {
            let mut request = self.store.pending(id).await?;
            if request.generation != self.generation {
                request.state = "expired".into();
            }
            builder = builder.components(render::controls(&request));
        }
        let send = async {
            let metadata = self.task_channel(&session).await?;
            if metadata
                .thread_metadata
                .is_some_and(|m| m.archived && !m.locked)
            {
                channel
                    .edit_thread(&self.http, EditThread::new().archived(false))
                    .await?;
            }
            Ok::<_, anyhow::Error>(channel.send_message(&self.http, builder).await?)
        };
        match tokio::time::timeout(Duration::from_secs(25), send).await {
            Ok(Ok(sent)) => {
                sqlx::query("UPDATE discord_outbox SET delivered_at=now(),discord_message_id=$2 WHERE id=$1")
                    .bind(message.id).bind(sent.id.to_string()).execute(&self.store.pool).await?;
            }
            _ => {
                let delay = (1i32 << message.attempts.clamp(0, 8)).min(300);
                sqlx::query("UPDATE discord_outbox SET attempts=attempts+1,next_attempt_at=now()+make_interval(secs => $2::double precision) WHERE id=$1")
                    .bind(message.id).bind(f64::from(delay)).execute(&self.store.pool).await?;
                tracing::warn!(
                    outbox_id = message.id,
                    retry_seconds = delay,
                    "Discord delivery failed; retaining message"
                );
            }
        }
        Ok(())
    }
}
