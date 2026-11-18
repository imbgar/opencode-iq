# opencode-iq

Run [OpenCode](https://opencode.ai) on Isoquant `deepseek-v4.1-flash-1017-fast`
with zero data retention (ZDR) forced on every request, and proven before the
session starts.

```
opencode-iq                    # interactive
opencode-iq run "a prompt"     # one-shot; any opencode arguments work
```

## What it does

1. It reads the Isoquant key from the macOS Keychain item
   `mina.isoquant.dev.key` (account `mina`).
2. It proves ZDR. The model list must show the model with
   `zdr_supported: true` and the accepted privacy policy version
   (`ACCEPTED_POLICY` in `src/isoquant.rs`). The gateway must refuse a request
   with an invalid `Isoquant-ZDR` value (HTTP 400 `invalid_zdr_requirement`).
   If either check fails, OpenCode does not start (exit 70).
3. It starts a proxy on `127.0.0.1`, in the same process. The proxy:
   - holds the key; OpenCode never gets it;
   - sends exactly `Isoquant-ZDR: required` and drops any auth or ZDR header
     that OpenCode sends;
   - forwards only `GET /v1/models` and `POST /v1/chat/completions` for the
     one allowed model, and refuses everything else;
   - writes one receipt row per request (no bodies, no key).
4. It starts OpenCode with a session config in `OPENCODE_CONFIG_CONTENT`. That
   config applies after every config file, so a project's `opencode.json`
   cannot move the session to another provider. Provider keys are removed
   from OpenCode's environment.
5. When OpenCode exits, it reads the receipts. If any request left without
   the header, or to another host, it exits 71.

Receipts go to `~/ext/scratch/opencode-iq/` when `~/ext` exists, else to
`~/.local/state/opencode-iq/`.

## Exit codes

| Code | Meaning |
|---|---|
| OpenCode's | Clean run |
| 1 | Setup failed (no key, OpenCode not found) |
| 70 | ZDR not proven; OpenCode not started |
| 71 | A receipt shows a request without ZDR |

## What ZDR means here, and what is not proven

- **We prove:** every request we send carries `Isoquant-ZDR: required`. The
  gateway rejects an invalid value, so it reads the header. The model list
  attests ZDR under the policy version we accepted.
- **The provider promises:** the Isoquant Privacy Policy says a model that
  cannot honor the requirement returns an error instead of serving the request.
- **Not proven:** no response field confirms that ZDR applied to a request.
  The Terms of Service hold no ZDR promise. The provider does not name its
  GPU hosts.

Send personal-project code only. Do not send employer data, customer data,
credentials or production data.

## Skills

OpenCode reads skills from `~/.claude/skills` by itself. Check with
`opencode debug skill > skills.json` (write to a file; a pipe cuts the list).

## Build

```
cargo test
cargo build --release
install -m 0755 target/release/opencode-iq ~/.local/bin/opencode-iq
```
