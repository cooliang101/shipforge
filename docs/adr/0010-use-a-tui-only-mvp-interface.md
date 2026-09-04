---
status: accepted
date: 2026-09-03
---

# Use a TUI-only MVP interface

The MVP exposes project setup, SSH connection and Key selection, configuration validation, deployment, health inspection, history, logs, and rollback through the Ratatui interface. It does not expose deployment subcommands, headless JSON output, CI automation, Agent calls, or Agent-driven configuration edits. Application services remain independent from TUI views so a later automation interface can reuse behavior without becoming an MVP constraint. This prioritizes discoverable selection-based operation and avoids forcing users to memorize commands, at the cost of deferring all automated control use cases.
