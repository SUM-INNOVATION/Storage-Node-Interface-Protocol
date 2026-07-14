//! Test-only in-process JSON-RPC mock server.
//!
//! Records every request it receives and serves a canned response per
//! method, so contract / behavior tests can drive [`crate::rpc_client::L1RpcClient`]
//! against a deterministic chain stand-in without a live node.
//!
//! Each response sets `Connection: close`, so the `reqwest` client opens
//! a fresh TCP connection per JSON-RPC call. That keeps request
//! accounting exact — the server accepts exactly one request per
//! connection, so `method_count` reflects the true number of calls the
//! client made (e.g. proving "the finalized height is read exactly once
//! per operation").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How the mock should answer a given JSON-RPC method.
#[derive(Clone)]
pub enum MockResponse {
    /// Reply `{"result": <value>}` — a successful JSON-RPC response.
    Result(Value),
    /// Reply `{"error": {"code", "message"}}` — a JSON-RPC error.
    Error { code: i64, message: String },
    /// Accept the connection then close it without replying — simulates a
    /// transport-layer failure (the client's HTTP request never gets a
    /// response).
    Hangup,
}

impl MockResponse {
    /// Convenience: a JSON-RPC error with the given message.
    pub fn error(message: &str) -> Self {
        MockResponse::Error {
            code: -32000,
            message: message.to_string(),
        }
    }
}

/// Handle to a running mock RPC server. Drop it to leave the background
/// task running until the test process exits (tests are short-lived, so
/// no explicit shutdown is needed).
pub struct MockRpcServer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl MockRpcServer {
    /// URL the [`crate::rpc_client::L1RpcClient`] should target.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// All JSON-RPC request bodies received so far, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// Number of requests whose `method` field equals `method`.
    pub fn method_count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.get("method").and_then(|m| m.as_str()) == Some(method))
            .count()
    }

    /// The `params` value of the first request with the given method.
    pub fn first_params(&self, method: &str) -> Option<Value> {
        self.requests()
            .into_iter()
            .find(|r| r.get("method").and_then(|m| m.as_str()) == Some(method))
            .and_then(|r| r.get("params").cloned())
    }
}

/// Start a mock RPC server that answers each method per `routes`.
///
/// Unknown methods get a generic JSON-RPC "method not found" error. The
/// server loops, accepting one request per connection.
pub async fn start_mock_rpc(routes: HashMap<String, MockResponse>) -> MockRpcServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_task = Arc::clone(&requests);
    let routes = Arc::new(routes);

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };

            // Read the full HTTP request (headers + body) so the JSON-RPC
            // method can be parsed reliably even when reqwest writes the
            // headers and body in separate TCP segments.
            let req = read_request(&mut sock).await.unwrap_or(Value::Null);
            let method = req
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            requests_task.lock().unwrap().push(req);

            let response = routes
                .get(&method)
                .cloned()
                .unwrap_or_else(|| MockResponse::Error {
                    code: -32601,
                    message: format!("method not found: {method}"),
                });

            match response {
                MockResponse::Hangup => {
                    // Drop the socket without replying → transport failure.
                    let _ = sock.shutdown().await;
                }
                MockResponse::Result(v) => {
                    let out = json!({"jsonrpc": "2.0", "id": 1, "result": v}).to_string();
                    write_http(&mut sock, &out).await;
                }
                MockResponse::Error { code, message } => {
                    let out = json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": {"code": code, "message": message},
                    })
                    .to_string();
                    write_http(&mut sock, &out).await;
                }
            }
        }
    });

    MockRpcServer { url, requests }
}

/// Convenience: build a `routes` map from `(method, response)` pairs.
pub fn routes<const N: usize>(pairs: [(&str, MockResponse); N]) -> HashMap<String, MockResponse> {
    pairs.into_iter().map(|(m, r)| (m.to_string(), r)).collect()
}

/// Read a complete HTTP request from `sock` and return its JSON body.
/// Reads until the header terminator is seen, then until `Content-Length`
/// bytes of body have arrived — robust to segmented writes.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Value> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);

        let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") else {
            continue; // headers not complete yet
        };
        let header_str = String::from_utf8_lossy(&buf[..header_end]);
        let content_length = header_str
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().ok())
            })
            .flatten()
            .unwrap_or(0);
        let body_start = header_end + 4;
        if buf.len() >= body_start + content_length {
            return serde_json::from_slice(&buf[body_start..body_start + content_length]).ok();
        }
    }
}

/// First index of `needle` within `haystack`, if present.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn write_http(sock: &mut tokio::net::TcpStream, body: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.flush().await;
}
