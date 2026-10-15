use crate::sessions::Manager;
use anyhow::{Context, Result, bail};
use serenity::all::*;

pub fn definitions() -> Vec<CreateCommand> {
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
            .description("Start a Codex task in its own thread")
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
        CreateCommand::new("status").description("Show this task's state"),
        CreateCommand::new("stop").description("Interrupt this task's running turn"),
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
                        "• {} — **{}**, last turn: {}{}",
                        s.discord_thread_id
                            .as_ref()
                            .map(|id| format!("<#{}>", id))
                            .unwrap_or_else(|| format!("`{}`", s.id)),
                        s.status,
                        s.last_outcome.as_deref().unwrap_or("none"),
                        if s.repository_claimed {
                            " (repository reserved)"
                        } else {
                            ""
                        }
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
            let s = manager
                .store
                .by_discord(c.channel_id.get())
                .await?
                .context("Use /status inside a task thread")?;
            anyhow::ensure!(s.guild_id == guild.to_string(), "Wrong guild");
            let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_requests WHERE session_id=$1 AND state IN ('pending','responding')").bind(s.id).fetch_one(&manager.store.pool).await?;
            let queued: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NULL",
            )
            .bind(s.id)
            .fetch_one(&manager.store.pool)
            .await?;
            Ok(format!(
                "State: **{}**\nPermissions: **{}**\nLatest turn: {}\nRepository reserved: {}\nPending requests: {} · Undelivered messages: {}\nUpdated: {}{}",
                s.status,
                manager.config.approvals_reviewer.label(),
                s.last_outcome.as_deref().unwrap_or("none"),
                s.repository_claimed,
                pending,
                queued,
                s.updated_at,
                s.last_error.map(|e| format!("\n{e}")).unwrap_or_default()
            ))
        }
        "stop" => {
            manager
                .stop(c.user.id.get(), guild, c.channel_id.get())
                .await
        }
        _ => bail!("Unknown command"),
    }
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
