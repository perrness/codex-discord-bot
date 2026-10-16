use super::*;
use codex_discord_bot::queue::{Run, State};

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn migration_reopens_only_idle_missing_policy_attempts_without_replaying_or_erasing_work()
-> Result<()> {
    let pool = PgPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
    let mut tx = pool.begin().await?;
    let schema = format!("workflow_migration_{}", Uuid::new_v4().simple());
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
        include_str!("../../migrations/0009_issue_concurrency.sql"),
        include_str!("../../migrations/0010_issue_releases.sql"),
    ] {
        sqlx::raw_sql(migration).execute(&mut *tx).await?;
    }
    let mut cases = Vec::new();
    for case in ["missing", "uncertain", "configured"] {
        let (repo, run, session, validation) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        sqlx::query("INSERT INTO repositories(id,name,path) VALUES($1,$2,$3)")
            .bind(repo)
            .bind(case)
            .bind(format!("/repos/{case}"))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO issue_queues(repository_id,requester_id,guild_id) VALUES($1,'42','7')",
        )
        .bind(repo)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO issue_runs(id,repository_id,github_repository,issue_number,state,branch,worktree) VALUES($1,$2,'owner/repo',456,'blocked',$3,$4)")
            .bind(run).bind(repo).bind(format!("branch-{case}")).bind(format!("/repos/.worktrees/{case}")).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO sessions(id,repository_id,issue_run_id,requester_id,guild_id,parent_channel_id,working_directory,codex_thread_id,status,channel_kind,channel_state,repository_claimed,last_outcome) VALUES($1,$2,$3,'42','7','8',$4,$5,'idle','text','active',false,'completed')")
            .bind(session).bind(repo).bind(run).bind(format!("/repos/.worktrees/{case}")).bind(format!("thread-{case}")).execute(&mut *tx).await?;
        let error = "Validation setup blocked: No trusted build, test and lint policy is configured for this repository";
        sqlx::query("INSERT INTO issue_validations(id,run_id,turn_id,state,error,finished_at) VALUES($1,$2,'turn-456','blocked',$3,now())")
            .bind(validation).bind(run).bind(error).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE issue_runs SET session_id=$2,validation_id=$3,last_error=$4 WHERE id=$1",
        )
        .bind(run)
        .bind(session)
        .bind(validation)
        .bind(error)
        .execute(&mut *tx)
        .await?;
        if case == "uncertain" {
            sqlx::query("INSERT INTO inbound_requests(discord_id,session_id,kind,state) VALUES('unknown-input',$1,'message','uncertain')")
                .bind(session).execute(&mut *tx).await?;
        } else if case == "configured" {
            sqlx::query("UPDATE issue_runs SET validation_policy='{}' WHERE id=$1")
                .bind(run)
                .execute(&mut *tx)
                .await?;
        }
        cases.push((case, run, session, validation));
    }
    sqlx::raw_sql(include_str!(
        "../../migrations/0011_repository_workflow.sql"
    ))
    .execute(&mut *tx)
    .await?;
    for (case, id, session, validation) in cases {
        let (state, workflow, evidence): (String, String, Option<Uuid>) =
            sqlx::query_as("SELECT state,workflow,validation_id FROM issue_runs WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if case == "missing" {
            assert_eq!(
                (state.as_str(), workflow.as_str(), evidence),
                ("awaiting_input", "repository", None)
            );
        } else {
            assert_eq!(
                (state.as_str(), workflow.as_str(), evidence),
                ("blocked", "managed", Some(validation))
            );
        }
        let unchanged: (String, String, String) = sqlx::query_as("SELECT s.codex_thread_id,s.working_directory,r.branch FROM sessions s JOIN issue_runs r ON r.session_id=s.id WHERE s.id=$1")
            .bind(session).fetch_one(&mut *tx).await?;
        assert_eq!(
            unchanged,
            (
                format!("thread-{case}"),
                format!("/repos/.worktrees/{case}"),
                format!("branch-{case}")
            )
        );
        let validation_state: String =
            sqlx::query_scalar("SELECT state FROM issue_validations WHERE id=$1")
                .bind(validation)
                .fetch_one(&mut *tx)
                .await?;
        assert_eq!(
            validation_state, "blocked",
            "Keep the old evidence for inspection"
        );
    }
    let inputs: i64 = sqlx::query_scalar("SELECT count(*) FROM inbound_requests")
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(inputs, 1, "Upgrade must not reserve another turn");
    let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM discord_outbox WHERE dedupe_key LIKE 'issue-input:%:workflow-upgrade:0' AND notify_requester")
        .fetch_one(&mut *tx).await?;
    assert_eq!(
        notices, 1,
        "Only the safely reopened task receives an upgrade notice"
    );
    tx.rollback().await?;
    Ok(())
}

