use super::protocol::{Event, RpcId};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};
use tokio_util::{
    codec::{FramedRead, FramedWrite, LinesCodec},
    sync::CancellationToken,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum RpcError {
    #[error("Codex rejected the request (code {code}): {message}")]
    Remote { code: i64, message: String },
    #[error("Codex disconnected or timed out; the operation's outcome may be unknown")]
    Uncertain,
    #[error("Unexpected Codex protocol response")]
    Protocol,
}

type Pending = Arc<Mutex<HashMap<RpcId, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Clone)]
pub struct Client {
    tx: mpsc::Sender<Value>,
    pending: Pending,
    next: Arc<AtomicI64>,
    closed: CancellationToken,
    timeout: Duration,
}

impl Client {
    pub fn connect<R, W>(reader: R, writer: W, timeout: Duration) -> (Self, mpsc::Receiver<Event>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<Value>(128);
        let (events_tx, events) = mpsc::channel(512);
        let closed = CancellationToken::new();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let client = Self {
            tx,
            pending: pending.clone(),
            next: Arc::new(AtomicI64::new(1)),
            closed: closed.clone(),
            timeout,
        };
        let write_closed = closed.clone();
        tokio::spawn(async move {
            let mut writer =
                FramedWrite::new(writer, LinesCodec::new_with_max_length(4 * 1024 * 1024));
            loop {
                tokio::select! {
                    _ = write_closed.cancelled() => break,
                    frame = rx.recv() => {
                        let Some(frame) = frame else { break; };
                        if writer.send(frame.to_string()).await.is_err() { break; }
                    }
                }
            }
            write_closed.cancel();
        });
        tokio::spawn(async move {
            let mut reader =
                FramedRead::new(reader, LinesCodec::new_with_max_length(4 * 1024 * 1024));
            loop {
                let line = tokio::select! { _ = closed.cancelled() => break, line = reader.next() => line };
                let Some(Ok(line)) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    break;
                };
                if let Some(method) = value.get("method").and_then(Value::as_str) {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let event = if let Some(id) = value.get("id") {
                        let Ok(id) = serde_json::from_value(id.clone()) else {
                            break;
                        };
                        Event::Request {
                            id,
                            method: method.into(),
                            params,
                        }
                    } else {
                        // Full completed items carry authoritative text. Token/output deltas need no storage.
                        if !matches!(
                            method,
                            "turn/started"
                                | "turn/completed"
                                | "turn/plan/updated"
                                | "item/started"
                                | "item/completed"
                                | "serverRequest/resolved"
                                | "error"
                                | "warning"
                                | "configWarning"
                        ) {
                            continue;
                        }
                        Event::Notification {
                            method: method.into(),
                            params,
                        }
                    };
                    if events_tx.try_send(event).is_err() {
                        tracing::error!(
                            "Codex event consumer unavailable or overloaded; stopping the connection"
                        );
                        break;
                    }
                } else if let Some(id) = value.get("id") {
                    let Ok(id) = serde_json::from_value::<RpcId>(id.clone()) else {
                        break;
                    };
                    if let Some(reply) = pending.lock().unwrap().remove(&id) {
                        let result = if let Some(error) = value.get("error") {
                            Err(RpcError::Remote {
                                code: error["code"].as_i64().unwrap_or(-32603),
                                message: error["message"]
                                    .as_str()
                                    .unwrap_or("Request failed")
                                    .into(),
                            })
                        } else {
                            value.get("result").cloned().ok_or(RpcError::Protocol)
                        };
                        let _ = reply.send(result);
                    }
                } else {
                    break;
                }
            }
            closed.cancel();
            for (_, reply) in pending.lock().unwrap().drain() {
                let _ = reply.send(Err(RpcError::Uncertain));
            }
        });
        (client, events)
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        if self.closed.is_cancelled() {
            return Err(RpcError::Uncertain);
        }
        let id = RpcId::Number(self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let operation = async {
            self.tx
                .send(json!({"id":id,"method":method,"params":params}))
                .await
                .map_err(|_| RpcError::Uncertain)?;
            rx.await.map_err(|_| RpcError::Uncertain)?
        };
        let result = tokio::time::timeout(self.timeout, operation)
            .await
            .unwrap_or(Err(RpcError::Uncertain));
        self.pending.lock().unwrap().remove(&id);
        serde_json::from_value(result?).map_err(|_| RpcError::Protocol)
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.send(json!({"method":method,"params":params})).await
    }

    pub async fn respond(&self, id: &RpcId, result: Value) -> Result<(), RpcError> {
        self.send(json!({"id":id,"result":result})).await
    }

    pub async fn unsupported(&self, id: &RpcId) -> Result<(), RpcError> {
        self.send(
            json!({"id":id,"error":{"code":-32601,"message":"Unsupported interactive request"}}),
        )
        .await
    }

    async fn send(&self, value: Value) -> Result<(), RpcError> {
        if self.closed.is_cancelled() {
            return Err(RpcError::Uncertain);
        }
        tokio::time::timeout(self.timeout, self.tx.send(value))
            .await
            .map_err(|_| RpcError::Uncertain)?
            .map_err(|_| RpcError::Uncertain)
    }

    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }
    pub fn disconnect(&self) {
        self.closed.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn correlates_out_of_order_responses_and_server_requests() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = tokio::io::split(a);
        let (client, mut events) = Client::connect(ar, aw, Duration::from_secs(1));
        let c1 = client.clone();
        let c2 = client.clone();
        let a = tokio::spawn(async move { c1.request::<Value>("one", json!({})).await.unwrap() });
        let bcall =
            tokio::spawn(async move { c2.request::<Value>("two", json!({})).await.unwrap() });
        let (br, mut bw) = tokio::io::split(b);
        let mut lines = BufReader::new(br).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        for request in [&second, &first] {
            bw.write_all(
                format!(
                    "{}\n",
                    json!({"id":request["id"],"result":request["method"]})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        }
        bw.write_all(b"{\"id\":1,\"method\":\"item/fileChange/requestApproval\",\"params\":{\"threadId\":\"other\"}}\n").await.unwrap();
        assert_eq!(a.await.unwrap(), "one");
        assert_eq!(bcall.await.unwrap(), "two");
        let Event::Request { id, params, .. } = events.recv().await.unwrap() else {
            panic!()
        };
        assert_eq!(params["threadId"], "other");
        client
            .respond(&id, json!({"decision":"decline"}))
            .await
            .unwrap();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(response, json!({"id":1,"result":{"decision":"decline"}}));
        client.disconnect();
    }

    #[tokio::test]
    async fn disconnect_and_timeout_never_replay_requests() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = tokio::io::split(a);
        let (client, _events) = Client::connect(ar, aw, Duration::from_millis(20));
        assert!(matches!(
            client.request::<Value>("turn/start", json!({})).await,
            Err(RpcError::Uncertain)
        ));
        assert!(client.pending.lock().unwrap().is_empty());
        drop(b);
        tokio::time::timeout(Duration::from_secs(1), client.closed())
            .await
            .unwrap();
        assert!(matches!(
            client.request::<Value>("turn/start", json!({})).await,
            Err(RpcError::Uncertain)
        ));
    }
}
