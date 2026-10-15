use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Repository {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub github_repository: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Session {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub requester_id: String,
    pub guild_id: String,
    pub parent_channel_id: String,
    pub discord_thread_id: Option<String>,
    pub discord_channel_id: Option<String>,
    pub channel_kind: String,
    pub channel_state: String,
    pub task_title: String,
    pub status_message_id: Option<String>,
    pub status_snapshot: Value,
    pub status_revision: i64,
    pub status_delivered_revision: i64,
    pub discord_failures: i32,
    pub discord_error: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
    pub delete_after: Option<DateTime<Utc>>,
    pub codex_thread_id: Option<String>,
    pub working_directory: String,
    pub status: String,
    pub repository_claimed: bool,
    pub current_turn_id: Option<String>,
    pub last_outcome: Option<String>,
    pub last_error: Option<String>,
    pub last_seen_message_id: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl Session {
    pub fn channel_id(&self) -> Option<&str> {
        self.discord_channel_id
            .as_deref()
            .or(self.discord_thread_id.as_deref())
    }

    pub fn terminal_task(&self) -> bool {
        matches!(self.channel_state.as_str(), "completed" | "dismissed")
    }

    pub fn cleanup_due(&self, now: DateTime<Utc>, control: u64) -> bool {
        self.channel_kind == "text"
            && self.terminal_task()
            && self
                .channel_id()
                .is_some_and(|id| id != control.to_string() && id != self.parent_channel_id)
            && !self.repository_claimed
            && self.current_turn_id.is_none()
            && matches!(self.status.as_str(), "idle" | "unavailable")
            && (self.codex_thread_id.is_some() || self.channel_state == "dismissed")
            && self.completed_at.is_some()
            && self.delete_after.is_some_and(|date| date <= now)
            && self.status_message_id.is_some()
            && self.status_delivered_revision == self.status_revision
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PendingRequest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub generation: Uuid,
    pub rpc_id: Value,
    pub method: String,
    pub turn_id: String,
    pub item_id: Option<String>,
    pub payload: Value,
    pub state: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct OutboxMessage {
    pub notify_requester: bool,
    pub id: i64,
    pub session_id: Uuid,
    pub content: String,
    pub request_id: Option<Uuid>,
    pub discord_message_id: Option<String>,
    pub attempts: i32,
    pub attachment: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET statement_timeout = '15s'")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .context("PostgreSQL connection failed (check DATABASE_URL)")?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!().run(&self.pool).await?;
        Ok(())
    }

    pub async fn repository(&self, name: &str) -> Result<Repository> {
        sqlx::query_as("SELECT id,name,path,github_repository FROM repositories WHERE name=$1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .context("Repository is not registered; use /repo add")
    }
    pub async fn repositories(&self) -> Result<Vec<Repository>> {
        Ok(
            sqlx::query_as("SELECT id,name,path,github_repository FROM repositories ORDER BY name")
                .fetch_all(&self.pool)
                .await?,
        )
    }
    pub async fn register(&self, name: &str, path: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO repositories(id,name,path) VALUES($1,$2,$3) ON CONFLICT(name) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(path)
        .execute(&self.pool)
        .await?;
        anyhow::ensure!(
            self.repository(name).await?.path == path,
            "Registered repository path differs"
        );
        Ok(())
    }
    pub async fn session(&self, id: Uuid) -> Result<Session> {
        Ok(sqlx::query_as("SELECT * FROM sessions WHERE id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?)
    }
    pub async fn by_discord(&self, thread: u64) -> Result<Option<Session>> {
        Ok(
            sqlx::query_as("SELECT * FROM sessions WHERE COALESCE(discord_channel_id,discord_thread_id)=$1 AND channel_state <> 'deleted'")
                .bind(thread.to_string())
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    pub async fn by_codex(&self, thread: &str) -> Result<Option<Session>> {
        Ok(
            sqlx::query_as("SELECT * FROM sessions WHERE codex_thread_id=$1")
                .bind(thread)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    pub async fn sessions(&self, limit: i64) -> Result<Vec<Session>> {
        Ok(sqlx::query_as(
            "SELECT * FROM sessions WHERE NOT (channel_state='dismissed' AND (channel_kind='thread' OR COALESCE(discord_channel_id,discord_thread_id) IS NULL)) ORDER BY repository_claimed DESC, updated_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
    pub async fn receipt(&self, source: &str, kind: &str, session: Option<Uuid>) -> Result<bool> {
        Ok(sqlx::query("INSERT INTO inbound_requests(discord_id,kind,session_id,state) VALUES($1,$2,$3,'received') ON CONFLICT DO NOTHING")
            .bind(source).bind(kind).bind(session).execute(&self.pool).await?.rows_affected() == 1)
    }
    pub async fn receipt_state(&self, source: &str, state: &str) -> Result<()> {
        sqlx::query("UPDATE inbound_requests SET state=$2, updated_at=now() WHERE discord_id=$1")
            .bind(source)
            .bind(state)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn reserve(
        &self,
        repo: &Repository,
        requester: u64,
        guild: u64,
        parent: u64,
        source: &str,
        task_title: Option<&str>,
    ) -> Result<Session> {
        let id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query("INSERT INTO sessions(id,repository_id,requester_id,guild_id,parent_channel_id,working_directory,status,channel_kind,channel_state,task_title) VALUES($1,$2,$3,$4,$5,$6,'provisioning',$7,$8,$9)")
            .bind(id).bind(repo.id).bind(requester.to_string()).bind(guild.to_string()).bind(parent.to_string()).bind(&repo.path)
            .bind(if task_title.is_some() { "text" } else { "thread" })
            .bind(if task_title.is_some() { "provisioning" } else { "legacy" })
            .bind(task_title.unwrap_or_default()).execute(&mut *tx).await;
        if let Err(e) = result {
            if e.as_database_error()
                .is_some_and(|e| e.is_unique_violation())
            {
                anyhow::bail!("This repository is busy; use /sessions to find its active task");
            }
            return Err(e.into());
        }
        sqlx::query(
            "UPDATE inbound_requests SET session_id=$2,updated_at=now() WHERE discord_id=$1",
        )
        .bind(source)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE issue_runs SET session_id=$2,updated_at=now() WHERE 'issue:' || id::text = $1 AND state='preparing'")
            .bind(source).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        self.session(id).await
    }
    pub async fn pending(&self, id: Uuid) -> Result<PendingRequest> {
        sqlx::query_as("SELECT * FROM pending_requests WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .context("Interactive request no longer exists")
    }
    pub async fn activity(&self, id: Uuid, text: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($2::text)),updated_at=now() WHERE id=$1")
            .bind(id).bind(crate::discord::render::truncate(text, 300)).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn unresolved(&self, id: Uuid) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pending_requests WHERE session_id=$1 AND state IN ('pending','responding')) OR EXISTS(SELECT 1 FROM inbound_requests WHERE session_id=$1 AND state IN ('received','sending','uncertain'))")
            .bind(id).fetch_one(&self.pool).await?)
    }

    pub async fn undelivered(&self, id: Uuid) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM discord_outbox WHERE session_id=$1 AND delivered_at IS NULL)")
            .bind(id).fetch_one(&self.pool).await?)
    }
    pub async fn enqueue(
        &self,
        session: Uuid,
        key: &str,
        content: &str,
        request: Option<Uuid>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        enqueue(&mut tx, session, key, content, request).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn outbox(&self) -> Result<Vec<OutboxMessage>> {
        Ok(sqlx::query_as("SELECT o.* FROM discord_outbox o JOIN sessions s ON s.id=o.session_id WHERE o.delivered_at IS NULL AND o.next_attempt_at<=now() AND s.channel_state NOT IN ('missing','deleted') AND COALESCE(s.discord_channel_id,s.discord_thread_id) IS NOT NULL AND NOT EXISTS (SELECT 1 FROM discord_outbox earlier WHERE earlier.session_id=o.session_id AND earlier.id<o.id AND earlier.delivered_at IS NULL) ORDER BY o.id LIMIT 20")
            .fetch_all(&self.pool).await?)
    }
}

pub async fn enqueue(
    conn: &mut PgConnection,
    session: Uuid,
    key: &str,
    content: &str,
    request: Option<Uuid>,
) -> Result<()> {
    let (chunks, attachment) = if request.is_some() {
        (crate::discord::render::interactive_chunks(content), None)
    } else {
        let preview = crate::discord::render::truncate(content, 1700);
        let attachment = (preview != content).then(|| content.to_owned());
        (vec![preview], attachment)
    };
    let last = chunks.len().saturating_sub(1);
    let notify_requester = needs_attention(key, request.is_some());
    for (index, chunk) in chunks.into_iter().enumerate() {
        sqlx::query("INSERT INTO discord_outbox(dedupe_key,session_id,content,request_id,notify_requester,attachment) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(dedupe_key) DO NOTHING")
            .bind(format!("{key}:{index}")).bind(session).bind(chunk).bind(if index == last { request } else { None }).bind(notify_requester && index == last)
            .bind(&attachment)
            .execute(&mut *conn).await?;
    }
    Ok(())
}

// Event keys describe durable bot events; never infer notifications from model prose.
fn needs_attention(key: &str, interactive: bool) -> bool {
    interactive
        || key.ends_with(":terminal")
        || [
            "recovery:",
            "recovered:",
            "expired:",
            "task-completed:",
            "task-dismissed:",
            "failure:",
            "question:",
        ]
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::needs_attention;

    #[test]
    fn attention_events_notify_but_progress_and_retry_errors_do_not() {
        assert!(needs_attention("request:question", true));
        for key in [
            "turn:terminal",
            "recovery:generation:session",
            "recovered:generation:session",
            "expired:request",
        ] {
            assert!(needs_attention(key, false), "{key}");
        }
        for key in [
            "turn:message",
            "turn:plan",
            "turn:error:true",
            "turn:error:false",
        ] {
            assert!(!needs_attention(key, false), "{key}");
        }
    }
}
