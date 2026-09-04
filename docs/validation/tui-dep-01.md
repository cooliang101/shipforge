# TUI-DEP-01 阶段验收记录

日期：2026-09-04。环境：Windows / PowerShell。范围：TUI 部署工作包及其接入时发现的安全缺口，不代表整个 M1 或 MVP 完成。

## 已完成的闭环

- 选择 Environment 和 Component 子集，进行本地构建输入、Git 和远端只读预检；预览完整目标、版本、构建命令和提示后按 `c` 确认。
- 在构建前创建 Deployment 和持久化意图；构建、标准 `tar.gz` 打包、全量准备、顺序激活和补偿共用同一 ID。
- 实时处理 stdout/stderr，跨分块脱敏秘密和私钥，独立写盘队列与有界轮转文件通过 SQLite schema v4 索引。UI 单独限长，日志故障保留首错并触发安全取消。
- 每次 Driver 写操作前复核配置快照；Prepare 写入前核验 current、marker、manifest 及实际解压空间。回滚显式携带预期源版本，不以未知状态冒充未部署。
- `Esc` / `Ctrl+C` 安全取消，错误结果保留逐 Component 事实和人工恢复指引。终端 I/O 失败先恢复终端，再等待正在执行的操作安全结束。

## 审查与回归

已分工审查配置/日志、远端安全、阶段验收，并修复这些问题：日志错误覆盖真实部署报告；跨行秘密和超长行私钥漏脱敏；同步写盘阻塞取消；取消后的观察错误被吞成空状态；回滚及 Prepare 的预期源检查遗漏；父进程退出后输出管道使超时失效；错误持久化失败时丢失 Deployment ID。

| 门禁 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --all-targets --all-features -- -D warnings` | 通过 |
| `cargo test --quiet` | 287 项单元测试、1 项协议集成测试通过；1 项显式 disposable 测试忽略 |
| `cargo audit` | 退出码 0；未报告已知漏洞，存在下述撤回版本警告 |
| `cargo build --release` | 通过，生成本机 Windows 可执行程序 |
| `git diff --check` | 通过 |

协议集成测试使用真实 loopback SSH/SFTP 传输与模拟远端命令/文件系统；其中生产 `DeploymentService::plan→execute` 路径执行真实 `rustc` 构建，检查子集选择、源码提交号、同 ID 的步骤意图、日志索引及文件。TUI 通过输入/状态和 Ratatui TestBackend 测试验证，不作为全部交互视觉验收。

## 尚未通过的里程碑门禁

- M1 的两台一次性真实 Linux Destination 联合发布、指定版本回滚及失败补偿仍待执行。默认忽略的 OpenSSH 测试仅验证握手、认证与探测，不能替代该验收。
- 两项 Unix 专用的真实后台子进程继承管道回归已加入，但本次 Windows 测试不执行；跨平台编译和 smoke test 仍属 QA-01。
- 审计提示传递依赖 `wnaf 0.14.0` 已被撤回，路径为 `russh → p256/p384/p521 → primeorder → wnaf`。本阶段未修改或隐藏告警；发布前须跟进上游可用版本并重新审计。
- 历史浏览、重启对账、库存/保留管理、完整退出对话框和日志搜索导出仍属 M2/M3；远端容量校验只是瞬时检查，不做空间预留或并发协调。

本阶段按“审查 → 测试与门禁 → 独立提交”交付；后续阶段沿用同样节奏。
