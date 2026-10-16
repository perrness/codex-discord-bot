use anyhow::{Context, Result, ensure};
use codex_discord_bot::{
    codex::Server,
    config::{self, Config},
    discord::Handler,
    persistence::Store,
    sessions::Manager,
};
use rustix::fs::{FlockOperation, flock};
use serenity::all::{Client, GatewayIntents, Http};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, sync::Arc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("codex_discord_bot=info,serenity=warn")),
        )
        .json()
        .init();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    if mode == "migrate" {
        let store = Store::connect(&config::required("DATABASE_URL")?).await?;
        store.migrate().await?;
        tracing::info!("Migrations complete");
        return Ok(());
    }
    ensure!(mode == "run", "Usage: codex-discord-bot [run|migrate]");
    ensure!(
        !rustix::process::geteuid().is_root(),
        "Run the bot as a non-root service account"
    );
    let config = Arc::new(Config::load()?);
    config.prepare_directories()?;
    let instance = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config.state_dir.join("instance.lock"))?;
    flock(&instance, FlockOperation::NonBlockingLockExclusive)
        .context("Another bot instance owns STATE_DIR")?;
    let store = Store::connect(&config.database_url).await?;
    store.migrate().await?;
    // A dedicated connection holds a deployment-wide lock even if STATE_DIR was misconfigured.
    let mut leader = store.pool.acquire().await?;
    let elected: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(7248306189301)")
        .fetch_one(&mut *leader)
        .await?;
    ensure!(elected, "Another bot instance owns this database");
    let (mut server, events) = Server::spawn(&config).await?;
    let http = Arc::new(Http::new(&config.discord_token));
    let manager = Arc::new(Manager::new(
        config.clone(),
        store,
        server.client.clone(),
        server.generation,
        http,
    )?);
    if let Err(error) = manager.prepare_discord().await {
        server.shutdown().await;
        return Err(error.context("Discord category/control configuration failed"));
    }
    if let Err(error) = manager.recover().await {
        server.shutdown().await;
        return Err(error.context("Session recovery failed"));
    }
    manager.reconcile_integrations().await?;
    manager.store.reconcile_issues().await?;
    let intents =
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut discord = Client::builder(&config.discord_token, intents)
        .event_handler(Handler {
            manager: manager.clone(),
        })
        .await?;
    let shard_manager = discord.shard_manager.clone();
    let mut dispatch = tokio::spawn(manager.clone().dispatch(events));
    let mut delivery = tokio::spawn(manager.clone().deliver());
    let mut maintenance = tokio::spawn(manager.clone().maintain());
    let mut issue_queue = tokio::spawn(manager.clone().run_issue_queue());
    let mut channels = tokio::spawn(manager.clone().maintain_channels());
    let mut gateway = tokio::spawn(async move { discord.start().await });
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tracing::info!("Codex Discord manager started");
    let normal = tokio::select! {
        _ = tokio::signal::ctrl_c() => true,
        _ = term.recv() => true,
        result = server.child.wait() => { tracing::error!(?result, "Codex process exited"); false },
        _ = server.client.closed() => { tracing::error!("Codex protocol connection closed"); false },
        _ = manager.fatal.cancelled() => false,
        _ = &mut dispatch => { tracing::error!("Codex event dispatch stopped"); false },
        _ = &mut delivery => { tracing::error!("Discord outbox worker stopped; check database connectivity"); false },
        _ = &mut maintenance => { tracing::error!("Database/request maintenance stopped"); false },
        _ = &mut issue_queue => { tracing::error!("Issue queue stopped"); false },
        _ = &mut channels => { tracing::error!("Discord channel maintenance stopped"); false },
        _ = &mut gateway => { tracing::error!("Discord gateway stopped"); false },
        _ = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if sqlx::query("SELECT 1").execute(&mut *leader).await.is_err() { break; }
            }
        } => { tracing::error!("Lost PostgreSQL deployment lock connection"); false },
    };
    manager.fatal.cancel();
    server.shutdown().await;
    if !normal {
        manager.system_notice("A critical bot subsystem stopped. The service is restarting to reconcile sessions; no task or approval will be replayed automatically.").await;
    }
    shard_manager.shutdown_all().await;
    dispatch.abort();
    delivery.abort();
    maintenance.abort();
    channels.abort();
    issue_queue.abort();
    gateway.abort();
    // Claims stay durable until next startup has reconciled history and process cleanup.
    tracing::info!(
        normal,
        "Service stopped; systemd will clean remaining cgroup processes"
    );
    ensure!(
        normal,
        "A critical subsystem stopped; systemd will restart and reconcile the service"
    );
    Ok(())
}
