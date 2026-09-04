# HIS-01 本地历史模型验收

日期：2026-09-04。范围：M2 的 `HIS-01` 工作包，不代表整个 M2 或 MVP 完成。

## 已实现

- SQLite schema v5 在构建前保存所选 Component 集合、执行顺序、完整目标/原版本引用、generation、Destination revision、端点与能力快照。
- TUI 部署保存确认的 Git 分支、提交和工作区状态；操作者取本机账号环境信息，缺失时保持未知。直接调用核心编排器和显式回滚不伪造源码上下文。
- 打包后、上传前保存与归档内完全相同的 manifest、SHA-256 和大小；准备回执独立记录，不冒充当前版本或健康证据。不保存临时包路径、凭据或连接设置。
- 构建打包、准备、激活/回滚、补偿步骤保留状态和时间；意图与步骤开始/完成原子更新。终态时未尝试步骤标为 skipped，未解决意图仍可查询。
- 观察明确区分 Release、确认未部署和未知；健康信息只能来自通过契约验证的回执。查询提供按 Project/Environment 分页、详情及包含终态未完成意图的排查入口。

## 审查修正

独立审查覆盖存储与编排。修复了副作用后历史写失败跳过补偿、无效回执误记健康、初始化失败丢失 Deployment ID、包记录失败遗留已知构建步骤、独立时钟造成即时失败时间倒退，以及结果/步骤引用与时间约束不足等问题。

恢复仍须先写入意图；无法落盘的恢复操作不会执行。已知远端结果和人工处理信息不被历史故障覆盖，报告附带本地历史警告。初始化未产生副作用时尝试终态化；若终态本身写失败，错误保留原 ID 和两项诊断。

## 自动化证据

| 范围 | 验证入口 |
| --- | --- |
| v1–v4 升级、失败回滚、新版本拒绝、查询与数据一致性 | `src/history/store/details/tests.rs`、`store.rs`、`store/log_index_tests.rs`；历史模块共 39 项测试 |
| 冻结计划、准备失败、未知观察、写入故障及补偿顺序 | `src/application/orchestrator/tests.rs` |
| 回滚到版本/未部署、漂移、回执与结果持久化失败 | `src/application/rollback/tests.rs`，24 项测试 |
| 构建前完整快照、初始化与包元数据失败 | `src/application/deployment.rs` 的测试模块 |
| 真实本地构建到持久化详情 | `tests/support/deployment_service.rs`：Git 提交、包/准备引用、健康观察、步骤与同一个 Deployment ID 一致 |

```sh
cargo test --locked --all-targets --all-features --quiet
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo build --locked --release
cargo audit
```

Windows / Rust 1.96.1：335 项单测与 1 项协议集成测试通过；3 项外部环境测试默认忽略。严格 Clippy、格式检查和 release 构建通过。依赖审计退出码 0，未报告已知漏洞；保留既有 `wnaf 0.14.0` 撤回警告。新增测试只使用自有临时数据库/项目和 loopback 协议夹具，不读取真实部署配置。

## 范围限制与下一步

本轮没有重跑 WSL systemd、双 Linux Destination 或完整平台矩阵；既有实机验收记录保留其历史范围。协议测试是真实 SSH/SFTP 传输，但远端命令与文件系统由模拟服务器实现。

历史 API 仍是内部 Rust 接口，不新增用户配置或 CLI。旧数据库缺少的历史字段不补造，查询本身不执行恢复；TUI 历史管理、远端 Release 库存/JSONL、对账和保留清理分别属于后续工作包。下一步是 `HIS-02`，不是发布完整 MVP。
