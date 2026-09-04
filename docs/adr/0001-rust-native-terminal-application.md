---
status: accepted
---

# Build ShipForge as a Rust-native terminal application

ShipForge will ship as a Rust executable with a `ratatui` interface and `crossterm` terminal backend. This replaces the earlier Go recommendation: Rust provides a cohesive boundary for application and TUI behavior, while Ratatui's buffered, differential rendering fits the high-volume deployment-log experience. The trade-off is a less uniform SSH/SFTP ecosystem, so transport selection must pass an early cross-platform spike before it becomes a stable adapter.
