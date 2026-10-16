//! Opt-in contract check against the pinned installed CLI. No authentication or model turn.
use anyhow::{Context, Result, ensure};
use codex_discord_bot::{
    codex::Client,
    config::{ApprovalsReviewer, SUPPORTED_CODEX_VERSION},
};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

#[tokio::test]
#[ignore = "requires CODEX_TEST_BIN pointing to codex 0.162.1"]
async fn installed_codex_negotiates_and_applies_safe_thread_policy() -> Result<()> {
    let binary = std::env::var("CODEX_TEST_BIN").context("Set CODEX_TEST_BIN")?;
    let version = Command::new(&binary).arg("--version").output().await?;
    ensure!(
        String::from_utf8_lossy(&version.stdout).trim()
            == format!("codex-cli {SUPPORTED_CODEX_VERSION}"),
        "Wrong Codex version"
    );
    for reviewer in [ApprovalsReviewer::AutoReview, ApprovalsReviewer::User] {
        check_thread_policy(&binary, reviewer).await?;
    }
    Ok(())
}

async fn check_thread_policy(binary: &str, reviewer: ApprovalsReviewer) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("codex");
    let repo = directory.path().join("project");
    std::fs::create_dir(&home)?;
    std::fs::create_dir(&repo)?;
    let mut child = Command::new(binary)
        .env_clear()
        .env("HOME", directory.path())
        .env("CODEX_HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .args([
            "app-server",
            "--listen",
            "stdio://",
            "--disable",
            "multi_agent",
            "--disable",
            "multi_agent_v2",
            "-c",
            "approval_policy=\"on-request\"",
            "-c",
            "sandbox_mode=\"workspace-write\"",
            "-c",
            "sandbox_workspace_write.network_access=false",
            "-c",
            "sandbox_workspace_write.writable_roots=[]",
        ])
        .arg("-c")
        .arg(format!("approvals_reviewer=\"{}\"", reviewer.as_str()))
        .current_dir(&repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let (client, _events) = Client::connect(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        Duration::from_secs(20),
    );
    let result = async {
        let initialized: Value = client.request("initialize", json!({"clientInfo":{"name":"codex_discord_smoke","version":"0.1.0"},"capabilities":{"experimentalApi":true}})).await?;
        ensure!(initialized["userAgent"].is_string(), "Missing initialization result");
        client.notify("initialized", json!({})).await?;
        let auth: Value = client.request("account/read", json!({"refreshToken":false})).await?;
        ensure!(auth["account"].is_null(), "Test unexpectedly accessed credentials");
        // Verify both reviewers with the same effective cwd, sandbox and network restrictions.
        let thread = client.start_thread(repo.to_str().unwrap(), None, None, reviewer).await?;
        ensure!(!thread.thread.id.is_empty(), "Missing Codex thread ID");
        client.clean_terminals(&thread.thread.id).await?;
        // Exercise the real protocol's model and config override fields without inference.
        let thread = client.start_thread(repo.to_str().unwrap(), Some("gpt-6-astra"), Some("max"), reviewer).await?;
        ensure!(thread.model.as_deref() == Some("gpt-6-astra"), "Model override was lost");
        ensure!(thread.reasoning_effort.as_deref() == Some("max"), "Effort override was lost");
        client.clean_terminals(&thread.thread.id).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    client.disconnect();
    child.kill().await?;
    child.wait().await?;
    result
}
