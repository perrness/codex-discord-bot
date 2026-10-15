use crate::sessions::Manager;
use anyhow::{Context, Result, bail};
use serenity::all::*;

pub fn definitions() -> Vec<CreateCommand> {
    let session = || {
        CreateCommandOption::new(
            CommandOptionType::String,
            "session",
            "Session ID from /sessions (optional inside its channel)",
        )
        .max_length(36)
    };
    let name = || {
        CreateCommandOption::new(
            CommandOptionType::String,
            "name",
            "Repository name under REPOS_ROOT",
        )
        .required(true)
        .max_length(64)
    };
    vec![
        CreateCommand::new("repo")
            .description("Register and list repositories")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "add",
                    "Register an existing repository",
                )
                .add_sub_option(name()),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "list",
                "List registered repositories",
            )),
        CreateCommand::new("project")
            .description("Create a new Git project")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "create",
                    "Create an empty Git repository",
                )
                .add_sub_option(name()),
            ),
        CreateCommand::new("codex")
            .description("Start a Codex task in its own text channel")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "repo",
                    "Registered repository",
                )
                .required(true)
                .set_autocomplete(true),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "task",
                    "What should Codex do?",
                )
                .required(true)
                .max_length(6000),
            ),
        CreateCommand::new("sessions").description("List active and recent tasks"),
        CreateCommand::new("status")
            .description("Show task state or the control-channel overview")
            .add_option(session()),
        CreateCommand::new("stop")
            .description("Interrupt a task's running turn")
            .add_option(session()),
        CreateCommand::new("complete")
            .description(
                "Confirm the task, validation, and any integration/deployment are finished",
            )
            .add_option(session()),
        CreateCommand::new("dismiss")
            .description(
                "Dismiss a stopped task; clean up its text channel or hide its legacy thread",
            )
            .add_option(session()),
        CreateCommand::new("reconnect")
            .description("Restore a missing task channel without replaying Codex work")
            .add_option(session().required(true)),
    ]
    .into_iter()
    .map(|c| c.contexts(vec![InteractionContext::Guild]))
    .collect()
}

