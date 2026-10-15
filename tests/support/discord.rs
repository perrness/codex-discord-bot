//! Small local HTTP fixture for the actual Serenity client; no live Discord or mocking framework.
use serde_json::{Value, json};
use serenity::http::{Http, HttpBuilder};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

pub fn channel(id: &str, kind: u8, parent: u64, topic: &str) -> Value {
    json!({"id":id,"guild_id":"7","type":kind,"name":format!("task-{id}"),
        "parent_id":parent.to_string(),"topic":topic,"permission_overwrites":[]})
}

fn user() -> Value {
    json!({"id":"99","username":"codex","discriminator":"0001","avatar":null,"bot":true})
}

pub fn message(id: &str, channel: &str, body: &Value) -> Value {
    json!({"id":id,"channel_id":channel,"author":user(),"content":body["content"].as_str().unwrap_or(""),
        "timestamp":"2026-10-15T00:00:00Z","edited_timestamp":null,"tts":false,"mention_everyone":false,
        "mentions":[],"mention_roles":[],"attachments":[],"embeds":body["embeds"].as_array().cloned().unwrap_or_default(),
        "pinned":false,"type":0,"components":body["components"].as_array().cloned().unwrap_or_default()})
}

pub struct State {
    pub channels: HashMap<String, Value>,
    pub messages: HashMap<String, Vec<Value>>,
    pub calls: Vec<(String, String, Value)>,
    pub fail_next: Option<(String, String, u16, i64)>,
    pub fail_after_create: Option<(u16, i64)>,
    pub fail_message: Option<String>,
    pub created_overwrites: Option<Value>,
    pub permissions: u64,
    pub next_id: u64,
    nonces: HashMap<String, Value>,
}

impl State {
    pub fn count(&self, method: &str, path: &str) -> usize {
        self.calls
            .iter()
            .filter(|(m, p, _)| m == method && p == path)
            .count()
    }

