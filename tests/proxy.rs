//! The proxy and the ZDR proof against a local fake of the Isoquant API that
//! records exactly what reached it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::stream;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::HeaderMap;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use opencode_iq::isoquant::{self, ACCEPTED_POLICY, MODEL, ProofFailure, Upstream};
use opencode_iq::keychain::ApiKey;
use opencode_iq::proxy::{self, ProxyConfig, RunningProxy};
use opencode_iq::receipt::{Outcome, Receipt, ReceiptLog, Summary};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const KEY: &str = "sk-test-key-never-in-receipts";

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: HeaderMap,
}

/// How the fake answers one request: status, content type, body chunks.
type Answer = (u16, &'static str, Vec<String>);

struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn upstream(&self) -> Upstream {
        Upstream::at(&format!("http://{}", self.addr))
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn fake(answer: fn(&Seen) -> Answer) -> Fake {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let log = Arc::clone(&log);
                    async move {
                        let request_seen = Seen {
                            method: request.method().to_string(),
                            path: request.uri().path().to_string(),
                            headers: request.headers().clone(),
                        };
                        let _ = request.into_body().collect().await;
                        let (status, content_type, chunks) = answer(&request_seen);
                        log.lock().unwrap().push(request_seen);
                        let frames = chunks
                            .into_iter()
                            .map(|c| Ok::<_, Infallible>(Frame::data(Bytes::from(c))));
                        let body = StreamBody::new(stream::iter(frames));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", content_type)
                                .header("x-request-id", "iqr_test")
                                .body(body)
                                .unwrap(),
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    Fake { addr, seen }
}

fn sse(_: &Seen) -> Answer {
    (
        200,
        "text/event-stream",
        vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"rea\"}}]}\n\n".into(),
            "data: {\"choices\":[{\"delta\":{\"content\":\"dy\"}}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ],
    )
}

async fn start_proxy(fake: &Fake) -> (RunningProxy, Arc<ReceiptLog>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let receipts = Arc::new(ReceiptLog::create(&dir.path().join("r.jsonl")).unwrap());
    let running = proxy::start(
        ProxyConfig {
            upstream: fake.upstream(),
            key: ApiKey::new(KEY).unwrap(),
            allowed_models: vec![MODEL.to_string()],
            receipts: Arc::clone(&receipts),
        },
        isoquant::http_client().unwrap(),
    )
    .await
    .unwrap();
    (running, receipts, dir)
}

fn rows(receipts: &ReceiptLog) -> Vec<Receipt> {
    std::fs::read_to_string(receipts.path())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn chat(model: &str) -> String {
    json!({"model": model, "stream": true, "messages": [{"role": "user", "content": "hi"}]})
        .to_string()
}

#[tokio::test]
async fn a_chat_reaches_isoquant_with_our_key_and_zdr_required_and_streams_back_unchanged() {
    let fake = fake(sse).await;
    let (running, receipts, _dir) = start_proxy(&fake).await;

    let answer = reqwest::Client::new()
        .post(format!("{}/chat/completions", running.base_url()))
        .header("authorization", "Bearer client-junk")
        .header("isoquant-zdr", "none")
        .header("x-api-key", "client-junk")
        .header("content-type", "application/json")
        .body(chat(MODEL))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.headers()["content-type"], "text/event-stream");
    let text = answer.text().await.unwrap();
    assert_eq!(text, sse(&fake.seen()[0]).2.concat());
    drop(running);

    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("POST", "/v1/chat/completions")
    );
    let values = |name: &str| -> Vec<String> {
        seen[0]
            .headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(values("isoquant-zdr"), vec!["required"]);
    assert_eq!(values("authorization"), vec![format!("Bearer {KEY}")]);
    assert!(values("x-api-key").is_empty());
    assert_eq!(values("user-agent"), vec!["generic-client"]);

    let rows = rows(&receipts);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.outcome, Outcome::Forwarded);
    assert!(row.zdr_header_sent);
    assert_eq!(row.upstream_host.as_deref(), Some("127.0.0.1"));
    assert_eq!(row.model.as_deref(), Some(MODEL));
    assert_eq!(row.status, 200);
    assert_eq!(row.request_id.as_deref(), Some("iqr_test"));
    assert_eq!(row.bytes_out, text.len() as u64);
    let raw = std::fs::read_to_string(receipts.path()).unwrap();
    assert!(!raw.contains(KEY) && !raw.contains("client-junk") && !raw.contains("\"hi\""));
}

#[tokio::test]
async fn another_model_is_refused_without_reaching_isoquant() {
    let fake = fake(sse).await;
    let (running, receipts, _dir) = start_proxy(&fake).await;
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", running.base_url());
    for body in [
        chat("glm-5.3-flash"),
        json!({"messages": []}).to_string(),
        "not json".into(),
    ] {
        let answer = client.post(&url).body(body).send().await.unwrap();
        assert_eq!(answer.status(), 403);
        let error: Value = answer.json().await.unwrap();
        assert_eq!(error["error"]["code"], "opencode-iq-model-refused");
    }
    assert!(fake.seen().is_empty());
    let rows = rows(&receipts);
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|r| r.outcome == Outcome::Refused && r.upstream_host.is_none())
    );
}

