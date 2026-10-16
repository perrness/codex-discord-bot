use super::*;
use codex_discord_bot::releases::{HealthResult, Policy, Release};

pub(super) fn policy() -> Policy {
    serde_json::from_value(json!({
        "github_repository":"owner/repo", "workflow_id":17,
        "workflow_ref":"release-controller", "workflow_sha":"a".repeat(40),
        "deployment_timeout_seconds":60, "health_timeout_seconds":30,
        "health_checks":[{"url":"https://app.example/health","timeout_seconds":5}],
        "disposable_test_run":"https://github.com/owner/repo/actions/runs/10"
    }))
    .unwrap()
}

fn observation(release: &Release) -> Value {
    let policy = policy();
    json!({"id":77,"workflow_id":policy.workflow_id,"repository":{"full_name":"owner/repo"},
        "head_repository":{"full_name":"owner/repo"},"event":"workflow_dispatch","run_attempt":1,
        "head_sha":policy.workflow_sha,"display_title":policy.run_title(release),
        "status":"completed","conclusion":"success"})
}

fn health(release: &Release) -> Vec<HealthResult> {
    vec![HealthResult {
        url: policy().health_checks[0].url.clone(),
        healthy: true,
        revision: Some(release.expected_sha.clone()),
        error: None,
    }]
}

pub(super) async fn verified_release(store: &Store, run: Uuid) -> Result<()> {
    store.request_issue_release(run).await?;
    let release = store.begin_issue_release(run, &policy()).await?;
    store
        .observe_release_workflow(run, &observation(&release))
        .await?;
    store.record_release_health(run, &health(&release)).await?;
    store
        .finish_issue_release(run, Duration::from_secs(48 * 3600))
        .await
}

