//! Receipts: one JSON line per request the proxy handled. A row holds no
//! body, no key and no header value other than the fields below.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Sent upstream; the upstream answered (any status).
    Forwarded,
    /// Not sent upstream: path, model or body refused by the proxy.
    Refused,
    /// Sent upstream, but the connection or the streamed answer failed.
    UpstreamError,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub ts: String,
    pub method: String,
    pub path: String,
    pub model: Option<String>,
    /// The host the request was built for; None when nothing was sent.
    pub upstream_host: Option<String>,
    /// Read from the built request: exactly one ZDR header, value "required".
    pub zdr_header_sent: bool,
    pub status: u16,
    pub request_id: Option<String>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub outcome: Outcome,
}

impl Receipt {
    pub fn now(method: &str, path: &str) -> Self {
        Self {
            ts: humantime::format_rfc3339_millis(SystemTime::now()).to_string(),
            method: method.to_string(),
            path: path.to_string(),
            model: None,
            upstream_host: None,
            zdr_header_sent: false,
            status: 0,
            request_id: None,
            bytes_in: 0,
            bytes_out: 0,
            outcome: Outcome::Refused,
        }
    }
}

/// The receipt file of one run: created owner-only, appended one line at a time.
pub struct ReceiptLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl ReceiptLog {
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("could not create the receipt file {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A lost row would weaken the end-of-run check, so a failed write is
    /// reported on stderr; the request itself is not failed for it.
    pub fn append(&self, receipt: &Receipt) {
        let mut line = serde_json::to_string(receipt).expect("a receipt always serializes");
        line.push('\n');
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(error) = file.write_all(line.as_bytes()) {
            eprintln!(
                "opencode-iq: could not write a receipt to {}: {error}",
                self.path.display()
            );
        }
    }
}

/// What a run's receipts show.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub forwarded_chats: usize,
    /// Rows sent upstream without the ZDR header, or to another host.
    pub without_zdr: usize,
    pub refused: usize,
    pub upstream_errors: usize,
}

impl Summary {
    pub fn read(path: &Path, expected_host: &str) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("could not read {}", path.display()))?;
        let mut summary = Self::default();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let row: Receipt =
                serde_json::from_str(&line).context("a receipt row is not readable")?;
            match row.outcome {
                Outcome::Refused => summary.refused += 1,
                Outcome::UpstreamError => summary.upstream_errors += 1,
                Outcome::Forwarded if row.path == "/v1/chat/completions" => {
                    summary.forwarded_chats += 1
                }
                Outcome::Forwarded => {}
            }
            let sent = row.outcome != Outcome::Refused;
            if sent && (!row.zdr_header_sent || row.upstream_host.as_deref() != Some(expected_host))
            {
                summary.without_zdr += 1;
            }
        }
        Ok(summary)
    }

    /// The ZDR assertion: nothing left this machine without the header.
    pub fn holds(&self) -> bool {
        self.without_zdr == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, outcome: Outcome, zdr: bool, host: Option<&str>) -> Receipt {
        Receipt {
            outcome,
            zdr_header_sent: zdr,
            upstream_host: host.map(str::to_string),
            ..Receipt::now("POST", path)
        }
    }

    #[test]
    fn the_summary_counts_each_outcome_and_any_row_without_zdr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let log = ReceiptLog::create(&path).unwrap();
        let chat = "/v1/chat/completions";
        log.append(&row(
            chat,
            Outcome::Forwarded,
            true,
            Some("api.isoquant.ai"),
        ));
        log.append(&row(
            chat,
            Outcome::Forwarded,
            true,
            Some("api.isoquant.ai"),
        ));
        log.append(&row(
            "/v1/models",
            Outcome::Forwarded,
            true,
            Some("api.isoquant.ai"),
        ));
        log.append(&row("/v1/embeddings", Outcome::Refused, false, None));
        log.append(&row(
            chat,
            Outcome::UpstreamError,
            true,
            Some("api.isoquant.ai"),
        ));
        let summary = Summary::read(&path, "api.isoquant.ai").unwrap();
        assert_eq!(
            summary,
            Summary {
                forwarded_chats: 2,
                without_zdr: 0,
                refused: 1,
                upstream_errors: 1
            }
        );
        assert!(summary.holds());

        log.append(&row(
            chat,
            Outcome::Forwarded,
            false,
            Some("api.isoquant.ai"),
        ));
        log.append(&row(
            chat,
            Outcome::Forwarded,
            true,
            Some("elsewhere.example"),
        ));
        let summary = Summary::read(&path, "api.isoquant.ai").unwrap();
        assert_eq!(summary.without_zdr, 2);
        assert!(!summary.holds());
    }

    #[test]
    fn the_receipt_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        ReceiptLog::create(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
