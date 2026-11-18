//! The local ZDR proxy. OpenCode talks to it on 127.0.0.1. It alone holds
//! the key and sets the ZDR header, so no request reaches Isoquant without
//! it. It forwards two routes only, and one allow-listed model.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::isoquant::{Upstream, ZDR_HEADER, ZDR_REQUIRED};
use crate::keychain::ApiKey;
use crate::receipt::{Outcome, Receipt, ReceiptLog};

pub type ProxyBody = UnsyncBoxBody<Bytes, io::Error>;

const MAX_BODY: usize = 64 * 1024 * 1024;

/// Client headers never copied upstream: credentials, the ZDR header (the
/// proxy sets its own), hop-by-hop headers, and ones that would identify the
/// client or change the response encoding.
const DROP_REQUEST: &[&str] = &[
    "authorization",
    "x-api-key",
    "isoquant-zdr",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "expect",
    "accept-encoding",
    "user-agent",
    "cookie",
];
/// Upstream headers never copied back: hop-by-hop ones, and the length,
/// because the body is streamed through.
const DROP_RESPONSE: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "content-length",
    "set-cookie",
];

pub struct ProxyConfig {
    pub upstream: Upstream,
    pub key: ApiKey,
    pub allowed_models: Vec<String>,
    pub receipts: Arc<ReceiptLog>,
}

/// A started proxy. Dropping it stops it.
pub struct RunningProxy {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl RunningProxy {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The OpenAI-compatible base URL OpenCode is given.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }
}

impl Drop for RunningProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Binds 127.0.0.1 on a port the OS picks and serves until dropped.
pub async fn start(config: ProxyConfig, client: reqwest::Client) -> io::Result<RunningProxy> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let proxy = Arc::new(Proxy { config, client });
    let task = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let proxy = Arc::clone(&proxy);
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let proxy = Arc::clone(&proxy);
                    async move { Ok::<_, Infallible>(proxy.handle(request).await) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(RunningProxy { addr, task })
}

struct Proxy {
    config: ProxyConfig,
    client: reqwest::Client,
}

impl Proxy {
    async fn handle(&self, request: Request<Incoming>) -> Response<ProxyBody> {
        let method = request.method().clone();
        let path = request.uri().path().to_string();
        let mut receipt = Receipt::now(method.as_str(), &path);
        let is_chat = match (&method, path.as_str()) {
            (&Method::GET, "/v1/models") => false,
            (&Method::POST, "/v1/chat/completions") => true,
            _ => return self.refuse(receipt, StatusCode::NOT_FOUND, "path-refused"),
        };

        let (parts, body) = request.into_parts();
        let body = match Limited::new(body, MAX_BODY).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return self.refuse(receipt, StatusCode::PAYLOAD_TOO_LARGE, "body-refused"),
        };
        receipt.bytes_in = body.len() as u64;
        if is_chat {
            let model = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v.get("model")?.as_str().map(str::to_string));
            receipt.model = model.clone();
            if !model.is_some_and(|m| self.config.allowed_models.contains(&m)) {
                return self.refuse(receipt, StatusCode::FORBIDDEN, "model-refused");
            }
        }

        let mut builder = self.client.request(method, self.config.upstream.url(&path));
        for (name, value) in &parts.headers {
            if !DROP_REQUEST.contains(&name.as_str()) {
                builder = builder.header(name, value);
            }
        }
        let built = builder
            .header(AUTHORIZATION, self.config.key.bearer())
            .header(ZDR_HEADER, ZDR_REQUIRED)
            .body(body)
            .build();
        let Ok(built) = built else {
            return self.refuse(receipt, StatusCode::BAD_REQUEST, "request-refused");
        };
        // The evidence is the request as built, not the intent above.
        receipt.zdr_header_sent = carries_zdr_required(built.headers());
        receipt.upstream_host = built.url().host_str().map(str::to_string);
        if !receipt.zdr_header_sent {
            receipt.upstream_host = None;
            return self.refuse(
                receipt,
                StatusCode::INTERNAL_SERVER_ERROR,
                "zdr-header-missing",
            );
        }

        let answer = match self.client.execute(built).await {
            Ok(answer) => answer,
            Err(_) => {
                receipt.outcome = Outcome::UpstreamError;
                receipt.status = StatusCode::BAD_GATEWAY.as_u16();
                self.config.receipts.append(&receipt);
                return error(StatusCode::BAD_GATEWAY, "upstream-unreachable");
            }
        };
        receipt.status = answer.status().as_u16();
        receipt.request_id = answer
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        receipt.outcome = Outcome::Forwarded;

        let mut response = Response::builder().status(answer.status());
        for (name, value) in answer.headers() {
            if !DROP_RESPONSE.contains(&name.as_str()) {
                response = response.header(name, value);
            }
        }
        let finish = Finish {
            receipts: Arc::clone(&self.config.receipts),
            receipt,
            bytes_out: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        };
        let stream = answer.bytes_stream().map(move |chunk| {
            match &chunk {
                Ok(bytes) => {
                    finish
                        .bytes_out
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
                Err(_) => finish.failed.store(true, Ordering::Relaxed),
            }
            chunk.map(Frame::data).map_err(io::Error::other)
        });
        response
            .body(StreamBody::new(stream).boxed_unsync())
            .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "upstream-headers-refused"))
    }

    fn refuse(&self, mut receipt: Receipt, status: StatusCode, code: &str) -> Response<ProxyBody> {
        receipt.outcome = Outcome::Refused;
        receipt.status = status.as_u16();
        self.config.receipts.append(&receipt);
        error(status, code)
    }
}

/// Writes the receipt of a forwarded request when its streamed answer ends:
/// fully read, failed, or abandoned by the client.
struct Finish {
    receipts: Arc<ReceiptLog>,
    receipt: Receipt,
    bytes_out: AtomicU64,
    failed: AtomicBool,
}

impl Drop for Finish {
    fn drop(&mut self) {
        self.receipt.bytes_out = self.bytes_out.load(Ordering::Relaxed);
        if self.failed.load(Ordering::Relaxed) {
            self.receipt.outcome = Outcome::UpstreamError;
        }
        self.receipts.append(&self.receipt);
    }
}

fn carries_zdr_required(headers: &reqwest::header::HeaderMap) -> bool {
    let name = HeaderName::from_static("isoquant-zdr");
    let values: Vec<_> = headers.get_all(name).iter().collect();
    values.len() == 1 && values[0] == ZDR_REQUIRED
}

fn error(status: StatusCode, code: &str) -> Response<ProxyBody> {
    let body =
        json!({"error": {"code": format!("opencode-iq-{code}"), "type": "opencode_iq_proxy"}});
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(
            Full::new(Bytes::from(body.to_string()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .expect("a static error response always builds")
}
