mod interactive;
mod recovery;
mod tasks;

use crate::{
    codex::{Client, RpcError, protocol::*},
    config::Config,
    discord::render,
    persistence::{self, Repository, Session, Store},
    repositories::RepositoryRoot,
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, flock};
use serde_json::{Value, json};
use serenity::all::Http;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct Manager {
    pub config: Arc<Config>,
    pub store: Store,
    pub client: Client,
    pub generation: Uuid,
    pub http: Arc<Http>,
    pub fatal: CancellationToken,
    pub(crate) control_channel: AtomicU64,
    pub(crate) discord_guild: AtomicU64,
    pub(crate) bot_user: AtomicU64,
    pub(crate) discord_ready: AtomicBool,
    pub(crate) provisioning_gate: AsyncMutex<()>,
    root: RepositoryRoot,
    gates: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
    repository_locks: Mutex<HashMap<Uuid, (Uuid, File)>>,
    // Also allows /stop during database outages.
    live: Mutex<HashMap<u64, (String, String)>>,
    previews: Mutex<HashMap<(String, String), Value>>,
}

impl Manager {
    pub fn new(
        config: Arc<Config>,
        store: Store,
        client: Client,
        generation: Uuid,
        http: Arc<Http>,
    ) -> Result<Self> {
        let root = RepositoryRoot::open(&config.repos_root)?;
        std::fs::create_dir_all(config.state_dir.join("locks"))?;
        let control_channel = AtomicU64::new(config.channel_id);
        let discord_guild = AtomicU64::new(config.access.guild.unwrap_or(0));
        Ok(Self {
            config,
            store,
            client,
            generation,
            http,
            root,
            fatal: CancellationToken::new(),
            control_channel,
            discord_guild,
            bot_user: AtomicU64::new(0),
            discord_ready: AtomicBool::new(false),
            provisioning_gate: AsyncMutex::new(()),
            gates: Mutex::new(HashMap::new()),
            repository_locks: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            previews: Mutex::new(HashMap::new()),
        })
    }

    pub fn authorize(&self, user: u64, guild: Option<u64>) -> Result<u64> {
        ensure!(
            self.config.access.allows(user, guild, false)
                && (self.discord_guild.load(Ordering::Relaxed) == 0
                    || guild == Some(self.discord_guild.load(Ordering::Relaxed))),
            "Not authorized"
        );
        Ok(guild.unwrap())
    }