pub async fn execute(manager: &Manager, c: &CommandInteraction) -> Result<String> {
    let guild = manager.authorize(c.user.id.get(), c.guild_id.map(|g| g.get()))?;
    let options = c.data.options();
    match c.data.name.as_str() {
        "repo" | "project" => {
            let sub = options.first().context("Missing subcommand")?;
            let ResolvedValue::SubCommand(fields) = &sub.value else {
                bail!("Invalid subcommand");
            };
            if c.data.name == "repo" && sub.name == "list" {
                let repos = manager.store.repositories().await?;
                return Ok(if repos.is_empty() {
                    "No repositories registered. Use /repo add.".into()
                } else {
                    repos
                        .iter()
                        .map(|r| format!("• `{}`", r.name))
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            }
            let name = string(fields, "name")?;
            let create = c.data.name == "project" && sub.name == "create";
            anyhow::ensure!(
                create || c.data.name == "repo" && sub.name == "add",
                "Unknown subcommand"
            );
            anyhow::ensure!(
                manager
                    .store
                    .receipt(
                        &c.id.to_string(),
                        if create { "project" } else { "repository" },
                        None
                    )
                    .await?,
                "Command already handled"
            );
            let result = manager.register_repository(name, create).await;
            manager
                .store
                .receipt_state(
                    &c.id.to_string(),
                    if result.is_ok() {
                        "accepted"
                    } else {
                        "rejected"
                    },
                )
                .await?;
            result
        }
        "codex" => {
            manager
                .create_task(
                    c.user.id.get(),
                    guild,
                    &c.id.to_string(),
                    string(&options, "repo")?,
                    string(&options, "task")?,
                )
                .await
        }
        "sessions" => {
            let sessions = manager.store.sessions(20).await?;
            let lines: Vec<_> = sessions
                .iter()
                .filter(|s| s.guild_id == guild.to_string())
                .map(|s| {
                    format!(
                        "• {} — **{}** / {}{}\n  Session: `{}`",
                        s.channel_id()
                            .filter(|_| !matches!(s.channel_state.as_str(), "missing" | "deleted"))
                            .map(|id| format!("<#{}>", id))
                            .unwrap_or_else(|| format!("`{}`", s.id)),
                        s.status,
                        s.channel_state,
                        if s.repository_claimed {
                            " (repository reserved)"
                        } else {
                            ""
                        },
                        s.id
                    )
                })
                .collect();
            Ok(if lines.is_empty() {
                "No sessions yet.".into()
            } else {
                lines.join("\n")
            })
        }
        "status" => {
            let id = optional_session(&options)?;
            if id.is_none()
                && manager
                    .store
                    .by_discord(c.channel_id.get())
                    .await?
                    .is_none()
            {
                let active: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE guild_id=$1 AND (repository_claimed OR channel_state IN ('active','provisioning','missing'))")
                    .bind(guild.to_string()).fetch_one(&manager.store.pool).await?;
                let cleanup: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE guild_id=$1 AND channel_kind='text' AND discord_channel_id IS NOT NULL AND channel_state IN ('completed','dismissed')")
                    .bind(guild.to_string()).fetch_one(&manager.store.pool).await?;
                return Ok(format!(
                    "Active sessions: {active}\nCompleted channels awaiting cleanup: {cleanup}\nRetention: {} hours\nUse /codex to start work and /sessions to find session IDs.",
                    manager.config.channel_retention.as_secs() / 3600
                ));
            }
            let s = manager
                .task_for_user(c.user.id.get(), guild, c.channel_id.get(), id)
                .await?;
            let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_requests WHERE session_id=$1 AND state IN ('pending','responding')").bind(s.id).fetch_one(&manager.store.pool).await?;
            let queued: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NULL",
            )
            .bind(s.id)
            .fetch_one(&manager.store.pool)
            .await?;
            Ok(format!(
                "State: **{}** · Channel: **{}**\nPermissions: **{}**\nLatest turn: {}\nRepository reserved: {}\nPending requests: {} · Undelivered messages: {}\nUpdated: {}{}{}{}",
                s.status,
                s.channel_state,
                manager.config.approvals_reviewer.label(),
                s.last_outcome.as_deref().unwrap_or("none"),
                s.repository_claimed,
                pending,
                queued,
                s.updated_at,
                s.last_error
                    .as_ref()
                    .map(|e| format!("\n{e}"))
                    .unwrap_or_default(),
                s.discord_error
                    .as_ref()
                    .map(|e| format!("\nDiscord: {e}"))
                    .unwrap_or_default(),
                s.channel_id()
                    .zip(s.status_message_id.as_deref())
                    .map(|(channel, message)| format!(
                        "\n[Task status](https://discord.com/channels/{guild}/{channel}/{message})"
                    ))
                    .unwrap_or_default()
            ))
        }
        "stop" => {
            if let Some(id) = optional_session(&options)? {
                return manager.stop_session(c.user.id.get(), guild, id).await;
            }
            manager
                .stop(c.user.id.get(), guild, c.channel_id.get())
                .await
        }
        "complete" | "dismiss" | "reconnect" => {
            let s = manager
                .task_for_user(
                    c.user.id.get(),
                    guild,
                    c.channel_id.get(),
                    optional_session(&options)?,
                )
                .await?;
            if c.data.name == "reconnect" {
                manager
                    .reconnect_channel(c.user.id.get(), guild, s.id)
                    .await
            } else {
                manager
                    .finish_task(c.user.id.get(), guild, s.id, c.data.name == "dismiss")
                    .await
            }
        }
        _ => bail!("Unknown command"),
    }
}

fn optional_session(options: &[ResolvedOption<'_>]) -> Result<Option<uuid::Uuid>> {
    options
        .iter()
        .find(|o| o.name == "session")
        .map(|_| {
            string(options, "session")?
                .parse()
                .context("Invalid session ID")
        })
        .transpose()
}

fn string<'a>(options: &'a [ResolvedOption<'a>], name: &str) -> Result<&'a str> {
    options
        .iter()
        .find_map(|o| {
            if o.name == name {
                if let ResolvedValue::String(s) = o.value {
                    Some(s)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .context("Missing command argument")
}
