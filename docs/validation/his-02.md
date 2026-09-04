# HIS-02 远端库存与审计验收

日期：2026-09-04。范围：M2 的 `HIS-02`，不代表 M2 或 MVP 完成。代码、独立审查、默认质量检查及真实 Linux 验收通过。

## 实现边界

- Driver `inventory` 返回独立的归档库存和辅助审计；不依赖本地数据库，不修改 YAML、目录、current 或服务。
- 校验 marker、规范版本、文件类型、稳定文件身份、远端 SHA-256/gzip 和首个 tar manifest，并对照解压目录 manifest。只传元数据，不下载历史压缩包。
- 仅归档版本带 `extracted=false` 和提示；缺归档、错误 manifest、符号链接及未知 current 明确报告。超限或截断不冒充完整列表。
- Prepare 写 `releases.jsonl`，激活/回滚写 `deployments.jsonl`。记录逐 Component 结果和原始非秘密引用，不补造历史 revision、端点或健康状态。
- JSONL 追加不截断、不加锁；固定描述符防止最终路径替换重定向写入，拒绝符号链接和共享硬链接。读取有界，损坏尾部、未知格式和重复冲突明确提示。
- Prepare 审计失败不继续激活；生效后的辅助审计失败保留回执并附警告，不触发本地持久化失败策略或阻碍补偿。

完整数据形状、资源上限、远端前置条件与失败语义见 [架构](../architecture.md)。没有新增用户配置、CLI 或数据库 schema。

## 独立审查与回归

交叉审查覆盖库存、审计、Driver 接线、编排警告和测试夹具。修复了远端超时被误当单条损坏版本、审计命令的服务器端期限与写后 inode 检查，以及构建前遗漏 GNU `timeout`/`dd` 和 `/proc/self/fd` 能力检查。撤部署审计保留传入的原始 Release 引用，不用当前能力重写旧引用；专门的协议回归验证旧能力快照。协议模拟器精确解析参数并区分 SFTP READ/WRITE/APPEND/EXCLUDE，不用无条件成功掩盖新命令。

| 验证入口 | 结果与范围 |
| --- | --- |
| `cargo test --locked --all-targets --all-features --quiet` | Windows / Rust 1.96.1：368 单测与 6 协议测试通过；3 外部测试默认忽略 |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` / `git diff --check` | 通过 |
| `cargo build --locked --release` | Windows 发布构建通过 |
| `cargo audit` | 退出码 0，无已知漏洞报告；仍有 `wnaf 0.14.0` 撤回警告 |
| `./tests/run-linux-acceptance-cleanup-tests.ps1` | 原生命令检查及 6 个隔离清理场景通过 |
| `./tests/run-linux-acceptance.ps1 -Distribution Ubuntu-22.04` | 首轮 408.73 秒通过；补强前置检查、硬链接及仅归档用例后复跑 429.16 秒通过，两轮清理均成功 |

真实验收仅使用两个独立固定 Host Key 的 Debian/OpenSSH 测试容器、回环随机端口和临时身份。生产部署/回滚路径验证 HTTP 失败补偿及取消；库存验证摘要/manifest、历史 revision 不被新上下文覆盖、审计缺失/截断、损坏与孤立版本。审计权限故障必须仍返回成功回滚与警告；链接测试要求无关哨兵文件不变。

复跑期间最后修正了撤部署审计的旧能力引用保留；该字段回归由随后通过的协议测试验证，不把实机用例宣称为旧能力快照测试。容器、唯一镜像标签与 Windows 临时身份目录均清理；普通 Docker 构建缓存可能保留。

## 不在本次范围

没有重跑 WSL systemd 或原生 Linux 客户端矩阵；Linux 专用审计 Shell 单测未在 Windows 执行，真实脚本行为由上述容器用例提供证据。库存仅验证身份/归档元数据，不证明整个 payload 可安全激活，也不把 current 当健康。前缀读取不是完整历史分页；超限明确不完整。

中断对账、数据库丢失后的恢复决策、引用保护清理和 TUI 管理页面分别属于 `REC-01`、`RET-01`、`TUI-MGT-01`；本包不修改远端事实来恢复服务。