async fn merged(f: &Fixture, name: &str) -> Result<(codex_discord_bot::queue::Run, Session)> {
    let (run, session) = ready_for_integration(f, name).await?;
    f.manager
        .store
        .begin_issue_integration(
            &run,
            &integration_policy(),
            f.directory.path().join("candidate").to_str().unwrap(),
        )
        .await?;
    validated_candidate(&f.manager.store, run.id).await?;
    f.manager
        .store
        .integration_pr(run.id, 7, "https://github.com/owner/repo/pull/7")
        .await?;
    f.manager
        .store
        .finish_integration(run.id, &"e".repeat(40))
        .await?;
    Ok((run, session))
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn release_requires_exact_workflow_and_all_revision_health_evidence() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = merged(&f, "release-evidence").await?;
    let store = &f.manager.store;
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    let release_policy = policy();
    assert!(
        store
            .begin_issue_release(run.id, &release_policy)
            .await
            .is_err()
    );
    store.request_issue_release(run.id).await?;
    let (first, second) = tokio::join!(
        store.begin_issue_release(run.id, &release_policy),
        store.begin_issue_release(run.id, &release_policy)
    );
    assert_ne!(
        first.is_ok(),
        second.is_ok(),
        "Exactly one dispatch intent wins"
    );
    let release = store.release(run.id).await?.unwrap();
    let mut wrong = observation(&release);
    wrong["head_sha"] = "b".repeat(40).into();
    assert!(
        store
            .observe_release_workflow(run.id, &wrong)
            .await
            .is_err()
    );
    assert!(
        store
            .finish_issue_release(run.id, Duration::from_secs(60))
            .await
            .is_err()
    );
    store
        .observe_release_workflow(run.id, &observation(&release))
        .await?;
    assert!(store.release_run_id(run.id, 78).await.is_err());
    assert!(
        store
            .finish_issue_release(run.id, Duration::from_secs(60))
            .await
            .is_err()
    );
    let mut wrong = health(&release);
    wrong[0].revision = Some("f".repeat(40));
    store.record_release_health(run.id, &wrong).await?;
    assert_eq!(
        store.release(run.id).await?.unwrap().deployed_revision,
        Some("f".repeat(40))
    );
    assert!(
        store
            .finish_issue_release(run.id, Duration::from_secs(60))
            .await
            .is_err()
    );
    assert!(store.require_issue_release(session.id).await.is_err());
    store
        .record_release_health(run.id, &health(&release))
        .await?;
    store
        .finish_issue_release(run.id, Duration::from_secs(60))
        .await?;
    store
        .finish_issue_release(run.id, Duration::from_secs(60))
        .await?;
    store.require_issue_release(session.id).await?;
    assert_eq!(
        store.release(run.id).await?.unwrap().deployed_revision,
        Some("e".repeat(40))
    );
    let (receipts, locks, notices): (i64, i64, i64) = sqlx::query_as(
        "SELECT
        (SELECT count(*) FROM issue_release_confirmations WHERE verification='controller'),
        (SELECT count(*) FROM issue_release_locks),
        (SELECT count(*) FROM discord_outbox WHERE dedupe_key LIKE 'release-succeeded:%')",
    )
    .fetch_one(&store.pool)
    .await?;
    assert_eq!((receipts, locks, notices), (1, 0, 1));
    assert_eq!(store.session(session.id).await?.channel_state, "completed");
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn missing_policy_failed_deployments_and_timeouts_hold_the_release_slot() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, session) = merged(&f, "release-failed").await?;
    let store = &f.manager.store;
    let controller = codex_discord_bot::releases::Controller::new(
        Store {
            pool: store.pool.clone(),
        },
        Default::default(),
        Duration::from_secs(60),
    );
    store.request_issue_release(run.id).await?;
    controller.poll_issue_release(run.id, true).await?;
    assert!(
        store.release(run.id).await?.is_none(),
        "Missing configuration must not reserve a dispatch"
    );
    assert!(store.require_issue_release(session.id).await.is_err());
    store.request_issue_release(run.id).await?;
    let release = store.begin_issue_release(run.id, &policy()).await?;
    store
        .block_release(run.id, "Lost dispatch response", true)
        .await?;
    store.recover_releases().await?;
    assert_eq!(store.release(run.id).await?.unwrap().state, "uncertain");
    assert!(store.begin_issue_release(run.id, &policy()).await.is_err());
    store
        .observe_release_workflow(run.id, &observation(&release))
        .await?;
    store
        .record_release_health(run.id, &health(&release))
        .await?;
    sqlx::query(
        "UPDATE issue_releases SET health_deadline=now()-interval '1 second' WHERE run_id=$1",
    )
    .bind(run.id)
    .execute(&store.pool)
    .await?;
    assert!(
        store
            .finish_issue_release(run.id, Duration::from_secs(60))
            .await
            .is_err()
    );
    controller.poll_issue_release(run.id, false).await?;
    controller.poll_issue_release(run.id, true).await?;
    assert_eq!(store.release(run.id).await?.unwrap().state, "failed");
    assert!(
        f.manager
            .finish_task(42, 7, session.id, false)
            .await
            .is_err()
    );
    assert!(store.retry_issue(run.repository_id, 5).await.is_err());
    let (locks,receipts): (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM issue_release_locks),(SELECT count(*) FROM issue_release_confirmations)")
        .fetch_one(&store.pool).await?;
    assert_eq!((locks, receipts), (1, 0));
    let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM discord_outbox WHERE dedupe_key LIKE 'release-blocked:%' AND notify_requester")
        .fetch_one(&store.pool).await?;
    assert_eq!(
        notices, 3,
        "Configuration, uncertain dispatch and terminal failure each notify once"
    );
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn recovery_never_dispatches_an_integrated_issue_without_intent() -> Result<()> {
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, _) = merged(&f, "release-recovery").await?;
    for _ in 0..2 {
        f.manager.store.reconcile_issues().await?;
        f.manager.store.recover_releases().await?;
    }
    assert!(f.manager.store.release(run.id).await?.is_none());
    let state: String = sqlx::query_scalar("SELECT state FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&f.manager.store.pool)
        .await?;
    assert_eq!(state, "blocked");
    sqlx::query("UPDATE issue_integrations SET policy=jsonb_set(policy,'{release_gate}','\"deployments_disabled\"') WHERE run_id=$1")
        .bind(run.id).execute(&f.manager.store.pool).await?;
    assert!(f.manager.store.request_issue_release(run.id).await.is_err());
    f.cleanup().await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL"]
async fn recovery_racing_the_separate_controller_never_invalidates_committed_dispatch() -> Result<()>
{
    let api = discord::Api::start().await;
    let f = Fixture::with_http(api.http.clone()).await?;
    let (run, _) = merged(&f, "release-race").await?;
    let store = &f.manager.store;
    store.request_issue_release(run.id).await?;
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT id FROM issue_runs WHERE id=$1 FOR UPDATE")
        .bind(run.id)
        .execute(&mut *tx)
        .await?;
    let mut recovery = Box::pin(store.recover_releases());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut recovery)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO issue_releases(run_id,github_repository,expected_sha,policy,deployment_deadline) VALUES($1,'owner/repo',$2,$3,now()+interval '1 minute')")
        .bind(run.id).bind("e".repeat(40)).bind(serde_json::to_value(policy())?).execute(&mut *tx).await?;
    sqlx::query("UPDATE issue_runs SET release_requested=false WHERE id=$1")
        .bind(run.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    recovery.await?;
    assert_eq!(store.release(run.id).await?.unwrap().state, "dispatching");
    assert!(store.request_issue_release(run.id).await.is_err());
    let state: String = sqlx::query_scalar("SELECT state FROM issue_runs WHERE id=$1")
        .bind(run.id)
        .fetch_one(&store.pool)
        .await?;
    assert_eq!(state, "releasing");
    f.cleanup().await
}
