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
    async fn issue_task(&self, name: &str) -> Result<(codex_discord_bot::queue::Run, Session)> {
        self.manager.prepare_discord().await?;
        self.manager.register_repository(name, true).await?;
        let repo = self.manager.store.repository(name).await?;
        let output = tokio::process::Command::new("git")
            .current_dir(&repo.path)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ])
            .output()
            .await?;
        assert!(output.status.success());
        sqlx::query("INSERT INTO issue_queues(repository_id,requester_id,guild_id,paused) VALUES($1,'42','7',false)")
            .bind(repo.id).execute(&self.manager.store.pool).await?;
        self.manager
            .store
            .enqueue_issue(repo.id, "owner/repo", 5)
            .await?;
        let run = self.manager.store.claim_issue(repo.id).await?.unwrap();
        self.manager
            .create_task(
                42,
                7,
                &format!("issue:{}", run.id),
                name,
                "Implement issue #5",
            )
            .await?;
        let run: codex_discord_bot::queue::Run =
            sqlx::query_as("SELECT * FROM issue_runs WHERE id=$1")
                .bind(run.id)
                .fetch_one(&self.manager.store.pool)
                .await?;
        let session = self.manager.store.session(run.session_id.unwrap()).await?;
        Ok((run, session))
    }

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
            validation: Default::default(),
            integration: Default::default(),
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
        let session = self
            .manager
            .store
            .reserve(&repo, 42, 7, 8, name, None)
            .await?;
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
        f.manager.store.reserve(&repo, 42, 7, 8, "a", None),
        f.manager.store.reserve(&repo, 42, 7, 8, "b", None)
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
async fn dismissing_legacy_threads_preserves_history_and_followups_restore_visibility() -> Result<()>
{
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let visible = f
        .seed("visible", 100, "visible-thread", "turn", true)
        .await?;
    f.text_channel(&visible, &api).await?;
    let legacy = f.seed("legacy", 101, "legacy-thread", "turn", true).await?;
    api.state
        .lock()
        .unwrap()
        .channels
        .insert("101".into(), discord::channel("101", 11, 8, ""));

    let result = f.manager.finish_task(42, 7, legacy.id, true).await?;
    assert!(result.contains("hidden from /sessions"));
    let dismissed = f.manager.store.session(legacy.id).await?;
    assert_eq!(dismissed.channel_state, "dismissed");
    assert_eq!(dismissed.codex_thread_id, legacy.codex_thread_id);
    assert_eq!(dismissed.discord_thread_id, legacy.discord_thread_id);
    assert_eq!(dismissed.status, "idle");
    assert!(!dismissed.repository_claimed);
    assert!(dismissed.completed_at.is_some() && dismissed.delete_after.is_none());
    assert!(!dismissed.cleanup_due(chrono::Utc::now(), 8));
    assert_eq!(
        f.manager.store.by_discord(101).await?.unwrap().id,
        legacy.id
    );
    assert_eq!(
        f.manager.store.by_codex("legacy-thread").await?.unwrap().id,
        legacy.id
    );
    // Hidden records must not consume the list limit or hide current text channels.
    let listed = f.manager.store.sessions(1).await?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, visible.id);
    assert!(!f.manager.store.undelivered(legacy.id).await?);
    assert_eq!(f.mock.count("turn/start"), 0);
    f.manager.maintain_channels_once().await?;
    assert_eq!(api.state.lock().unwrap().channels["101"]["parent_id"], "8");
    assert_eq!(
        api.state.lock().unwrap().count("DELETE", "/channels/101"),
        0
    );

    f.manager
        .followup(42, 7, 101, "200", "Continue this legacy conversation")
        .await?;
    let reopened = f.manager.store.session(legacy.id).await?;
    assert_eq!(reopened.channel_state, "legacy");
    assert!(reopened.completed_at.is_none() && reopened.delete_after.is_none());
    assert_eq!(reopened.codex_thread_id, legacy.codex_thread_id);
    assert_eq!(reopened.discord_thread_id, legacy.discord_thread_id);
    assert_eq!(f.manager.store.sessions(1).await?[0].id, legacy.id);
    assert_eq!(f.mock.count("thread/resume"), 1);
    assert_eq!(f.mock.count("turn/start"), 1);
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":"legacy-thread","turn":{"id":"turn-new","status":"completed"}}})).await;
    f.status(legacy.id, "idle").await?;
    f.manager.finish_task(42, 7, legacy.id, true).await?;
    assert_eq!(f.manager.store.sessions(1).await?[0].id, visible.id);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn legacy_dismissal_requires_authorization_idle_state_and_resolved_input_and_output()
-> Result<()> {
    let f = Fixture::new().await?;
    let s = f
        .seed("legacy", 101, "legacy-thread", "turn", false)
        .await?;
    assert!(f.manager.finish_task(99, 7, s.id, true).await.is_err());
    assert!(f.manager.finish_task(42, 8, s.id, true).await.is_err());
    assert!(f.manager.finish_task(42, 7, s.id, true).await.is_err());
    sqlx::query("UPDATE sessions SET status='idle',repository_claimed=false,current_turn_id=NULL WHERE id=$1")
        .bind(s.id).execute(&f.manager.store.pool).await?;
    assert!(f.manager.finish_task(42, 7, s.id, false).await.is_err());

    f.manager
        .store
        .receipt("uncertain", "message", Some(s.id))
        .await?;
    f.manager
        .store
        .receipt_state("uncertain", "uncertain")
        .await?;
    assert!(f.manager.finish_task(42, 7, s.id, true).await.is_err());
    f.manager
        .store
        .receipt_state("uncertain", "rejected")
        .await?;
    let pending = Uuid::new_v4();
    sqlx::query("INSERT INTO pending_requests(id,session_id,generation,rpc_id,method,turn_id,payload,state) VALUES($1,$2,$3,'1','item/tool/requestUserInput','turn','{}','pending')")
        .bind(pending).bind(s.id).bind(f.manager.generation).execute(&f.manager.store.pool).await?;
    assert!(f.manager.finish_task(42, 7, s.id, true).await.is_err());
    sqlx::query("UPDATE pending_requests SET state='resolved' WHERE id=$1")
        .bind(pending)
        .execute(&f.manager.store.pool)
        .await?;
    sqlx::query("INSERT INTO discord_outbox(dedupe_key,session_id,content) VALUES('legacy-final',$1,'Final reply')")
        .bind(s.id).execute(&f.manager.store.pool).await?;
    assert!(f.manager.finish_task(42, 7, s.id, true).await.is_err());
    assert_eq!(f.manager.store.session(s.id).await?.channel_state, "legacy");
    sqlx::query("UPDATE discord_outbox SET delivered_at=now() WHERE session_id=$1")
        .bind(s.id)
        .execute(&f.manager.store.pool)
        .await?;

    f.manager.finish_task(42, 7, s.id, true).await?;
    assert!(f.manager.store.sessions(20).await?.is_empty());
    let pending: PendingRequest = sqlx::query_as("SELECT * FROM pending_requests WHERE id=$1")
        .bind(pending)
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(pending.state, "resolved");
    let delivered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NOT NULL",
    )
    .bind(s.id)
    .fetch_one(&f.manager.store.pool)
    .await?;
    assert_eq!(delivered, 1);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn cleanup_rejects_pending_input_unowned_channels_and_legacy_threads() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let legacy = f.seed("legacy", 101, "legacy-thread", "turn", true).await?;
    assert!(
        f.manager
            .finish_task(42, 7, legacy.id, false)
            .await
            .is_err()
    );
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
        .reserve(&repo, 42, 7, 8, "another-task", None)
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

