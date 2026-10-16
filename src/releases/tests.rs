use super::*;
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn policy() -> Policy {
    serde_json::from_value(json!({
        "github_repository":"owner/repo", "workflow_id":17,
        "workflow_ref":"releases/v1.0", "workflow_sha":"a".repeat(40),
        "deployment_timeout_seconds":60,"health_timeout_seconds":30,
        "health_checks":[{"url":"https://app.example/health","timeout_seconds":1}],
        "disposable_test_run":"https://github.com/owner/repo/actions/runs/10"
    }))
    .unwrap()
}

fn release() -> Release {
    Release {
        run_id: Uuid::new_v4(),
        github_repository: "owner/repo".into(),
        expected_sha: "b".repeat(40),
        policy: serde_json::to_value(policy()).unwrap(),
        state: "dispatching".into(),
        workflow_run_id: None,
        workflow_observation: json!({}),
        health_results: json!([]),
        deployed_revision: None,
        deployment_deadline: Utc::now() + chrono::Duration::seconds(60),
        health_deadline: None,
        verified_at: None,
        last_error: None,
    }
}

fn observation(release: &Release) -> Value {
    let p = policy();
    json!({"id":77,"workflow_id":p.workflow_id,"repository":{"full_name":"owner/repo"},
        "head_repository":{"full_name":"owner/repo"},"event":"workflow_dispatch","run_attempt":1,
        "head_sha":p.workflow_sha,"display_title":p.run_title(release),"status":"completed","conclusion":"success"})
}

#[test]
fn release_configuration_requires_pins_tested_workflow_and_independent_https_checks() {
    let p = policy();
    p.validate().unwrap();
    let original = serde_json::to_value(&p).unwrap();
    for (field, invalid) in [
        ("workflow_id", json!(0)),
        ("workflow_sha", json!("main")),
        ("workflow_ref", json!("../main")),
        ("disposable_test_run", json!("")),
        ("health_checks", json!([])),
        ("deployment_timeout_seconds", json!(0)),
        ("health_timeout_seconds", json!(7201)),
    ] {
        let mut value = original.clone();
        value[field] = invalid;
        assert!(
            serde_json::from_value::<Policy>(value)
                .unwrap()
                .validate()
                .is_err(),
            "{field}"
        );
    }
    for url in [
        "http://app.example/health",
        "file:///etc/passwd",
        "https://user:secret@app.example/health",
        "https:///health",
        "https://app.example/health#fragment",
    ] {
        let mut p = p.clone();
        p.health_checks[0].url = url.into();
        assert!(p.validate().is_err(), "{url}");
    }
    let policies = Policies {
        repositories: [("app".into(), p.clone())].into(),
    };
    let mut integration = crate::integration::Policies::default();
    assert!(policies.validate_gates(&integration).is_err());
    let mut gate: crate::integration::Policy = serde_json::from_value(json!({"github_repository":"owner/repo","release_gate":"deployments_disabled","protected_paths":[],"checks_timeout_seconds":60})).unwrap();
    integration.repositories.insert("app".into(), gate.clone());
    assert!(policies.validate_gates(&integration).is_err());
    gate.release_gate = crate::integration::ReleaseGate::DispatchOnly;
    integration.repositories.insert("app".into(), gate);
    policies.validate_gates(&integration).unwrap();
}

#[test]
fn workflow_correlation_rejects_other_revisions_events_repositories_and_reruns() {
    let p = policy();
    let mut release = release();
    let original = observation(&release);
    p.verify_run(&release, &original).unwrap();
    for (field, value) in [
        ("id", json!(0)),
        ("workflow_id", json!(18)),
        ("head_sha", json!(release.expected_sha)),
        ("display_title", json!("latest")),
        ("event", json!("push")),
        ("run_attempt", json!(2)),
        ("repository", json!({"full_name":"fork/repo"})),
        ("head_repository", json!({"full_name":"fork/repo"})),
    ] {
        let mut wrong = original.clone();
        wrong[field] = value;
        assert!(p.verify_run(&release, &wrong).is_err(), "{field}");
    }
    release.workflow_run_id = Some(99);
    assert!(p.verify_run(&release, &original).is_err());
}

