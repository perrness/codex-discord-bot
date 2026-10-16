use super::*;
use codex_discord_bot::{
    config::IssueWorkers,
    persistence::Repository,
    queue::{Run, State},
};

async fn queue(f: &Fixture, name: &str, count: i64) -> Result<Repository> {
    f.manager.register_repository(name, true).await?;
    let repo = f.manager.store.repository(name).await?;
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
        .bind(repo.id).execute(&f.manager.store.pool).await?;
    for number in 1..=count {
        f.manager
            .store
            .enqueue_issue(repo.id, "owner/repo", number)
            .await?;
    }
    Ok(repo)
}

async fn start(f: &Fixture, repo: &Repository, run: &Run) -> Result<(Run, Session)> {
    f.manager
        .create_task(
            42,
            7,
            &format!("issue:{}", run.id),
            &repo.name,
            "Implement issue",
        )
        .await?;
    let run: Run = sqlx::query_as("SELECT * FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    let session = f.manager.store.session(run.session_id.unwrap()).await?;
    Ok((run, session))
}

async fn pass_checks(store: &Store, run: &Run) -> Result<()> {
    validating(store, run).await?;
    let (id, _) = store
        .begin_issue_validation(run, "turn-new", Some(&validation_policy(0)))
        .await?;
    store.record_issue_checks(id, &validation_results()).await?;
    store.finish_issue_validation(run, id, None).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn concurrent_admission_enforces_global_and_repository_limits() -> Result<()> {
    let f = Fixture::new().await?;
    let store = &f.manager.store;
    store
        .configure_issue_workers(IssueWorkers {
            global: 3,
            per_repository: 2,
        })
        .await?;
    let a = queue(&f, "alpha", 4).await?;
    let b = queue(&f, "beta", 4).await?;
    let claims = futures_util::future::join_all(
        [a.id, b.id, a.id, b.id, a.id, b.id].map(|repo| store.claim_issue(repo)),
    )
    .await;
    let claims: Vec<Run> = claims
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(claims.len(), 3);
    for repo in [a.id, b.id] {
        assert!(
            claims
                .iter()
                .filter(|run| run.repository_id == repo)
                .count()
                <= 2
        );
        assert!(store.claim_issue(repo).await?.is_none());
    }
    store
        .configure_issue_workers(IssueWorkers {
            global: 1,
            per_repository: 1,
        })
        .await?;
    let occupied: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_worker_slots")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(
        occupied, 3,
        "Lowering limits must not cancel existing workers"
    );
    for run in &claims {
        store
            .transition_issue(run, State::Preparing, State::Cancelled, None)
            .await?;
    }
    let next = store.claim_issue(a.id).await?.unwrap();
    assert!(store.claim_issue(b.id).await?.is_none());
    store
        .transition_issue(&next, State::Preparing, State::Blocked, Some("inspect"))
        .await?;
    assert!(
        store.claim_issue(b.id).await?.is_none(),
        "Blocked workers retain capacity"
    );
    let status = f
        .manager
        .queue_command("status", "alpha", 42, 7, None)
        .await?;
    assert!(
        status.contains("1/1 global") && status.contains("Queued:") && status.contains("blocked")
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn manual_reservation_racing_issue_admission_cannot_share_a_repository() -> Result<()> {
    let f = Fixture::new().await?;
    let store = &f.manager.store;
    store
        .configure_issue_workers(IssueWorkers {
            global: 32,
            per_repository: 2,
        })
        .await?;
    for number in 0..8 {
        let repo = queue(&f, &format!("race{number}"), 1).await?;
        let (issue, manual) = tokio::join!(
            store.claim_issue(repo.id),
            store.reserve(&repo, 42, 7, 8, "manual", None)
        );
        assert_ne!(issue?.is_some(), manual.is_ok());
    }
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn same_repository_workers_isolate_files_threads_channels_and_terminal_events() -> Result<()>
{
    use rustix::fs::{FlockOperation, flock};
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let store = &f.manager.store;
    store
        .configure_issue_workers(IssueWorkers {
            global: 2,
            per_repository: 2,
        })
        .await?;
    let repo = queue(&f, "parallel", 2).await?;
    let a = store.claim_issue(repo.id).await?.unwrap();
    let b = store.claim_issue(repo.id).await?.unwrap();
    let (a, b) = tokio::join!(start(&f, &repo, &a), start(&f, &repo, &b));
    let ((a, sa), (b, sb)) = (a?, b?);
    assert_ne!(sa.codex_thread_id, sb.codex_thread_id);
    assert_ne!(sa.channel_id(), sb.channel_id());
    assert_ne!(sa.working_directory, sb.working_directory);
    assert_eq!(sa.issue_run_id, Some(a.id));
    assert_eq!(sb.issue_run_id, Some(b.id));
    let path_a = std::path::Path::new(&sa.working_directory);
    let path_b = std::path::Path::new(&sb.working_directory);
    std::fs::write(path_a.join("file"), "worker a")?;
    std::fs::write(path_b.join("file"), "worker b")?;
    assert_eq!(std::fs::read_to_string(path_a.join("file"))?, "worker a");
    assert_eq!(std::fs::read_to_string(path_b.join("file"))?, "worker b");
    assert!(!std::path::Path::new(&repo.path).join("file").exists());
    let branches: Vec<String> = sqlx::query_scalar(
        "SELECT branch FROM issue_runs WHERE repository_id=$1 ORDER BY issue_number",
    )
    .bind(repo.id)
    .fetch_all(&store.pool)
    .await?;
    assert_ne!(branches[0], branches[1]);
    assert!(
        store
            .reserve(&repo, 42, 7, 8, "manual", None)
            .await
            .is_err()
    );
    let manual = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.manager.config.state_dir.join("locks/parallel.lock"))?;
    assert!(flock(&manual, FlockOperation::NonBlockingLockExclusive).is_err());
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":sa.codex_thread_id,"turn":{"id":"turn-new","status":"failed"}}})).await;
    f.status(sa.id, "idle").await?;
    assert!(store.session(sb.id).await?.repository_claimed);
    assert!(
        flock(&manual, FlockOperation::NonBlockingLockExclusive).is_err(),
        "One worker cannot release another worker's lock"
    );
    assert!(
        f.manager
            .followup(
                42,
                7,
                sa.channel_id().unwrap().parse()?,
                "bypass",
                "Run again"
            )
            .await
            .is_err()
    );
    assert_eq!(f.mock.count("turn/start"), 2);
    f.mock.send(json!({"method":"turn/completed","params":{"threadId":sb.codex_thread_id,"turn":{"id":"turn-new","status":"interrupted"}}})).await;
    f.status(sb.id, "idle").await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while flock(&manual, FlockOperation::NonBlockingLockExclusive).is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn integration_contention_waits_through_uncertainty_and_release_confirmation() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager.prepare_discord().await?;
    let store = &f.manager.store;
    store
        .configure_issue_workers(IssueWorkers {
            global: 3,
            per_repository: 3,
        })
        .await?;
    let repo = queue(&f, "releases", 3).await?;
    let mut workers = Vec::new();
    for _ in 0..3 {
        let run = store.claim_issue(repo.id).await?.unwrap();
        let (run, session) = start(&f, &repo, &run).await?;
        pass_checks(store, &run).await?;
        workers.push((run, session));
    }
    let (a, _) = &workers[0];
    let (b, sb) = &workers[1];
    let (c, _) = &workers[2];
    let policy = integration_policy();
    let (first, second) = tokio::join!(
        store.try_begin_issue_integration(a, &policy, "/candidate-a"),
        store.try_begin_issue_integration(b, &policy, "/candidate-b")
    );
    assert!(first?.is_some());
    assert!(second?.is_none());
    assert!(
        store.integration(b.id).await?.is_none(),
        "Waiting does not record publication intent"
    );
    store
        .block_integration(a.id, "uncertain", true, false)
        .await?;
    assert!(
        store
            .try_begin_issue_integration(b, &policy, "/candidate-b")
            .await?
            .is_none()
    );
    store
        .block_integration(a.id, "confirmed unmerged", false, true)
        .await?;
    assert!(
        store
            .try_begin_issue_integration(b, &policy, "/candidate-b")
            .await?
            .is_some()
    );
    validated_candidate(store, b.id).await?;
    store
        .integration_pr(b.id, 123, "https://github.com/owner/repo/pull/123")
        .await?;
    store.finish_integration(b.id, &"e".repeat(40)).await?;
    assert!(
        store
            .try_begin_issue_integration(c, &policy, "/candidate-c")
            .await?
            .is_none()
    );
    let status = f
        .manager
        .queue_command("status", "releases", 42, 7, None)
        .await?;
    assert!(status.contains("waiting for integration/release: 1"));
    assert!(status.contains("Integration/release slot: #2 (releasing)"));
    f.manager.finish_task(42, 7, sb.id, false).await?;
    assert!(
        store
            .try_begin_issue_integration(c, &policy, "/candidate-c")
            .await?
            .is_some()
    );
    // A crash preserves active/uncertain release ownership and never replays a
    // waiting implementation or resurrects its validation.
    store.reconcile_issues().await?;
    store.reconcile_issues().await?;
    let holder: Uuid =
        sqlx::query_scalar("SELECT run_id FROM issue_release_locks WHERE repository_id=$1")
            .bind(repo.id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(holder, c.id);
    assert_eq!(f.mock.count("turn/start"), 3);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn aliases_of_the_same_github_repository_share_the_release_slot() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let store = &f.manager.store;
    store
        .configure_issue_workers(IssueWorkers {
            global: 2,
            per_repository: 1,
        })
        .await?;
    let (a, _) = ready_for_integration(&f, "checkout-a").await?;
    let (b, _) = ready_for_integration(&f, "checkout-b").await?;
    let policy = integration_policy();
    let (first, second) = tokio::join!(
        store.try_begin_issue_integration(&a, &policy, "/candidate-a"),
        store.try_begin_issue_integration(&b, &policy, "/candidate-b")
    );
    assert_ne!(first?.is_some(), second?.is_some());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn dependency_waits_do_not_consume_workers_or_replay_allocated_attempts() -> Result<()> {
    let f = Fixture::new().await?;
    let store = &f.manager.store;
    let repo = queue(&f, "dependencies", 2).await?;
    let waiting = store.claim_issue(repo.id).await?.unwrap();
    store
        .defer_issue_dependencies(&waiting, "Waiting for dependency #8")
        .await?;
    let next = store.claim_issue(repo.id).await?.unwrap();
    assert_eq!(next.issue_number, 2);
    let status = f
        .manager
        .queue_command("status", "dependencies", 42, 7, None)
        .await?;
    assert!(status.contains("Waiting for dependency #8"));
    let session = store
        .reserve(&repo, 42, 7, 8, &format!("issue:{}", next.id), Some("task"))
        .await?;
    assert!(
        store
            .defer_issue_dependencies(&next, "Cannot replay")
            .await
            .is_err()
    );
    assert_eq!(session.issue_run_id, Some(next.id));
    assert!(
        store
            .require_released_dependencies(&[("OWNER/REPO".into(), 2)])
            .await
            .is_err()
    );
    store
        .require_released_dependencies(&[("another/repository".into(), 2)])
        .await?;
    assert_eq!(f.mock.count("turn/start"), 0);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn concurrency_upgrade_preserves_uncertain_release_ownership_and_session_identity()
-> Result<()> {
    let pool = PgPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
    let mut tx = pool.begin().await?;
    let schema = format!("codex_upgrade_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!("SET LOCAL search_path TO {schema}"))
        .execute(&mut *tx)
        .await?;
    for migration in [
        include_str!("../../migrations/0001_initial.sql"),
        include_str!("../../migrations/0002_attention_mentions.sql"),
        include_str!("../../migrations/0003_task_channels.sql"),
        include_str!("../../migrations/0004_github_repository.sql"),
        include_str!("../../migrations/0005_issue_queue.sql"),
        include_str!("../../migrations/0006_issue_validation.sql"),
        include_str!("../../migrations/0007_issue_integration.sql"),
        include_str!("../../migrations/0008_issue_recovery.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&mut *tx).await?;
    }
    let (repo, session, run, validation) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO repositories(id,name,path) VALUES($1,'upgrade','/repos/upgrade')")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO issue_queues(repository_id,requester_id,guild_id) VALUES($1,'42','7')",
    )
    .bind(repo)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO sessions(id,repository_id,requester_id,guild_id,parent_channel_id,working_directory,status,repository_claimed) VALUES($1,$2,'42','7','8','/repos/.worktrees/upgrade/old','idle',false)")
        .bind(session).bind(repo).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO issue_runs(id,repository_id,github_repository,issue_number,state,session_id) VALUES($1,$2,'Owner/Repo',9,'cancelled',$3)")
        .bind(run).bind(repo).bind(session).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO issue_validations(id,run_id,turn_id,policy) VALUES($1,$2,'previous-turn','{}')")
        .bind(validation).bind(run).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO issue_integrations(run_id,repository_id,validation_id,policy,validation_policy,branch,worktree,state,operation) VALUES($1,$2,$3,'{}','{}','codex/integration-old','/repos/.worktrees/upgrade/candidate','uncertain','create_pr')")
        .bind(run).bind(repo).bind(validation).execute(&mut *tx).await?;
    sqlx::raw_sql(include_str!("../../migrations/0009_issue_concurrency.sql"))
        .execute(&mut *tx)
        .await?;
    let identity: Option<Uuid> =
        sqlx::query_scalar("SELECT issue_run_id FROM sessions WHERE id=$1")
            .bind(session)
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(identity, Some(run));
    let owner: (Uuid, String) = sqlx::query_as(
        "SELECT run_id,github_repository FROM issue_release_locks WHERE repository_id=$1",
    )
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(owner, (run, "Owner/Repo".into()));
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_worker_slots")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(
        held, 1,
        "A cancelled run with an uncertain remote operation still holds capacity"
    );
    let limits: (i32, i32) =
        sqlx::query_as("SELECT global_limit,repository_limit FROM issue_worker_limits")
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(limits, (1, 1));
    tx.rollback().await?;
    pool.close().await;
    Ok(())
}
