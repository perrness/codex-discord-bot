pub mod channels;
mod commands;
pub(crate) mod errors;
mod outbox;
pub mod render;

use crate::{codex::protocol::InteractiveParams, sessions::Manager};
use anyhow::{Context as _, Result, ensure};
use serenity::all::*;
use std::sync::{Arc, atomic::Ordering};
use uuid::Uuid;

pub struct Handler {
    pub manager: Arc<Manager>,
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, _: Ready) {
        self.manager.discord_ready.store(false, Ordering::Relaxed);
        let result = match self.manager.config.access.guild {
            Some(id) => {
                GuildId::new(id)
                    .set_commands(&ctx.http, commands::definitions())
                    .await
            }
            None => Command::set_global_commands(&ctx.http, commands::definitions()).await,
        };
        if let Err(error) = result {
            self.manager.log_discord_failure(
                None,
                &anyhow::Error::from(error).context(errors::Operation("register_commands")),
            );
            self.manager.system_notice("Discord slash-command registration failed. Check application permissions and service logs.").await;
        } else {
            tracing::info!("Discord connected; commands registered");
        }
        if let Err(error) = self.manager.reconcile_channels().await {
            self.manager.log_discord_failure(None, &error);
            self.manager.system_notice("Discord channel reconciliation failed. Each task channel will be checked again before updates or cleanup; Codex execution state is preserved.").await;
        }
        if self.backfill().await.is_err() {
            tracing::warn!(
                "Could not fetch missed Discord messages; they remain available for the next reconnect"
            );
            self.manager
                .fail_closed("Discord catch-up persistence failed");
            return;
        }
        self.manager.discord_ready.store(true, Ordering::Relaxed);
    }

    async fn message(&self, _: Context, message: Message) {
        if self.process_message(message).await.is_err() {
            self.manager.fail_closed("Discord input persistence failed");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let result = match interaction {
            Interaction::Command(command) => self.command(&ctx, command).await,
            Interaction::Autocomplete(command) => self.autocomplete(&ctx, command).await,
            Interaction::Component(component) => self.component(&ctx, component).await,
            Interaction::Modal(modal) => self.modal(&ctx, modal).await,
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.manager.log_discord_failure(
                None,
                &error.context(errors::Operation("respond_to_interaction")),
            );
        }
    }
}

