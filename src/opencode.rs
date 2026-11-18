//! The OpenCode side: the session config that names only the proxy, and the
//! child process.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::isoquant::{MODEL, ZDR_HEADER, ZDR_REQUIRED};

/// Variables OpenCode must not see: provider keys it could use to reach
/// another host or print. `OPENCODE_CONFIG_CONTENT` is ours to set.
const SCRUBBED_ENV: &[&str] = &[
    "ISOQUANT_API_KEY",
    "FIREWORKS_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENROUTER_API_KEY",
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
];

/// The session config. OpenCode applies `OPENCODE_CONFIG_CONTENT` after
/// every config file (global and project), so a project's `opencode.json`
/// cannot switch the session to another provider. The small model, which
/// OpenCode uses for titles, is pinned too, so no prompt text goes to a
/// default provider. The file holds no key.
pub fn session_config(proxy_base_url: &str) -> Value {
    let model = format!("isoquant/{MODEL}");
    json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model,
        "small_model": model,
        "enabled_providers": ["isoquant"],
        "share": "disabled",
        "autoupdate": false,
        "provider": {
            "isoquant": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Isoquant (ZDR required)",
                "options": {
                    "baseURL": proxy_base_url,
                    "apiKey": "via-opencode-iq-proxy",
                    "headers": { ZDR_HEADER: ZDR_REQUIRED },
                },
                "models": {
                    MODEL: {
                        "name": "DeepSeek V4.1 Flash 1017 (Isoquant ZDR)",
                        "limit": { "context": 1_000_000, "output": 32_000 },
                    },
                },
            },
        },
    })
}

/// The OpenCode binary: `OPENCODE_BIN`, else the installer's path, else `opencode` on PATH.
pub fn binary() -> PathBuf {
    if let Some(bin) = std::env::var_os("OPENCODE_BIN") {
        return PathBuf::from(bin);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let installed = Path::new(&home).join(".opencode/bin/opencode");
        if installed.is_file() {
            return installed;
        }
    }
    PathBuf::from("opencode")
}

pub fn command(bin: &Path, proxy_base_url: &str, args: &[OsString]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(bin);
    command.args(args).env(
        "OPENCODE_CONFIG_CONTENT",
        session_config(proxy_base_url).to_string(),
    );
    for name in SCRUBBED_ENV {
        command.env_remove(name);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_names_only_the_proxy_and_the_one_model() {
        let config = session_config("http://127.0.0.1:4242/v1");
        assert_eq!(config["enabled_providers"], json!(["isoquant"]));
        assert_eq!(config["model"], json!(format!("isoquant/{MODEL}")));
        assert_eq!(config["small_model"], config["model"]);
        let options = &config["provider"]["isoquant"]["options"];
        assert_eq!(options["baseURL"], json!("http://127.0.0.1:4242/v1"));
        assert_eq!(options["apiKey"], json!("via-opencode-iq-proxy"));
        let models = config["provider"]["isoquant"]["models"]
            .as_object()
            .unwrap();
        assert_eq!(models.keys().collect::<Vec<_>>(), vec![MODEL]);
    }
}
