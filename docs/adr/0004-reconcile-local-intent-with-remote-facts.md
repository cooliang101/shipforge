---
status: accepted
---

# Reconcile local intent with remote facts

2026-09-07 update: the reconciliation decision remains accepted; the Linux layout below is historical. Current facts come from application snapshots and operation phases in `.shipforge-deploy/state.json`, with one previous application archive. See the [current architecture](../architecture.md).

SQLite records local Deployment intent and outcomes before and after each side effect, while each Driver observes the authoritative external state for its resolved Component target. For `linux-ssh`, that state is the Component's `current` link, versioned Release files, extracted directories, and manifests, with JSONL as an audit aid. Recovery records discrepancies rather than trusting local history after an uncertain call, accepting that local build logs cannot be reconstructed after local data loss.
