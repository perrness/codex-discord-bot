use super::render;
use crate::{persistence::OutboxMessage, sessions::Manager};
use anyhow::{Context, Result};
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
                    (s.discord_thread_id, message.discord_message_id)
                {
                    let result = tokio::time::timeout(
                        Duration::from_secs(5),
                        ChannelId::new(channel.parse()?).edit_message(
                            &self.http,
                            MessageId::new(message_id.parse()?),
                            EditMessage::new().components(Vec::new()),
                        ),
                    )
                    .await;
                    if matches!(result, Ok(Ok(_))) {
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
        let session = self.store.session(message.session_id).await?;
        let Some(thread) = session.discord_thread_id else {
            return Ok(());
        };
        let channel = ChannelId::new(thread.parse()?);
        let mut builder = CreateMessage::new()
            .content(&message.content)
            .allowed_mentions(CreateAllowedMentions::new())
            .nonce(Nonce::String(format!("outbox:{}", message.id)))
            .enforce_nonce(true);
        if let Some(id) = message.request_id {
            let mut request = self.store.pending(id).await?;
            if request.generation != self.generation {
                request.state = "expired".into();
            }
            builder = builder.components(render::controls(&request));
        }
        let send = async {
            let metadata = channel
                .to_channel(&self.http)
                .await?
                .guild()
                .context("Task channel is not a guild thread")?;
            anyhow::ensure!(
                metadata.guild_id.to_string() == session.guild_id
                    && metadata.parent_id.map(|p| p.to_string()).as_deref()
                        == Some(&session.parent_channel_id),
                "Task channel moved outside its configured parent"
            );
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