    pub(crate) fn gate(&self, session: Uuid) -> Arc<AsyncMutex<()>> {
        self.gates
            .lock()
            .unwrap()
            .entry(session)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn take_repository_lock(&self, repo: &Repository, session: Uuid) -> Result<()> {
        let mut locks = self.repository_locks.lock().unwrap();
        if let Some((owner, _)) = locks.get(&repo.id) {
            ensure!(
                *owner == session,
                "Repository already has an executing task"
            );
            return Ok(());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(
                self.config
                    .state_dir
                    .join("locks")
                    .join(format!("{}.lock", repo.name)),
            )?;
        flock(&file, FlockOperation::NonBlockingLockExclusive)
            .context("Repository is locked by a manual Codex session")?;
        locks.insert(repo.id, (session, file));
        Ok(())
    }

    fn drop_repository_lock(&self, session: &Session) {
        let mut locks = self.repository_locks.lock().unwrap();
        if locks
            .get(&session.repository_id)
            .is_some_and(|(owner, _)| *owner == session.id)
        {
            locks.remove(&session.repository_id);
        }
    }

    pub fn fail_closed(&self, context: &'static str) {
        tracing::error!(
            context,
            "Stopping service to preserve session and process ownership"
        );
        self.fatal.cancel();
        self.client.disconnect();
    }

    pub async fn register_repository(&self, name: &str, create: bool) -> Result<String> {
        let path = if create {
            self.root.create(name).await?
        } else {
            self.root.validate(name).await?
        };
        self.store.register(name, path.to_str().context("Repository path is not UTF-8")?).await
            .context("Registration failed; the directory was preserved. Retry /repo add after restoring PostgreSQL")?;
        Ok(format!("Registered `{name}`."))
    }

    pub async fn create_task(
        &self,
        user: u64,
        guild: u64,
        source: &str,
        name: &str,
        task: &str,
    ) -> Result<String> {
        self.authorize(user, Some(guild))?;
        ensure!(
            !task.trim().is_empty() && task.len() <= 32000,
            "Provide a task of 1–32000 bytes"
        );
        let repo = self.store.repository(name).await?;
        ensure!(
            self.root.validate(name).await?.to_str() == Some(repo.path.as_str()),
            "Repository path changed"
        );
        ensure!(
            self.store.receipt(source, "task", None).await?,
            "This command was already handled; check /sessions"
        );
        let session = self
            .store
            .reserve(
                &repo,
                user,
                guild,
                self.control_channel.load(Ordering::Relaxed),
                source,
            )
            .await?;
        let gate = self.gate(session.id);
        let _guard = gate.lock().await;
        let result = async {
            self.take_repository_lock(&repo, session.id)?;
            sqlx::query("UPDATE sessions SET channel_kind='text',channel_state='provisioning',task_title=$2 WHERE id=$1")
                .bind(session.id).bind(render::truncate(task, 500)).execute(&self.store.pool).await?;
            let session = self.store.session(session.id).await?;
            let channel = self.provision_channel(&session, name).await?;
            self.sync_status(&self.store.session(session.id).await?).await?;
            let codex = self
                .client
                .start_thread(
                    &repo.path,
                    self.config.model.as_deref(),
                    self.config.approvals_reviewer,
                )
                .await?;
            sqlx::query("UPDATE sessions SET codex_thread_id=$2,updated_at=now() WHERE id=$1")
                .bind(session.id)
                .bind(&codex.thread.id)
                .execute(&self.store.pool)
                .await?;
            let session = self.store.session(session.id).await?;
            self.submit_turn(&session, source, task).await?;
            Ok(format!("Started in <#{channel}>."))
        }
        .await;
        if result.is_err() {
            let current = self.store.session(session.id).await?;
            if current.status == "provisioning" {
                sqlx::query("UPDATE sessions SET status='unavailable',repository_claimed=false,last_error='Setup failed; no task was submitted',updated_at=now() WHERE id=$1")
                    .bind(session.id).execute(&self.store.pool).await?;
                self.drop_repository_lock(&current);
                self.store.receipt_state(source, "rejected").await?;
            }
        }
        result
    }

    async fn submit_turn(&self, session: &Session, source: &str, text: &str) -> Result<()> {
        let thread = session
            .codex_thread_id
            .as_deref()
            .context("Codex thread not provisioned")?;
        let mut tx = self.store.pool.begin().await?;
        sqlx::query(
            "UPDATE inbound_requests SET state='sending',updated_at=now() WHERE discord_id=$1",
        )
        .bind(source)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE sessions SET status='running',repository_claimed=true,current_turn_id=NULL,last_error=NULL,status_snapshot='{}',updated_at=now() WHERE id=$1")
            .bind(session.id).execute(&mut *tx).await?;
        tx.commit().await?;
        let turn = match self
            .client
            .start_turn(
                thread,
                text,
                &session.working_directory,
                source,
                self.config.approvals_reviewer,
            )
            .await
        {
            Ok(r) => r.turn,
            Err(_) => {
                self.fail_closed("turn submission outcome uncertain");
                let _ = self.store.receipt_state(source, "uncertain").await;
                bail!(
                    "Codex submission could not be confirmed. It will not be replayed. Check /status after recovery."
                );
            }
        };
        if let Some(channel) = session.channel_id().and_then(|s| s.parse().ok()) {
            self.live
                .lock()
                .unwrap()
                .insert(channel, (thread.into(), turn.id.clone()));
        }
        let result = async {
            let mut tx = self.store.pool.begin().await?;
            sqlx::query("UPDATE sessions SET current_turn_id=$2,updated_at=now() WHERE id=$1")
                .bind(session.id)
                .bind(&turn.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE inbound_requests SET state='accepted',updated_at=now() WHERE discord_id=$1",
            )
            .bind(source)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            self.fail_closed("could not persist accepted turn");
        }
        result
    }

    pub async fn followup(
        &self,
        user: u64,
        guild: u64,
        channel: u64,
        source: &str,
        text: &str,
    ) -> Result<()> {
        self.authorize(user, Some(guild))?;
        let session = self
            .store
            .by_discord(channel)
            .await?
            .context("This is not a Codex task channel")?;
        ensure!(session.guild_id == guild.to_string(), "Wrong guild");
        let gate = self.gate(session.id);
        let _guard = gate.lock().await;
        let session = self.store.session(session.id).await?;
        ensure!(
            session.channel_id() == Some(channel.to_string().as_str())
                && !matches!(session.channel_state.as_str(), "missing" | "deleted"),
            "This task channel is unavailable; use /reconnect from the control channel"
        );
        ensure!(
            !text.trim().is_empty() && text.len() <= 32000,
            "Send a text message of 1–32000 bytes"
        );
        ensure!(
            self.store
                .receipt(source, "message", Some(session.id))
                .await?,
            "Message already handled"
        );
        let result = self.followup_locked(&session, source, text).await;
        if let Err(error) = &result {
            sqlx::query("UPDATE inbound_requests SET state='rejected',updated_at=now() WHERE discord_id=$1 AND state='received'").bind(source).execute(&self.store.pool).await?;
            self.store
                .activity(session.id, &format!("Could not continue: {error}"))
                .await?;
        }
        result
    }

    async fn followup_locked(&self, session: &Session, source: &str, text: &str) -> Result<()> {
        if session.terminal_task() {
            // An authorized follow-up cancels retention even if resuming is blocked by another
            // repository owner or an unavailable Codex history.
            let mut tx = self.store.pool.begin().await?;
            sqlx::query("UPDATE sessions SET channel_state='active',completed_at=NULL,delete_after=NULL WHERE id=$1")
                .bind(session.id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NULL AND (dedupe_key LIKE 'task-completed:%' OR dedupe_key LIKE 'task-dismissed:%')")
                .bind(session.id).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        let thread = session
            .codex_thread_id
            .as_deref()
            .context("Session setup did not complete; start a new /codex task")?;
        match session.status.as_str() {
            "waiting" => {
                bail!("Use the question or approval controls above, or /stop to cancel the turn")
            }
            "running" => {
                let turn = session
                    .current_turn_id
                    .as_deref()
                    .context("Turn is still starting")?;
                self.store.receipt_state(source, "sending").await?;
                match self.client.steer(thread, turn, text, source).await {
                    Ok(_) => self.store.receipt_state(source, "accepted").await?,
                    Err(RpcError::Remote { .. }) => {
                        self.store.receipt_state(source, "rejected").await?;
                        bail!(
                            "The active turn changed before your message was accepted. Please send it again."
                        );
                    }
                    Err(_) => {
                        self.fail_closed("steering outcome uncertain");
                        let _ = self.store.receipt_state(source, "uncertain").await;
                        bail!(
                            "Message delivery is uncertain. It will not be replayed automatically."
                        );
                    }
                }
                self.store
                    .activity(session.id, "Follow-up sent to the running turn")
                    .await?;
            }
            "idle" => {
                let repo: Repository =
                    sqlx::query_as("SELECT id,name,path FROM repositories WHERE id=$1")
                        .bind(session.repository_id)
                        .fetch_one(&self.store.pool)
                        .await?;
                ensure!(
                    self.root.validate(&repo.name).await?.to_str()
                        == Some(session.working_directory.as_str()),
                    "Repository directory changed"
                );
                let claim = sqlx::query("UPDATE sessions SET repository_claimed=true,status='provisioning',updated_at=now() WHERE id=$1 AND status='idle'")
                    .bind(session.id).execute(&self.store.pool).await;
                match claim {
                    Ok(result) => ensure!(
                        result.rows_affected() == 1,
                        "Session state changed; check /status"
                    ),
                    Err(error)
                        if error
                            .as_database_error()
                            .is_some_and(|e| e.is_unique_violation()) =>
                    {
                        bail!("Another task owns this repository; check /sessions");
                    }
                    Err(error) => return Err(error.into()),
                }
                if let Err(error) = self.take_repository_lock(&repo, session.id) {
                    sqlx::query(
                        "UPDATE sessions SET repository_claimed=false,status='idle' WHERE id=$1",
                    )
                    .bind(session.id)
                    .execute(&self.store.pool)
                    .await?;
                    return Err(error);
                }
                if self
                    .client
                    .resume_thread(
                        thread,
                        &session.working_directory,
                        self.config.approvals_reviewer,
                    )
                    .await
                    .is_err()
                {
                    self.fail_closed("resume could not be verified");
                    bail!("Could not safely resume the conversation; recovery is required");
                }
                self.submit_turn(session, source, text).await?;
            }
            _ => bail!(
                "Session is {}. Check /status before sending another task.",
                session.status
            ),
        }
        Ok(())
    }

    pub async fn stop(&self, user: u64, guild: u64, channel: u64) -> Result<String> {
        self.authorize(user, Some(guild))?;
        // Read this before the database so an outage cannot prevent cancellation.
        let live = self.live.lock().unwrap().get(&channel).cloned();
        if let Some((thread, turn)) = live {
            if self.client.interrupt(&thread, &turn).await.is_err() {
                self.fail_closed("interrupt could not be confirmed");
                bail!("Interrupt failed; restarting the service to stop its processes");
            }
            let _ = sqlx::query("UPDATE sessions SET status='stopping',updated_at=now() WHERE COALESCE(discord_channel_id,discord_thread_id)=$1 AND current_turn_id=$2 AND repository_claimed")
                .bind(channel.to_string()).bind(turn).execute(&self.store.pool).await;
            return Ok("Interruption requested. Waiting for Codex to stop.".into());
        }
        let session = self
            .store
            .by_discord(channel)
            .await?
            .context("Use /stop inside a task channel, or supply a session ID")?;
        ensure!(session.guild_id == guild.to_string(), "Wrong guild");
        ensure!(
            !session.repository_claimed,
            "Session is starting or recovering; wait for its status update"
        );
        Ok("There is no running turn in this channel.".into())
    }

    pub async fn dispatch(self: Arc<Self>, mut events: mpsc::Receiver<Event>) -> Result<()> {
        let mut workers: HashMap<Uuid, mpsc::Sender<Event>> = HashMap::new();
        while let Some(event) = events.recv().await {
            let params = match &event {
                Event::Notification { params, .. } | Event::Request { params, .. } => params,
            };
            let thread = params["threadId"].as_str();
            let session = match thread {
                Some(t) => self.store.by_codex(t).await?,
                None => None,
            };
            let Some(session) = session else {
                if let Event::Request { id, method, .. } = event {
                    if let Some(result) = denied_response(&method) {
                        self.client.respond(&id, result).await?;
                    } else {
                        self.client.unsupported(&id).await?;
                    }
                }
                continue;
            };
            let sender = workers.entry(session.id).or_insert_with(|| {
                let (tx, mut rx) = mpsc::channel(128);
                let manager = self.clone();
                tokio::spawn(async move {
                    while let Some(event) = rx.recv().await {
                        let gate = manager.gate(session.id);
                        let _guard = gate.lock().await;
                        if manager.handle_event(session.id, event).await.is_err() {
                            manager.fail_closed("session event handling failed");
                            break;
                        }
                    }
                });
                tx
            });
            if sender.try_send(event).is_err() {
                bail!("Session event queue overloaded or worker stopped");
            }
        }
        bail!("Codex event stream closed")
    }

    async fn handle_event(&self, session_id: Uuid, event: Event) -> Result<()> {
        let session = self.store.session(session_id).await?;
        let Event::Notification { method, params } = event else {
            let Event::Request { id, method, params } = event else {
                unreachable!()
            };
            return self
                .interactive_request(&session, id, &method, params)
                .await;
        };
        match method.as_str() {
            "turn/started" => {
                let e: TurnEvent = serde_json::from_value(params)?;
                if session.current_turn_id.as_deref() == Some(&e.turn.id) {
                    return Ok(());
                }
                // An unexpected turn is never silently adopted as the requested task.
                ensure!(
                    session.status == "running" && session.current_turn_id.is_none(),
                    "Unexpected turn started"
                );
            }
            "item/started" | "item/completed" => {
                let e: ItemEvent = serde_json::from_value(params)?;
                if session.current_turn_id.as_deref() != Some(&e.turn_id) {
                    return Ok(());
                }
                let item_id = e.item["id"].as_str().unwrap_or("").to_owned();
                if method == "item/started" && e.item["type"] == "fileChange" {
                    self.previews
                        .lock()
                        .unwrap()
                        .insert((e.thread_id, item_id), e.item);
                } else if method == "item/completed" {
                    self.present_item(&session, &e.turn_id, &e.item).await?;
                    self.previews
                        .lock()
                        .unwrap()
                        .remove(&(e.thread_id, item_id));
                }
            }
            "turn/plan/updated" => {
                if session.current_turn_id.is_some()
                    && params["turnId"].as_str() == session.current_turn_id.as_deref()
                {
                    let plan = crate::discord::channels::reported_plan(&params["plan"]);
                    sqlx::query("UPDATE sessions SET status_snapshot=jsonb_set(status_snapshot,'{plan}',$2),updated_at=now() WHERE id=$1")
                        .bind(session.id).bind(plan).execute(&self.store.pool).await?;
                }
            }
            "turn/completed" => {
                let e: TurnEvent = serde_json::from_value(params)?;
                if session.current_turn_id.as_deref() != Some(&e.turn.id) {
                    return Ok(());
                }
                let outcome = e
                    .turn
                    .status
                    .outcome()
                    .context("Completion event was not terminal")?;
                self.client.clean_terminals(&e.thread_id).await?;
                self.deliver_response(&session, &e.turn.id).await?;
                let mut tx = self.store.pool.begin().await?;
                sqlx::query("UPDATE sessions SET status='idle',repository_claimed=false,current_turn_id=NULL,last_outcome=$2,last_error=$3,updated_at=now() WHERE id=$1")
                    .bind(session.id).bind(outcome).bind(if outcome == "failed" { Some("Codex turn failed; check the task output and service logs") } else { None }).execute(&mut *tx).await?;
                sqlx::query("UPDATE pending_requests SET state='expired',updated_at=now() WHERE session_id=$1 AND state IN ('pending','responding')")
                    .bind(session.id).execute(&mut *tx).await?;
                if outcome != "completed" {
                    persistence::enqueue(
                        &mut tx,
                        session.id,
                        &format!("{}:terminal", e.turn.id),
                        &format!(
                            "Turn **{outcome}**. This task stays active; reply here to continue."
                        ),
                        None,
                    )
                    .await?;
                }
                tx.commit().await?;
                self.drop_repository_lock(&session);
                if let Some(channel) = session.channel_id().and_then(|v| v.parse().ok()) {
                    self.live.lock().unwrap().remove(&channel);
                }
                self.previews
                    .lock()
                    .unwrap()
                    .retain(|(thread, _), _| thread != &e.thread_id);
            }
            "serverRequest/resolved" => {
                let e: ResolvedEvent = serde_json::from_value(params)?;
                sqlx::query("UPDATE pending_requests SET state='resolved',updated_at=now() WHERE generation=$1 AND rpc_id=$2 AND session_id=$3")
                    .bind(self.generation).bind(serde_json::to_value(e.request_id)?).bind(session.id).execute(&self.store.pool).await?;
                self.refresh_waiting(session.id).await?;
            }
            "error" => {
                let e: ErrorEvent = serde_json::from_value(params)?;
                if session.current_turn_id.as_deref() == Some(&e.turn_id) {
                    let text = if e.will_retry {
                        "Codex is retrying the model connection"
                    } else {
                        "Codex reported an error; waiting for the final turn status"
                    };
                    self.store.activity(session.id, text).await?;
                    if !e.will_retry {
                        self.store
                            .enqueue(session.id, &format!("failure:{}", e.turn_id), text, None)
                            .await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn present_item(&self, session: &Session, turn: &str, item: &Value) -> Result<()> {
        let Some(text) = item["text"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        match item["type"].as_str() {
            Some("plan") => self.store.activity(session.id, text).await?,
            Some("agentMessage") => {
                self.store.activity(session.id, text).await?;
                if item["phase"] != "commentary" {
                    let snapshot = json!({"response":render::truncate(text, 60000),"response_key":format!("{turn}:{}", item["id"].as_str().unwrap_or("answer"))});
                    sqlx::query("UPDATE sessions SET status_snapshot=status_snapshot || $2,updated_at=now() WHERE id=$1")
                        .bind(session.id).bind(snapshot).execute(&self.store.pool).await?;
                    // Explicit answers/questions are conversational output, not progress chatter.
                    // Models without phase metadata are coalesced until the turn ends.
                    if item["phase"] == "final_answer" {
                        self.deliver_response(&self.store.session(session.id).await?, turn)
                            .await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn deliver_response(&self, session: &Session, turn: &str) -> Result<()> {
        if let (Some(key), Some(text)) = (
            session.status_snapshot["response_key"].as_str(),
            session.status_snapshot["response"].as_str(),
        ) && key.starts_with(&format!("{turn}:"))
        {
            self.store.enqueue(session.id, key, text, None).await?;
        }
        Ok(())
    }

    async fn refresh_waiting(&self, session: Uuid) -> Result<()> {
        sqlx::query("UPDATE sessions SET status=CASE WHEN EXISTS(SELECT 1 FROM pending_requests WHERE session_id=$1 AND state IN ('pending','responding') AND COALESCE((payload->>'blocking')::boolean,false)) THEN 'waiting' ELSE 'running' END,updated_at=now() WHERE id=$1 AND status IN ('running','waiting')")
            .bind(session).execute(&self.store.pool).await?;
        Ok(())
    }
}
