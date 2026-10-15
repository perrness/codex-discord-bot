use crate::{
    codex::protocol::{COMMAND_APPROVAL, FILE_APPROVAL, PERMISSIONS_APPROVAL, USER_INPUT},
    persistence::PendingRequest,
};
use serde_json::Value;
use serenity::all::{ButtonStyle, CreateActionRow, CreateButton};

/// Discord counts UTF-16 code units. Leave room for formatting and avoid byte slicing.
pub fn chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    // Preserve all text; chunks deliberately don't reconstruct arbitrary Markdown fences.
    for c in text.chars() {
        if units + c.len_utf16() > 1850 {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(c);
        units += c.len_utf16();
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push("(No text output)".into());
    }
    chunks
}

pub fn short(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Keep fenced commands and diffs readable when an interactive preview spans messages.
pub fn interactive_chunks(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut units = 0;
    let mut fence: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let marker = line.trim_end();
        let is_fence = marker.starts_with("```") && marker.len() <= 32;
        // Keep a fence marker intact even at a message boundary.
        let parts = if is_fence {
            vec![line.to_owned()]
        } else {
            chunks(line)
        };
        for part in parts {
            let length = part.encode_utf16().count();
            if units + length > 1850 && !current.is_empty() {
                if fence.is_some() {
                    current.push_str("\n```");
                }
                result.push(std::mem::take(&mut current));
                if let Some(opening) = fence {
                    current.push_str(opening);
                    current.push('\n');
                }
                units = current.encode_utf16().count();
            }
            current.push_str(&part);
            units += length;
        }
        if is_fence {
            fence = if fence.is_some() { None } else { Some(marker) };
        }
    }
    if !current.is_empty() {
        if fence.is_some() {
            current.push_str("\n```");
        }
        result.push(current);
    }
    if result.is_empty() {
        return chunks(text);
    }
    result
}

pub fn approval_decision_available(params: &Value, decision: &str) -> bool {
    params.get("availableDecisions").is_none_or(|v| {
        v.is_null()
            || v.as_array()
                .is_some_and(|values| values.iter().any(|v| v == decision))
    })
}

pub fn controls(request: &PendingRequest) -> Vec<CreateActionRow> {
    let prefix = format!("req:{}:", request.id);
    let mut buttons = Vec::new();
    let active = request.state == "pending";
    if request.method == USER_INPUT {
        buttons.push(
            CreateButton::new(format!("{prefix}answer"))
                .label("Answer questions")
                .style(ButtonStyle::Primary)
                .disabled(!active),
        );
    } else if matches!(
        request.method.as_str(),
        COMMAND_APPROVAL | FILE_APPROVAL | PERMISSIONS_APPROVAL
    ) {
        let label = if request.method == PERMISSIONS_APPROVAL {
            "Allow this turn"
        } else {
            "Approve once"
        };
        buttons.push(
            CreateButton::new(format!("{prefix}accept"))
                .label(label)
                .style(ButtonStyle::Success)
                .disabled(!active || request.payload["canApprove"] != true),
        );
        buttons.push(
            CreateButton::new(format!("{prefix}decline"))
                .label("Decline")
                .style(ButtonStyle::Secondary)
                .disabled(
                    !active
                        || (request.method != PERMISSIONS_APPROVAL
                            && !approval_decision_available(&request.payload["params"], "decline")),
                ),
        );
    }
    buttons.push(
        CreateButton::new(format!("{prefix}cancel"))
            .label("Cancel turn")
            .style(ButtonStyle::Danger)
            .disabled(!active),
    );
    vec![CreateActionRow::Buttons(buttons)]
}

