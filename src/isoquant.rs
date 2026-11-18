//! The Isoquant API: where it is, the one model this tool allows, and the
//! proof that ZDR holds before any prompt is sent.

use std::time::Duration;

use serde_json::{Value, json};

use crate::keychain::ApiKey;

pub const ZDR_HEADER: &str = "Isoquant-ZDR";
pub const ZDR_REQUIRED: &str = "required";
pub const MODEL: &str = "deepseek-v4.1-flash-1017-fast";
/// The privacy policy version we read and accepted for [`MODEL`]. When
/// Isoquant lists another version, every run stops until someone reads the
/// new policy and changes this value.
pub const ACCEPTED_POLICY: &str = "deepseek-1017-required-zdr-20261024";

/// A header value the gateway must refuse. If it does not, the gateway does
/// not read the header, and "required" would prove nothing.
const PROBE_VALUE: &str = "opencode-iq-invalid";
const PROBE_REFUSAL: &str = "invalid_zdr_requirement";
const PROOF_TIMEOUT: Duration = Duration::from_secs(30);

/// Where requests go. The binary always uses [`Upstream::isoquant`]; tests
/// point it at a local fake. No flag or environment variable changes it.
#[derive(Clone, Debug)]
pub struct Upstream {
    base: String,
    host: String,
}

impl Upstream {
    pub fn isoquant() -> Self {
        Self::at("https://api.isoquant.ai")
    }

    pub fn at(base: &str) -> Self {
        let base = base.trim_end_matches('/').to_string();
        let host = base
            .split_once("://")
            .map_or(base.as_str(), |(_, rest)| rest)
            .split(['/', ':'])
            .next()
            .unwrap_or_default()
            .to_string();
        Self { base, host }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

/// The one HTTP client for proof and proxy. A generic User-Agent, no system
/// proxy, and no overall timeout, because a streamed answer can run for
/// minutes; a stalled read still ends.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("generic-client")
        .no_proxy()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(900))
        .build()
}

/// Why ZDR is not proven. Each message says what to check.
#[derive(Debug, PartialEq, Eq)]
pub enum ProofFailure {
    ModelsUnreachable,
    ModelsStatus(u16),
    ModelsUnreadable,
    ModelUnlisted,
    ZdrUnsupported,
    PolicyChanged { listed: Option<String> },
    ProbeUnreachable,
    HeaderNotEnforced { status: u16, code: Option<String> },
}

impl std::fmt::Display for ProofFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelsUnreachable => write!(f, "the model list could not be reached"),
            Self::ModelsStatus(status) => {
                write!(f, "the model list answered HTTP {status} (check the key)")
            }
            Self::ModelsUnreadable => write!(f, "the model list was not JSON"),
            Self::ModelUnlisted => write!(f, "{MODEL} is not in the model list"),
            Self::ZdrUnsupported => write!(f, "{MODEL} is not listed as zdr_supported"),
            Self::PolicyChanged { listed } => write!(
                f,
                "{MODEL} lists privacy policy {}, not the accepted {ACCEPTED_POLICY}; read the new policy before changing ACCEPTED_POLICY",
                listed.as_deref().unwrap_or("(none)")
            ),
            Self::ProbeUnreachable => write!(f, "the header probe could not be reached"),
            Self::HeaderNotEnforced { status, code } => write!(
                f,
                "the gateway did not refuse an invalid {ZDR_HEADER} value (HTTP {status}, code {}), so it may not enforce the header",
                code.as_deref().unwrap_or("none")
            ),
        }
    }
}

/// Proves ZDR for [`MODEL`]:
/// 1. the model list shows the model with `zdr_supported: true` and the
///    accepted privacy policy version;
/// 2. a chat request with an invalid ZDR value is refused with HTTP 400
///    `invalid_zdr_requirement`, so the gateway reads the header.
///
/// The probe is refused before any model runs, so it costs nothing.
pub async fn prove_zdr(
    client: &reqwest::Client,
    upstream: &Upstream,
    key: &ApiKey,
) -> Result<(), ProofFailure> {
    let listed = client
        .get(upstream.url("/v1/models"))
        .header(reqwest::header::AUTHORIZATION, key.bearer())
        .header(ZDR_HEADER, ZDR_REQUIRED)
        .timeout(PROOF_TIMEOUT)
        .send()
        .await
        .map_err(|_| ProofFailure::ModelsUnreachable)?;
    if listed.status() != reqwest::StatusCode::OK {
        return Err(ProofFailure::ModelsStatus(listed.status().as_u16()));
    }
    let listed: Value = listed
        .json()
        .await
        .map_err(|_| ProofFailure::ModelsUnreadable)?;
    let entry = listed
        .get("data")
        .and_then(Value::as_array)
        .and_then(|models| models.iter().find(|m| m.get("id") == Some(&json!(MODEL))))
        .ok_or(ProofFailure::ModelUnlisted)?;
    let privacy = entry.pointer("/isoquant/privacy");
    let supported = entry.get("zdr_supported") == Some(&Value::Bool(true))
        && privacy.and_then(|p| p.get("zdr_supported")) == Some(&Value::Bool(true));
    if !supported {
        return Err(ProofFailure::ZdrUnsupported);
    }
    let policy = privacy
        .and_then(|p| p.get("policy_version"))
        .and_then(Value::as_str);
    if policy != Some(ACCEPTED_POLICY) {
        return Err(ProofFailure::PolicyChanged {
            listed: policy.map(str::to_string),
        });
    }

    let probe = client
        .post(upstream.url("/v1/chat/completions"))
        .header(reqwest::header::AUTHORIZATION, key.bearer())
        .header(ZDR_HEADER, PROBE_VALUE)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(
            json!({
                "model": MODEL,
                "messages": [{"role": "user", "content": "zdr header check"}],
                "max_tokens": 1,
            })
            .to_string(),
        )
        .timeout(PROOF_TIMEOUT)
        .send()
        .await
        .map_err(|_| ProofFailure::ProbeUnreachable)?;
    let status = probe.status().as_u16();
    let code = probe.json::<Value>().await.ok().and_then(|body| {
        body.pointer("/error/code")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    if status != 400 || code.as_deref() != Some(PROBE_REFUSAL) {
        return Err(ProofFailure::HeaderNotEnforced { status, code });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upstream_host_drops_scheme_port_and_path() {
        assert_eq!(Upstream::isoquant().host(), "api.isoquant.ai");
        assert_eq!(Upstream::at("http://127.0.0.1:4000/").host(), "127.0.0.1");
        assert_eq!(
            Upstream::at("http://127.0.0.1:4000/").url("/v1/models"),
            "http://127.0.0.1:4000/v1/models"
        );
    }
}
