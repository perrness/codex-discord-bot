pub mod protocol;
mod transport;
pub use transport::{Client, RpcError};

use crate::config::{Config, SUPPORTED_CODEX_VERSION};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use protocol::{Event, ThreadResponse, TurnResponse};
use serde_json::{Value, json};
use std::{os::unix::process::CommandExt, process::Stdio, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::mpsc,
};
use tokio_util::codec::{FramedRead, LinesCodec};
use uuid::Uuid;

pub struct Server {
    pub client: Client,
    pub child: Child,
    pub generation: Uuid,
    pid: i32,
}

impl Server {
    pub async fn spawn(config: &Config) -> Result<(Self, mpsc::Receiver<Event>)> {
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new(&config.codex_bin)
                .env_clear()
                .env("PATH", &config.tool_path)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("CODEX_BIN version check timed out")?
        .context("Run CODEX_BIN")?;
        ensure!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim()
                    == format!("codex-cli {SUPPORTED_CODEX_VERSION}"),
            "Unsupported Codex version; this build requires codex-cli {SUPPORTED_CODEX_VERSION}. Review schemas before upgrading."
        );
        std::fs::create_dir_all(&config.codex_home)?;
        let mut command = Command::new(&config.codex_bin);
        command
            .env_clear()
            .env("PATH", &config.tool_path)
            .env("CODEX_HOME", &config.codex_home);
        for key in [
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "SSL_CERT_FILE",
            "CODEX_CA_CERTIFICATE",
        ] {
            if let Ok(v) = std::env::var(key) {
                command.env(key, v);
            }
        }
        command
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
                "approvals_reviewer=\"user\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "sandbox_workspace_write.network_access=false",
                "-c",
                "sandbox_workspace_write.writable_roots=[]",
            ])
            .current_dir(&config.state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        // Only async-signal-safe syscalls in the forked child. systemd also owns the full cgroup.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("Start Codex App Server")?;
        let pid = child.id().context("Missing child PID")? as i32;
        let (client, events) = Client::connect(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
            config.rpc_timeout,
        );
        let stderr = child.stderr.take().unwrap();
        let log_content = std::env::var("CODEX_LOG_STDERR").is_ok_and(|s| s == "1");
        tokio::spawn(async move {
            let mut lines = FramedRead::new(stderr, LinesCodec::new_with_max_length(65536));
            // Raw stderr is opt-in because tool output may contain sensitive project data.
            let mut count = 0u64;
            while let Some(line) = lines.next().await {
                match line {
                    Ok(line) => {
                        count += 1;
                        if log_content {
                            tracing::warn!(codex_stderr = %line, "Codex App Server stderr");
                        } else if count == 1 || count.is_multiple_of(100) {
                            tracing::warn!(
                                lines = count,
                                "Codex stderr output observed (content redacted)"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::warn!("Discarded an oversized or unreadable Codex stderr line")
                    }
                }
            }
        });
        let mut server = Self {
            client,
            child,
            pid,
            generation: Uuid::new_v4(),
        };
        let init = server.client.request::<Value>("initialize", json!({
            "clientInfo":{"name":"codex_discord_bot","title":"Codex Discord Bot","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}
        })).await;
        if let Err(e) = init {
            server.shutdown().await;
            return Err(e.into());
        }
        server.client.notify("initialized", json!({})).await?;
        let auth: Value = server
            .client
            .request("account/read", json!({"refreshToken":false}))
            .await?;
        if auth["requiresOpenaiAuth"] == true && auth["account"].is_null() {
            server.shutdown().await;
            anyhow::bail!(
                "Codex is not signed in. Run codex login as the service user with the same CODEX_HOME."
            );
        }
        Ok((server, events))
    }

    pub async fn shutdown(&mut self) {
        if self.pid == 0 {
            return;
        }
        self.client.disconnect();
        // The pgid is created for this child; never signal the bot's own process group.
        unsafe {
            libc::kill(-self.pid, libc::SIGTERM);
        }
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait().await;
        self.pid = 0;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.pid > 0 {
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
    }
}

impl Client {
    pub async fn start_thread(&self, cwd: &str, model: Option<&str>) -> Result<ThreadResponse> {
        let result: ThreadResponse = self.request("thread/start", json!({"cwd":cwd,"model":model,
            "sandbox":"workspace-write","approvalPolicy":"on-request","approvalsReviewer":"user","ephemeral":false})).await?;
        verify_policy(&result, cwd)?;
        Ok(result)
    }

    pub async fn resume_thread(&self, thread: &str, cwd: &str) -> Result<ThreadResponse> {
        let result: ThreadResponse = self
            .request(
                "thread/resume",
                json!({"threadId":thread,"cwd":cwd,"excludeTurns":true,
            "sandbox":"workspace-write","approvalPolicy":"on-request","approvalsReviewer":"user"}),
            )
            .await?;
        verify_policy(&result, cwd)?;
        Ok(result)
    }

    pub async fn start_turn(
        &self,
        thread: &str,
        text: &str,
        cwd: &str,
        source_id: &str,
    ) -> Result<TurnResponse, RpcError> {
        self.request("turn/start", json!({"threadId":thread,"input":[{"type":"text","text":text}],
            "clientUserMessageId":source_id,"cwd":cwd,"approvalPolicy":"on-request","approvalsReviewer":"user",
            "sandboxPolicy":{"type":"workspaceWrite","writableRoots":[],"networkAccess":false,
                "excludeSlashTmp":false,"excludeTmpdirEnvVar":false}})).await
    }

    pub async fn steer(
        &self,
        thread: &str,
        turn: &str,
        text: &str,
        source_id: &str,
    ) -> Result<Value, RpcError> {
        self.request("turn/steer", json!({"threadId":thread,"expectedTurnId":turn,"input":[{"type":"text","text":text}],"clientUserMessageId":source_id})).await
    }

    pub async fn interrupt(&self, thread: &str, turn: &str) -> Result<(), RpcError> {
        self.request::<Value>("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
            .await
            .map(|_| ())
    }

    pub async fn clean_terminals(&self, thread: &str) -> Result<()> {
        self.request::<Value>(
            "thread/backgroundTerminals/clean",
            json!({"threadId":thread}),
        )
        .await?;
        let terminals: Value = self
            .request(
                "thread/backgroundTerminals/list",
                json!({"threadId":thread}),
            )
            .await?;
        // Checked below against the installed response schema; missing data is not proof of cleanup.
        ensure!(
            terminals["data"].as_array().is_some_and(Vec::is_empty),
            "Background processes remain or their status is unknown"
        );
        Ok(())
    }
}

fn verify_policy(r: &ThreadResponse, cwd: &str) -> Result<()> {
    ensure!(
        r.cwd == cwd && r.thread.cwd == cwd,
        "Codex returned a different working directory"
    );
    ensure!(
        r.approval_policy == "on-request" && r.approvals_reviewer == "user",
        "Codex did not apply human approval policy"
    );
    ensure!(
        r.sandbox["type"] == "workspaceWrite" && r.sandbox["networkAccess"] == false,
        "Codex did not apply the required sandbox"
    );
    if let Some(roots) = r.sandbox["writableRoots"].as_array() {
        ensure!(
            roots.iter().all(|r| r.as_str() == Some(cwd)),
            "Codex granted additional writable roots"
        );
    } else {
        anyhow::bail!("Codex omitted effective writable roots");
    }
    Ok(())
}
