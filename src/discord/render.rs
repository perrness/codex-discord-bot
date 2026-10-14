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
                .disabled(!active),
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
                || params["networkApprovalContext"].is_object()
        }
        FILE_APPROVAL => {
            item.is_some_and(|v| v["changes"].as_array().is_some_and(|a| !a.is_empty()))
        }
        PERMISSIONS_APPROVAL => false, // Explicitly handled below by the permission validator.
        _ => false,
    };
    if let Some(decisions) = params["availableDecisions"].as_array() {
        can_approve &= decisions.iter().any(|d| d == "accept");
    }
    let title = match method {
        COMMAND_APPROVAL => "Command / network approval",
        FILE_APPROVAL => "File change approval",
        _ => "Permission approval",
    };
    // Display all requested permissions, stdin data, network scope, and command details.
    // Never offer acceptance on a truncated or missing preview.
    let details = serde_json::to_string_pretty(params).unwrap_or_default();
    let preview = item
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_default();
    let mut text = format!("**{title}**\n{details}\n{preview}");
    if text.len() > 18000 {
        text = format!(
            "**{title}**\nThe operation is too large to review in Discord. Approval is disabled; decline or cancel and narrow the operation."
        );
        can_approve = false;
    }
    (text, can_approve)
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
}
