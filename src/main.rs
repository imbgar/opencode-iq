//! `opencode-iq [opencode arguments]`: OpenCode on Isoquant with ZDR forced.
//!
//! Exit codes: OpenCode's own on a clean run; 1 setup failed; 70 ZDR not
//! proven (OpenCode not started); 71 a receipt shows a request without ZDR.

use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use opencode_iq::isoquant::{self, MODEL, Upstream};
use opencode_iq::keychain::{self, ACCOUNT, SERVICE};
use opencode_iq::opencode;
use opencode_iq::proxy::{self, ProxyConfig};
use opencode_iq::receipt::{ReceiptLog, Summary};
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("opencode-iq: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let key = keychain::read_key(SERVICE, ACCOUNT)?;
    let client = isoquant::http_client().context("could not build the HTTP client")?;
    let upstream = Upstream::isoquant();

    if let Err(failure) = isoquant::prove_zdr(&client, &upstream, &key).await {
        eprintln!("opencode-iq: ZDR is not proven, so OpenCode was not started: {failure}");
        return Ok(ExitCode::from(70));
    }

    let receipts_path = receipt_dir()?.join(format!(
        "{}-{}.zdr-receipt.jsonl",
        humantime::format_rfc3339_seconds(SystemTime::now())
            .to_string()
            .replace(':', ""),
        std::process::id()
    ));
    let receipts = Arc::new(ReceiptLog::create(&receipts_path)?);
    let proxy = proxy::start(
        ProxyConfig {
            upstream: upstream.clone(),
            key,
            allowed_models: vec![MODEL.to_string()],
            receipts: Arc::clone(&receipts),
        },
        client,
    )
    .await
    .context("could not start the ZDR proxy")?;

    let bin = opencode::binary();
    let mut child = opencode::command(&bin, &proxy.base_url(), &args)
        .spawn()
        .with_context(|| format!("could not start {}", bin.display()))?;
    let status = wait_for(&mut child).await?;
    drop(proxy);

    let summary = Summary::read(receipts.path(), upstream.host())?;
    eprintln!(
        "opencode-iq: {} chat request(s) forwarded, {} without ZDR, {} refused, {} upstream error(s); receipt {}",
        summary.forwarded_chats,
        summary.without_zdr,
        summary.refused,
        summary.upstream_errors,
        receipts.path().display()
    );
    if !summary.holds() {
        eprintln!(
            "opencode-iq: ZDR ASSERTION FAILED: a request left without the ZDR header or to another host"
        );
        return Ok(ExitCode::from(71));
    }
    Ok(exit_code(status))
}

/// Waits for OpenCode. Ctrl-C reaches OpenCode from the terminal, and
/// OpenCode decides what it means, so this process ignores it. SIGTERM and
/// SIGHUP are passed on, so OpenCode never outlives the proxy it needs.
async fn wait_for(child: &mut tokio::process::Child) -> Result<std::process::ExitStatus> {
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    loop {
        tokio::select! {
            status = child.wait() => return status.context("could not wait for OpenCode"),
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => pass_on(child, libc::SIGTERM),
            _ = hangup.recv() => pass_on(child, libc::SIGHUP),
        }
    }
}

fn pass_on(child: &tokio::process::Child, signal: libc::c_int) {
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) with a pid we own and a valid signal number.
        unsafe { libc::kill(pid as libc::pid_t, signal) };
    }
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code.clamp(0, 255) as u8),
        (None, Some(signal)) => ExitCode::from((128 + signal).clamp(0, 255) as u8),
        (None, None) => ExitCode::from(1),
    }
}

/// `~/ext/scratch/opencode-iq` when the scratch drive is mounted, else
/// `~/.local/state/opencode-iq`. Owner-only.
fn receipt_dir() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let dir = if home.join("ext").is_dir() {
        home.join("ext/scratch/opencode-iq")
    } else {
        home.join(".local/state/opencode-iq")
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    Ok(dir)
}