    fn respond(&mut self, method: &str, path: &str, query: &str, body: Value) -> (u16, Value) {
        self.calls.push((method.into(), path.into(), body.clone()));
        if self
            .fail_next
            .as_ref()
            .is_some_and(|(m, p, _, _)| m == method && p == path)
        {
            let (_, _, status, code) = self.fail_next.take().unwrap();
            return (
                status,
                json!({"code":code,"message": self.fail_message.take().unwrap_or_else(|| match code {
                    50013 => "Missing Permissions".into(),
                    50001 => "Missing Access".into(),
                    _ => "Injected API failure".into(),
                })}),
            );
        }
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("GET", ["users", "@me"]) => (200, user()),
            ("GET", ["guilds", "7"]) => (
                200,
                json!({"id":"7","name":"Test","owner_id":"42",
                "verification_level":0,"default_message_notifications":0,"explicit_content_filter":0,
                "roles":[{"id":"7","name":"everyone","color":0,"colors":{"primary_color":0},"hoist":false,"position":0,"permissions":self.permissions.to_string(),"managed":false,"mentionable":false,"flags":0}],
                "emojis":[],"features":[],"mfa_level":0,"system_channel_flags":0,"premium_tier":0,
                "nsfw_level":0,"premium_progress_bar_enabled":false,"preferred_locale":"en-US","stickers":[]}),
            ),
            ("GET", ["guilds", "7", "members", "99"]) => (
                200,
                json!({"user":user(),"roles":[],"joined_at":"2026-01-01T00:00:00Z","deaf":false,"mute":false,"flags":0}),
            ),
            ("GET", ["guilds", "7", "channels"]) => {
                (200, json!(self.channels.values().collect::<Vec<_>>()))
            }
            ("POST", ["guilds", "7", "channels"]) => {
                use serenity::all::Permissions;
                let explicit = body["permission_overwrites"].as_array();
                let requested = explicit.into_iter().flatten().fold(0, |bits, p| {
                    bits | p["allow"].as_str().unwrap_or("0").parse::<u64>().unwrap()
                        | p["deny"].as_str().unwrap_or("0").parse::<u64>().unwrap()
                });
                if self.permissions & Permissions::ADMINISTRATOR.bits() == 0
                    && (self.permissions & Permissions::MANAGE_CHANNELS.bits() == 0
                        || requested & !self.permissions != 0
                        || requested & Permissions::MANAGE_ROLES.bits() != 0)
                {
                    return (403, json!({"code":50013,"message":"Missing Permissions"}));
                }
                self.next_id += 1;
                let id = self.next_id.to_string();
                let mut result = body;
                if result.get("permission_overwrites").is_none() {
                    // Omitting overwrites preserves the parent's complete visibility rules.
                    result["permission_overwrites"] = self
                        .channels
                        .get(result["parent_id"].as_str().unwrap())
                        .unwrap()["permission_overwrites"]
                        .clone();
                }
                if let Some(overwrites) = self.created_overwrites.take() {
                    result["permission_overwrites"] = overwrites;
                }
                result["id"] = json!(id);
                result["guild_id"] = json!("7");
                self.channels.insert(id, result.clone());
                if let Some((status, code)) = self.fail_after_create.take() {
                    return (
                        status,
                        json!({"code":code,"message":"Response lost after creation"}),
                    );
                }
                (200, result)
            }
            (_, ["channels", id, ..]) if !self.channels.contains_key(*id) => {
                (404, json!({"code":10003,"message":"Unknown Channel"}))
            }
            ("GET", ["channels", id]) => (200, self.channels[*id].clone()),
            ("PATCH", ["channels", id]) => {
                let channel = self.channels.get_mut(*id).unwrap();
                channel
                    .as_object_mut()
                    .unwrap()
                    .extend(body.as_object().unwrap().clone());
                (200, channel.clone())
            }
            ("DELETE", ["channels", id]) => (200, self.channels.remove(*id).unwrap()),
            ("GET", ["channels", id, "messages"]) => {
                let bound = |name: &str| {
                    query.split('&').find_map(|part| {
                        part.strip_prefix(&format!("{name}="))
                            .and_then(|s| s.parse::<u64>().ok())
                    })
                };
                let mut messages: Vec<_> = self
                    .messages
                    .get(*id)
                    .into_iter()
                    .flatten()
                    .filter(|m| {
                        let n: u64 = m["id"].as_str().unwrap().parse().unwrap();
                        bound("before").is_none_or(|b| n < b)
                            && bound("after").is_none_or(|a| n > a)
                    })
                    .cloned()
                    .collect();
                messages.sort_by_key(|m| {
                    std::cmp::Reverse(m["id"].as_str().unwrap().parse::<u64>().unwrap())
                });
                messages.truncate(100);
                (200, json!(messages))
            }
            ("POST", ["channels", id, "messages"]) => {
                let nonce = body["nonce"]
                    .as_str()
                    .filter(|_| body["enforce_nonce"] == true);
                if let Some(previous) = nonce.and_then(|n| self.nonces.get(n)) {
                    return (200, previous.clone());
                }
                self.next_id += 1;
                let m = message(&self.next_id.to_string(), id, &body);
                if let Some(nonce) = nonce {
                    self.nonces.insert(nonce.into(), m.clone());
                }
                self.messages
                    .entry((*id).into())
                    .or_default()
                    .push(m.clone());
                (200, m)
            }
            ("GET" | "PATCH", ["channels", id, "messages", mid]) => {
                if let Some(m) = self
                    .messages
                    .entry((*id).into())
                    .or_default()
                    .iter_mut()
                    .find(|m| m["id"] == *mid)
                {
                    if method == "PATCH" {
                        m.as_object_mut()
                            .unwrap()
                            .extend(body.as_object().unwrap().clone());
                    }
                    (200, m.clone())
                } else {
                    (404, json!({"code":10008,"message":"Unknown Message"}))
                }
            }
            ("PUT", ["channels", _, "pins", _]) => (204, Value::Null),
            _ => (
                500,
                json!({"code":0,"message":format!("Unexpected test request {method} {path}")}),
            ),
        }
    }
}

pub struct Api {
    pub state: Arc<Mutex<State>>,
    pub http: Arc<Http>,
    worker: tokio::task::JoinHandle<()>,
}

impl Api {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            channels: [
                ("8".into(), channel("8", 0, 9, "control")),
                ("9".into(), channel("9", 4, 9, "")),
                ("10".into(), channel("10", 4, 9, "")),
            ]
            .into(),
            messages: HashMap::new(),
            calls: Vec::new(),
            fail_next: None,
            fail_after_create: None,
            fail_message: None,
            created_overwrites: None,
            permissions: serenity::all::Permissions::all().bits()
                & !serenity::all::Permissions::ADMINISTRATOR.bits(),
            next_id: 1000,
            nonces: HashMap::new(),
        }));
        let shared = state.clone();
        let worker = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut first = String::new();
                    reader.read_line(&mut first).await.unwrap();
                    let parts: Vec<_> = first.split_whitespace().collect();
                    if parts.len() < 2 {
                        return;
                    }
                    let method = parts[0].to_owned();
                    let (path, query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
                    let path = path.strip_prefix("/api/v10").unwrap_or(path).to_owned();
                    let query = query.to_owned();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).await.unwrap();
                        if line == "\r\n" || line.is_empty() {
                            break;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).await.unwrap();
                    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
                    let (status, value) =
                        state.lock().unwrap().respond(&method, &path, &query, body);
                    let body = if status == 204 {
                        String::new()
                    } else {
                        value.to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = reader.get_mut().write_all(response.as_bytes()).await;
                });
            }
        });
        let http = Arc::new(
            HttpBuilder::new("test")
                .proxy(format!("http://{address}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        Self {
            state,
            http,
            worker,
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