pub fn approval_preview(method: &str, params: &Value, item: Option<&Value>) -> (String, bool) {
    let mut can_approve = match method {
        COMMAND_APPROVAL => {
            params["command"].as_str().is_some_and(|c| !c.is_empty())
                || (params["networkApprovalContext"]["host"]
                    .as_str()
                    .is_some_and(|h| !h.is_empty())
                    && params["networkApprovalContext"]["protocol"].is_string())
        }
        FILE_APPROVAL => {
            item.is_some_and(|v| v["changes"].as_array().is_some_and(|a| !a.is_empty()))
        }
        // The session layer also validates the actual paths and supported permission fields.
        PERMISSIONS_APPROVAL => params["permissions"].is_object(),
        _ => false,
    };
    if method == COMMAND_APPROVAL && !params["kind"].is_null() {
        can_approve &= matches!(params["kind"].as_str(), Some("command" | "writeStdin"));
    }
    can_approve &= approval_decision_available(params, "accept");
    let title = match method {
        COMMAND_APPROVAL if params["kind"] == "writeStdin" => "Allow terminal input?",
        COMMAND_APPROVAL if params["networkApprovalContext"].is_object() => "Allow network access?",
        COMMAND_APPROVAL => "Run this command?",
        FILE_APPROVAL => "Allow these file changes?",
        _ => "Allow additional access?",
    };
    let mut text = format!("**{title}**\n");
    detail(&mut text, "Reason", &params["reason"]);
    detail(&mut text, "Directory", &params["cwd"]);
    if params["environmentId"] != "local" {
        detail(&mut text, "Environment", &params["environmentId"]);
    }
    if let Some(command) = params["command"].as_str() {
        let label = if params["kind"] == "writeStdin" {
            "Terminal input"
        } else {
            "Command"
        };
        text.push_str(&format!("\n**{label}**\n{}", code_block(command, "sh")));
    }
    detail(
        &mut text,
        "Network access",
        &params["networkApprovalContext"],
    );
    permission_details(
        &mut text,
        "Additional access",
        &params["additionalPermissions"],
    );
    permission_details(&mut text, "Requested access", &params["permissions"]);
    detail(
        &mut text,
        "Write access for this session",
        &params["grantRoot"],
    );
    // Omit correlation IDs and duplicate command parsing. Rule proposals are optional
    // alternatives, never part of our accept-once response. Keep unfamiliar details visible.
    extra_details(
        &mut text,
        params,
        &[
            "threadId",
            "turnId",
            "itemId",
            "approvalId",
            "startedAtMs",
            "kind",
            "reason",
            "cwd",
            "environmentId",
            "command",
            "commandActions",
            "availableDecisions",
            "proposedExecpolicyAmendment",
            "proposedNetworkPolicyAmendments",
            "networkApprovalContext",
            "additionalPermissions",
            "permissions",
            "grantRoot",
        ],
    );
    if method == FILE_APPROVAL
        && let Some(item) = item
    {
        if let Some(changes) = item["changes"].as_array() {
            for change in changes {
                let action = match change["kind"]["type"].as_str() {
                    Some("add") => "Create file",
                    Some("delete") => "Delete file",
                    Some("update") => "Update file",
                    _ => {
                        can_approve = false;
                        "File change"
                    }
                };
                detail(&mut text, action, &change["path"]);
                detail(&mut text, "Move to", &change["kind"]["move_path"]);
                if let Some(diff) = change["diff"].as_str() {
                    text.push_str(&code_block(diff, "diff"));
                } else {
                    can_approve = false;
                }
                can_approve &= change["path"].as_str().is_some_and(|p| !p.is_empty());
                extra_details(&mut text, &change["kind"], &["type", "move_path"]);
                extra_details(&mut text, change, &["kind", "path", "diff"]);
            }
        }
        extra_details(&mut text, item, &["id", "type", "status", "changes"]);
    }
    text.push_str(if method == PERMISSIONS_APPROVAL {
        "\nApplies for this turn only."
    } else if params["grantRoot"].is_null() {
        "\nApplies to this request only."
    } else {
        "\nIncludes the session write access shown above."
    });
    if !can_approve {
        text.push_str("\nApproval is unavailable: complete details and an allowed approval decision are required.");
    }
    // Never offer acceptance on a truncated or missing preview.
    if text.len() > 18000 {
        text = format!(
            "**{title}**\nThe operation is too large to review in Discord. Approval is disabled; decline or cancel and narrow the operation."
        );
        can_approve = false;
    }
    (text, can_approve)
}

fn code_block(value: &str, language: &str) -> String {
    // Prevent tool data from closing the Discord fence; do not interpret shell syntax.
    format!(
        "```{language}\n{}\n```\n",
        value.replace("``", "`\u{200b}`")
    )
}

fn escaped(value: &str) -> String {
    let mut text = String::new();
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '`' | '*' | '_' | '~' | '|' | '>' | '#' | '[' | ']'
        ) {
            text.push('\\');
        }
        text.push(ch);
    }
    text
}

fn field_label(key: &str) -> String {
    let mut label = String::new();
    for ch in key.chars() {
        if ch == '_' {
            label.push(' ');
        } else if label.is_empty() {
            label.extend(ch.to_uppercase());
        } else if ch.is_uppercase() {
            label.push(' ');
            label.extend(ch.to_lowercase());
        } else {
            label.push(ch);
        }
    }
    escaped(&label)
}