#[tokio::test]
async fn other_routes_are_refused_without_reaching_isoquant() {
    let fake = fake(sse).await;
    let (running, receipts, _dir) = start_proxy(&fake).await;
    let client = reqwest::Client::new();
    let base = running.base_url();
    let refused = [
        client.post(format!("{base}/embeddings")).body(chat(MODEL)),
        client.get(format!("{base}/chat/completions")),
        client.post(format!("{base}/models")),
        client.get(format!("{base}/../admin")),
    ];
    for request in refused {
        assert_eq!(request.send().await.unwrap().status(), 404);
    }
    assert!(fake.seen().is_empty());
    assert_eq!(rows(&receipts).len(), 4);
}

#[tokio::test]
async fn the_model_list_is_forwarded_with_zdr_required() {
    let fake = fake(|_| (200, "application/json", vec!["{\"data\":[]}".into()])).await;
    let (running, receipts, _dir) = start_proxy(&fake).await;
    let answer = reqwest::get(format!("{}/models", running.base_url()))
        .await
        .unwrap();
    assert_eq!(answer.text().await.unwrap(), "{\"data\":[]}");
    drop(running);
    assert_eq!(fake.seen()[0].headers["isoquant-zdr"], "required");
    let summary = Summary::read(receipts.path(), "127.0.0.1").unwrap();
    assert!(summary.holds());
    assert_eq!(summary.forwarded_chats, 0);
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_receipt_not_a_hang() {
    let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let dir = tempfile::tempdir().unwrap();
    let receipts = Arc::new(ReceiptLog::create(&dir.path().join("r.jsonl")).unwrap());
    let running = proxy::start(
        ProxyConfig {
            upstream: Upstream::at(&format!("http://{addr}")),
            key: ApiKey::new(KEY).unwrap(),
            allowed_models: vec![MODEL.to_string()],
            receipts: Arc::clone(&receipts),
        },
        isoquant::http_client().unwrap(),
    )
    .await
    .unwrap();
    let answer = reqwest::Client::new()
        .post(format!("{}/chat/completions", running.base_url()))
        .body(chat(MODEL))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 502);
    let rows = rows(&receipts);
    assert_eq!(rows[0].outcome, Outcome::UpstreamError);
    assert!(rows[0].zdr_header_sent);
}

// The ZDR proof.

fn listing(policy: &str, zdr: bool) -> String {
    json!({"data": [{
        "id": MODEL,
        "zdr_supported": zdr,
        "isoquant": {"privacy": {"policy_version": policy, "zdr_supported": zdr}},
    }]})
    .to_string()
}

fn enforcing_gateway(seen: &Seen) -> Answer {
    if seen.path == "/v1/models" {
        return (
            200,
            "application/json",
            vec![listing(ACCEPTED_POLICY, true)],
        );
    }
    match seen
        .headers
        .get("isoquant-zdr")
        .and_then(|v| v.to_str().ok())
    {
        Some("required") => (200, "application/json", vec!["{}".into()]),
        _ => (
            400,
            "application/json",
            vec![json!({"error": {"code": "invalid_zdr_requirement"}}).to_string()],
        ),
    }
}

async fn prove(answer: fn(&Seen) -> Answer) -> (Result<(), ProofFailure>, Vec<Seen>) {
    let fake = fake(answer).await;
    let result = isoquant::prove_zdr(
        &isoquant::http_client().unwrap(),
        &fake.upstream(),
        &ApiKey::new(KEY).unwrap(),
    )
    .await;
    (result, fake.seen())
}

#[tokio::test]
async fn zdr_is_proven_when_the_list_attests_it_and_the_gateway_refuses_a_bad_header() {
    let (result, seen) = prove(enforcing_gateway).await;
    assert_eq!(result, Ok(()));
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].headers["isoquant-zdr"], "required");
    assert_ne!(seen[1].headers["isoquant-zdr"], "required");
}

#[tokio::test]
async fn a_gateway_that_accepts_any_header_value_is_not_proven() {
    let (result, _) = prove(|seen| match seen.path.as_str() {
        "/v1/models" => (
            200,
            "application/json",
            vec![listing(ACCEPTED_POLICY, true)],
        ),
        _ => (200, "application/json", vec!["{}".into()]),
    })
    .await;
    assert_eq!(
        result,
        Err(ProofFailure::HeaderNotEnforced {
            status: 200,
            code: None
        })
    );
}

#[tokio::test]
async fn a_new_privacy_policy_stops_the_run() {
    let (result, seen) =
        prove(|_| (200, "application/json", vec![listing("policy-2027", true)])).await;
    assert_eq!(
        result,
        Err(ProofFailure::PolicyChanged {
            listed: Some("policy-2027".into())
        })
    );
    assert_eq!(seen.len(), 1, "no probe is sent once the listing fails");
}

#[tokio::test]
async fn a_model_without_zdr_or_missing_from_the_list_is_not_proven() {
    let (result, _) = prove(|_| {
        (
            200,
            "application/json",
            vec![listing(ACCEPTED_POLICY, false)],
        )
    })
    .await;
    assert_eq!(result, Err(ProofFailure::ZdrUnsupported));
    let (result, _) = prove(|_| (200, "application/json", vec!["{\"data\":[]}".into()])).await;
    assert_eq!(result, Err(ProofFailure::ModelUnlisted));
    let (result, _) = prove(|_| (401, "application/json", vec!["{}".into()])).await;
    assert_eq!(result, Err(ProofFailure::ModelsStatus(401)));
}
