use super::*;
use codex_discord_bot::queue::{Run, State};
use std::path::{Path, PathBuf};

async fn git(path: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .current_dir(path)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn local_candidate(f: &Fixture, name: &str) -> Result<(Run, Session, PathBuf)> {
    let (run, session) = ready_for_integration(f, name).await?;
    let store = &f.manager.store;
    let repo = store.repository(name).await?;
    let source_path = Path::new(&session.working_directory);
    std::fs::write(source_path.join("implementation"), "change")?;
    git(source_path, &["add", "implementation"]).await?;
    git(source_path, &["commit", "-m", "Implement issue"]).await?;
    let source = git(source_path, &["rev-parse", "HEAD"]).await?;
    let base = git(Path::new(&repo.path), &["rev-parse", "HEAD"]).await?;
    let candidate_path = source_path
        .parent()
        .unwrap()
        .join(format!("integration-{}", run.id));
    let i = store
        .begin_issue_integration(
            &run,
            &integration_policy(),
            candidate_path.to_str().unwrap(),
        )
        .await?;
    store
        .integration_source(run.id, "main", &base, &source)
        .await?;
    git(
        Path::new(&repo.path),
        &[
            "worktree",
            "add",
            "-b",
            &i.branch,
            candidate_path.to_str().unwrap(),
            &source,
        ],
    )
    .await?;
    let tree = git(&candidate_path, &["rev-parse", "HEAD^{tree}"]).await?;
    store.integration_candidate(run.id, &source, &tree).await?;
    store
        .integration_checks(run.id, &validation_results())
        .await?;
    store.validate_integration(run.id).await?;
    Ok((run, session, candidate_path))
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn dispatch_crashes_never_allocate_or_submit_again_and_keep_the_original_phase() -> Result<()>
{
    for boundary in [
        "claim",
        "session",
        "allocation_intent",
        "worktree",
        "thread",
    ] {
        let f = Fixture::new().await?;
        f.manager.register_repository("project", true).await?;
        let store = &f.manager.store;
        let repo = store.repository("project").await?;
        git(
            Path::new(&repo.path),
            &["commit", "--allow-empty", "-m", "Initial"],
        )
        .await?;
        sqlx::query("INSERT INTO issue_queues(repository_id,requester_id,guild_id,paused) VALUES($1,'42','7',false)")
            .bind(repo.id).execute(&store.pool).await?;
        store.enqueue_issue(repo.id, "owner/repo", 8).await?;
        let run = store.claim_issue(repo.id).await?.unwrap();
        let path = f
            .manager
            .config
            .repos_root
            .join(".worktrees/project")
            .join(run.id.to_string());
        if boundary != "claim" {
            let receipt = format!("issue:{}", run.id);
            store.receipt(&receipt, "task", None).await?;
            let session = store
                .reserve(&repo, 42, 7, 8, &receipt, Some("Issue #8"))
                .await?;
            if boundary != "session" {
                let branch = format!("codex/issue-8-attempt-1-{}", run.id);
                sqlx::query("UPDATE issue_runs SET worktree=$2,branch=$3 WHERE id=$1")
                    .bind(run.id)
                    .bind(path.to_str().unwrap())
                    .bind(&branch)
                    .execute(&store.pool)
                    .await?;
                sqlx::query("UPDATE sessions SET working_directory=$2 WHERE id=$1")
                    .bind(session.id)
                    .bind(path.to_str().unwrap())
                    .execute(&store.pool)
                    .await?;
                if boundary != "allocation_intent" {
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    git(
                        Path::new(&repo.path),
                        &["worktree", "add", "-b", &branch, path.to_str().unwrap()],
                    )
                    .await?;
                }
                if boundary == "thread" {
                    sqlx::query("UPDATE sessions SET codex_thread_id='stored-thread' WHERE id=$1")
                        .bind(session.id)
                        .execute(&store.pool)
                        .await?;
                }
            }
        }
        for _ in 0..2 {
            f.manager.recover().await?;
            f.manager.recover_issue_queue().await?;
        }
        let (state, phase, error): (String, String, Option<String>) = sqlx::query_as(
            "SELECT state,recovery_phase,worktree_error FROM issue_runs WHERE id=$1",
        )
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
        assert_eq!(
            (state.as_str(), phase.as_str()),
            ("blocked", "preparing"),
            "{boundary}"
        );
        assert_eq!(
            error.is_some(),
            boundary == "allocation_intent",
            "{boundary}: {error:?}"
        );
        assert_eq!(path.exists(), matches!(boundary, "worktree" | "thread"));
        assert_eq!(f.mock.count("thread/start"), 0);
        assert_eq!(f.mock.count("turn/start"), 0);
        assert!(store.claim_issue(repo.id).await?.is_none());
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM discord_outbox WHERE dedupe_key=$1")
                .bind(format!("issue-recovery:{}:preparing:0", run.id))
                .fetch_one(&store.pool)
                .await?;
        assert_eq!(count, i64::from(boundary != "claim"));
        let status = f
            .manager
            .queue_command("status", "project", 42, 7, None)
            .await?;
        assert!(status.contains("restart phase: preparing") && status.contains("blocked"));
        f.cleanup().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn partial_validation_recovery_preserves_work_and_notifies_each_retry_once() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = f.issue_task("validation-crash").await?;
    let store = &f.manager.store;
    let diagnostic = Path::new(&session.working_directory).join("diagnostic");
    std::fs::write(&diagnostic, "keep for inspection")?;
    validating(store, &run).await?;
    let (id, _) = store
        .begin_issue_validation(&run, "turn-new", Some(&validation_policy(2)))
        .await?;
    store
        .record_issue_checks(id, &validation_results()[..1])
        .await?;
    f.manager.recover().await?;
    for _ in 0..2 {
        f.manager.recover_issue_queue().await?;
    }
    let (state, results): (String, Value) =
        sqlx::query_as("SELECT state,results FROM issue_validations WHERE id=$1")
            .bind(id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(state, "interrupted");
    assert_eq!(results.as_array().unwrap().len(), 1);
    assert!(store.require_issue_validation(session.id).await.is_err());
    let notices: Vec<bool> =
        sqlx::query_scalar("SELECT notify_requester FROM discord_outbox WHERE dedupe_key=$1")
            .bind(format!("issue-recovery:{}:validating:0", run.id))
            .fetch_all(&store.pool)
            .await?;
    assert_eq!(notices, vec![true]);
    f.manager.finish_task(42, 7, session.id, true).await?;
    f.manager
        .queue_command("retry", "validation-crash", 42, 7, Some(5))
        .await?;
    assert!(
        f.manager
            .queue_command("retry", "validation-crash", 42, 7, Some(5))
            .await
            .is_err()
    );
    f.manager.cleanup_completed_worktrees().await?;
    assert_eq!(std::fs::read_to_string(diagnostic)?, "keep for inspection");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM discord_outbox WHERE dedupe_key LIKE 'issue-retry:%' AND notify_requester")
        .fetch_one(&store.pool).await?;
    assert_eq!(count, 1);
    assert_eq!(f.mock.count("turn/start"), 1);
    let next = store.claim_issue(run.repository_id).await?.unwrap();
    assert_eq!(next.attempt, 2);
    assert_ne!(next.id, run.id);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn integration_crash_preserves_candidate_and_blocks_republication() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session, candidate) = local_candidate(&f, "integration-crash").await?;
    let store = &f.manager.store;
    f.manager.recover().await?;
    for _ in 0..2 {
        f.manager.recover_issue_queue().await?;
    }
    let i = store.integration(run.id).await?.unwrap();
    assert!(i.retry_safe && i.state == "blocked");
    assert!(
        store
            .integration_step(run.id, "validated", "pushing")
            .await
            .is_err()
    );
    assert!(candidate.join("implementation").exists());
    assert!(
        Path::new(&session.working_directory)
            .join("implementation")
            .exists()
    );
    let (phase, error): (String, Option<String>) =
        sqlx::query_as("SELECT recovery_phase,worktree_error FROM issue_runs WHERE id=$1")
            .bind(run.id)
            .fetch_one(&store.pool)
            .await?;
    assert_eq!(phase, "integrating");
    assert!(error.is_none(), "{error:?}");
    std::fs::write(candidate.join("implementation"), "diagnostic changes")?;
    f.manager.recover_issue_queue().await?;
    let error: String = sqlx::query_scalar("SELECT worktree_error FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert!(error.contains("worktree changed"));
    assert_eq!(
        std::fs::read_to_string(candidate.join("implementation"))?,
        "diagnostic changes"
    );
    assert_eq!(f.mock.count("turn/start"), 1);
    f.manager.cleanup_completed_worktrees().await?;
    assert!(candidate.exists());
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn release_confirmation_and_cleanup_survive_crashes_without_releasing_twice() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session, candidate) = local_candidate(&f, "release-crash").await?;
    let store = &f.manager.store;
    store
        .integration_pr(run.id, 8, "https://github.com/owner/repo/pull/8")
        .await?;
    store.finish_integration(run.id, &"e".repeat(40)).await?;
    assert!(
        store
            .transition_issue(&run, State::Releasing, State::Released, None)
            .await
            .is_err()
    );
    // Simulate a crash inside the completion transaction: the receipt must roll back.
    let mut tx = store.pool.begin().await?;
    sqlx::query("INSERT INTO issue_release_confirmations(run_id,github_repository,merge_sha,confirmed_by) VALUES($1,'owner/repo',$2,'42')")
        .bind(run.id).bind("e".repeat(40)).execute(&mut *tx).await?;
    tx.rollback().await?;
    f.manager.recover().await?;
    for _ in 0..2 {
        f.manager.recover_issue_queue().await?;
    }
    let state: String = sqlx::query_scalar("SELECT state FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(state, "blocked");
    assert!(candidate.exists());
    assert!(store.claim_issue(run.repository_id).await?.is_none());
    assert!(store.retry_issue(run.repository_id, 5).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_release_confirmations")
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(count, 0);
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    issue_releases::verified_release(store, run.id).await?;
    // Lost Discord response does not record a second release or new cleanup intent.
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    let receipts: Vec<(String, String)> =
        sqlx::query_as("SELECT merge_sha,confirmed_by FROM issue_release_confirmations")
            .fetch_all(&store.pool)
            .await?;
    assert_eq!(
        receipts,
        vec![("e".repeat(40), "release-controller".into())]
    );
    let error: Option<String> = sqlx::query_scalar("SELECT last_error FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert!(error.is_none());
    let source = Path::new(&session.working_directory);
    std::fs::write(source.join("diagnostic"), "preserve")?;
    f.manager.cleanup_completed_worktrees().await?;
    assert!(!candidate.exists());
    assert_eq!(
        std::fs::read_to_string(source.join("diagnostic"))?,
        "preserve"
    );
    let state: String = sqlx::query_scalar(
        "SELECT state FROM issue_worktree_cleanup WHERE run_id=$1 AND kind='issue'",
    )
    .bind(run.id)
    .fetch_one(&store.pool)
    .await?;
    assert_eq!(state, "blocked");
    let status = f
        .manager
        .queue_command("status", "release-crash", 42, 7, None)
        .await?;
    assert!(
        status.contains("release confirmed by release-controller")
            && status.contains("issue cleanup: blocked")
    );
    std::fs::remove_file(source.join("diagnostic"))?;
    let repo = store.repository("release-crash").await?;
    sqlx::query(
        "UPDATE issue_worktree_cleanup SET state='removing' WHERE run_id=$1 AND kind='issue'",
    )
    .bind(run.id)
    .execute(&store.pool)
    .await?;
    git(
        Path::new(&repo.path),
        &["worktree", "remove", "--", source.to_str().unwrap()],
    )
    .await?;
    // Crash after Git removed the path, before storing the success in PostgreSQL.
    for _ in 0..2 {
        f.manager.recover_issue_queue().await?;
    }
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM issue_worktree_cleanup WHERE run_id=$1")
            .bind(run.id)
            .fetch_all(&store.pool)
            .await?;
    assert_eq!(states, vec!["removed", "removed"]);
    assert_eq!(f.mock.count("turn/start"), 1);
    let branch = format!("refs/heads/codex/issue-5-attempt-1-{}", run.id);
    assert!(
        !git(Path::new(&repo.path), &["rev-parse", "--verify", &branch])
            .await?
            .is_empty()
    );
    store.enqueue_issue(repo.id, "owner/repo", 9).await?;
    assert_eq!(store.claim_issue(repo.id).await?.unwrap().issue_number, 9);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn remote_identities_and_worktree_allocations_cannot_belong_to_two_runs() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    f.manager
        .store
        .configure_issue_workers(codex_discord_bot::config::IssueWorkers {
            global: 2,
            per_repository: 1,
        })
        .await?;
    let (a, _, _) = local_candidate(&f, "first").await?;
    // Historical candidates still reserve their remote identities after a safe
    // block permits a different local checkout to acquire the release slot.
    f.manager
        .store
        .block_integration(a.id, "No publication attempted", false, true)
        .await?;
    let (b, _, _) = local_candidate(&f, "second").await?;
    let store = &f.manager.store;
    // GitHub names are case-insensitive, including when two local registrations
    // use different spelling for the same remote repository.
    sqlx::query("UPDATE issue_integrations SET policy=jsonb_set(policy,'{github_repository}',$2) WHERE run_id=$1")
        .bind(b.id).bind(json!("Owner/Repo")).execute(&store.pool).await?;
    store
        .integration_pr(a.id, 8, "https://github.com/owner/repo/pull/8")
        .await?;
    assert!(
        store
            .integration_pr(b.id, 8, "https://github.com/owner/repo/pull/8")
            .await
            .is_err()
    );
    sqlx::query("UPDATE issue_integrations SET merge_sha=$2 WHERE run_id=$1")
        .bind(a.id)
        .bind("e".repeat(40))
        .execute(&store.pool)
        .await?;
    assert!(
        sqlx::query("UPDATE issue_integrations SET merge_sha=$2 WHERE run_id=$1")
            .bind(b.id)
            .bind("e".repeat(40))
            .execute(&store.pool)
            .await
            .is_err()
    );
    for column in ["branch", "worktree"] {
        assert!(sqlx::query(&format!("UPDATE issue_runs SET {column}=(SELECT {column} FROM issue_runs WHERE id=$1) WHERE id=$2"))
            .bind(a.id).bind(b.id).execute(&store.pool).await.is_err());
    }
    f.cleanup().await
}