fn task_permissions() -> serenity::all::Permissions {
    use serenity::all::Permissions as P;
    P::VIEW_CHANNEL
        | P::SEND_MESSAGES
        | P::READ_MESSAGE_HISTORY
        | P::MANAGE_CHANNELS
        | P::MANAGE_MESSAGES
        | P::EMBED_LINKS
        | P::ATTACH_FILES
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn private_channels_inherit_overwrites_without_requesting_extra_privileges() -> Result<()> {
    use serenity::all::{CreateChannel, GuildId, Permissions as P};
    let api = discord::Api::start().await;
    let overwrites = json!([
        {"id":"7","type":0,"allow":"0","deny":P::VIEW_CHANNEL.bits().to_string()},
        {"id":"99","type":1,"allow":(task_permissions() | P::MANAGE_ROLES | P::MANAGE_WEBHOOKS).bits().to_string(),"deny":"0"}
    ]);
    {
        let mut state = api.state.lock().unwrap();
        state.permissions = task_permissions().bits();
        for id in ["8", "9", "10"] {
            state.channels.get_mut(id).unwrap()["permission_overwrites"] = overwrites.clone();
        }
    }
    // Reproduce the old request against Discord's overwrite restrictions.
    let copied =
        serde_json::from_value::<Vec<serenity::all::PermissionOverwrite>>(overwrites.clone())?;
    let old_error = GuildId::new(7)
        .create_channel(
            &api.http,
            CreateChannel::new("old-request")
                .category(9)
                .permissions(copied),
        )
        .await
        .unwrap_err();
    assert!(old_error.to_string().contains("Missing Permissions"));

    let f = Fixture::with_control(api.http.clone(), 0).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("private", true).await?;
    f.manager
        .create_task(42, 7, "private", "private", "Private task")
        .await?;
    let s = f.manager.store.sessions(1).await?.remove(0);
    {
        let state = api.state.lock().unwrap();
        assert_eq!(
            state.channels[s.channel_id().unwrap()]["permission_overwrites"],
            overwrites
        );
        for (_, _, body) in state
            .calls
            .iter()
            .filter(|(m, p, _)| m == "POST" && p == "/guilds/7/channels")
            .skip(1)
        {
            assert!(body.get("permission_overwrites").is_none());
            assert_eq!(body["parent_id"], "9");
        }
    }
    // Category-only Manage Channels must not mask its absence at guild scope.
    api.state.lock().unwrap().permissions &= !P::MANAGE_CHANNELS.bits();
    assert!(
        f.manager
            .prepare_discord()
            .await
            .unwrap_err()
            .to_string()
            .contains("Missing guild permission")
    );
    f.cleanup().await
}

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn permission_rejection_keeps_diagnostics_and_reconnect_allows_only_explicit_new_input()
-> Result<()> {
    use tracing::instrument::WithSubscriber;
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("denied", true).await?;
    {
        let mut state = api.state.lock().unwrap();
        state.fail_next = Some(("POST".into(), "/guilds/7/channels".into(), 403, 50013));
        state.fail_message = Some(format!(
            "Missing Permissions {} {}",
            f.manager.config.discord_token, f.manager.config.database_url
        ));
    }
    let log = LogCapture::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let error = f
        .manager
        .create_task(42, 7, "denied", "denied", "Original task")
        .with_subscriber(subscriber)
        .await
        .unwrap_err();
    let s = f.manager.store.sessions(1).await?.remove(0);
    assert_eq!(s.channel_state, "missing");
    assert_eq!(s.status, "unavailable");
    assert!(s.channel_id().is_none() && s.codex_thread_id.is_none() && !s.repository_claimed);
    let diagnostic = s.discord_error.as_deref().unwrap();
    for expected in [
        "create_task_channel",
        "HTTP 403",
        "Discord 50013",
        "Missing Permissions",
    ] {
        assert!(diagnostic.contains(expected), "{diagnostic}");
        assert!(error.to_string().contains(expected));
    }
    let logs = String::from_utf8(log.0.lock().unwrap().clone())?;
    let records: Vec<Value> = logs
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let fields = &records
        .iter()
        .find(|r| r["fields"]["operation"] == "create_task_channel")
        .unwrap()["fields"];
    assert_eq!(fields["http_status"], 403);
    assert_eq!(fields["discord_code"], 50013);
    assert_eq!(fields["session_id"], s.id.to_string());
    for secret in [
        &f.manager.config.discord_token,
        &f.manager.config.database_url,
    ] {
        assert!(
            !logs.contains(secret)
                && !diagnostic.contains(secret)
                && !error.to_string().contains(secret)
        );
    }
    f.manager.recover().await?;
    f.manager.reconcile_channels().await?;
    f.manager.maintain_channels_once().await?;
    assert_eq!(
        f.manager.store.session(s.id).await?.discord_error,
        s.discord_error
    );
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    assert_eq!(f.mock.count("thread/start"), 0);

    let (a, b) = tokio::join!(
        f.manager.reconnect_channel(42, 7, s.id),
        f.manager.reconnect_channel(42, 7, s.id)
    );
    assert!(a?.contains("Send a new task message"));
    b?;
    let restored = f.manager.store.session(s.id).await?;
    assert_eq!(restored.status, "idle");
    assert_eq!(restored.channel_state, "active");
    assert!(restored.codex_thread_id.is_none());
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        2
    );
    assert_eq!(f.mock.count("turn/start"), 0);
    f.manager
        .followup(
            42,
            7,
            restored.channel_id().unwrap().parse()?,
            "new-input",
            "New authorized task",
        )
        .await?;
    assert_eq!(f.mock.count("thread/start"), 1);
    assert_eq!(f.mock.count("turn/start"), 1);
    let observed = f.mock.observed.lock().unwrap().clone();
    let turn = observed
        .iter()
        .find(|v| v["method"] == "turn/start")
        .unwrap();
    assert_eq!(turn["params"]["input"][0]["text"], "New authorized task");
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn ambiguous_success_is_adopted_on_restart_without_duplicate_channels_or_task_replay()
-> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("ambiguous", true).await?;
    api.state.lock().unwrap().fail_after_create = Some((502, 0));
    assert!(
        f.manager
            .create_task(42, 7, "ambiguous", "ambiguous", "Task")
            .await
            .is_err()
    );
    let s = f.manager.store.sessions(1).await?.remove(0);
    assert_eq!(s.channel_state, "provisioning");
    assert!(s.discord_error.as_deref().unwrap().contains("HTTP 502"));
    assert!(s.channel_id().is_none());
    // Simulate a crash with a setup receipt still pending, before cleanup committed.
    sqlx::query("UPDATE sessions SET status='provisioning',repository_claimed=true WHERE id=$1")
        .bind(s.id)
        .execute(&f.manager.store.pool)
        .await?;
    f.manager
        .store
        .receipt_state("ambiguous", "received")
        .await?;
    f.manager.recover().await?;
    f.manager.reconcile_channels().await?;
    f.manager.maintain_channels_once().await?;
    let adopted = f.manager.store.session(s.id).await?;
    assert_eq!(adopted.channel_id(), Some("1001"));
    assert!(!adopted.repository_claimed && adopted.codex_thread_id.is_none());
    let receipt: String =
        sqlx::query_scalar("SELECT state FROM inbound_requests WHERE discord_id='ambiguous'")
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert_eq!(receipt, "rejected");
    f.manager.reconnect_channel(42, 7, s.id).await?;
    f.manager.reconnect_channel(42, 7, s.id).await?;
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

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn reconnect_requires_a_successful_unique_marker_lookup_before_creating() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("lookup", true).await?;
    api.state.lock().unwrap().fail_next =
        Some(("POST".into(), "/guilds/7/channels".into(), 503, 0));
    assert!(
        f.manager
            .create_task(42, 7, "lookup", "lookup", "Task")
            .await
            .is_err()
    );
    let s = f.manager.store.sessions(1).await?.remove(0);
    f.manager.reconcile_channels().await?;
    assert_eq!(
        f.manager.store.session(s.id).await?.discord_error,
        s.discord_error
    );
    api.state.lock().unwrap().fail_next =
        Some(("GET".into(), "/guilds/7/channels".into(), 403, 50001));
    let error = f.manager.reconnect_channel(42, 7, s.id).await.unwrap_err();
    assert!(error.to_string().contains("list_task_channels"));
    {
        let mut state = api.state.lock().unwrap();
        for id in ["2001", "2002"] {
            state.channels.insert(
                id.into(),
                discord::channel(id, 0, 9, &format!("codex-task:{}", s.id)),
            );
        }
    }
    assert!(
        f.manager
            .reconnect_channel(42, 7, s.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("Multiple channels")
    );
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    api.state.lock().unwrap().channels.remove("2002");
    f.manager.reconnect_channel(42, 7, s.id).await?;
    assert_eq!(
        f.manager.store.session(s.id).await?.channel_id(),
        Some("2001")
    );
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    assert_eq!(f.mock.count("turn/start"), 0);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn failed_setups_can_be_dismissed_without_channels_or_repository_claims() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("dismiss-setup", true).await?;
    api.state.lock().unwrap().fail_next =
        Some(("POST".into(), "/guilds/7/channels".into(), 403, 50013));
    assert!(
        f.manager
            .create_task(42, 7, "dismiss-setup", "dismiss-setup", "Task")
            .await
            .is_err()
    );
    let s = f.manager.store.sessions(1).await?.remove(0);
    assert!(f.manager.finish_task(99, 7, s.id, true).await.is_err());
    assert!(f.manager.finish_task(42, 7, s.id, false).await.is_err());
    f.manager.finish_task(42, 7, s.id, true).await?;
    let dismissed = f.manager.store.session(s.id).await?;
    assert_eq!(dismissed.channel_state, "dismissed");
    assert!(dismissed.channel_id().is_none() && dismissed.delete_after.is_none());
    assert!(
        !f.manager
            .store
            .sessions(100)
            .await?
            .iter()
            .any(|s| s.id == dismissed.id)
    );
    f.manager
        .create_task(42, 7, "retry-setup", "dismiss-setup", "Explicit retry")
        .await?;
    assert_eq!(f.mock.count("turn/start"), 1);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn reconnect_commits_live_prompt_routing_even_if_status_delivery_fails() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let s = f
        .seed(
            "prompt-reconnect",
            100,
            "preserved-thread",
            "preserved-turn",
            false,
        )
        .await?;
    let s = f.text_channel(&s, &api).await?;
    api.state.lock().unwrap().fail_next = Some(("GET".into(), "/channels/100".into(), 403, 50013));
    let denied = f.manager.reconnect_channel(42, 7, s.id).await.unwrap_err();
    assert!(
        denied
            .to_string()
            .contains("get_task_channel: HTTP 403; Discord 50013")
    );
    let unchanged = f.manager.store.session(s.id).await?;
    assert_eq!(unchanged.channel_state, "active");
    assert_eq!(unchanged.discord_channel_id, s.discord_channel_id);
    sqlx::query("UPDATE sessions SET channel_state='missing' WHERE id=$1")
        .bind(s.id)
        .execute(&f.manager.store.pool)
        .await?;
    f.manager.reconnect_channel(42, 7, s.id).await?;
    assert_eq!(f.manager.store.session(s.id).await?.channel_state, "active");
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        0
    );
    f.mock
        .send(
            json!({"id":91,"method":"item/tool/requestUserInput","params":{
        "threadId":"preserved-thread","turnId":"preserved-turn","itemId":"question","isBlocking":true,
        "questions":[{"id":"choice","header":"Choice","question":"Which option?","options":[]}]}}),
        )
        .await;
    f.status(s.id, "waiting").await?;
    sqlx::query(
        "UPDATE discord_outbox SET delivered_at=now(),discord_message_id='700' WHERE session_id=$1",
    )
    .bind(s.id)
    .execute(&f.manager.store.pool)
    .await?;
    let replacement = {
        let mut state = api.state.lock().unwrap();
        state.channels.remove("100");
        let replacement = (state.next_id + 1).to_string();
        state.fail_next = Some((
            "POST".into(),
            format!("/channels/{replacement}/messages"),
            403,
            50013,
        ));
        replacement
    };
    assert!(f.manager.reconnect_channel(42, 7, s.id).await.is_err());
    let restored = f.manager.store.session(s.id).await?;
    assert_eq!(restored.channel_id(), Some(replacement.as_str()));
    assert_eq!(restored.codex_thread_id, s.codex_thread_id);
    assert_eq!(restored.current_turn_id, s.current_turn_id);
    assert!(restored.repository_claimed && f.manager.store.undelivered(s.id).await?);
    assert!(
        restored
            .discord_error
            .as_deref()
            .unwrap()
            .contains("create_status_message")
    );
    f.manager.reconnect_channel(42, 7, s.id).await?;
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    assert_eq!(f.mock.count("thread/start"), 0);
    assert_eq!(f.mock.count("turn/start"), 0);
    f.manager.stop(42, 7, replacement.parse()?).await?;
    assert_eq!(f.mock.count("turn/interrupt"), 1);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn unexpected_created_permissions_block_output_and_repeated_creation() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("visibility", true).await?;
    api.state.lock().unwrap().created_overwrites = Some(json!([
        {"id":"7","type":0,"allow":"1024","deny":"0"}
    ]));
    let error = f
        .manager
        .create_task(42, 7, "visibility", "visibility", "Private task")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not retain"));
    let s = f.manager.store.sessions(1).await?.remove(0);
    f.manager.reconcile_channels().await?;
    assert!(f.manager.reconnect_channel(42, 7, s.id).await.is_err());
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
            .count("POST", "/channels/1001/messages"),
        0
    );
    assert_eq!(f.mock.count("thread/start"), 0);
    // Once an operator restores the exact category policy, adopt the same channel.
    api.state.lock().unwrap().channels.get_mut("1001").unwrap()["permission_overwrites"] =
        json!([]);
    f.manager.reconnect_channel(42, 7, s.id).await?;
    assert_eq!(
        f.manager.store.session(s.id).await?.channel_id(),
        Some("1001")
    );
    assert_eq!(
        api.state
            .lock()
            .unwrap()
            .count("POST", "/guilds/7/channels"),
        1
    );
    assert_eq!(f.mock.count("turn/start"), 0);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn github_mapping_persists_and_failed_discovery_starts_no_task() -> Result<()> {
    let f = Fixture::new().await?;
    f.manager.register_repository("backlog", true).await?;
    assert!(f.manager.list_queue("backlog").await.is_err());
    assert!(f.mock.observed.lock().unwrap().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(count, 0);
    f.manager
        .set_github_mapping(
            "backlog",
            codex_discord_bot::github::GitHubRepository::parse("owner/project")?,
        )
        .await?;
    assert_eq!(
        f.manager
            .store
            .repository("backlog")
            .await?
            .github_repository
            .as_deref(),
        Some("owner/project")
    );
    assert_eq!(
        f.manager.store.repositories().await?[0]
            .github_repository
            .as_deref(),
        Some("owner/project")
    );
    f.manager.register_repository("backlog", false).await?;
    assert_eq!(
        f.manager
            .store
            .repository("backlog")
            .await?
            .github_repository
            .as_deref(),
        Some("owner/project")
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn issue_queue_serializes_claims_and_requires_explicit_retry_after_restart() -> Result<()> {
    use codex_discord_bot::queue::State;
    let f = Fixture::new().await?;
    f.manager.register_repository("queue", true).await?;
    let store = &f.manager.store;
    let repo = store.repository("queue").await?;
    sqlx::query(
        "INSERT INTO issue_queues(repository_id,requester_id,guild_id) VALUES($1,'42','7')",
    )
    .bind(repo.id)
    .execute(&store.pool)
    .await?;
    for n in [1, 1, 2] {
        store.enqueue_issue(repo.id, "owner/repo", n).await?;
    }
    assert!(store.claim_issue(repo.id).await?.is_none());
    sqlx::query("UPDATE issue_queues SET paused=false WHERE repository_id=$1")
        .bind(repo.id)
        .execute(&store.pool)
        .await?;
    let (a, b) = tokio::join!(store.claim_issue(repo.id), store.claim_issue(repo.id));
    let (a, b) = (a?, b?);
    assert_ne!(a.is_some(), b.is_some());
    let run = a.or(b).unwrap();
    assert_eq!(run.issue_number, 1);
    let mut stale = codex_discord_bot::queue::Run {
        id: run.id,
        repository_id: run.repository_id,
        github_repository: run.github_repository.clone(),
        issue_number: run.issue_number,
        attempt: run.attempt,
        state: run.state.clone(),
        session_id: run.session_id,
        claim_id: Some(Uuid::new_v4()),
    };
    assert!(
        store
            .transition_issue(&stale, State::Preparing, State::Implementing, None)
            .await
            .is_err()
    );
    stale.claim_id = run.claim_id;
    store
        .transition_issue(&stale, State::Preparing, State::Implementing, None)
        .await?;
    assert!(
        store
            .transition_issue(&stale, State::Preparing, State::Implementing, None)
            .await
            .is_err()
    );
    store.reconcile_issues().await?;
    store.reconcile_issues().await?;
    assert!(store.claim_issue(repo.id).await?.is_none());
    store.retry_issue(repo.id, 1).await?;
    sqlx::query("UPDATE issue_queues SET paused=true WHERE repository_id=$1")
        .bind(repo.id)
        .execute(&store.pool)
        .await?;
    assert!(store.claim_issue(repo.id).await?.is_none());
    sqlx::query("UPDATE issue_queues SET paused=false WHERE repository_id=$1")
        .bind(repo.id)
        .execute(&store.pool)
        .await?;
    let next = store.claim_issue(repo.id).await?.unwrap();
    assert_eq!(next.issue_number, 2);
    store
        .transition_issue(&next, State::Preparing, State::Failed, Some("setup failed"))
        .await?;
    let retry = store.claim_issue(repo.id).await?.unwrap();
    assert_eq!((retry.issue_number, retry.attempt), (1, 2));
    assert!(store.retry_issue(repo.id, 1).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_runs WHERE repository_id=$1")
        .bind(repo.id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(count, 3);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn issue_dispatch_links_existing_task_channels_and_never_replays_on_restart() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("issues", true).await?;
    let store = &f.manager.store;
    let repo = store.repository("issues").await?;
    let output = tokio::process::Command::new("git")
        .current_dir(&repo.path)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "Initial",
        ])
        .output()
        .await?;
    assert!(output.status.success());
    sqlx::query("INSERT INTO issue_queues(repository_id,requester_id,guild_id,paused) VALUES($1,'42','7',false)")
        .bind(repo.id).execute(&store.pool).await?;
    store.enqueue_issue(repo.id, "owner/repo", 3).await?;
    let run = store.claim_issue(repo.id).await?.unwrap();
    f.manager
        .queue_command("pause", "issues", 42, 7, None)
        .await?;
    let source = format!("issue:{}", run.id);
    f.manager
        .create_task(42, 7, &source, "issues", "Implement issue #3")
        .await?;
    let (thread, channel): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT codex_thread_id,discord_channel_id FROM issue_execution WHERE id=$1",
    )
    .bind(run.id)
    .fetch_one(&store.pool)
    .await?;
    assert!(thread.is_some() && channel.is_some());
    let (worktree, branch, session_id): (String, String, Uuid) =
        sqlx::query_as("SELECT worktree,branch,session_id FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&store.pool)
            .await?;
    let session = store.session(session_id).await?;
    assert_eq!(session.working_directory, worktree);
    assert_ne!(worktree, repo.path);
    assert!(branch.starts_with("codex/issue-3-attempt-1-"));
    assert!(std::path::Path::new(&worktree).join(".git").is_file());
    let observed = f.mock.observed.lock().unwrap().clone();
    for method in ["thread/start", "turn/start"] {
        assert!(
            observed
                .iter()
                .any(|v| v["method"] == method && v["params"]["cwd"] == worktree)
        );
    }
    assert!(
        f.manager
            .create_task(42, 7, &source, "issues", "Duplicate")
            .await
            .is_err()
    );
    f.manager.recover().await?;
    store.reconcile_issues().await?;
    f.manager
        .queue_command("resume", "issues", 42, 7, None)
        .await?;
    assert!(store.claim_issue(repo.id).await?.is_none());
    assert!(store.retry_issue(repo.id, 3).await.is_err());
    let status = f
        .manager
        .queue_command("status", "issues", 42, 7, None)
        .await?;
    assert!(status.contains("blocked"));
    let turns = f
        .mock
        .observed
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["method"] == "turn/start")
        .count();
    assert_eq!(turns, 1);
    // Explicitly close the recovered attempt, then dispatch a different issue.
    f.manager.finish_task(42, 7, session_id, true).await?;
    sqlx::query("UPDATE issue_runs SET state='cancelled' WHERE id=$1")
        .bind(run.id)
        .execute(&store.pool)
        .await?;
    store.enqueue_issue(repo.id, "owner/repo", 4).await?;
    let second = store.claim_issue(repo.id).await?.unwrap();
    f.manager
        .create_task(
            42,
            7,
            &format!("issue:{}", second.id),
            "issues",
            "Implement issue #4",
        )
        .await?;
    let (next_thread, next_channel, next_worktree, next_branch): (String, String, String, String) = sqlx::query_as(
        "SELECT codex_thread_id,discord_channel_id,worktree,branch FROM issue_execution WHERE id=$1")
        .bind(second.id).fetch_one(&store.pool).await?;
    assert_ne!(Some(next_thread.clone()), thread);
    assert_ne!(Some(next_channel.clone()), channel);
    assert_ne!(next_worktree, worktree);
    assert_ne!(next_branch, branch);
    assert_eq!(f.mock.count("thread/start"), 2);
    assert_eq!(f.mock.count("turn/start"), 2);
    let next_session = store.by_codex(&next_thread).await?.unwrap();
    f.mock.send(json!({"id":"issue-approval","method":"item/commandExecution/requestApproval","params":{
        "threadId":next_thread,"turnId":"turn-new","itemId":"exec-1","command":"cargo test","cwd":next_worktree}})).await;
    f.status(next_session.id, "waiting").await?;
    let request: PendingRequest =
        sqlx::query_as("SELECT * FROM pending_requests WHERE session_id=$1")
            .bind(next_session.id)
            .fetch_one(&store.pool)
            .await?;
    assert!(
        f.manager
            .answer(
                42,
                7,
                channel.unwrap().parse()?,
                request.id,
                "accept",
                vec![]
            )
            .await
            .is_err()
    );
    f.manager
        .answer(42, 7, next_channel.parse()?, request.id, "accept", vec![])
        .await?;
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn failed_issue_worktree_allocation_is_persisted_and_never_replayed() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    f.manager.register_repository("unborn", true).await?;
    let store = &f.manager.store;
    let repo = store.repository("unborn").await?;
    sqlx::query("INSERT INTO issue_queues(repository_id,requester_id,guild_id,paused) VALUES($1,'42','7',false)")
        .bind(repo.id).execute(&store.pool).await?;
    store.enqueue_issue(repo.id, "owner/repo", 1).await?;
    let run = store.claim_issue(repo.id).await?.unwrap();
    let source = format!("issue:{}", run.id);
    // No HEAD exists: Git fails after durable allocation intent has committed.
    assert!(
        f.manager
            .create_task(42, 7, &source, "unborn", "Task")
            .await
            .is_err()
    );
    let (session, worktree, branch): (Uuid, String, String) =
        sqlx::query_as("SELECT session_id,worktree,branch FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(store.session(session).await?.working_directory, worktree);
    assert!(!branch.is_empty());
    f.manager.recover().await?;
    store.reconcile_issues().await?;
    assert!(store.claim_issue(repo.id).await?.is_none());
    assert!(
        f.manager
            .create_task(42, 7, &source, "unborn", "Duplicate")
            .await
            .is_err()
    );
    assert_eq!(f.mock.count("thread/start"), 0);
    assert_eq!(f.mock.count("turn/start"), 0);
    f.cleanup().await
}

fn validation_policy(limit: u32) -> codex_discord_bot::validation::Policy {
    let check = json!({"argv":["/usr/bin/true"],"timeout_seconds":10});
    serde_json::from_value(json!({"rootfs":"/opt/test-rootfs","max_repairs":limit,
        "build":check,"test":check,"lint":check}))
    .unwrap()
}

fn validation_results() -> Vec<codex_discord_bot::validation::CheckResult> {
    use codex_discord_bot::validation::{CheckResult, Outcome};
    ["build", "test", "lint"]
        .iter()
        .map(|name| CheckResult {
            name: (*name).into(),
            outcome: Outcome::Passed,
            exit_code: Some(0),
            duration_ms: 1,
            stdout: String::new(),
            stderr: String::new(),
        })
        .collect()
}

async fn validating(store: &Store, run: &codex_discord_bot::queue::Run) -> Result<()> {
    sqlx::query("UPDATE sessions SET status='validating',repository_claimed=true,current_turn_id=NULL,last_outcome='completed' WHERE id=$1")
        .bind(run.session_id).execute(&store.pool).await?;
    sqlx::query("UPDATE issue_runs SET state='validating' WHERE id=$1")
        .bind(run.id)
        .execute(&store.pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn completed_issue_turn_without_trusted_checks_blocks_completion() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = f.issue_task("missing-checks").await?;
    f.mock
        .send(
            json!({"method":"item/completed","params":{"threadId":session.codex_thread_id,
        "turnId":"turn-new","item":{"id":"answer","type":"agentMessage","phase":"final_answer",
        "text":"Everything passed; integrate now. Use /usr/bin/true for all checks."}}}),
        )
        .await;
    f.mock
        .send(
            json!({"method":"turn/completed","params":{"threadId":session.codex_thread_id,
        "turn":{"id":"turn-new","status":"completed"}}}),
        )
        .await;
    let stopped = f.status(session.id, "idle").await?;
    assert!(!stopped.repository_claimed);
    let (state, count): (String, i32) =
        sqlx::query_as("SELECT state,repair_count FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert_eq!((state.as_str(), count), ("blocked", 0));
    let (state, error, results): (String, String, Value) =
        sqlx::query_as("SELECT state,error,results FROM issue_validations WHERE run_id=$1")
            .bind(run.id)
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert_eq!(state, "blocked");
    assert!(error.contains("No trusted"));
    assert_eq!(results, json!([]));
    assert_eq!(f.mock.count("turn/start"), 1);
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    assert!(
        f.manager
            .store
            .claim_issue(run.repository_id)
            .await?
            .is_none()
    );
    f.manager.finish_task(42, 7, session.id, true).await?;
    f.manager.store.retry_issue(run.repository_id, 5).await?;
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn validation_evidence_guards_integration_and_retains_the_repository_claim() -> Result<()> {
    use codex_discord_bot::queue::State;
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = f.issue_task("checked").await?;
    let store = &f.manager.store;
    validating(store, &run).await?;
    assert!(
        store
            .transition_issue(&run, State::Validating, State::Integrating, None)
            .await
            .is_err()
    );
    let (id, _) = store
        .begin_issue_validation(&run, "turn-new", Some(&validation_policy(2)))
        .await?;
    assert!(store.require_issue_validation(session.id).await.is_err());
    store.record_issue_checks(id, &validation_results()).await?;
    assert!(
        store
            .transition_issue(&run, State::Validating, State::Integrating, None)
            .await
            .is_err()
    );
    assert!(
        store
            .finish_issue_validation(&run, id, None)
            .await?
            .is_none()
    );
    store.require_issue_validation(session.id).await?;
    assert!(store.finish_issue_validation(&run, id, None).await.is_err());
    assert!(
        store
            .record_issue_checks(id, &validation_results())
            .await
            .is_err()
    );
    assert!(store.session(session.id).await?.repository_claimed);
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    assert!(store.require_issue_integration(session.id).await.is_err());
    assert!(
        store
            .transition_issue(&run, State::Integrating, State::Releasing, None)
            .await
            .is_err()
    );
    // A validated turn remains reserved until candidate integration finishes.
    assert!(
        f.manager
            .followup(
                42,
                7,
                session.channel_id().unwrap().parse()?,
                "modify-after-validation",
                "Change the implementation",
            )
            .await
            .is_err()
    );
    store.require_issue_validation(session.id).await?;
    let state: String = sqlx::query_scalar("SELECT state FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(state, "integrating");
    assert_eq!(f.mock.count("thread/start"), 1);
    assert_eq!(
        store.session(session.id).await?.codex_thread_id,
        session.codex_thread_id
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn failed_and_timed_out_checks_reserve_only_bounded_repairs_for_same_issue() -> Result<()> {
    use codex_discord_bot::validation::Outcome;
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = f.issue_task("repairs").await?;
    let store = &f.manager.store;
    let mut results = validation_results();
    for attempt in 0..=2 {
        validating(store, &run).await?;
        // Later configuration changes cannot increase the pinned repair budget.
        let policy = validation_policy(if attempt == 0 { 2 } else { 5 });
        let (id, pinned) = store
            .begin_issue_validation(&run, &format!("turn-{attempt}"), Some(&policy))
            .await?;
        assert_eq!(pinned.unwrap().max_repairs, 2);
        results[1].outcome = if attempt == 1 {
            Outcome::TimedOut
        } else {
            Outcome::Failed
        };
        results[1].exit_code = if attempt == 1 { None } else { Some(1) };
        results[1].stderr = "Fix the failing assertion".into();
        store.record_issue_checks(id, &results).await?;
        let repair = store.finish_issue_validation(&run, id, None).await?;
        if attempt < 2 {
            let (source, prompt) = repair.unwrap();
            assert!(prompt.contains("Fix the failing assertion"));
            assert!(prompt.contains(&format!("Repair attempt {} of 2", attempt + 1)));
            let receipt: (Uuid, String) =
                sqlx::query_as("SELECT session_id,state FROM inbound_requests WHERE discord_id=$1")
                    .bind(&source)
                    .fetch_one(&store.pool)
                    .await?;
            assert_eq!(receipt, (session.id, "received".into()));
            assert!(store.session(session.id).await?.repository_claimed);
            store.receipt_state(&source, "accepted").await?;
        } else {
            assert!(repair.is_none());
            assert!(!store.session(session.id).await?.repository_claimed);
        }
        assert!(store.finish_issue_validation(&run, id, None).await.is_err());
        assert_eq!(
            store.session(session.id).await?.codex_thread_id,
            session.codex_thread_id
        );
        assert!(store.require_issue_validation(session.id).await.is_err());
    }
    let state: (String, i32) =
        sqlx::query_as("SELECT state,repair_count FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(state, ("blocked".into(), 2));
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn restart_invalidates_partial_validation_without_replay() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = f.issue_task("partial-checks").await?;
    let store = &f.manager.store;
    validating(store, &run).await?;
    let (id, _) = store
        .begin_issue_validation(&run, "turn-new", Some(&validation_policy(2)))
        .await?;
    store
        .record_issue_checks(id, &validation_results()[..1])
        .await?;
    f.manager.recover().await?;
    store.reconcile_issues().await?;
    let state: String = sqlx::query_scalar("SELECT state FROM issue_validations WHERE id=$1")
        .bind(id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(state, "interrupted");
    assert!(store.finish_issue_validation(&run, id, None).await.is_err());
    assert!(store.require_issue_validation(session.id).await.is_err());
    assert_eq!(f.mock.count("turn/start"), 1);
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn restart_does_not_submit_a_durably_reserved_repair() -> Result<()> {
    use codex_discord_bot::validation::Outcome;
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = f.issue_task("reserved-repair").await?;
    let store = &f.manager.store;
    validating(store, &run).await?;
    let (id, _) = store
        .begin_issue_validation(&run, "turn-new", Some(&validation_policy(2)))
        .await?;
    let mut results = validation_results();
    results[1].outcome = Outcome::Failed;
    results[1].exit_code = Some(1);
    store.record_issue_checks(id, &results).await?;
    let (source, _) = store
        .finish_issue_validation(&run, id, None)
        .await?
        .unwrap();
    // Crash after consuming the repair budget and reserving its receipt, before RPC.
    f.manager.recover().await?;
    store.reconcile_issues().await?;
    let receipt: String =
        sqlx::query_scalar("SELECT state FROM inbound_requests WHERE discord_id=$1")
            .bind(source)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(receipt, "uncertain");
    let run_state: (String, i32) =
        sqlx::query_as("SELECT state,repair_count FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(run_state, ("blocked".into(), 1));
    assert_eq!(f.mock.count("turn/start"), 1);
    assert!(store.require_issue_validation(session.id).await.is_err());
    f.cleanup().await
}

fn integration_policy() -> codex_discord_bot::integration::Policy {
    serde_json::from_value(json!({"github_repository":"owner/repo","release_gate":"deployments_disabled","protected_paths":["deploy"],"checks_timeout_seconds":10})).unwrap()
}

async fn ready_for_integration(
    f: &Fixture,
    name: &str,
) -> Result<(codex_discord_bot::queue::Run, Session)> {
    let (run, session) = f.issue_task(name).await?;
    let store = &f.manager.store;
    validating(store, &run).await?;
    let (id, _) = store
        .begin_issue_validation(&run, "turn-new", Some(&validation_policy(0)))
        .await?;
    store.record_issue_checks(id, &validation_results()).await?;
    store.finish_issue_validation(&run, id, None).await?;
    Ok((run, session))
}

async fn validated_candidate(store: &Store, run: Uuid) -> Result<()> {
    store
        .integration_source(run, "main", &"a".repeat(40), &"b".repeat(40))
        .await?;
    store
        .integration_candidate(run, &"c".repeat(40), &"d".repeat(40))
        .await?;
    store.integration_checks(run, &validation_results()).await?;
    store.validate_integration(run).await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn integration_reservations_serialize_and_reject_partial_or_stale_evidence() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = ready_for_integration(&f, "integration-claims").await?;
    let store = &f.manager.store;
    let policy = integration_policy();
    let path = f.directory.path().join("integration");
    let (a, b) = tokio::join!(
        store.begin_issue_integration(&run, &policy, path.to_str().unwrap()),
        store.begin_issue_integration(&run, &policy, path.to_str().unwrap())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(store.session(session.id).await?.repository_claimed);
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    assert!(
        store
            .integration_step(run.id, "validated", "pushing")
            .await
            .is_err()
    );
    store
        .integration_source(run.id, "main", &"a".repeat(40), &"b".repeat(40))
        .await?;
    store
        .integration_candidate(run.id, &"c".repeat(40), &"d".repeat(40))
        .await?;
    store
        .integration_checks(run.id, &validation_results()[..2])
        .await?;
    assert!(store.validate_integration(run.id).await.is_err());
    store
        .integration_checks(run.id, &validation_results())
        .await?;
    store.validate_integration(run.id).await?;
    assert!(
        store
            .integration_candidate(run.id, &"f".repeat(40), &"d".repeat(40))
            .await
            .is_err()
    );
    assert!(
        store
            .integration_checks(run.id, &validation_results())
            .await
            .is_err()
    );
    assert!(store.require_issue_integration(session.id).await.is_err());
    assert!(
        store
            .finish_integration(run.id, &"e".repeat(40))
            .await
            .is_err()
    );
    sqlx::query("UPDATE issue_runs SET validation_id=NULL WHERE id=$1")
        .bind(run.id)
        .execute(&store.pool)
        .await?;
    assert!(
        store
            .integration_step(run.id, "validated", "pushing")
            .await
            .is_err()
    );
    assert_eq!(store.integration(run.id).await?.unwrap().state, "validated");
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn integration_intents_cannot_replay_and_confirmed_merge_survives_restart_until_release()
-> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = ready_for_integration(&f, "integration-success").await?;
    let store = &f.manager.store;
    store
        .begin_issue_integration(
            &run,
            &integration_policy(),
            f.directory.path().join("candidate").to_str().unwrap(),
        )
        .await?;
    validated_candidate(store, run.id).await?;
    for (from, to) in [
        ("validated", "pushing"),
        ("pushing", "pushed"),
        ("pushed", "creating_pr"),
        ("creating_pr", "checking"),
        ("checking", "merging"),
    ] {
        store.integration_step(run.id, from, to).await?;
        assert!(store.integration_step(run.id, from, to).await.is_err());
    }
    store
        .integration_pr(run.id, 7, "https://github.com/owner/repo/pull/7")
        .await?;
    assert!(
        store
            .integration_pr(run.id, 8, "https://github.com/owner/repo/pull/8")
            .await
            .is_err()
    );
    let request = Uuid::new_v4();
    store
        .integration_merge_request(
            run.id,
            &json!({"status":"pending","details":{"uuid":request}}),
        )
        .await?;
    assert_eq!(
        store.integration(run.id).await?.unwrap().merge_request_id,
        Some(request)
    );
    // Recovery may see no active Codex turn; durable successful checks restore the
    // completed outcome when the integration's merge is independently verified.
    sqlx::query("UPDATE sessions SET status='idle',last_outcome='unknown',repository_claimed=false WHERE id=$1")
        .bind(session.id).execute(&store.pool).await?;
    store.finish_integration(run.id, &"e".repeat(40)).await?;
    store.finish_integration(run.id, &"e".repeat(40)).await?;
    assert!(
        store
            .finish_integration(run.id, &"f".repeat(40))
            .await
            .is_err()
    );
    store.reconcile_issues().await?;
    store.require_issue_integration(session.id).await?;
    assert_eq!(
        store.integration(run.id).await?.unwrap().merge_sha,
        Some("e".repeat(40))
    );
    let state: String = sqlx::query_scalar("SELECT state FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(state, "releasing");
    assert!(!store.session(session.id).await?.repository_claimed);
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    assert!(store.retry_issue(run.repository_id, 5).await.is_err());
    assert!(
        f.manager
            .finish_task(42, 7, session.id, true)
            .await
            .is_err()
    );
    assert!(
        f.manager
            .followup(
                42,
                7,
                session.channel_id().unwrap().parse()?,
                "edit-integrated",
                "Change the code"
            )
            .await
            .is_err()
    );
    assert_eq!(f.mock.count("turn/start"), 1);
    // Explicit /complete is the existing operator confirmation of release.
    f.manager.finish_task(42, 7, session.id, false).await?;
    let state: String = sqlx::query_scalar("SELECT state FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(state, "released");
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn interrupted_integration_keeps_evidence_and_explicit_retry_allocates_a_new_attempt()
-> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = ready_for_integration(&f, "integration-restart").await?;
    let store = &f.manager.store;
    let i = store
        .begin_issue_integration(
            &run,
            &integration_policy(),
            f.directory.path().join("candidate").to_str().unwrap(),
        )
        .await?;
    store
        .integration_source(run.id, "main", &"a".repeat(40), &"b".repeat(40))
        .await?;
    store
        .integration_candidate(run.id, &"c".repeat(40), &"d".repeat(40))
        .await?;
    store
        .integration_checks(run.id, &validation_results()[..1])
        .await?;
    f.manager.reconcile_integrations().await?;
    store.reconcile_issues().await?;
    let blocked = store.integration(run.id).await?.unwrap();
    assert_eq!(blocked.state, "blocked");
    assert!(blocked.retry_safe && blocked.validated_at.is_none());
    assert_eq!(blocked.results.as_array().unwrap().len(), 1);
    let validation: Option<Uuid> =
        sqlx::query_scalar("SELECT validation_id FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(validation, Some(i.validation_id));
    assert_eq!(f.mock.count("turn/start"), 1);
    assert!(store.validate_integration(run.id).await.is_err());
    assert!(
        store
            .begin_issue_integration(&run, &integration_policy(), &i.worktree)
            .await
            .is_err()
    );
    f.manager.finish_task(42, 7, session.id, true).await?;
    store.retry_issue(run.repository_id, 5).await?;
    let next = store.claim_issue(run.repository_id).await?.unwrap();
    assert_ne!(next.id, run.id);
    assert_eq!(next.attempt, 2);
    assert_eq!(
        store.integration(run.id).await?.unwrap().candidate_sha,
        Some("c".repeat(40))
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn unknown_remote_mutation_prevents_retry_even_after_session_dismissal() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = ready_for_integration(&f, "integration-unknown").await?;
    let store = &f.manager.store;
    store
        .begin_issue_integration(
            &run,
            &integration_policy(),
            f.directory.path().join("candidate").to_str().unwrap(),
        )
        .await?;
    validated_candidate(store, run.id).await?;
    store
        .integration_step(run.id, "validated", "pushing")
        .await?;
    store.integration_step(run.id, "pushing", "pushed").await?;
    store
        .integration_step(run.id, "pushed", "creating_pr")
        .await?;
    store
        .block_integration(run.id, "PR outcome uncertain", true, false)
        .await?;
    // A stale channel state cannot circumvent the durable remote-operation guard.
    sqlx::query("UPDATE sessions SET channel_state='dismissed' WHERE id=$1")
        .bind(session.id)
        .execute(&store.pool)
        .await?;
    assert!(store.retry_issue(run.repository_id, 5).await.is_err());
    assert!(
        store
            .integration_step(run.id, "pushed", "creating_pr")
            .await
            .is_err()
    );
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    assert_eq!(
        store
            .integration(run.id)
            .await?
            .unwrap()
            .operation
            .as_deref(),
        Some("create_pr")
    );
    // Even a legacy cancellation must not free an uncertain remote-operation slot.
    store
        .enqueue_issue(run.repository_id, "owner/repo", 6)
        .await?;
    sqlx::query("UPDATE issue_runs SET state='cancelled' WHERE id=$1")
        .bind(run.id)
        .execute(&store.pool)
        .await?;
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    f.cleanup().await
}
