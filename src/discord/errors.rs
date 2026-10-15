//! Safe diagnostics: never format Serenity's Debug output, request URLs, headers or bodies.
use crate::{discord::render, sessions::Manager};
use serenity::http::{ErrorResponse, HttpError};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[error("Discord operation {0} failed")]
pub(crate) struct Operation(pub &'static str);

pub(crate) fn response(error: &anyhow::Error) -> Option<&ErrorResponse> {
    match error.downcast_ref::<serenity::Error>()? {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => Some(response),
        _ => None,
    }
}

pub(crate) fn code(error: &anyhow::Error) -> Option<isize> {
    response(error).map(|r| r.error.code)
}

/// A 4xx rejection of the create request proves that this attempt created no channel.
/// Timeouts, transport/decoding errors and 5xx responses do not prove that.
pub(crate) fn creation_rejected(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Operation>()
        .is_some_and(|op| op.0 == "create_task_channel")
        && response(error)
            .is_some_and(|r| r.status_code.is_client_error() && r.status_code.as_u16() != 408)
}

impl Manager {
    pub(crate) fn discord_diagnostic(&self, error: &anyhow::Error) -> String {
        let operation = error.downcast_ref::<Operation>().map(|op| op.0);
        // Display on HttpError intentionally omits the request URL (which can contain tokens).
        // Do not use the entire source chain: reqwest sources can contain credentialed URLs.
        let detail = if let Some(discord) = error.downcast_ref::<serenity::Error>() {
            discord.to_string()
        } else {
            error.root_cause().to_string()
        };
        let mut detail = detail;
        for secret in [&self.config.discord_token, &self.config.database_url] {
            if !secret.is_empty() {
                detail = detail.replace(secret, "[redacted]");
            }
        }
        let metadata = response(error)
            .map(|r| format!("HTTP {}; Discord {}", r.status_code.as_u16(), r.error.code))
            .unwrap_or_else(|| "no Discord API response".into());
        format!(
            "{}: {metadata}: {}",
            operation.unwrap_or("channel_lifecycle"),
            render::truncate(&detail, 1000)
        )
    }

    pub(crate) fn log_discord_failure(&self, session: Option<Uuid>, error: &anyhow::Error) {
        let response = response(error);
        tracing::warn!(
            session_id = session.map(|id| id.to_string()),
            operation = error.downcast_ref::<Operation>().map(|op| op.0).unwrap_or("channel_lifecycle"),
            http_status = response.map(|r| r.status_code.as_u16()),
            discord_code = response.map(|r| r.error.code),
            error = %self.discord_diagnostic(error),
            "Discord operation failed; execution state preserved"
        );
    }
}
