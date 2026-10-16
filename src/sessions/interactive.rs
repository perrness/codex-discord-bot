use super::*;
use crate::persistence::PendingRequest;
use std::path::{Component, Path};

impl Manager {
    pub(super) async fn interactive_request(
        &self,
        session: &Session,
        rpc_id: RpcId,
        method: &str,
        params: Value,
    ) -> Result<()> {
        let parsed = serde_json::from_value::<InteractiveParams>(params.clone());
        let supported = matches!(
            method,
            COMMAND_APPROVAL | FILE_APPROVAL | PERMISSIONS_APPROVAL | USER_INPUT
        );
        if !supported || parsed.is_err() {
            if let Some(result) = denied_response(method) {
                self.client.respond(&rpc_id, result).await?;
            } else {
                self.client.unsupported(&rpc_id).await?;
            }
            self.store.enqueue(session.id, &format!("unsupported:{}:{rpc_id:?}", self.generation),
                "Codex requested an interaction this bot cannot safely handle. It was declined. Configure optional tools or authentication over SSH.", None).await?;
            if let (Some(thread), Some(turn)) = (&session.codex_thread_id, &session.current_turn_id)
            {
                self.client.interrupt(thread, turn).await?;
            }
            return Ok(());
        }
        let parsed = parsed?;
        ensure!(
            Some(parsed.thread_id.as_str()) == session.codex_thread_id.as_deref(),
            "Interactive request routed to wrong thread"
        );
        if parsed.turn_id.is_none()
            || parsed.turn_id.as_deref() != session.current_turn_id.as_deref()
            || !matches!(session.status.as_str(), "running" | "waiting")
        {
            if let Some(result) = denied_response(method) {
                self.client.respond(&rpc_id, result).await?;
            } else {
                self.client.unsupported(&rpc_id).await?;
            }
            return Ok(());
        }
        let (text, can_approve) = if method == USER_INPUT {
            if parsed.questions.is_empty()
                || parsed.questions.len() > 5
                || parsed.questions.iter().any(|q| q.is_secret)
            {
                self.client.unsupported(&rpc_id).await?;
                self.client
                    .interrupt(&parsed.thread_id, parsed.turn_id.as_deref().unwrap())
                    .await?;
                self.store.enqueue(session.id, &format!("question-rejected:{rpc_id:?}:{}", self.generation),
                    "This question needs secret input or a form Discord cannot handle. The turn was interrupted; use SSH for credential setup.", None).await?;
                return Ok(());
            }
            let mut text = String::from("**Codex has a question**\n");
            for q in &parsed.questions {
                text.push_str(&format!("\n{}\n", q.question));
                for option in q.options.iter().flatten() {
                    text.push_str(&format!("• {} — {}\n", option.label, option.description));
                }
            }
            text.push_str("\nUse **Answer questions** to reply.");
            (text, false)
        } else {
            let preview = parsed.item_id.as_ref().and_then(|id| {
                self.previews
                    .lock()
                    .unwrap()
                    .get(&(parsed.thread_id.clone(), id.clone()))
                    .cloned()
            });
            let (text, mut allowed) = render::approval_preview(method, &params, preview.as_ref());
            if method == PERMISSIONS_APPROVAL {
                allowed &= params.to_string().len() < 12000
                    && permissions_within_repo(
                        &params["permissions"],
                        Path::new(&session.working_directory),
                    );
            }
            if !params["additionalPermissions"].is_null() {
                allowed &= permissions_within_repo(
                    &params["additionalPermissions"],
                    Path::new(&session.working_directory),
                );
            }
            if let Some(root) = params["grantRoot"].as_str() {
                allowed &= contained_path(root, Path::new(&session.working_directory));
            }
            (text, allowed)
        };
        let id = Uuid::new_v4();
        let blocking = method != USER_INPUT || parsed.is_blocking;
        let payload = json!({"params":params,"canApprove":can_approve,"blocking":blocking});
        let mut tx = self.store.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO pending_requests(id,session_id,generation,rpc_id,method,turn_id,item_id,payload,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,'pending') ON CONFLICT(generation,rpc_id) DO NOTHING")
            .bind(id).bind(session.id).bind(self.generation).bind(serde_json::to_value(&rpc_id)?)
            .bind(method).bind(parsed.turn_id.as_deref().unwrap()).bind(parsed.item_id).bind(payload)
            .execute(&mut *tx).await?.rows_affected();
        if inserted == 1 {
            persistence::enqueue(
                &mut tx,
                session.id,
                &format!("request:{id}"),
                &text,
                Some(id),
            )
            .await?;
            if blocking {
                sqlx::query("UPDATE sessions SET status='waiting',updated_at=now() WHERE id=$1")
                    .bind(session.id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_for_user(
        &self,
        user: u64,
        guild: u64,
        channel: u64,
        id: Uuid,
    ) -> Result<PendingRequest> {
        self.authorize(user, Some(guild))?;
        let request = self.store.pending(id).await?;
        let session = self.store.session(request.session_id).await?;
        validate_request(&request, &session, self.generation, guild, channel)?;
        Ok(request)
    }

    pub async fn answer(
        &self,
        user: u64,
        guild: u64,
        channel: u64,
        id: Uuid,
        action: &str,
        answers: Vec<String>,
    ) -> Result<String> {
        self.authorize(user, Some(guild))?;
        let request = self.store.pending(id).await?;
        let gate = self.gate(request.session_id);
        let _guard = gate.lock().await;
        let request = self.request_for_user(user, guild, channel, id).await?;
        let params = &request.payload["params"];
        let response = match action {
            "cancel" => denied_response(&request.method),
            "accept" => {
                ensure!(
                    request.payload["canApprove"] == true,
                    "This operation cannot be safely approved in Discord"
                );
                match request.method.as_str() {
                    COMMAND_APPROVAL | FILE_APPROVAL => Some(json!({"decision":"accept"})),
                    PERMISSIONS_APPROVAL => {
                        let session = self.store.session(request.session_id).await?;
                        ensure!(
                            permissions_within_repo(
                                &params["permissions"],
                                Path::new(&session.working_directory)
                            ),
                            "Permission paths changed or escaped the repository"
                        );
                        Some(json!({"permissions":params["permissions"],"scope":"turn"}))
                    }
                    _ => bail!("This request is not an approval"),
                }
            }
            "decline" => match request.method.as_str() {
                COMMAND_APPROVAL | FILE_APPROVAL => {
                    ensure!(
                        render::approval_decision_available(params, "decline"),
                        "Codex does not offer Decline for this request; use Cancel turn"
                    );
                    Some(json!({"decision":"decline"}))
                }
                PERMISSIONS_APPROVAL => Some(json!({"permissions":{},"scope":"turn"})),
                _ => bail!("This request is not an approval"),
            },
            "answer" => {
                ensure!(
                    request.method == USER_INPUT,
                    "This request does not accept text answers"
                );
                let p: InteractiveParams = serde_json::from_value(params.clone())?;
                ensure!(
                    p.questions.len() == answers.len()
                        && answers
                            .iter()
                            .all(|a| !a.trim().is_empty() && a.len() <= 16000),
                    "Answer every question"
                );
                let map: serde_json::Map<String, Value> = p
                    .questions
                    .iter()
                    .zip(answers)
                    .map(|(q, a)| (q.id.clone(), json!({"answers":[a]})))
                    .collect();
                Some(json!({"answers":map}))
            }
            _ => bail!("Unknown action"),
        };
        if matches!(action, "accept" | "answer") {
            let repository_workflow: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_runs WHERE session_id=$1 AND workflow='repository')")
                .bind(request.session_id).fetch_one(&self.store.pool).await?;
            if repository_workflow {
                self.store
                    .claim_issue_workflow(request.session_id, "", true)
                    .await?;
            }
        }
        let changed = sqlx::query("UPDATE pending_requests SET state='responding',updated_at=now() WHERE id=$1 AND state='pending' AND generation=$2")
            .bind(id).bind(self.generation).execute(&self.store.pool).await?.rows_affected();
        ensure!(changed == 1, "This request was already answered");
        let rpc_id = serde_json::from_value(request.rpc_id)?;
        if let Some(response) = response
            && self.client.respond(&rpc_id, response).await.is_err()
        {
            self.fail_closed("interactive response delivery uncertain");
            bail!("Connection lost; this approval will not be replayed");
        }
        if action == "cancel" {
            self.client
                .interrupt(
                    params["threadId"].as_str().context("Missing thread")?,
                    &request.turn_id,
                )
                .await?;
        }
        Ok("Response sent to Codex.".into())
    }

    pub async fn answer_reply(
        &self,
        user: u64,
        guild: u64,
        channel: u64,
        referenced_message: u64,
        text: &str,
    ) -> Result<bool> {
        let id: Option<Uuid> = sqlx::query_scalar("SELECT request_id FROM discord_outbox WHERE discord_message_id=$1 AND request_id IS NOT NULL")
            .bind(referenced_message.to_string()).fetch_optional(&self.store.pool).await?;
        let Some(id) = id else {
            return Ok(false);
        };
        let request = self.request_for_user(user, guild, channel, id).await?;
        ensure!(
            request.method == USER_INPUT,
            "Use the approval buttons; text replies cannot grant permission"
        );
        let params: InteractiveParams = serde_json::from_value(request.payload["params"].clone())?;
        ensure!(
            params.questions.len() == 1,
            "Use Answer questions to answer the complete form"
        );
        self.answer(user, guild, channel, id, "answer", vec![text.into()])
            .await?;
        Ok(true)
    }
}

fn validate_request(
    r: &PendingRequest,
    s: &Session,
    generation: Uuid,
    guild: u64,
    channel: u64,
) -> Result<()> {
    ensure!(
        r.generation == generation && r.state == "pending",
        "This request expired or was already answered"
    );
    ensure!(
        r.session_id == s.id
            && s.guild_id == guild.to_string()
            && s.channel_id() == Some(channel.to_string().as_str())
            && !matches!(s.channel_state.as_str(), "deleted" | "missing"),
        "Request belongs to a different or unavailable channel"
    );
    ensure!(
        s.current_turn_id.as_deref() == Some(r.turn_id.as_str())
            && matches!(s.status.as_str(), "running" | "waiting"),
        "The requesting turn is no longer active"
    );
    Ok(())
}

fn contained_path(path: &str, cwd: &Path) -> bool {
    let path = Path::new(path);
    path.is_absolute()
        && !path.components().any(|c| matches!(c, Component::ParentDir))
        && path.canonicalize().is_ok_and(|p| p.starts_with(cwd))
}

fn permissions_within_repo(value: &Value, cwd: &Path) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object
        .keys()
        .any(|k| !matches!(k.as_str(), "fileSystem" | "network"))
    {
        return false;
    }
    if !value["network"].is_null() {
        let Some(net) = value["network"].as_object() else {
            return false;
        };
        if net.keys().any(|k| k != "enabled")
            || !net
                .get("enabled")
                .is_none_or(|v| v.is_boolean() || v.is_null())
        {
            return false;
        }
    }
    if let Some(fs) = value.get("fileSystem").filter(|v| !v.is_null()) {
        let Some(fs) = fs.as_object() else {
            return false;
        };
        if fs.keys().any(|k| {
            !matches!(
                k.as_str(),
                "read" | "write" | "entries" | "globScanMaxDepth"
            )
        }) {
            return false;
        }
        for key in ["read", "write"] {
            if let Some(paths) = fs.get(key).filter(|v| !v.is_null()) {
                let Some(paths) = paths.as_array() else {
                    return false;
                };
                if paths
                    .iter()
                    .any(|p| !p.as_str().is_some_and(|p| contained_path(p, cwd)))
                {
                    return false;
                }
            }
        }
        if let Some(entries) = fs.get("entries").filter(|v| !v.is_null()) {
            let Some(entries) = entries.as_array() else {
                return false;
            };
            for entry in entries {
                if entry["path"]["type"] != "path"
                    || !entry["path"]["path"]
                        .as_str()
                        .is_some_and(|p| contained_path(p, cwd))
                    || !matches!(entry["access"].as_str(), Some("read" | "write" | "deny"))
                {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_grants_reject_unknown_and_outside_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(permissions_within_repo(
            &json!({"network":{"enabled":true}}),
            dir.path()
        ));
        assert!(!permissions_within_repo(
            &json!({"fileSystem":{"write":["/etc"]}}),
            dir.path()
        ));
        assert!(!permissions_within_repo(
            &json!({"futurePrivilege":true}),
            dir.path()
        ));
        assert!(!permissions_within_repo(
            &json!({"fileSystem":{"entries":[{"path":{"type":"special","value":"root"},"access":"write"}]}}),
            dir.path()
        ));
        assert!(permissions_within_repo(
            &json!({"fileSystem":{"write":[dir.path()]}}),
            dir.path()
        ));
    }

    #[test]
    fn rpc_request_ids_keep_string_and_number_distinct() {
        assert_ne!(RpcId::String("1".into()), RpcId::Number(1));
    }
}