async fn finish_turn(f: &Fixture, session: &Session) -> Result<()> {
    f.mock
        .send(json!({"method":"turn/completed","params":{
            "threadId":session.codex_thread_id,"turn":{"id":"turn-new","status":"completed"}
        }}))
        .await;
    f.status(session.id, "idle").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn interactive_approvals_and_answers_reserve_the_workflow_slot_before_replying() -> Result<()>
{
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (a, sa) = f.issue_task("interactive-a").await?;
    let (b, sb) = f.issue_task("interactive-b").await?;
    f.mock.send(json!({"id":"workflow-command","method":"item/commandExecution/requestApproval","params":{
        "threadId":sa.codex_thread_id,"turnId":"turn-new","itemId":"publish","command":"git push origin HEAD","cwd":sa.working_directory
    }})).await;
    f.status(sa.id, "waiting").await?;
    let request: Uuid = sqlx::query_scalar("SELECT id FROM pending_requests WHERE session_id=$1")
        .bind(sa.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    let ca = sa.channel_id().unwrap().parse()?;
    assert!(
        f.manager
            .answer(99, 7, ca, request, "accept", vec![])
            .await
            .is_err()
    );
    let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_release_locks")
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(slots, 0);
    f.manager
        .answer(42, 7, ca, request, "accept", vec![])
        .await?;
    let owner: Uuid = sqlx::query_scalar("SELECT run_id FROM issue_release_locks")
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(owner, a.id);

    f.mock.send(json!({"id":"workflow-question","method":"item/tool/requestUserInput","params":{
        "threadId":sb.codex_thread_id,"turnId":"turn-new","itemId":"workflow-plan","isBlocking":true,
        "questions":[{"id":"approve","header":"Approval","question":"Approve the commit, push, PR/review, merge and release workflow?"}]
    }})).await;
    f.status(sb.id, "waiting").await?;
    let question: Uuid = sqlx::query_scalar("SELECT id FROM pending_requests WHERE session_id=$1")
        .bind(sb.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    let cb = sb.channel_id().unwrap().parse()?;
    assert!(
        f.manager
            .answer(
                42,
                7,
                cb,
                question,
                "answer",
                vec!["Approve the whole workflow".into()]
            )
            .await
            .is_err()
    );
    assert_eq!(f.manager.store.pending(question).await?.state, "pending");
    assert!(
        !f.mock
            .observed
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["id"] == "workflow-question" && v["result"].is_object())
    );
    finish_turn(&f, &sa).await?;
    f.manager.finish_task(42, 7, sa.id, false).await?;
    f.manager
        .answer(
            42,
            7,
            cb,
            question,
            "answer",
            vec!["Approve the whole workflow".into()],
        )
        .await?;
    let owner: Uuid = sqlx::query_scalar("SELECT run_id FROM issue_release_locks")
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(owner, b.id);
    assert!(
        f.manager
            .answer(42, 7, cb, question, "answer", vec!["Approve again".into()])
            .await
            .is_err()
    );
    f.cleanup().await
}

async fn run(f: &Fixture, id: Uuid) -> Result<Run> {
    Ok(sqlx::query_as("SELECT * FROM issue_runs WHERE id=$1")
        .bind(id)
        .fetch_one(&f.manager.store.pool)
        .await?)
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn repository_workflow_continues_without_policies_and_user_confirms_completion() -> Result<()>
{
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (issue, session) = f.issue_task("repository-flow").await?;
    assert_eq!(issue.workflow, "repository");
    std::fs::write(
        std::path::Path::new(&session.working_directory).join("implementation"),
        "preserved",
    )?;
    finish_turn(&f, &session).await?;
    assert_eq!(run(&f, issue.id).await?.state, "awaiting_input");
    let evidence: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM issue_validations), (SELECT count(*) FROM issue_integrations), (SELECT count(*) FROM issue_release_locks)")
        .fetch_one(&f.manager.store.pool).await?;
    assert_eq!(evidence, (0, 0, 0));
    assert_eq!(f.mock.count("turn/start"), 1);
    assert!(
        f.manager
            .store
            .transition_issue(&issue, State::AwaitingInput, State::Completed, None)
            .await
            .is_err()
    );
    let repo = f.manager.store.repository("repository-flow").await?;
    assert!(
        f.manager
            .store
            .reserve(&repo, 42, 7, 8, "manual", None)
            .await
            .is_err()
    );
    assert!(f.manager.store.claim_issue(repo.id).await?.is_none());
    let channel = session.channel_id().unwrap().parse()?;
    assert!(
        f.manager
            .followup(43, 7, channel, "unauthorized", "push and release")
            .await
            .is_err()
    );
    assert_eq!(f.mock.count("turn/start"), 1);
    f.manager
        .followup(
            42,
            7,
            channel,
            "workflow-approved",
            "Commit, push, open a PR, follow this repo's review process, merge and release.",
        )
        .await?;
    assert_eq!(f.mock.count("turn/start"), 2);
    let continued = f.manager.store.session(session.id).await?;
    assert_eq!(continued.codex_thread_id, session.codex_thread_id);
    assert_eq!(continued.working_directory, session.working_directory);
    assert_eq!(
        std::fs::read_to_string(
            std::path::Path::new(&continued.working_directory).join("implementation")
        )?,
        "preserved"
    );
    let owned: Uuid =
        sqlx::query_scalar("SELECT run_id FROM issue_release_locks WHERE repository_id=$1")
            .bind(repo.id)
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert_eq!(owned, issue.id);
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    finish_turn(&f, &session).await?;
    assert_eq!(
        run(&f, issue.id).await?.state,
        "awaiting_input",
        "A completed agent turn is not user confirmation"
    );
    f.manager.finish_task(42, 7, session.id, false).await?;
    assert_eq!(run(&f, issue.id).await?.state, "completed");
    let confirmed: String =
        sqlx::query_scalar("SELECT confirmed_by FROM issue_workflow_completions WHERE run_id=$1")
            .bind(issue.id)
            .fetch_one(&f.manager.store.pool)
            .await?;
    assert_eq!(confirmed, "42");
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM issue_worker_slots), (SELECT count(*) FROM issue_release_locks), (SELECT count(*) FROM issue_release_confirmations)")
        .fetch_one(&f.manager.store.pool).await?;
    assert_eq!(
        counts,
        (0, 0, 0),
        "User confirmation must not fabricate managed release evidence"
    );
    assert!(std::path::Path::new(&session.working_directory).exists());
    f.manager
        .store
        .require_released_dependencies(&[("owner/repo".into(), 5)])
        .await?;
    assert!(f.manager.store.retry_issue(repo.id, 5).await.is_err());
    assert!(
        f.manager
            .followup(42, 7, channel, "after-completion", "publish again")
            .await
            .is_err()
    );
    // Restoring channel presentation must not make completed work retryable.
    sqlx::query("UPDATE sessions SET channel_state='active' WHERE id=$1")
        .bind(session.id)
        .execute(&f.manager.store.pool)
        .await?;
    f.manager.finish_task(42, 7, session.id, true).await?;
    assert_eq!(run(&f, issue.id).await?.state, "completed");
    assert!(f.manager.store.retry_issue(repo.id, 5).await.is_err());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn workflow_slot_serializes_aliases_and_rejects_messages_without_queueing_them() -> Result<()>
{
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (a, sa) = f.issue_task("workflow-a").await?;
    let (b, sb) = f.issue_task("workflow-b").await?;
    finish_turn(&f, &sa).await?;
    finish_turn(&f, &sb).await?;
    let ca = sa.channel_id().unwrap().parse()?;
    let cb = sb.channel_id().unwrap().parse()?;
    f.manager
        .followup(
            42,
            7,
            ca,
            "first-approval",
            "Commit, push, merge and release",
        )
        .await?;
    let error = f
        .manager
        .followup(
            42,
            7,
            cb,
            "conflicting-approval",
            "Commit, push, merge and release",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("integration/release slot"));
    assert_eq!(f.mock.count("turn/start"), 3);
    assert_eq!(run(&f, b.id).await?.state, "awaiting_input");
    let receipt: String = sqlx::query_scalar(
        "SELECT state FROM inbound_requests WHERE discord_id='conflicting-approval'",
    )
    .fetch_one(&f.manager.store.pool)
    .await?;
    assert_eq!(receipt, "rejected");
    finish_turn(&f, &sa).await?;
    f.manager.finish_task(42, 7, sa.id, false).await?;
    assert_eq!(run(&f, a.id).await?.state, "completed");
    assert_eq!(
        f.mock.count("turn/start"),
        3,
        "Rejected instructions are never replayed"
    );
    f.manager
        .followup(
            42,
            7,
            cb,
            "second-approval",
            "Commit, push, merge and release",
        )
        .await?;
    assert_eq!(f.mock.count("turn/start"), 4);
    finish_turn(&f, &sb).await?;
    f.manager.finish_task(42, 7, sb.id, true).await?;
    let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_release_locks")
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(slots, 0);
    f.manager.store.retry_issue(b.repository_id, 5).await?;
    let retry = f.manager.store.claim_issue(b.repository_id).await?.unwrap();
    assert_eq!(retry.workflow, "repository");
    assert_eq!(retry.attempt, 2);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn waiting_repository_workflow_survives_restart_without_replaying_turns() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (issue, session) = f.issue_task("workflow-restart").await?;
    finish_turn(&f, &session).await?;
    f.manager.recover().await?;
    f.manager.recover_issue_queue().await?;
    assert_eq!(run(&f, issue.id).await?.state, "awaiting_input");
    assert_eq!(f.mock.count("turn/start"), 1);
    f.manager
        .followup(
            42,
            7,
            session.channel_id().unwrap().parse()?,
            "after-restart",
            "Open a PR and follow this repository's review process before releasing",
        )
        .await?;
    assert_eq!(f.mock.count("turn/start"), 2);
    finish_turn(&f, &session).await?;
    f.manager.recover().await?;
    f.manager.recover_issue_queue().await?;
    assert_eq!(run(&f, issue.id).await?.state, "awaiting_input");
    let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_release_locks WHERE run_id=$1")
        .bind(issue.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(
        slots, 1,
        "Idle approved work retains its publication slot across restart"
    );
    assert_eq!(f.mock.count("turn/start"), 2);
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn uncertain_workflow_submission_keeps_ownership_and_cannot_complete_or_retry() -> Result<()>
{
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (issue, session) = f.issue_task("uncertain-workflow").await?;
    finish_turn(&f, &session).await?;
    f.mock.omit_turn_response.store(true, Ordering::Relaxed);
    assert!(
        f.manager
            .followup(
                42,
                7,
                session.channel_id().unwrap().parse()?,
                "lost-workflow",
                "Commit, push, merge and release"
            )
            .await
            .is_err()
    );
    f.manager.store.reconcile_issues().await?;
    assert_eq!(run(&f, issue.id).await?.state, "blocked");
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    assert!(
        f.manager
            .finish_task(42, 7, session.id, true)
            .await
            .is_err()
    );
    assert!(
        f.manager
            .store
            .retry_issue(issue.repository_id, 5)
            .await
            .is_err()
    );
    let slots: i64 = sqlx::query_scalar("SELECT count(*) FROM issue_release_locks WHERE run_id=$1")
        .bind(issue.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(slots, 1);
    assert_eq!(f.mock.count("turn/start"), 2);
    f.cleanup().await
}