fn actions_fixture(release: &Release) -> (tempfile::TempDir, Actions) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    std::fs::write(
        dir.path().join("run.json"),
        serde_json::to_vec(&observation(release)).unwrap(),
    )
    .unwrap();
    std::fs::write(
        &script,
        r#"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
endpoint = next(arg for arg in sys.argv if arg.startswith('repos/'))
method = sys.argv[sys.argv.index('--method') + 1]
with (root / 'calls').open('a') as f: f.write(method + ' ' + endpoint + '\n')
run = json.loads((root / 'run.json').read_text())
if method == 'POST':
    (root / 'inputs.json').write_text(sys.stdin.read())
    if (root / 'lost').exists(): sys.exit(1)
    print(json.dumps({'workflow_run_id':77}))
elif '/commits/' in endpoint:
    print(json.dumps({'sha':run['head_sha']}))
elif endpoint.endswith('/actions/workflows/17'):
    print(json.dumps({'id':17,'state':'active'}))
elif '/runs?' in endpoint:
    print(json.dumps({'workflow_runs':[run,run] if (root / 'duplicate').exists() else [run]}))
else:
    print(json.dumps(run))
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let actions = Actions {
        github: crate::integration::github::GitHub::test_client("owner/repo", script).unwrap(),
    };
    (dir, actions)
}

#[tokio::test]
async fn lost_dispatch_response_reconciles_only_the_correlated_run_without_redeployment() {
    let release = release();
    let p = policy();
    let (dir, actions) = actions_fixture(&release);
    actions.preflight(&p).await.unwrap();
    std::fs::write(dir.path().join("lost"), "").unwrap();
    assert!(actions.dispatch(&release, &p).await.is_err());
    let observed = actions.observe(&release, &p).await.unwrap().unwrap();
    assert_eq!(observed["id"], 77);
    let inputs: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("inputs.json")).unwrap()).unwrap();
    assert_eq!(
        inputs,
        json!({"ref":p.workflow_ref,"inputs":{"revision":release.expected_sha,"release_id":release.run_id.to_string()}})
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .lines()
            .filter(|s| s.starts_with("POST "))
            .count(),
        1
    );
    std::fs::write(dir.path().join("duplicate"), "").unwrap();
    assert!(actions.observe(&release, &p).await.is_err());
}

#[tokio::test]
async fn failed_disposable_test_or_moved_workflow_ref_blocks_dispatch() {
    let release = release();
    let p = policy();
    let (dir, actions) = actions_fixture(&release);
    let mut failed = observation(&release);
    failed["conclusion"] = "failure".into();
    std::fs::write(
        dir.path().join("run.json"),
        serde_json::to_vec(&failed).unwrap(),
    )
    .unwrap();
    assert!(actions.preflight(&p).await.is_err());
    failed["conclusion"] = "success".into();
    failed["head_sha"] = "c".repeat(40).into();
    std::fs::write(
        dir.path().join("run.json"),
        serde_json::to_vec(&failed).unwrap(),
    )
    .unwrap();
    assert!(actions.preflight(&p).await.is_err());
    assert!(!dir.path().join("inputs.json").exists());
}

#[tokio::test]
async fn unresponsive_health_endpoint_times_out_without_success() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let check = HealthCheck {
        url: format!("http://{}/health", listener.local_addr().unwrap()),
        timeout_seconds: 1,
    };
    let task = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), check_health(&check))
        .await
        .unwrap();
    task.abort();
    assert!(!result.healthy && result.error.is_some());
}

async fn endpoint(status: u16, body: String) -> (HealthCheck, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
    });
    (
        HealthCheck {
            url: format!("http://{address}/health"),
            timeout_seconds: 1,
        },
        task,
    )
}

#[tokio::test]
async fn disposable_deployment_requires_workflow_completion_and_exact_healthy_revision() {
    let mut release = release();
    let mut p = policy();
    let (_dir, actions) = actions_fixture(&release);
    release.workflow_run_id = Some(actions.dispatch(&release, &p).await.unwrap());
    let observed = actions.observe(&release, &p).await.unwrap().unwrap();
    assert_eq!(observed["status"], "completed");
    assert_eq!(observed["conclusion"], "success");
    for (status, body, healthy) in [
        (
            200,
            json!({"healthy":true,"revision":"c".repeat(40)}).to_string(),
            false,
        ),
        (
            200,
            json!({"healthy":false,"revision":release.expected_sha}).to_string(),
            false,
        ),
        (
            302,
            json!({"healthy":true,"revision":release.expected_sha}).to_string(),
            false,
        ),
        (
            503,
            json!({"healthy":true,"revision":release.expected_sha}).to_string(),
            false,
        ),
        (200, "invalid json".into(), false),
        (200, "x".repeat(20000), false),
        (
            200,
            json!({"healthy":true,"revision":release.expected_sha}).to_string(),
            true,
        ),
    ] {
        let (check, task) = endpoint(status, body).await;
        p.health_checks = vec![check.clone()];
        let result = check_health(&check).await;
        task.await.unwrap();
        assert_eq!(
            p.verify_health(&release.expected_sha, &[result]).is_ok(),
            healthy
        );
    }
    assert!(p.verify_health(&release.expected_sha, &[]).is_err());
    let result = check_health(&p.health_checks[0]).await;
    assert!(result.error.is_some());
}
