use super::{
    errors::{self, Operation},
    render,
};
use crate::{persistence::Session, sessions::Manager};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde_json::{Value, json};
use serenity::all::*;
use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use uuid::Uuid;

fn marker(id: Uuid) -> String {
    format!("codex-task:{id}")
}
fn status_marker(id: Uuid) -> String {
    format!("Session: {id}")
}

fn ensure_category_permissions(channel: &GuildChannel, category: &GuildChannel) -> Result<()> {
    ensure!(
        channel.parent_id == Some(category.id)
            && channel.permission_overwrites.len() == category.permission_overwrites.len()
            && category
                .permission_overwrites
                .iter()
                .all(|p| channel.permission_overwrites.contains(p)),
        "Created channel did not retain the active category's permissions; inspect it before using this task"
    );
    Ok(())
}

/// ASCII slugs avoid Discord's Unicode normalization changing collision checks.
pub fn channel_name(
    repo: &str,
    issue: Option<u64>,
    task: &str,
    id: Uuid,
    taken: &HashSet<String>,
) -> String {
    fn slug(value: &str) -> String {
        value
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .to_ascii_lowercase()
    }
    let title = slug(task);
    let base = format!(
        "{}{}-{}",
        slug(repo),
        issue.map(|n| format!("-{n}")).unwrap_or_default(),
        if title.is_empty() { "task" } else { &title }
    );
    let base = render::short(base.trim_matches('-'), 100)
        .trim_end_matches('-')
        .to_owned();
    if !taken.contains(&base) && base != "codex-control" {
        return base;
    }
    // Use the durable session identity for collisions, then a checked numeric suffix.
    let suffix = &id.simple().to_string()[..8];
    for n in 0.. {
        let tail = if n == 0 {
            format!("-{suffix}")
        } else {
            format!("-{suffix}-{n}")
        };
        let candidate = format!(
            "{}{}",
            render::short(&base, 100 - tail.len()).trim_end_matches('-'),
            tail
        );
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

pub(crate) fn reported_plan(value: &Value) -> Value {
    json!(
        value
            .as_array()
            .into_iter()
            .flatten()
            .take(20)
            .filter_map(|step| {
                let text = step["step"].as_str()?;
                let state = step["status"].as_str()?;
                matches!(state, "pending" | "inProgress" | "completed")
                    .then(|| json!({"step": render::truncate(text, 120), "status": state}))
            })
            .collect::<Vec<_>>()
    )
}

fn status_text(s: &Session, repo: &str) -> String {
    let state = if s.terminal_task() {
        s.channel_state.as_str()
    } else if s.status == "idle" && s.codex_thread_id.is_none() {
        "Setup recovered; send a new task message to begin"
    } else if s.status == "idle" {
        match s.last_outcome.as_deref() {
            Some("failed") => "Failed — needs attention",
            Some("interrupted" | "unknown") => "Paused — needs attention",
            _ => "Awaiting your response or /complete",
        }
    } else {
        &s.status
    };
    let mut text = format!("Repository: **{repo}**\nStatus: **{state}**");
    if !s.task_title.is_empty() {
        text.push_str(&format!("\nTask: {}", render::truncate(&s.task_title, 300)));
    }
    if let Some(plan) = s.status_snapshot["plan"]
        .as_array()
        .filter(|p| !p.is_empty())
    {
        text.push_str("\n\nProgress (Codex plan):");
        for step in plan.iter().take(10) {
            let label = match step["status"].as_str() {
                Some("completed") => "Complete",
                Some("inProgress") => "Running",
                _ => "Pending",
            };
            text.push_str(&format!(
                "\n• {}: {label}",
                render::truncate(step["step"].as_str().unwrap_or(""), 90)
            ));
        }
        if plan.len() > 10 {
            text.push_str("\n…additional steps in the Codex plan");
        }
    }
    if let Some(activity) = s.status_snapshot["activity"].as_str() {
        text.push_str(&format!(
            "\n\nLast activity: {}",
            render::truncate(activity, 300)
        ));
    }
    if let Some(error) = &s.last_error {
        text.push_str(&format!("\n\n{}", render::truncate(error, 250)));
    }
    if s.terminal_task() {
        if let Some(result) = s.status_snapshot["response"].as_str() {
            text.push_str(&format!("\n\nResult: {}", render::truncate(result, 500)));
        }
        if let Some(date) = s.delete_after {
            text.push_str(&format!(
                "\n\nScheduled deletion: <t:{}:R>",
                date.timestamp()
            ));
        }
    }
    render::truncate(&text, 3500)
}

pub(crate) fn discord_error_code(error: &anyhow::Error) -> Option<isize> {
    errors::code(error)
}

pub(crate) fn retry_seconds(attempts: i32) -> i32 {
    (5 * (1i32 << attempts.clamp(0, 6))).min(300)
}

impl Manager {
    pub async fn prepare_discord(&self) -> Result<()> {
        let result = self.prepare_discord_inner().await;
        if let Err(error) = &result {
            self.log_discord_failure(None, error);
        }
        result
    }

    async fn prepare_discord_inner(&self) -> Result<()> {
        let active = ChannelId::new(self.config.active_category_id)
            .to_channel(&self.http)
            .await
            .context(Operation("get_active_category"))?
            .guild()
            .context("Active category is not in a guild")?;
        let completed = ChannelId::new(self.config.completed_category_id)
            .to_channel(&self.http)
            .await
            .context(Operation("get_completed_category"))?
            .guild()
            .context("Completed category is not in a guild")?;
        ensure!(
            active.kind == ChannelType::Category
                && completed.kind == ChannelType::Category
                && active.guild_id == completed.guild_id,
            "Configure two categories in the same guild"
        );
        ensure!(
            self.config
                .access
                .guild
                .is_none_or(|id| id == active.guild_id.get()),
            "Categories belong to the wrong guild"
        );
        self.discord_guild
            .store(active.guild_id.get(), Ordering::Relaxed);
        let bot = self
            .http
            .get_current_user()
            .await
            .context(Operation("get_bot_user"))?;
        self.bot_user.store(bot.id.get(), Ordering::Relaxed);
        let guild = active
            .guild_id
            .to_partial_guild(&self.http)
            .await
            .context(Operation("get_guild_permissions"))?;
        let member = active
            .guild_id
            .member(&self.http, bot.id)
            .await
            .context(Operation("get_bot_member"))?;
        ensure!(
            guild
                .member_permissions(&member)
                .contains(Permissions::MANAGE_CHANNELS),
            "Missing guild permission: Manage Channels is required to create task channels"
        );
        let required = Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::READ_MESSAGE_HISTORY
            | Permissions::MANAGE_CHANNELS
            | Permissions::MANAGE_MESSAGES
            | Permissions::EMBED_LINKS
            | Permissions::ATTACH_FILES;
        for category in [&active, &completed] {
            ensure!(
                guild
                    .user_permissions_in(category, &member)
                    .contains(required),
                "Missing task-channel permissions in category {}: View/Send/History, Manage Channels/Messages, Embed Links, Attach Files are required",
                category.id
            );
        }
        let configured = self.control_channel.load(Ordering::Relaxed);
        let control = if configured != 0 {
            ChannelId::new(configured)
                .to_channel(&self.http)
                .await
                .context(Operation("get_control_channel"))?
                .guild()
                .context("Control must be a guild text channel")?
        } else {
            let channels = active
                .guild_id
                .channels(&self.http)
                .await
                .context(Operation("list_control_channels"))?;
            let matches: Vec<_> = channels
                .values()
                .filter(|c| c.kind == ChannelType::Text && c.name == "codex-control")
                .collect();
            ensure!(
                matches.len() <= 1,
                "Multiple codex-control channels exist; set DISCORD_CONTROL_CHANNEL_ID"
            );
            if let Some(channel) = matches.first() {
                (*channel).clone()
            } else {
                let channel = active
                    .guild_id
                    .create_channel(
                        &self.http,
                        CreateChannel::new("codex-control")
                            .kind(ChannelType::Text)
                            .category(active.id),
                    )
                    .await
                    .context(Operation("create_control_channel"))?;
                ensure_category_permissions(&channel, &active)?;
                channel
            }
        };
        ensure!(
            control.kind == ChannelType::Text && control.guild_id == active.guild_id,
            "Control must be a text channel in the configured guild"
        );
        ensure!(
            self.store.by_discord(control.id.get()).await?.is_none(),
            "Control channel is already assigned to a task"
        );
        ensure!(
            guild
                .user_permissions_in(&control, &member)
                .contains(required),
            "Control channel is missing required bot permissions"
        );
        self.control_channel
            .store(control.id.get(), Ordering::Relaxed);
        Ok(())
    }

    pub async fn system_notice(&self, text: &str) {
        let id = self.control_channel.load(Ordering::Relaxed);
        if id == 0 {
            return;
        }
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            ChannelId::new(id).send_message(
                &self.http,
                CreateMessage::new()
                    .content(render::truncate(text, 1800))
                    .allowed_mentions(CreateAllowedMentions::new()),
            ),
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            let error = match result {
                Ok(Err(error)) => anyhow::Error::from(error),
                Err(error) => anyhow::Error::from(error),
                Ok(Ok(_)) => unreachable!(),
            }
            .context(Operation("send_system_notice"));
            self.log_discord_failure(None, &error);
        }
    }

    /// Called once for a new task, or on an explicit /reconnect. Never blindly retried on restart.
    pub(crate) async fn provision_channel(
        &self,
        session: &Session,
        repo: &str,
    ) -> Result<ChannelId> {
        let _guard = self.provisioning_gate.lock().await;
        let result = self.provision_channel_inner(session, repo).await;
        if let Err(error) = &result {
            let current = match self.store.session(session.id).await {
                Ok(current) => current,
                Err(persistence_error) => {
                    self.log_discord_failure(Some(session.id), error);
                    return Err(persistence_error.context("Could not record channel setup failure; reconcile its session marker before retrying"));
                }
            };
            self.channel_failure(&current, error).await?;
        }
        result.map_err(|error| {
            let diagnostic = self.discord_diagnostic(&error);
            error.context(format!("{diagnostic}. Use /reconnect session:{} after fixing the error, or /dismiss this failed setup", session.id))
        })
    }

    async fn provision_channel_inner(&self, session: &Session, repo: &str) -> Result<ChannelId> {
        let guild = GuildId::new(session.guild_id.parse()?);
        let channels = guild
            .channels(&self.http)
            .await
            .context(Operation("list_task_channels"))?;
        let matches: Vec<_> = channels
            .values()
            .filter(|c| {
                c.guild_id == guild
                    && c.kind == ChannelType::Text
                    && c.topic.as_deref() == Some(&marker(session.id))
            })
            .collect();
        ensure!(
            matches.len() <= 1,
            "Multiple channels carry this session marker; inspect them before reconnecting"
        );
        let channel = if let Some(existing) = matches.first() {
            (*existing).clone()
        } else {
            let category = channels
                .get(&ChannelId::new(self.config.active_category_id))
                .context("Active category is missing")?;
            ensure!(
                category.kind == ChannelType::Category,
                "Active category is not a category"
            );
            let taken = channels.values().map(|c| c.name.clone()).collect();
            // Omit overwrites: Discord copies the parent's permissions on creation. Rewriting
            // them can require permissions the bot lacks, including administrator-only bits.
            guild
                .create_channel(
                    &self.http,
                    CreateChannel::new(channel_name(
                        repo,
                        None,
                        &session.task_title,
                        session.id,
                        &taken,
                    ))
                    .kind(ChannelType::Text)
                    .category(category.id)
                    .topic(marker(session.id))
                    .audit_log_reason("Create Codex task channel"),
                )
                .await
                .context(Operation("create_task_channel"))?
        };
        // A channel with this marker must be adopted or rejected, never duplicated. For setups
        // without a conversation, verify privacy before persisting an active presentation.
        if matches.is_empty() || session.codex_thread_id.is_none() {
            let category = channels
                .get(&ChannelId::new(self.config.active_category_id))
                .context("Active category is missing")?;
            ensure_category_permissions(&channel, category)?;
        }
        self.bind_task_channel(session, &channel).await?;
        Ok(channel.id)
    }

    pub(crate) async fn bind_task_channel(
        &self,
        session: &Session,
        channel: &GuildChannel,
    ) -> Result<()> {
        ensure!(
            channel.guild_id.to_string() == session.guild_id
                && channel.kind == ChannelType::Text
                && channel.topic.as_deref() == Some(&marker(session.id))
                && channel.id.get() != self.control_channel.load(Ordering::Relaxed)
                && channel.id.to_string() != session.parent_channel_id,
            "Invalid task channel ownership; inspect its guild, type and topic"
        );
        let mut tx = self.store.pool.begin().await?;
        let changed = session.channel_id() != Some(channel.id.to_string().as_str());
        sqlx::query("UPDATE sessions SET discord_channel_id=$2,channel_state='active',discord_error=NULL,discord_failures=0,discord_retry_at=now(),last_seen_message_id=CASE WHEN $3 THEN $2 ELSE last_seen_message_id END,status_message_id=CASE WHEN $3 THEN NULL ELSE status_message_id END,status_delivered_revision=CASE WHEN $3 THEN -1 ELSE status_delivered_revision END,updated_at=now() WHERE id=$1")
            .bind(session.id).bind(channel.id.to_string()).bind(changed).execute(&mut *tx).await?;
        if changed {
            // Routing and redisplay must commit together. A failed status send or restart must
            // not strand a live approval in the deleted channel or replay an answered request.
            sqlx::query("DELETE FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NULL AND (dedupe_key LIKE 'task-completed:%' OR dedupe_key LIKE 'task-dismissed:%')")
                .bind(session.id).execute(&mut *tx).await?;
            sqlx::query("UPDATE discord_outbox o SET delivered_at=NULL,discord_message_id=NULL,controls_closed=false,attempts=0,next_attempt_at=now() WHERE session_id=$1 AND EXISTS(SELECT 1 FROM pending_requests p WHERE p.session_id=$1 AND p.state='pending' AND p.generation=$2 AND o.dedupe_key LIKE 'request:' || p.id::text || ':%')")
                .bind(session.id).bind(self.generation).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn task_channel(&self, session: &Session) -> Result<GuildChannel> {
        let id = ChannelId::new(
            session
                .channel_id()
                .context("Task channel has not been provisioned")?
                .parse()?,
        );
        let channel = id
            .to_channel(&self.http)
            .await
            .context(Operation("get_task_channel"))?
            .guild()
            .context("Task channel is not in a guild")?;
        ensure!(
            channel.guild_id.to_string() == session.guild_id,
            "Task channel belongs to another guild"
        );
        if session.channel_kind == "text" {
            ensure!(
                channel.kind == ChannelType::Text
                    && channel.topic.as_deref() == Some(&marker(session.id))
                    && id.get() != self.control_channel.load(Ordering::Relaxed)
                    && id.to_string() != session.parent_channel_id,
                "Task channel ownership marker is missing or invalid; inspect its topic"
            );
        } else {
            ensure!(
                matches!(
                    channel.kind,
                    ChannelType::PublicThread | ChannelType::PrivateThread
                ) && channel.parent_id.map(|p| p.to_string()).as_deref()
                    == Some(&session.parent_channel_id),
                "Legacy thread moved outside its original parent"
            );
        }
        Ok(channel)
    }

    pub(crate) async fn sync_status(&self, session: &Session) -> Result<()> {
        if session.status_delivered_revision == session.status_revision
            && session.status_message_id.is_some()
        {
            return Ok(());
        }
        let channel = self.task_channel(session).await?;
        let repo: String = sqlx::query_scalar("SELECT name FROM repositories WHERE id=$1")
            .bind(session.repository_id)
            .fetch_one(&self.store.pool)
            .await?;
        let embed = CreateEmbed::new()
            .title("Codex task")
            .description(status_text(session, &repo))
            .footer(CreateEmbedFooter::new(status_marker(session.id)));
        let mut revision = session.status_revision;
        let mut message_id = session
            .status_message_id
            .as_ref()
            .map(|id| id.parse::<u64>())
            .transpose()?
            .map(MessageId::new);
        if let Some(id) = message_id {
            match channel
                .id
                .edit_message(
                    &self.http,
                    id,
                    EditMessage::new()
                        .embed(embed.clone())
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    let error =
                        anyhow::Error::from(error).context(Operation("edit_status_message"));
                    if discord_error_code(&error) == Some(10008) {
                        revision += 1;
                        sqlx::query("UPDATE sessions SET status_message_id=NULL,status_revision=$2,status_delivered_revision=-1 WHERE id=$1")
                            .bind(session.id).bind(revision).execute(&self.store.pool).await?;
                        message_id = None;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        if message_id.is_none() {
            // Exhaust history before creating: nonce deduplication alone expires after a short window.
            let mut before = None;
            loop {
                let mut query = GetMessages::new().limit(100);
                if let Some(id) = before {
                    query = query.before(id);
                }
                let messages = channel
                    .id
                    .messages(&self.http, query)
                    .await
                    .context(Operation("list_status_messages"))?;
                if let Some(message) = messages.iter().find(|m| {
                    m.author.id.get() == self.bot_user.load(Ordering::Relaxed)
                        && m.embeds.iter().any(|e| {
                            e.footer
                                .as_ref()
                                .is_some_and(|f| f.text == status_marker(session.id))
                        })
                }) {
                    message_id = Some(message.id);
                    channel
                        .id
                        .edit_message(
                            &self.http,
                            message.id,
                            EditMessage::new()
                                .embed(embed.clone())
                                .allowed_mentions(CreateAllowedMentions::new()),
                        )
                        .await
                        .context(Operation("edit_recovered_status_message"))?;
                    break;
                }
                before = messages.iter().map(|m| m.id).min();
                if messages.len() < 100 {
                    break;
                }
            }
            if message_id.is_none() {
                let sent = channel
                    .id
                    .send_message(
                        &self.http,
                        CreateMessage::new()
                            .embed(embed)
                            .allowed_mentions(CreateAllowedMentions::new())
                            .nonce(render::message_nonce("status", channel.id.get(), revision))
                            .enforce_nonce(true),
                    )
                    .await
                    .context(Operation("create_status_message"))?;
                message_id = Some(sent.id);
            }
        }
        sqlx::query(
            "UPDATE sessions SET status_message_id=$2,status_delivered_revision=$3 WHERE id=$1",
        )
        .bind(session.id)
        .bind(message_id.unwrap().to_string())
        .bind(revision)
        .execute(&self.store.pool)
        .await?;
        // A status link in /status works even without Discord's optional Pin Messages permission.
        if session.status_message_id.is_none()
            && let Err(error) = channel.id.pin(&self.http, message_id.unwrap()).await
        {
            self.log_discord_failure(
                Some(session.id),
                &anyhow::Error::from(error).context(Operation("pin_status_message")),
            );
        }
        Ok(())
    }

    pub async fn reconcile_channels(&self) -> Result<()> {
        // Also finds a channel created immediately before a crash prevented persisting its ID.
        let guild = GuildId::new(self.discord_guild.load(Ordering::Relaxed));
        let channels = guild
            .channels(&self.http)
            .await
            .context(Operation("list_channels_for_recovery"))?;
        let sessions: Vec<Session> = sqlx::query_as("SELECT * FROM sessions WHERE channel_kind='text' AND discord_channel_id IS NULL AND channel_state='provisioning'")
            .fetch_all(&self.store.pool).await?;
        for session in sessions {
            let gate = self.gate(session.id);
            let _guard = gate.lock().await;
            let session = self.store.session(session.id).await?;
            if session.channel_id().is_some() || session.channel_state != "provisioning" {
                continue;
            }
            let found: Vec<_> = channels
                .values()
                .filter(|c| {
                    c.guild_id.to_string() == session.guild_id
                        && c.kind == ChannelType::Text
                        && c.topic.as_deref() == Some(&marker(session.id))
                })
                .collect();
            if found.len() == 1 {
                let result = async {
                    if session.codex_thread_id.is_none() {
                        let category = channels
                            .get(&ChannelId::new(self.config.active_category_id))
                            .context("Active category is missing")?;
                        ensure_category_permissions(found[0], category)?;
                    }
                    self.bind_task_channel(&session, found[0]).await
                }
                .await;
                if let Err(error) = result {
                    self.channel_failure(&session, &error).await?;
                }
            } else {
                // Preserve a recorded API failure instead of replacing it with a generic guess.
                if found.is_empty() {
                    sqlx::query("UPDATE sessions SET channel_state='missing' WHERE id=$1")
                        .bind(session.id)
                        .execute(&self.store.pool)
                        .await?;
                }
                if session.discord_error.is_none() || found.len() > 1 {
                    let message = if found.is_empty() {
                        "No channel found after interrupted setup; use /reconnect with this session ID or /dismiss the failed setup"
                    } else {
                        "Multiple channels carry this session marker; inspect them before reconnecting"
                    };
                    self.channel_failure(
                        &session,
                        &anyhow::anyhow!(message).context(Operation("reconcile_channel_creation")),
                    )
                    .await?;
                }
            }
        }
        let sessions: Vec<Session> = sqlx::query_as("SELECT * FROM sessions WHERE COALESCE(discord_channel_id,discord_thread_id) IS NOT NULL AND channel_state NOT IN ('deleted','missing')")
            .fetch_all(&self.store.pool).await?;
        for s in sessions {
            let gate = self.gate(s.id);
            let _guard = gate.lock().await;
            let s = self.store.session(s.id).await?;
            if matches!(s.channel_state.as_str(), "missing" | "deleted") {
                continue;
            }
            let result = async {
                let channel = self.task_channel(&s).await?;
                if let Some(id) = &s.status_message_id {
                    match channel.id.message(&self.http, MessageId::new(id.parse()?)).await {
                        Ok(message) => ensure!(message.author.id.get() == self.bot_user.load(Ordering::Relaxed)
                            && message.embeds.iter().any(|e| e.footer.as_ref().is_some_and(|f| f.text == status_marker(s.id))), "Stored status message does not belong to this task"),
                        Err(error) => {
                            let error = anyhow::Error::from(error).context(Operation("get_status_message"));
                            if discord_error_code(&error) == Some(10008) {
                                sqlx::query("UPDATE sessions SET status_message_id=NULL,status_revision=status_revision+1,status_delivered_revision=-1 WHERE id=$1")
                                    .bind(s.id).execute(&self.store.pool).await?;
                            } else { return Err(error); }
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }.await;
            if let Err(error) = result {
                self.channel_failure(&s, &error).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn channel_failure(
        &self,
        session: &Session,
        error: &anyhow::Error,
    ) -> Result<()> {
        self.log_discord_failure(Some(session.id), error);
        let missing = (session.channel_id().is_some() && discord_error_code(error) == Some(10003))
            || (session.channel_id().is_none() && errors::creation_rejected(error));
        let mut message = self.discord_diagnostic(error);
        if missing {
            message.push_str(". Channel unavailable; use /reconnect with this session ID. Codex state is preserved.");
        } else if session.channel_id().is_none() {
            message.push_str(". No create request will be retried automatically; /reconnect searches for an existing channel first.");
        }
        sqlx::query("UPDATE sessions SET channel_state=CASE WHEN $2 THEN 'missing' ELSE channel_state END,discord_error=$3,discord_failures=discord_failures+1,discord_retry_at=now()+make_interval(secs => $4::double precision) WHERE id=$1")
            .bind(session.id).bind(missing).bind(&message).bind(f64::from(retry_seconds(session.discord_failures))).execute(&self.store.pool).await?;
        if session.discord_error.as_deref() != Some(&message) {
            self.system_notice(&format!("Task `{}` needs attention: {message}", session.id))
                .await;
        }
        Ok(())
    }

    pub async fn maintain_channels(self: Arc<Self>) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! { _ = self.fatal.cancelled() => return Ok(()), _ = tick.tick() => {} }
            if !self.discord_ready.load(Ordering::Relaxed) {
                continue;
            }
            self.maintain_channels_once().await?;
        }
    }

    pub async fn maintain_channels_once(&self) -> Result<()> {
        let sessions: Vec<Session> = sqlx::query_as("SELECT * FROM sessions WHERE COALESCE(discord_channel_id,discord_thread_id) IS NOT NULL AND channel_state NOT IN ('missing','deleted') AND discord_retry_at<=now() AND (status_revision<>status_delivered_revision OR discord_checked_at IS NULL OR discord_checked_at<now()-interval '1 minute') ORDER BY discord_checked_at NULLS FIRST LIMIT 20")
            .fetch_all(&self.store.pool).await?;
        for old in sessions {
            let gate = self.gate(old.id);
            let _guard = gate.lock().await;
            let session = self.store.session(old.id).await?;
            if matches!(session.channel_state.as_str(), "missing" | "deleted") {
                continue;
            }
            // Keep slow/unreachable Discord from indefinitely blocking approvals and turn events.
            let result =
                tokio::time::timeout(Duration::from_secs(20), self.maintain_channel(&session))
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("Discord channel maintenance timed out")
                            .context(Operation("maintain_task_channel")))
                    });
            match result {
                Ok(()) => {
                    sqlx::query("UPDATE sessions SET discord_checked_at=now(),discord_retry_at=now()+interval '5 seconds',discord_failures=0,discord_error=NULL WHERE id=$1")
                        .bind(session.id).execute(&self.store.pool).await?;
                }
                Err(error) => self.channel_failure(&session, &error).await?,
            }
        }
        Ok(())
    }

    async fn maintain_channel(&self, session: &Session) -> Result<()> {
        let channel = self.task_channel(session).await?;
        if channel
            .thread_metadata
            .is_some_and(|m| m.archived && !m.locked)
        {
            channel
                .id
                .edit_thread(&self.http, EditThread::new().archived(false))
                .await
                .context(Operation("unarchive_legacy_thread"))?;
        }
        self.sync_status(session).await?;
        if session.channel_kind != "text" {
            return Ok(());
        }
        let current = self.store.session(session.id).await?;
        if current.terminal_task() && self.store.undelivered(current.id).await? {
            return Ok(());
        }
        let target = ChannelId::new(if current.terminal_task() {
            self.config.completed_category_id
        } else {
            self.config.active_category_id
        });
        if channel.parent_id != Some(target) {
            // Keep the task's original visibility/overwrites when moving between categories.
            channel
                .id
                .edit(
                    &self.http,
                    EditChannel::new()
                        .category(target)
                        .audit_log_reason("Update Codex task lifecycle"),
                )
                .await
                .context(Operation("move_task_channel"))?;
        }
        if current.cleanup_due(Utc::now(), self.control_channel.load(Ordering::Relaxed))
            && !self.store.unresolved(current.id).await?
            && !self.store.undelivered(current.id).await?
            && !self.unread_task_input(&current, channel.id).await?
        {
            // This gate also guards normal input, completion, and interactive responses.
            channel
                .id
                .delete(&self.http)
                .await
                .context(Operation("delete_task_channel"))?;
            sqlx::query("UPDATE sessions SET channel_state='deleted' WHERE id=$1")
                .bind(current.id)
                .execute(&self.store.pool)
                .await?;
        }
        Ok(())
    }

    async fn unread_task_input(&self, session: &Session, channel: ChannelId) -> Result<bool> {
        let date = session
            .completed_at
            .context("Missing completion timestamp")?;
        let snowflake = ((date.timestamp_millis() - 1_420_070_400_000).max(0) as u64) << 22;
        // Fetch the oldest page after completion; any unresolved input stops deletion.
        let messages = channel
            .messages(
                &self.http,
                GetMessages::new()
                    .after(MessageId::new(snowflake.max(1)))
                    .limit(100),
            )
            .await
            .context(Operation("list_messages_before_cleanup"))?;
        for message in &messages {
            if self.config.access.allows(
                message.author.id.get(),
                Some(session.guild_id.parse()?),
                message.author.bot || message.webhook_id.is_some(),
            ) {
                let handled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM inbound_requests WHERE discord_id=$1 AND state IN ('accepted','rejected'))")
                    .bind(message.id.to_string()).fetch_one(&self.store.pool).await?;
                if !handled {
                    return Ok(true);
                }
            }
        }
        // More history needs catch-up; never assume a truncated page proves there is no input.
        Ok(messages.len() == 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_readable_valid_and_collision_checked() {
        let id = Uuid::nil();
        assert_eq!(
            channel_name("FantasyAI", Some(42), "Player Stats!", id, &HashSet::new()),
            "fantasyai-42-player-stats"
        );
        let mut taken = HashSet::from(["editor-fix-lsp".into()]);
        let a = channel_name("Editor", None, "Fix LSP", id, &taken);
        assert_eq!(a, "editor-fix-lsp-00000000");
        taken.insert(a.clone());
        assert_ne!(channel_name("Editor", None, "Fix LSP", id, &taken), a);
        for title in ["💻 中文", &"ab-".repeat(100), "---", "../@everyone"] {
            let name = channel_name("repo", None, title, id, &taken);
            assert!(!name.is_empty() && name.len() <= 100);
            assert!(
                name.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            );
        }
    }

    #[test]
    fn plans_use_only_reported_steps_and_known_states() {
        assert_eq!(reported_plan(&Value::Null), json!([]));
        assert_eq!(
            reported_plan(
                &json!([{"step":"Run tests","status":"inProgress"},{"step":"Deploy","status":"invented"}])
            ),
            json!([{"step":"Run tests","status":"inProgress"}])
        );
    }

    #[test]
    fn retries_are_bounded() {
        assert_eq!(retry_seconds(0), 5);
        assert_eq!(retry_seconds(2), 20);
        assert_eq!(retry_seconds(i32::MAX), 300);
    }
}
