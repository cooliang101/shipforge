# SSH-PWD-01 — Windows SSH password login

Date: 2026-09-07. Status: implementation and scoped local acceptance completed. Scope: Windows x64 GNU with the existing Rust/MinGW toolchain. Adds standard password authentication to the existing TUI-managed connection lifecycle.

## Delivered behavior

- First-time setup and standalone connection create/edit use the same F5 password input handler. Passwords are masked, bounded to 1024 UTF-8 bytes, and never serialized from form state. Backspace and Delete erase draft input; cancelled edits leave saved credentials unchanged.
- The user credential registry stores only Windows current-user DPAPI ciphertext. Registry reload does not decrypt; the transport opens the password after the pinned host handshake and sends a standard SSH password request. Wrong credentials do not trigger alternate authentication or retries.
- Existing confirmation, request ownership, cancellation, worker join, stale-preview and atomic save behavior is retained. Authentication failure cannot save a connection. New credentials remain distinct from historical credential references.
- User-account protection does not defend against other programs running as that same Windows user. The draft and owned decrypted buffers are zeroized on drop, but complete erasure of library/OS copies is not guaranteed. Foreign-user/corrupt ciphertext has no plaintext fallback. Older binaries cannot read the new credential variant. Keyboard-interactive/MFA and encrypted private-key passphrase entry are outside this feature.

## Review and regression evidence

Local code review covered plaintext serialization and debug output, masked rendering, per-user encryption, Host Key ordering, failed authentication, persistence and cancellation boundaries. No subagents or external review were used.

New tests cover DPAPI encryption/reload, corrupted ciphertext, Unicode and punctuation, input bounds, clear/delete, masked connection/setup flows, failed setup authentication with no registry write, saving only after explicit host confirmation, reopened credential selection, cancelled password replacement, and a real loopback SSH/SFTP password session. The protocol case proves a mismatched host receives zero password attempts, rejects a wrong password, reopens a saved credential, executes/uploads/prepares/activates a Release, and exercises pre-cancelled, in-flight cancelled and timed-out authentication. A first fixture run omitted the HTTP status; setting its explicit healthy response corrected the fixture before the full passing run.

## Gates

- `cargo test --offline --locked --all-targets --all-features --no-fail-fast`: 999 library + 2 entry point + 15 protocol + 7 release helpers + 7 platform + 1 recovery parent = **1,031 passed**; ignored cases excluded.
- `cargo clippy --offline --locked --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check`, `git diff --check`: passed.
- `cargo audit`: passed; 1,239 advisories / 364 dependencies; the existing `wnaf 0.14.0` yanked warning remains.
- `cargo build --offline --locked --release`: passed; `target/release/shipforge.exe`, 11,031,552 bytes.
- Exact release-binary ConPTY checks: both normal q exit and idle Ctrl+C-then-q passed with terminal recovery.

Release SHA-256: `79ae08b2bc01341e1ef5f8d72631c3d56a1f3c6586b9095649b91eab83ea5f26`.

The requested aiagent deployment is separate from these isolated regression tests. This record does not claim successful authentication or deployment to the user's server. Final MVP gates QA-02, QA-03, REL-03 and ACC-01 remain outstanding.

## Password form layout follow-up

Password mode now uses one shared compact renderer in setup and connection editing: Host, User, Port and a single masked Password field. Agent discovery and candidate-host notices are suppressed in this mode, while the next Host Key step stays explicit. Focused fields scroll into view without wrapping long values. A regression checks 80×6, 80×10 and 120×24 rendering, a single focus marker, absence of discovery noise and masked content.

Full tests passed with 1,032 cases (1,000 library plus the same integration totals), strict Clippy and formatting passed, release build passed and both exact-binary ConPTY smokes passed. Updated release SHA-256: `a1abd5b4f3d50840bca7880b666ef30b9e75227c80f44c78c28c14a3cf49ab0f`.