fn detail(text: &mut String, label: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::Object(fields) => {
            for (key, value) in fields {
                detail(text, &format!("{label} · {}", field_label(key)), value);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                detail(text, &format!("{label} {}", index + 1), value);
            }
        }
        Value::String(value) => text.push_str(&format!("**{label}:** {}\n", escaped(value))),
        Value::Bool(value) => text.push_str(&format!(
            "**{label}:** {}\n",
            if *value { "Yes" } else { "No" }
        )),
        Value::Number(value) => text.push_str(&format!("**{label}:** {value}\n")),
    }
}

fn extra_details(text: &mut String, value: &Value, displayed_or_metadata: &[&str]) {
    if let Some(fields) = value.as_object() {
        for (key, value) in fields {
            if !displayed_or_metadata.contains(&key.as_str()) {
                detail(text, &field_label(key), value);
            }
        }
    }
}

fn permission_details(text: &mut String, label: &str, value: &Value) {
    if !value.is_object() {
        detail(text, label, value);
        return;
    }
    text.push_str(&format!("\n**{label}**\n"));
    detail(text, "Network access", &value["network"]["enabled"]);
    extra_details(text, &value["network"], &["enabled"]);
    let fs = &value["fileSystem"];
    detail(text, "Read files", &fs["read"]);
    detail(text, "Write files", &fs["write"]);
    if let Some(entries) = fs["entries"].as_array() {
        for entry in entries {
            let access = match entry["access"].as_str() {
                Some("read") => "Read files",
                Some("write") => "Write files",
                Some("deny") => "Deny file access",
                _ => {
                    detail(text, "File access", entry);
                    continue;
                }
            };
            if entry["path"]["type"] == "path" {
                detail(text, access, &entry["path"]["path"]);
                extra_details(text, &entry["path"], &["type", "path"]);
            } else {
                detail(text, access, &entry["path"]);
            }
            extra_details(text, entry, &["access", "path"]);
        }
    } else {
        detail(text, "File access", &fs["entries"]);
    }
    detail(text, "Glob scan maximum depth", &fs["globScanMaxDepth"]);
    extra_details(text, fs, &["read", "write", "entries", "globScanMaxDepth"]);
    extra_details(text, value, &["fileSystem", "network"]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn chunks_preserve_unicode_and_stay_within_discord_limit() {
        let text = format!("{}\n```rust\n{}\n```", "💬".repeat(2500), "x".repeat(3000));
        let parts = chunks(&text);
        assert!(parts.iter().all(|s| s.encode_utf16().count() <= 1850));
        assert_eq!(parts.concat(), text);
    }
    #[test]
    fn approvals_require_complete_visible_details() {
        assert!(!approval_preview(FILE_APPROVAL, &json!({}), None).1);
        assert!(
            !approval_preview(
                COMMAND_APPROVAL,
                &json!({"command":"x".repeat(20000)}),
                None
            )
            .1
        );
        assert!(
            !approval_preview(
                COMMAND_APPROVAL,
                &json!({"command":"ls","availableDecisions":["decline"]}),
                None
            )
            .1
        );
        assert!(
            approval_preview(
                COMMAND_APPROVAL,
                &json!({"command":"cargo test","cwd":"/repos/test"}),
                None
            )
            .1
        );
    }

    #[test]
    fn command_preview_is_readable_without_protocol_metadata() {
        let command = "/bin/bash -lc 'cat AGENTS.md ARCHITECTURE.md VALIDATION.md Cargo.toml && gh issue view 4 --json title,body,comments,state,url && ls .github/workflows && git log -6 --oneline && git tag --sort=-version:refname | head -10'";
        let params = json!({
            "availableDecisions": ["accept", {"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["cat", "AGENTS.md"]}}, "cancel"],
            "command":command,"commandActions":[{"command":command,"type":"unknown"}],
            "cwd":"/repos/editor-pro","environmentId":"local","itemId":"exec-123","kind":"command",
            "proposedExecpolicyAmendment":["cat", "AGENTS.md"],
            "reason":"May I inspect the issue, repository guidance, and release history outside the broken sandbox?",
            "startedAtMs":1792086156083_i64,"threadId":"thread-123","turnId":"turn-123"
        });
        let (text, allowed) = approval_preview(COMMAND_APPROVAL, &params, None);
        assert!(allowed);
        assert!(text.starts_with("**Run this command?**"));
        assert!(text.contains("**Directory:** /repos/editor-pro"));
        assert!(text.contains(params["reason"].as_str().unwrap()));
        assert!(text.contains(&format!("```sh\n{command}\n```")));
        for metadata in [
            "thread-123",
            "turn-123",
            "exec-123",
            "1792086156083",
            "availableDecisions",
            "commandActions",
            "execpolicy_amendment",
            "Environment",
        ] {
            assert!(!text.contains(metadata), "leaked metadata: {metadata}");
        }
        assert_eq!(interactive_chunks(&text).len(), 1);
        assert!(!approval_decision_available(&params, "decline"));
        assert!(approval_decision_available(&params, "accept"));
    }

    #[test]
    fn previews_preserve_network_stdin_and_extra_permission_details() {
        let (text, allowed) = approval_preview(
            COMMAND_APPROVAL,
            &json!({
                "kind":"writeStdin","command":"yes\n","cwd":"/repos/test", "environmentId":"remote-1",
                "networkApprovalContext":{"host":"github.com","protocol":"https"},
                "additionalPermissions":{"network":{"enabled":true},"fileSystem":{"write":["/repos/test/output"]}},
                "stdin":"actual input", "futureScope":"visible scope"
            }),
            None,
        );
        assert!(allowed);
        for detail in [
            "Allow terminal input?",
            "yes",
            "remote-1",
            "github.com",
            "https",
            "Network access:** Yes",
            "/repos/test/output",
            "actual input",
            "visible scope",
        ] {
            assert!(text.contains(detail), "missing: {detail}");
        }
        assert!(
            !approval_preview(
                COMMAND_APPROVAL,
                &json!({"networkApprovalContext":{}}),
                None
            )
            .1
        );
        assert!(
            !approval_preview(
                COMMAND_APPROVAL,
                &json!({"command":"ls","kind":"unknown"}),
                None
            )
            .1
        );
    }

    #[test]
    fn file_preview_shows_changes_moves_and_requires_every_diff() {
        let mut item = json!({"id":"item-1","type":"fileChange","status":"inProgress","changes":[
            {"path":"/repos/test/src/old.rs","kind":{"type":"update","move_path":"/repos/test/src/new.rs"},"diff":"-old\n+new"},
            {"path":"/repos/test/README.md","kind":{"type":"add"},"diff":"+Hello"}
        ]});
        let (text, allowed) = approval_preview(
            FILE_APPROVAL,
            &json!({"reason":"Update the project"}),
            Some(&item),
        );
        assert!(allowed);
        for detail in [
            "Update file",
            "Create file",
            "old.rs",
            "Move to",
            "new.rs",
            "```diff\n-old\n+new",
            "+Hello",
        ] {
            assert!(text.contains(detail));
        }
        assert!(!text.contains("item-1"));
        item["changes"][1]["diff"] = Value::Null;
        assert!(!approval_preview(FILE_APPROVAL, &json!({}), Some(&item)).1);
    }

    #[test]
    fn permission_preview_shows_scope_and_cannot_approve_oversized_details() {
        let mut params = json!({"permissions":{"network":{"enabled":true},"fileSystem":{
            "read":["/repos/test/data"],"entries":[{"path":{"type":"path","path":"/repos/test/output"},"access":"write"}]
        }}});
        let (text, allowed) = approval_preview(PERMISSIONS_APPROVAL, &params, None);
        assert!(allowed);
        for detail in [
            "Network access:** Yes",
            "Read files",
            "/repos/test/data",
            "Write files",
            "/repos/test/output",
            "this turn only",
        ] {
            assert!(text.contains(detail));
        }
        params["reason"] = json!("x".repeat(19000));
        let (text, allowed) = approval_preview(PERMISSIONS_APPROVAL, &params, None);
        assert!(!allowed);
        assert!(text.contains("too large"));
    }

    #[test]
    fn interactive_chunks_balance_fences_and_preserve_unicode_content() {
        let text = format!(
            "**Command**\n```sh\n{}\n{}\n```\nApprove once.",
            "💬".repeat(2500),
            "x\n".repeat(3000)
        );
        let parts = interactive_chunks(&text);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|s| s.encode_utf16().count() <= 2000));
        assert!(parts.iter().all(|s| s.matches("```").count() % 2 == 0));
        assert_eq!(
            parts.iter().map(|s| s.matches('💬').count()).sum::<usize>(),
            2500
        );
        assert_eq!(
            parts.iter().map(|s| s.matches('x').count()).sum::<usize>(),
            3000
        );
        assert!(parts.last().unwrap().contains("Approve once."));
        let (text, _) = approval_preview(
            COMMAND_APPROVAL,
            &json!({"command":"echo '`````'", "reason":"**bold**"}),
            None,
        );
        assert_eq!(text.matches("```").count(), 2);
        assert!(text.contains("\\*\\*bold\\*\\*"));
    }
}
