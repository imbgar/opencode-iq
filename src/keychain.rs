//! The Isoquant key, read from the macOS Keychain.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// The Keychain item that holds the Isoquant key for local development.
pub const SERVICE: &str = "mina.isoquant.dev.key";
pub const ACCOUNT: &str = "mina";

/// An API key. It never prints: `Debug` is redacted and there is no `Display`.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into().trim().to_string();
        if value.is_empty() {
            bail!("the key is empty");
        }
        Ok(Self(value))
    }

    pub(crate) fn bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Reads the key with `security`. The key comes back on its stdout, never on
/// any command line.
pub fn read_key(service: &str, account: &str) -> Result<ApiKey> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .context("could not run /usr/bin/security")?;
    if !out.status.success() {
        bail!("no Keychain item {service} (account {account})");
    }
    let text = String::from_utf8(out.stdout).context("the Keychain item is not UTF-8")?;
    ApiKey::new(text).with_context(|| format!("Keychain item {service}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_never_shows_in_debug_output() {
        let key = ApiKey::new("sk-very-secret\n").unwrap();
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert_eq!(key.bearer(), "Bearer sk-very-secret");
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(ApiKey::new("  \n").is_err());
    }
}
