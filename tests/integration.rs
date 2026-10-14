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
                    "thread/resume" => {
                        json!({"thread":{"id":p["threadId"],"cwd":p["cwd"],"status":{"type":"idle"}},
                        "cwd":p["cwd"],"approvalPolicy":"on-request","approvalsReviewer":"user",
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
            channel_id: 8,
            repos_root: root,
            state_dir: directory.path().join("state"),
            codex_home: directory.path().join("codex"),
            codex_bin: "/not-used".into(),
            tool_path: "/usr/bin:/bin".into(),
            model: None,
            rpc_timeout: Duration::from_secs(1),
        });
        let (mock, client, events) = Mock::new();
        let manager = Arc::new(Manager::new(
            config,
            store,
            client,
            Uuid::new_v4(),
            Arc::new(Http::new("test")),
        )?);
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
        sqlx::query("UPDATE sessions SET discord_thread_id=$2,codex_thread_id=$3,current_turn_id=$4,status=$5,repository_claimed=$6 WHERE id=$1")
            .bind(session.id).bind(channel.to_string()).bind(thread).bind(if idle {None} else {Some(turn)})
            .bind(if idle {"idle"} else {"running"}).bind(!idle).execute(&self.manager.store.pool).await?;
        self.manager.store.session(session.id).await
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
    for (thread, turn, text) in [
        ("thread-a", "turn-a", "Alpha result"),
        ("thread-b", "turn-b", "Beta result"),
    ] {
        f.mock.send(json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":"same-item-id","type":"agentMessage","text":text}}})).await;
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
