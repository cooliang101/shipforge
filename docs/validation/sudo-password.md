# Sudo password authentication

Scope: explicit `sudo -S --` service argv uses the saved SSH login password under the existing server sudo policy. TUI `p` toggles this form for the selected service action, including conversion from `sudo -n`. No sudoers, group or server authentication changes.

Validation on Windows GNU, 2026-09-07:

- Default suite: 1,036 passed; ignored external/acceptance tests are not counted.
- Loopback SSH fixture covers fragmented prompts, success, rejection, cached/no-prompt completion, no unsolicited stdin, timeout, cancellation and hostile password echo suppression. Existing host-pin/password/release tests remain active.
- Unit tests cover canonical argv/credential requirements, newline refusal, bounded prompt scanning, one response only, and TUI action conversion preserving systemd checks and other actions.
- Formatting, Clippy all targets/features and release build passed. Both exact-binary ConPTY smokes (q exit, idle Ctrl+C then q) passed.
- Release SHA-256: `BB0887B90F81151AA6EF53B651996AD88A8644E3402AD2D6E5CA477646855087`.
- The release TUI converted and saved both aiagent service actions from `sudo -n` to `sudo -S --`; project/environment identity and website configuration were preserved.
- Separate authorized operational check against the user's saved 10.0.0.206 connection: `sudo true` returned 0. This was not a regression test and did not restart/stop a service or change sudo policy.

Limits: same SSH/sudo password only; no independent sudo credential, MFA or requiretty support. Password-channel output is intentionally unavailable, so diagnose privileged failures by exit status and separate non-secret inspection. A successful `sudo true` checks authorization/authentication, not full deployment or service health.
