//! Requires an explicitly supplied test PostgreSQL role with CREATE SCHEMA permission.
//! Every test uses a fresh UUID-named schema; no application tables are touched.
use anyhow::Result;
use codex_discord_bot::{
    codex::{Client, protocol::Event},
    config::{Access, Config},
    persistence::{PendingRequest, Session, Store},
    repositories::RepositoryRoot,
    sessions::Manager,
};
use serde_json::{Value, json};
use serenity::http::Http;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex as AsyncMutex, mpsc},
};
use uuid::Uuid;

#[path = "support/discord.rs"]
mod discord;

struct Mock {
    writer: Arc<AsyncMutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>,
    observed: Arc<Mutex<Vec<Value>>>,
    omit_turn_response: Arc<AtomicBool>,
    history: Arc<Mutex<Value>>,
}

impl Mock {
    fn new() -> (Self, Client, mpsc::Receiver<Event>) {
        let (a, b) = tokio::io::duplex(1024 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (client, events) = Client::connect(ar, aw, Duration::from_millis(500));
        let writer = Arc::new(AsyncMutex::new(bw));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let omit = Arc::new(AtomicBool::new(false));
        let history = Arc::new(Mutex::new(json!({"data":[],"nextCursor":null})));
        let (w, seen, skip, h) = (
            writer.clone(),
            observed.clone(),
            omit.clone(),
            history.clone(),
        );
        tokio::spawn(async move {
            let mut lines = BufReader::new(br).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let v: Value = serde_json::from_str(&line).unwrap();
                seen.lock().unwrap().push(v.clone());
                let Some(method) = v["method"].as_str() else {
                    continue;
                };
                let p = &v["params"];
                let result = match method {
                    "thread/start" => {
                        json!({"thread":{"id":format!("thread-{}", Uuid::new_v4()),"cwd":p["cwd"],"status":{"type":"idle"}},
                        "cwd":p["cwd"],"approvalPolicy":"on-request","approvalsReviewer":p["approvalsReviewer"],
                        "sandbox":{"type":"workspaceWrite","networkAccess":false,"writableRoots":[]}})
                    }
                    "thread/resume" => {
                        json!({"thread":{"id":p["threadId"],"cwd":p["cwd"],"status":{"type":"idle"}},
                        "cwd":p["cwd"],"approvalPolicy":"on-request","approvalsReviewer":p["approvalsReviewer"],
                        "sandbox":{"type":"workspaceWrite","networkAccess":false,"writableRoots":[]}})
                    }
                    "thread/backgroundTerminals/clean" | "turn/interrupt" => json!({}),
                    "thread/backgroundTerminals/list" => json!({"data":[],"nextCursor":null}),
                    "thread/turns/list" => h.lock().unwrap().clone(),
                    "turn/start" => {
                        if skip.load(Ordering::Relaxed) {
                            continue;
                        }
                        json!({"turn":{"id":"turn-new","status":"inProgress"}})
                    }
                    "turn/steer" => json!({"turnId":p["expectedTurnId"]}),
                    _ => panic!("Unexpected client method {method}"),
                };
                let data = format!("{}\n", json!({"id":v["id"],"result":result}));
                if w.lock().await.write_all(data.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        (
            Self {
                writer,
                observed,
                omit_turn_response: omit,
                history,
            },
            client,
            events,
        )
    }
    async fn send(&self, v: Value) {
        self.writer
            .lock()
            .await
            .write_all(format!("{v}\n").as_bytes())
            .await
            .unwrap();
    }
    fn count(&self, method: &str) -> usize {
        self.observed
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == method)
            .count()
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    schema: String,
    admin: PgPool,
    manager: Arc<Manager>,
    mock: Mock,
    dispatch: tokio::task::JoinHandle<Result<()>>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        Self::with_http(Arc::new(Http::new("test"))).await
    }

    async fn with_http(http: Arc<Http>) -> Result<Self> {
        Self::with_control(http, 8).await
    }

    async fn with_control(http: Arc<Http>, control: u64) -> Result<Self> {
        let url = std::env::var("TEST_DATABASE_URL")
            .expect("Set TEST_DATABASE_URL to a disposable test database");
        let admin = PgPool::connect(&url).await?;
        let schema = format!("codex_test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await?;
        let search = format!("SET search_path TO {schema}");
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .after_connect(move |conn, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&search).execute(conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await?;
        let store = Store { pool };
        store.migrate().await?;
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("repos");
        std::fs::create_dir(&root)?;
        let config = Arc::new(Config {
            database_url: url,
            discord_token: "not-a-live-token".into(),
            access: Access {
                users: [42].into(),
                guild: Some(7),
            },
            channel_id: control,
            active_category_id: 9,
            completed_category_id: 10,
            channel_retention: Duration::from_secs(48 * 3600),
            repos_root: root,
            state_dir: directory.path().join("state"),
            codex_home: directory.path().join("codex"),
            codex_bin: "/not-used".into(),
            tool_path: "/usr/bin:/bin".into(),
            model: None,
            approvals_reviewer: Default::default(),
            rpc_timeout: Duration::from_secs(1),
        });
        let (mock, client, events) = Mock::new();
        let manager = Arc::new(Manager::new(config, store, client, Uuid::new_v4(), http)?);
        let dispatch = tokio::spawn(manager.clone().dispatch(events));
        Ok(Self {
            directory,
            schema,
            admin,
            manager,
            mock,
            dispatch,
        })
    }
    async fn seed(
        &self,
        name: &str,
        channel: u64,
        thread: &str,
        turn: &str,
        idle: bool,
    ) -> Result<Session> {
        let path = RepositoryRoot::open(&self.manager.config.repos_root)?
            .create(name)
            .await?;
        self.manager
            .store
            .register(name, path.to_str().unwrap())
            .await?;
        let repo = self.manager.store.repository(name).await?;
        self.manager.store.receipt(name, "task", None).await?;
        let session = self.manager.store.reserve(&repo, 42, 7, 8, name).await?;
        self.manager.store.receipt_state(name, "accepted").await?;
        sqlx::query("UPDATE sessions SET discord_thread_id=$2,codex_thread_id=$3,current_turn_id=$4,status=$5,repository_claimed=$6 WHERE id=$1")
            .bind(session.id).bind(channel.to_string()).bind(thread).bind(if idle {None} else {Some(turn)})
            .bind(if idle {"idle"} else {"running"}).bind(!idle).execute(&self.manager.store.pool).await?;
        self.manager.store.session(session.id).await
    }

    async fn text_channel(&self, s: &Session, api: &discord::Api) -> Result<Session> {
        let channel = s.channel_id().unwrap();
        sqlx::query("UPDATE sessions SET discord_channel_id=$2,discord_thread_id=NULL,channel_kind='text',channel_state='active',task_title='Test task' WHERE id=$1")
            .bind(s.id).bind(channel).execute(&self.manager.store.pool).await?;
        api.state.lock().unwrap().channels.insert(
            channel.into(),
            discord::channel(channel, 0, 9, &format!("codex-task:{}", s.id)),
        );
        self.manager.store.session(s.id).await
    }
    async fn status(&self, id: Uuid, status: &str) -> Result<Session> {
        for _ in 0..200 {
            let session = self.manager.store.session(id).await?;
            if session.status == status {
                return Ok(session);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "Expected status {status}; got {}",
            self.manager.store.session(id).await?.status
        );
    }
    async fn cleanup(self) -> Result<()> {
        self.manager.client.disconnect();
        self.dispatch.abort();
        self.manager.store.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        drop(self.directory);
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn database_constraints_prevent_duplicate_repository_execution() -> Result<()> {
    let f = Fixture::new().await?;
    let first = f.seed("repo", 100, "thread-1", "turn-1", false).await?;
    let repo = f.manager.store.repository("repo").await?;
    let (a, b) = tokio::join!(
        f.manager.store.reserve(&repo, 42, 7, 8, "a"),
        f.manager.store.reserve(&repo, 42, 7, 8, "b")
    );
    assert!(a.is_err() && b.is_err());
    let second = f.seed("other", 101, "thread-2", "turn-2", false).await?;
    assert!(
        sqlx::query("UPDATE sessions SET discord_thread_id='100' WHERE id=$1")
            .bind(second.id)
            .execute(&f.manager.store.pool)
            .await
            .is_err()
    );
    assert_eq!(f.manager.store.by_discord(100).await?.unwrap().id, first.id);
    assert_eq!(
        f.manager.store.by_codex("thread-2").await?.unwrap().id,
        second.id
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn interleaved_events_and_approvals_stay_in_their_sessions() -> Result<()> {
    let f = Fixture::new().await?;
    let a = f.seed("alpha", 100, "thread-a", "turn-a", false).await?;
    let b = f.seed("beta", 101, "thread-b", "turn-b", false).await?;
    let api = discord::Api::start().await;
    let b = f.text_channel(&b, &api).await?;
    for (thread, turn, text) in [
        ("thread-a", "turn-a", "Alpha result"),
        ("thread-b", "turn-b", "Beta result"),
    ] {
        f.mock.send(json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":"same-item-id","type":"agentMessage","phase":"final_answer","text":text}}})).await;
    }
    f.mock.send(json!({"id":"approval-1","method":"item/commandExecution/requestApproval","params":{
        "threadId":"thread-b","turnId":"turn-b","itemId":"exec-1","command":"cargo test","cwd":b.working_directory}})).await;
    f.status(b.id, "waiting").await?;
    let request: PendingRequest =
        sqlx::query_as("SELECT * FROM pending_requests WHERE session_id=$1")
            .bind(b.id)
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert!(
        f.manager
            .answer(99, 7, 101, request.id, "accept", vec![])
            .await
            .is_err()
    );
    assert!(
        f.manager
            .answer(42, 7, 100, request.id, "accept", vec![])
            .await
            .is_err()
    );
    sqlx::query("UPDATE pending_requests SET generation=$2 WHERE id=$1")
        .bind(request.id)
        .bind(Uuid::new_v4())
        .execute(&f.manager.store.pool)
        .await?;
    assert!(
        f.manager
            .answer(42, 7, 101, request.id, "accept", vec![])
            .await
            .is_err()
    );
    sqlx::query("UPDATE pending_requests SET generation=$2 WHERE id=$1")
        .bind(request.id)
        .bind(f.manager.generation)
        .execute(&f.manager.store.pool)
        .await?;
    f.manager
        .answer(42, 7, 101, request.id, "accept", vec![])
        .await?;
    assert!(
        f.manager
            .answer(42, 7, 101, request.id, "accept", vec![])
            .await
            .is_err()
    );
    f.mock.send(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-b","requestId":"approval-1"}})).await;
    f.status(b.id, "running").await?;
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":"thread-a","turn":{"id":"turn-a","status":"completed"}}})).await;
    let done = f.status(a.id, "idle").await?;
    assert!(!done.repository_claimed);
    assert!(f.manager.store.session(b.id).await?.repository_claimed);
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT session_id,content FROM discord_outbox WHERE content LIKE '%result'",
    )
    .fetch_all(&f.manager.store.pool)
    .await?;
    let mapped: HashMap<_, _> = rows.into_iter().collect();
    assert_eq!(mapped[&a.id], "Alpha result");
    assert_eq!(mapped[&b.id], "Beta result");
    let notifications: Vec<(String, bool)> = sqlx::query_as(
        "SELECT dedupe_key,notify_requester FROM discord_outbox WHERE session_id=$1 OR session_id=$2",
    )
    .bind(a.id)
    .bind(b.id)
    .fetch_all(&f.manager.store.pool)
    .await?;
    assert!(
        !notifications
            .iter()
            .any(|(key, _)| key == "turn-a:terminal:0")
    );
    assert!(
        notifications
            .iter()
            .any(|(key, notify)| key.starts_with("request:") && *notify)
    );
    for (key, notify) in &notifications {
        if key.ends_with(":started:0") || key.ends_with(":same-item-id:0") {
            assert!(!notify, "Progress notification: {key}");
        }
    }

    let replies: Vec<_> = f
        .mock
        .observed
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["id"] == "approval-1" && v["result"].is_object())
        .cloned()
        .collect();
    assert_eq!(
        replies,
        vec![json!({"id":"approval-1","result":{"decision":"accept"}})]
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn followup_deduplication_and_stale_completion_preserve_claims() -> Result<()> {
    let f = Fixture::new().await?;
    let s = f.seed("project", 100, "thread", "old-turn", true).await?;
    f.manager
        .followup(42, 7, 100, "200", "Implement a test")
        .await?;
    assert!(
        f.manager
            .followup(42, 7, 100, "200", "Implement a test")
            .await
            .is_err()
    );
    assert_eq!(f.mock.count("turn/start"), 1);
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"old-turn","status":"completed"}}})).await;
    f.mock.send(json!({"method":"item/tool/requestUserInput","id":19,"params":{"threadId":"thread","turnId":"turn-new","itemId":"question","isBlocking":true,
        "questions":[{"id":"storage","header":"Storage","question":"Which storage?","options":[{"label":"PostgreSQL","description":"Persistent storage"}]}]}})).await;
    f.status(s.id, "waiting").await?;
    assert!(f.manager.store.session(s.id).await?.repository_claimed);
    let req: PendingRequest = sqlx::query_as("SELECT * FROM pending_requests WHERE session_id=$1")
        .bind(s.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    f.manager
        .answer(42, 7, 100, req.id, "answer", vec!["PostgreSQL".into()])
        .await?;
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"turn-new","status":"interrupted"}}})).await;
    let stopped = f.status(s.id, "idle").await?;
    assert_eq!(stopped.last_outcome.as_deref(), Some("interrupted"));
    assert!(
        f.manager
            .answer(42, 7, 100, req.id, "answer", vec!["late".into()])
            .await
            .is_err()
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn lost_submission_is_not_replayed_during_recovery() -> Result<()> {
    let f = Fixture::new().await?;
    let s = f.seed("project", 100, "thread", "old-turn", true).await?;
    f.mock.omit_turn_response.store(true, Ordering::Relaxed);
    assert!(
        f.manager
            .followup(42, 7, 100, "200", "A non-idempotent task")
            .await
            .is_err()
    );
    assert!(f.manager.fatal.is_cancelled());
    let state: String =
        sqlx::query_scalar("SELECT state FROM inbound_requests WHERE discord_id='200'")
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert_eq!(state, "uncertain");
    let (replacement, client, _events) = Mock::new();
    *replacement.history.lock().unwrap() =
        json!({"data":[{"id":"old-turn","status":"completed","items":[]}],"nextCursor":null});
    // Model systemd cleanup by dropping the old manager's file lock before recovery.
    f.dispatch.abort();
    let config = f.manager.config.clone();
    let store = f.manager.store.clone();
    let Fixture {
        directory,
        schema,
        admin,
        manager,
        mock,
        dispatch,
    } = f;
    drop(manager);
    drop(mock);
    drop(dispatch);
    tokio::task::yield_now().await;
    let restarted = Manager::new(
        config,
        store.clone(),
        client,
        Uuid::new_v4(),
        Arc::new(Http::new("test")),
    )?;
    restarted.recover().await?;
    assert_eq!(replacement.count("turn/start"), 0);
    let recovered = store.session(s.id).await?;
    assert_eq!(recovered.status, "idle");
    assert_eq!(recovered.last_outcome.as_deref(), Some("unknown"));
    restarted.client.disconnect();
    store.pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await?;
    drop(directory);
    Ok(())
}

async fn due(f: &Fixture, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE sessions SET discord_retry_at=now(),discord_checked_at=NULL WHERE id=$1")
        .bind(id)
        .execute(&f.manager.store.pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn new_tasks_create_one_channel_and_coalesce_actual_progress() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("editor", true).await?;
    f.manager
        .create_task(42, 7, "200", "editor", "Fix LSP startup")
        .await?;
    let s = f.manager.store.sessions(1).await?.remove(0);
    assert_eq!(s.channel_kind, "text");
    assert_eq!(s.channel_state, "active");
    assert!(s.discord_thread_id.is_none());
    assert!(s.status_message_id.is_some());
    let channel = s.channel_id().unwrap();
    assert_eq!(
        f.manager
            .store
            .by_discord(channel.parse()?)
            .await?
            .unwrap()
            .id,
        s.id
    );
    assert_eq!(
        api.state.lock().unwrap().channels[channel]["name"],
        "editor-fix-lsp-startup"
    );
    assert_eq!(f.mock.count("thread/start"), 1);
    assert_eq!(f.mock.count("turn/start"), 1);
    assert!(
        f.manager
            .create_task(42, 7, "200", "editor", "Duplicate")
            .await
            .is_err()
    );

    for n in 0..30 {
        f.mock.send(json!({"method":"item/completed","params":{"threadId":s.codex_thread_id,"turnId":"turn-new",
            "item":{"id":format!("comment-{n}"),"type":"agentMessage","phase":"commentary","text":format!("Activity {n}")}}})).await;
    }
    f.mock.send(json!({"method":"turn/plan/updated","params":{"threadId":s.codex_thread_id,"turnId":"turn-new",
        "plan":[{"step":"Run integration tests","status":"inProgress"}]}})).await;
    for _ in 0..200 {
        if f.manager.store.session(s.id).await?.status_snapshot["plan"].is_array() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    f.manager.maintain_channels_once().await?;
    f.manager.maintain_channels_once().await?;
    let current = f.manager.store.session(s.id).await?;
    assert_eq!(current.status_snapshot["activity"], "Activity 29");
    assert_eq!(current.status_revision, current.status_delivered_revision);
    assert_eq!(s.status_message_id, current.status_message_id);
    let preview = {
        let state = api.state.lock().unwrap();
        assert_eq!(state.count("POST", "/guilds/7/channels"), 1);
        assert_eq!(
            state.count("POST", &format!("/channels/{channel}/messages")),
            1
        );
        assert_eq!(
            state.count(
                "PATCH",
                &format!(
                    "/channels/{channel}/messages/{}",
                    s.status_message_id.as_ref().unwrap()
                )
            ),
            1
        );
        state.messages[channel][0]["embeds"][0]["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        preview.contains("Run integration tests: Running"),
        "{preview}"
    );
    assert!(!preview.contains("Release:") && !preview.contains("Investigation:"));
    assert!(!f.manager.store.undelivered(s.id).await?);
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":s.codex_thread_id,"turn":{"id":"turn-new","status":"completed"}}})).await;
    let idle = f.status(s.id, "idle").await?;
    assert_eq!(idle.channel_state, "active");
    assert!(idle.completed_at.is_none() && idle.delete_after.is_none());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn completed_channels_move_and_delete_only_after_safe_retention_with_retries() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let s = f.seed("finished", 100, "thread", "turn", true).await?;
    let s = f.text_channel(&s, &api).await?;
    assert!(f.manager.finish_task(42, 7, s.id, false).await.is_err());
    sqlx::query("UPDATE sessions SET last_outcome='completed' WHERE id=$1")
        .bind(s.id)
        .execute(&f.manager.store.pool)
        .await?;
    assert!(f.manager.finish_task(99, 7, s.id, false).await.is_err());
    f.manager.finish_task(42, 7, s.id, false).await?;
    let completed = f.manager.store.session(s.id).await?;
    assert_eq!(
        (completed.delete_after.unwrap() - completed.completed_at.unwrap()).num_hours(),
        48
    );
    f.manager.maintain_channels_once().await?;
    assert_eq!(
        api.state.lock().unwrap().channels["100"]["parent_id"],
        "9",
        "Notification must be delivered before moving"
    );
    sqlx::query("UPDATE discord_outbox SET delivered_at=now() WHERE session_id=$1")
        .bind(s.id)
        .execute(&f.manager.store.pool)
        .await?;
    due(&f, s.id).await?;
    f.manager.maintain_channels_once().await?;
    assert_eq!(api.state.lock().unwrap().channels["100"]["parent_id"], "10");
    assert_eq!(
        api.state.lock().unwrap().count("DELETE", "/channels/100"),
        0
    );
    sqlx::query("UPDATE sessions SET completed_at=now()-interval '49 hours',delete_after=now()-interval '1 hour',discord_retry_at=now() WHERE id=$1")
        .bind(s.id).execute(&f.manager.store.pool).await?;
    api.state.lock().unwrap().fail_next = Some(("DELETE".into(), "/channels/100".into(), 500, 0));
    f.manager.maintain_channels_once().await?;
    let failed = f.manager.store.session(s.id).await?;
    assert_eq!(failed.channel_state, "completed");
    assert_eq!(failed.discord_failures, 1);
    assert!(failed.discord_error.is_some());
    let attempts = api.state.lock().unwrap().count("DELETE", "/channels/100");
    f.manager.maintain_channels_once().await?;
    assert_eq!(
        api.state.lock().unwrap().count("DELETE", "/channels/100"),
        attempts
    );
    due(&f, s.id).await?;
    f.manager.maintain_channels_once().await?;
    let deleted = f.manager.store.session(s.id).await?;
    assert_eq!(
        deleted.channel_state, "deleted",
        "{:?}",
        deleted.discord_error
    );
    assert_eq!(deleted.codex_thread_id, s.codex_thread_id);
    assert!(f.manager.store.by_discord(100).await?.is_none());
    assert!(api.state.lock().unwrap().channels.contains_key("8"));
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn recovery_adopts_existing_channel_and_status_then_reconnects_missing_channel_without_replay()
-> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("recover", true).await?;
    f.manager
        .create_task(42, 7, "200", "recover", "Task")
        .await?;
    let s = f.manager.store.sessions(1).await?.remove(0);
    let channel = s.channel_id().unwrap();
    sqlx::query("UPDATE sessions SET discord_channel_id=NULL,channel_state='provisioning',status_message_id=NULL,status_delivered_revision=-1 WHERE id=$1")
        .bind(s.id).execute(&f.manager.store.pool).await?;
    f.manager.reconcile_channels().await?;
    f.manager.maintain_channels_once().await?;
    let recovered = f.manager.store.session(s.id).await?;
    assert_eq!(recovered.discord_channel_id, s.discord_channel_id);
    assert_eq!(recovered.status_message_id, s.status_message_id);
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", &format!("/channels/{channel}/messages")),
        1
    );

    api.state
        .lock()
        .unwrap()
        .messages
        .get_mut(channel)
        .unwrap()
        .clear();
    f.manager.reconcile_channels().await?;
    due(&f, s.id).await?;
    f.manager.maintain_channels_once().await?;
    assert_ne!(
        f.manager.store.session(s.id).await?.status_message_id,
        s.status_message_id
    );

    api.state.lock().unwrap().channels.remove(channel);
    f.manager.reconcile_channels().await?;
    let missing = f.manager.store.session(s.id).await?;
    assert_eq!(missing.channel_state, "missing");
    assert_eq!(missing.current_turn_id, s.current_turn_id);
    assert!(missing.repository_claimed);
    assert!(f.manager.reconnect_channel(99, 7, s.id).await.is_err());
    f.manager.reconnect_channel(42, 7, s.id).await?;
    let restored = f.manager.store.session(s.id).await?;
    assert_eq!(restored.channel_state, "active");
    assert_ne!(restored.discord_channel_id, s.discord_channel_id);
    assert_eq!(restored.codex_thread_id, s.codex_thread_id);
    assert_eq!(f.mock.count("turn/start"), 1);
    f.manager
        .stop(42, 7, restored.channel_id().unwrap().parse()?)
        .await?;
    assert_eq!(f.mock.count("turn/interrupt"), 1);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn cleanup_rejects_pending_input_unowned_channels_and_legacy_threads() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let legacy = f.seed("legacy", 101, "legacy-thread", "turn", true).await?;
    assert!(f.manager.finish_task(42, 7, legacy.id, true).await.is_err());
    let s = f.seed("cleanup", 100, "thread", "turn", true).await?;
    let s = f.text_channel(&s, &api).await?;
    let pending = Uuid::new_v4();
    sqlx::query("INSERT INTO pending_requests(id,session_id,generation,rpc_id,method,turn_id,payload,state) VALUES($1,$2,$3,'1','item/tool/requestUserInput','turn','{}','pending')")
        .bind(pending).bind(s.id).bind(f.manager.generation).execute(&f.manager.store.pool).await?;
    assert!(f.manager.finish_task(42, 7, s.id, true).await.is_err());
    sqlx::query("UPDATE pending_requests SET state='resolved' WHERE id=$1")
        .bind(pending)
        .execute(&f.manager.store.pool)
        .await?;
    f.manager.finish_task(42, 7, s.id, true).await?;
    sqlx::query("UPDATE discord_outbox SET delivered_at=now() WHERE session_id=$1")
        .bind(s.id)
        .execute(&f.manager.store.pool)
        .await?;
    sqlx::query("UPDATE sessions SET completed_at=now()-interval '49 hours',delete_after=now()-interval '1 hour',discord_retry_at=now() WHERE id=$1")
        .bind(s.id).execute(&f.manager.store.pool).await?;
    // A queued offline message can exist before the gateway has persisted its receipt.
    let id =
        (((chrono::Utc::now().timestamp_millis() - 1_420_070_400_000) as u64) << 22).to_string();
    let mut message = discord::message(&id, "100", &json!({"content":"Please do one more thing"}));
    message["author"]["id"] = json!("42");
    message["author"]["bot"] = json!(false);
    api.state
        .lock()
        .unwrap()
        .messages
        .entry("100".into())
        .or_default()
        .push(message);
    f.manager.maintain_channels_once().await?;
    assert_eq!(
        api.state.lock().unwrap().count("DELETE", "/channels/100"),
        0
    );
    let eligible = f.manager.store.session(s.id).await?;
    assert!(eligible.cleanup_due(chrono::Utc::now(), 8));
    for kind in ["thread", "text"] {
        let mut changed = eligible.clone();
        changed.channel_kind = kind.into();
        changed.channel_state = "active".into();
        assert!(!changed.cleanup_due(chrono::Utc::now(), 8));
    }
    assert!(!eligible.cleanup_due(chrono::Utc::now(), 100));
    let mut running = eligible.clone();
    running.repository_claimed = true;
    assert!(!running.cleanup_due(chrono::Utc::now(), 8));
    let mut unpersisted = eligible.clone();
    unpersisted.status_delivered_revision -= 1;
    assert!(!unpersisted.cleanup_due(chrono::Utc::now(), 8));
    api.state.lock().unwrap().channels.get_mut("100").unwrap()["topic"] =
        json!("someone else's channel");
    due(&f, s.id).await?;
    f.manager.maintain_channels_once().await?;
    assert_eq!(
        api.state.lock().unwrap().count("DELETE", "/channels/100"),
        0
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn text_channels_isolate_concurrent_followups_and_reopen_completed_tasks() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::new().await?;
    let a = f.seed("alpha", 100, "thread-a", "turn-a", false).await?;
    let b = f.seed("beta", 101, "thread-b", "turn-b", false).await?;
    let a = f.text_channel(&a, &api).await?;
    f.text_channel(&b, &api).await?;
    assert!(
        f.manager
            .followup(99, 7, 100, "bad-user", "No")
            .await
            .is_err()
    );
    assert!(
        f.manager
            .followup(42, 9, 100, "bad-guild", "No")
            .await
            .is_err()
    );
    let (left, right) = tokio::join!(
        f.manager.followup(42, 7, 100, "200", "Alpha only"),
        f.manager.followup(42, 7, 101, "201", "Beta only")
    );
    left?;
    right?;
    let steers: Vec<_> = f
        .mock
        .observed
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["method"] == "turn/steer")
        .cloned()
        .collect();
    assert_eq!(steers.len(), 2);
    assert!(steers.iter().any(|v| v["params"]["threadId"] == "thread-a"
        && v["params"]["expectedTurnId"] == "turn-a"
        && v["params"]["input"][0]["text"] == "Alpha only"));
    assert!(
        steers.iter().any(|v| v["params"]["threadId"] == "thread-b"
            && v["params"]["input"][0]["text"] == "Beta only")
    );
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":"thread-a","turn":{"id":"turn-a","status":"completed"}}})).await;
    f.status(a.id, "idle").await?;
    f.manager.finish_task(42, 7, a.id, false).await?;
    f.manager
        .followup(42, 7, 100, "202", "Continue in the same conversation")
        .await?;
    let reopened = f.manager.store.session(a.id).await?;
    assert_eq!(reopened.channel_state, "active");
    assert!(reopened.completed_at.is_none() && reopened.delete_after.is_none());
    assert_eq!(reopened.codex_thread_id, a.codex_thread_id);
    assert_eq!(reopened.discord_channel_id, a.discord_channel_id);
    assert!(!f.manager.store.undelivered(a.id).await?);
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":"thread-a","turn":{"id":"turn-new","status":"completed"}}})).await;
    f.status(a.id, "idle").await?;
    f.manager.finish_task(42, 7, a.id, false).await?;
    let repo = f.manager.store.repository("alpha").await?;
    f.manager
        .store
        .reserve(&repo, 42, 7, 8, "another-task")
        .await?;
    assert!(
        f.manager
            .followup(42, 7, 100, "203", "Reopen even if the repository is busy")
            .await
            .is_err()
    );
    let blocked = f.manager.store.session(a.id).await?;
    assert_eq!(blocked.channel_state, "active");
    assert!(blocked.delete_after.is_none());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn migration_preserves_legacy_execution_and_approval_records() -> Result<()> {
    let pool = PgPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
    let mut tx = pool.begin().await?;
    let schema = format!("codex_migration_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!("SET LOCAL search_path TO {schema}"))
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(include_str!("../migrations/0002_attention_mentions.sql"))
        .execute(&mut *tx)
        .await?;
    let repo = Uuid::new_v4();
    let session = Uuid::new_v4();
    let request = Uuid::new_v4();
    sqlx::query("INSERT INTO repositories(id,name,path) VALUES($1,'legacy','/repos/legacy')")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO sessions(id,repository_id,requester_id,guild_id,parent_channel_id,discord_thread_id,codex_thread_id,working_directory,status,current_turn_id) VALUES($1,$2,'42','7','8','100','codex-old','/repos/legacy','waiting','turn-old')")
        .bind(session).bind(repo).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO pending_requests(id,session_id,generation,rpc_id,method,turn_id,payload,state) VALUES($1,$2,$3,'1','item/commandExecution/requestApproval','turn-old','{}','pending')")
        .bind(request).bind(session).bind(Uuid::new_v4()).execute(&mut *tx).await?;
    sqlx::raw_sql(include_str!("../migrations/0003_task_channels.sql"))
        .execute(&mut *tx)
        .await?;
    let migrated: Session = sqlx::query_as("SELECT * FROM sessions WHERE id=$1")
        .bind(session)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(migrated.channel_id(), Some("100"));
    assert_eq!(migrated.discord_thread_id.as_deref(), Some("100"));
    assert_eq!(migrated.channel_state, "legacy");
    assert_eq!(migrated.status, "waiting");
    assert_eq!(migrated.current_turn_id.as_deref(), Some("turn-old"));
    assert!(migrated.repository_claimed);
    assert!(!migrated.cleanup_due(chrono::Utc::now(), 8));
    let state: String = sqlx::query_scalar("SELECT state FROM pending_requests WHERE id=$1")
        .bind(request)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(state, "pending");
    tx.rollback().await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn startup_checks_narrow_permissions_and_reuses_control_channel() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_control(api.http.clone(), 0).await?;
    f.manager.prepare_discord().await?;
    f.manager.prepare_discord().await?;
    let restarted = Manager::new(
        f.manager.config.clone(),
        f.manager.store.clone(),
        f.manager.client.clone(),
        Uuid::new_v4(),
        api.http.clone(),
    )?;
    restarted.prepare_discord().await?;
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    api.state.lock().unwrap().permissions &= !serenity::all::Permissions::MANAGE_CHANNELS.bits();
    assert!(f.manager.prepare_discord().await.is_err());
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn delivery_isolates_approvals_retries_api_errors_and_skips_missing_channels() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let a = f.seed("alpha", 100, "thread-a", "turn-a", false).await?;
    let b = f.seed("beta", 101, "thread-b", "turn-b", false).await?;
    f.text_channel(&a, &api).await?;
    f.text_channel(&b, &api).await?;
    f.manager
        .store
        .enqueue(
            a.id,
            "failure:alpha",
            "Alpha needs attention; @everyone must not be pinged",
            None,
        )
        .await?;
    api.state.lock().unwrap().fail_next =
        Some(("POST".into(), "/channels/100/messages".into(), 403, 50013));
    f.mock.send(json!({"method":"item/tool/requestUserInput","id":19,"params":{"threadId":"thread-b","turnId":"turn-b","itemId":"question","isBlocking":true,
        "questions":[{"id":"storage","header":"Storage","question":"Which storage?","options":[]}]}})).await;
    f.status(b.id, "waiting").await?;
    let delivery = tokio::spawn(f.manager.clone().deliver());
    for _ in 0..300 {
        if !f.manager.store.undelivered(a.id).await? && !f.manager.store.undelivered(b.id).await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!f.manager.store.undelivered(a.id).await? && !f.manager.store.undelivered(b.id).await?);
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/channels/100/messages"),
        2
    );
    let request: PendingRequest =
        sqlx::query_as("SELECT * FROM pending_requests WHERE session_id=$1")
            .bind(b.id)
            .fetch_one(&f.manager.store.pool)
            .await?;
    {
        let state = api.state.lock().unwrap();
        let b_message = &state.messages["101"][0];
        assert!(
            b_message["content"]
                .as_str()
                .unwrap()
                .contains("Which storage?")
        );
        assert!(
            b_message["components"][0]["components"][0]["custom_id"]
                .as_str()
                .unwrap()
                .contains(&request.id.to_string())
        );
        for (method, path, body) in &state.calls {
            if method == "POST"
                && (path == "/channels/100/messages" || path == "/channels/101/messages")
            {
                assert_eq!(body["allowed_mentions"]["parse"], json!([]));
                assert_eq!(body["allowed_mentions"]["users"], json!(["42"]));
            }
        }
    }
    api.state.lock().unwrap().channels.remove("101");
    f.manager.reconnect_channel(42, 7, b.id).await?;
    let restored = f.manager.store.session(b.id).await?;
    let restored_channel = restored.channel_id().unwrap();
    for _ in 0..200 {
        if !f.manager.store.undelivered(b.id).await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The status plus the still-live prompt must actually exist in the new channel,
    // even while Discord retains its nonce cache for the old channel's messages.
    assert_eq!(
        api.state.lock().unwrap().messages[restored_channel].len(),
        2
    );
    assert!(
        f.manager
            .answer(
                42,
                7,
                101,
                request.id,
                "answer",
                vec!["Wrong channel".into()]
            )
            .await
            .is_err()
    );
    f.manager
        .answer(
            42,
            7,
            restored_channel.parse()?,
            request.id,
            "answer",
            vec!["PostgreSQL".into()],
        )
        .await?;
    f.mock.send(json!({"method":"serverRequest/resolved","params":{"threadId":"thread-b","requestId":19}})).await;
    f.status(b.id, "running").await?;
    for _ in 0..200 {
        if api.state.lock().unwrap().messages[restored_channel][1]["components"] == json!([]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        api.state.lock().unwrap().messages[restored_channel][1]["components"],
        json!([])
    );
    delivery.abort();
    let _ = delivery.await;
    sqlx::query("UPDATE sessions SET channel_state='missing' WHERE id=$1")
        .bind(a.id)
        .execute(&f.manager.store.pool)
        .await?;
    f.manager
        .store
        .enqueue(a.id, "missing:a", "Keep this output for reconnection", None)
        .await?;
    f.manager
        .store
        .enqueue(b.id, "still-live:b", "Deliver to beta", None)
        .await?;
    let queued = f.manager.store.outbox().await?;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].session_id, b.id);
    let long = "😀".repeat(2000);
    f.manager
        .store
        .enqueue(b.id, "long:answer", &long, None)
        .await?;
    let (preview, attachment): (String, Option<String>) = sqlx::query_as(
        "SELECT content,attachment FROM discord_outbox WHERE dedupe_key='long:answer:0'",
    )
    .fetch_one(&f.manager.store.pool)
    .await?;
    assert!(preview.encode_utf16().count() <= 1700);
    assert_eq!(attachment.as_deref(), Some(long.as_str()));
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn failed_channel_creation_never_submits_or_replays_codex_work() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("setup", true).await?;
    api.state.lock().unwrap().fail_next =
        Some(("POST".into(), "/guilds/7/channels".into(), 500, 0));
    assert!(
        f.manager
            .create_task(42, 7, "200", "setup", "Task")
            .await
            .is_err()
    );
    let s = f.manager.store.sessions(1).await?.remove(0);
    assert_eq!(s.status, "unavailable");
    assert!(!s.repository_claimed && s.codex_thread_id.is_none());
    f.manager.reconcile_channels().await?;
    f.manager.maintain_channels_once().await?;
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    assert_eq!(f.mock.count("thread/start"), 0);
    assert_eq!(f.mock.count("turn/start"), 0);
    f.cleanup().await
}