impl Handler {
    async fn process_message(&self, message: Message) -> Result<()> {
        let guild = message.guild_id.map(|g| g.get());
        if !self.manager.config.access.allows(
            message.author.id.get(),
            guild,
            message.author.bot || message.webhook_id.is_some(),
        ) {
            return Ok(());
        }
        let Some(session) = self
            .manager
            .store
            .by_discord(message.channel_id.get())
            .await?
        else {
            return Ok(());
        };
        if session.guild_id != guild.unwrap().to_string() {
            return Ok(());
        }
        let seen: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM inbound_requests WHERE discord_id=$1)")
                .bind(message.id.to_string())
                .fetch_one(&self.manager.store.pool)
                .await?;
        if seen {
            return Ok(());
        }
        let result = async {
            if let Some(reply) = message
                .message_reference
                .as_ref()
                .and_then(|r| r.message_id)
                && self
                    .manager
                    .answer_reply(
                        message.author.id.get(),
                        guild.unwrap(),
                        message.channel_id.get(),
                        reply.get(),
                        &message.content,
                    )
                    .await?
            {
                self.manager
                    .store
                    .receipt(&message.id.to_string(), "answer", Some(session.id))
                    .await?;
                self.manager
                    .store
                    .receipt_state(&message.id.to_string(), "accepted")
                    .await?;
                return Ok(());
            }
            self.manager
                .followup(
                    message.author.id.get(),
                    guild.unwrap(),
                    message.channel_id.get(),
                    &message.id.to_string(),
                    &message.content,
                )
                .await
        }
        .await;
        if let Err(error) = result {
            self.manager
                .store
                .receipt(&message.id.to_string(), "message", Some(session.id))
                .await?;
            sqlx::query("UPDATE inbound_requests SET state='rejected',updated_at=now() WHERE discord_id=$1 AND state='received'")
                .bind(message.id.to_string()).execute(&self.manager.store.pool).await?;
            self.manager
                .store
                .enqueue(
                    session.id,
                    &format!("input-error:{}", message.id),
                    &error.to_string(),
                    None,
                )
                .await?;
        }
        Ok(())
    }

    async fn backfill(&self) -> Result<()> {
        let sessions: Vec<crate::persistence::Session> = sqlx::query_as("SELECT * FROM sessions WHERE COALESCE(discord_channel_id,discord_thread_id) IS NOT NULL AND channel_state NOT IN ('missing','deleted') AND status <> 'unavailable'")
            .fetch_all(&self.manager.store.pool).await?;
        for session in sessions {
            let channel = ChannelId::new(session.channel_id().unwrap().parse()?);
            let mut cursor: u64 = session
                .last_seen_message_id
                .as_deref()
                .unwrap_or(session.channel_id().unwrap())
                .parse()?;
            // Bound catch-up work, never fetch unrelated channels. Remaining history resumes next connection.
            for _ in 0..10 {
                let fetched = channel
                    .messages(
                        &self.manager.http,
                        GetMessages::new().after(MessageId::new(cursor)).limit(100),
                    )
                    .await;
                let Ok(mut messages) = fetched else {
                    break;
                };
                if messages.is_empty() {
                    break;
                }
                messages.sort_by_key(|m| m.id);
                let count = messages.len();
                for message in messages {
                    cursor = cursor.max(message.id.get());
                    self.process_message(message).await?;
                }
                // Only completed history pages advance this cursor. A live message arriving
                // during catch-up must not cause older offline messages to be skipped.
                sqlx::query("UPDATE sessions SET last_seen_message_id=$2 WHERE id=$1 AND COALESCE(last_seen_message_id::numeric,0)<$2::text::numeric")
                    .bind(session.id).bind(cursor.to_string()).execute(&self.manager.store.pool).await?;
                if count < 100 {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn command(&self, ctx: &Context, command: CommandInteraction) -> Result<()> {
        if self
            .manager
            .authorize(command.user.id.get(), command.guild_id.map(|g| g.get()))
            .is_err()
        {
            command
                .create_response(&ctx.http, ephemeral("Not authorized"))
                .await?;
            return Ok(());
        }
        command.defer_ephemeral(&ctx.http).await?;
        let result = commands::execute(&self.manager, &command).await;
        let text = result.unwrap_or_else(|error| {
            self.manager.log_discord_failure(None, &error);
            error.to_string()
        });
        command
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content(render::short(&text, 1800))
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
        Ok(())
    }

    async fn autocomplete(&self, ctx: &Context, command: CommandInteraction) -> Result<()> {
        let mut choices = CreateAutocompleteResponse::new();
        if self
            .manager
            .authorize(command.user.id.get(), command.guild_id.map(|g| g.get()))
            .is_ok()
        {
            let prefix = command
                .data
                .autocomplete()
                .map(|o| o.value)
                .unwrap_or("")
                .to_ascii_lowercase();
            for repo in self
                .manager
                .store
                .repositories()
                .await?
                .into_iter()
                .filter(|r| r.name.to_ascii_lowercase().starts_with(&prefix))
                .take(25)
            {
                choices = choices.add_string_choice(&repo.name, &repo.name);
            }
        }
        command
            .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(choices))
            .await?;
        Ok(())
    }

    async fn component(&self, ctx: &Context, component: ComponentInteraction) -> Result<()> {
        let authorized = self
            .manager
            .authorize(component.user.id.get(), component.guild_id.map(|g| g.get()));
        let Ok(guild) = authorized else {
            component
                .create_response(&ctx.http, ephemeral("Not authorized"))
                .await?;
            return Ok(());
        };
        let parsed = parse_component(&component.data.custom_id);
        let Ok((id, action)) = parsed else {
            component
                .create_response(&ctx.http, ephemeral("Unknown control"))
                .await?;
            return Ok(());
        };
        if action == "answer" {
            let form = async {
                let request = self
                    .manager
                    .request_for_user(
                        component.user.id.get(),
                        guild,
                        component.channel_id.get(),
                        id,
                    )
                    .await?;
                let params: InteractiveParams =
                    serde_json::from_value(request.payload["params"].clone())?;
                ensure!(
                    !params.questions.is_empty() && params.questions.len() <= 5,
                    "Invalid question form"
                );
                let rows = params
                    .questions
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        CreateActionRow::InputText(
                            CreateInputText::new(
                                InputTextStyle::Paragraph,
                                render::short(&q.header, 45),
                                format!("q{i}"),
                            )
                            .placeholder(render::short(&q.question, 100))
                            .required(true)
                            .max_length(4000),
                        )
                    })
                    .collect();
                Ok::<_, anyhow::Error>(
                    CreateModal::new(format!("req:{id}:answer"), "Reply to Codex").components(rows),
                )
            }
            .await;
            match form {
                Ok(form) => {
                    component
                        .create_response(&ctx.http, CreateInteractionResponse::Modal(form))
                        .await?
                }
                Err(e) => {
                    component
                        .create_response(&ctx.http, ephemeral(&e.to_string()))
                        .await?
                }
            }
        } else {
            component.defer_ephemeral(&ctx.http).await?;
            let result = self
                .manager
                .answer(
                    component.user.id.get(),
                    guild,
                    component.channel_id.get(),
                    id,
                    action,
                    Vec::new(),
                )
                .await;
            component
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new()
                        .content(result.unwrap_or_else(|e| e.to_string()))
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await?;
        }
        Ok(())
    }

    async fn modal(&self, ctx: &Context, modal: ModalInteraction) -> Result<()> {
        let Ok(guild) = self
            .manager
            .authorize(modal.user.id.get(), modal.guild_id.map(|g| g.get()))
        else {
            modal
                .create_response(&ctx.http, ephemeral("Not authorized"))
                .await?;
            return Ok(());
        };
        modal.defer_ephemeral(&ctx.http).await?;
        let result = async {
            let (id, action) = parse_component(&modal.data.custom_id)?;
            ensure!(action == "answer", "Invalid form action");
            let mut fields = std::collections::BTreeMap::new();
            for row in &modal.data.components {
                for c in &row.components {
                    if let ActionRowComponent::InputText(input) = c {
                        fields.insert(
                            input.custom_id.clone(),
                            input.value.clone().unwrap_or_default(),
                        );
                    }
                }
            }
            let request = self
                .manager
                .request_for_user(modal.user.id.get(), guild, modal.channel_id.get(), id)
                .await?;
            let params: InteractiveParams =
                serde_json::from_value(request.payload["params"].clone())?;
            let answers = (0..params.questions.len())
                .map(|i| fields.remove(&format!("q{i}")).unwrap_or_default())
                .collect();
            self.manager
                .answer(
                    modal.user.id.get(),
                    guild,
                    modal.channel_id.get(),
                    id,
                    "answer",
                    answers,
                )
                .await
        }
        .await;
        modal
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content(result.unwrap_or_else(|e| e.to_string()))
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await?;
        Ok(())
    }
}

fn ephemeral(text: &str) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(render::short(text, 1800))
            .ephemeral(true)
            .allowed_mentions(CreateAllowedMentions::new()),
    )
}

fn parse_component(value: &str) -> Result<(Uuid, &str)> {
    let mut parts = value.split(':');
    ensure!(parts.next() == Some("req"), "Invalid control");
    let id = parts.next().context("Missing request")?.parse()?;
    let action = parts.next().context("Missing action")?;
    ensure!(parts.next().is_none(), "Invalid control");
    Ok((id, action))
}
