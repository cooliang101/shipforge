# REC-01：只读对账验收

日期：2026-09-04。范围：M2 的 REC-01 工作包，不代表 M2 或完整 MVP 已完成。

## 已交付

- `RecoveryService::inspect` 只调用 Driver `inventory`，比较冻结的目标/原版本与远端 current、manifest、SHA-256 和大小；未知、未部署、仅有压缩包及不匹配保持区分，不推断健康或执行顺序。
- SQLite schema v6 独立保存不可变 `recovery_reports`、`recovery_report_components`；`deployment_revisions` 防止关联源历史变化后缓存过期结论。原 Deployment、Step、意图和结果不变。
- 有效 YAML 下，本地数据库缺失可新建纯库存缓存；不补造本地日志或历史操作。损坏、空文件和未初始化数据库被拒绝，检查期间截断也不覆盖。YAML 缺失仍是新 Project 流程，不从远端恢复身份。
- 按源记录精确解析旧 Destination revision；缺失或身份/generation 不符时报告未知，不回退连接。配置变化、取消、超限、保存失败均保留明确的不完整信息。
- Linux 有界、不跟随链接的残留扫描识别上传包、解压目录、激活/回滚链接及无归属的 Marker 发布残留；不删除、接管或修复。
- TUI 启动和打开有效 Project 时仅查询本地 `created/running OR pending intent`，包括终态待核实项。单后台查询、可替换请求和 UUID 校验抑制过期结果；不自动 SSH。

## 审查与回归

应用、存储、Driver 与 TUI 分别实施并交叉审查。修复了有界分页/UUID 索引读取、缓存插入顺序、旧修订缺失回退风险、归档冲突分类、检查期间历史截断和未检查项被误写为容量超限等问题，并补充回归。最终审查无剩余阻断项。

| 门禁 | 结果 |
| --- | --- |
| `cargo test --locked --all-targets --all-features --quiet` | 426 单测、6 协议测试、1 中断测试父用例通过 |
| 中断测试父用例 | 5 类阶段 × 3 个边界，共 15 次子进程退出/重开通过；辅助子用例默认忽略 |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | 通过 |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |
| `cargo build --locked --release` | Windows 发布构建通过 |
| `cargo audit` | 退出码 0；保留既有 `wnaf 0.14.0` 撤回警告，依赖未变 |
| `./tests/run-linux-acceptance-cleanup-tests.ps1` | 原生命令退出码及 6 个隔离清理场景通过 |
| `./tests/run-linux-acceptance.ps1 -Distribution Ubuntu-22.04` | 真实双 Debian/OpenSSH 验收通过，486.50 秒，未触及 600 秒上限 |

默认测试的三个外部 SSH/systemd 用例需显式一次性环境，不能将默认忽略算作通过。中断用例在 build、prepare、activate、compensate、rollback 副作用前、后及完成步骤间，真实提交 SQLite 后以退出码 77 结束子进程且不运行析构；父进程重开并运行生产对账服务。远端事实由本地文件模拟，不是真实 SSH 中途杀进程。

## 真实 Linux 证据

双 Destination 套件重跑单组件、联合发布、历史/未部署回滚、HTTP 失败补偿和取消，以及 HIS-02 库存/审计边界。新增两次生产对账服务调用：一份独立本地待核实意图和一份缺失数据库；验证目标版本及包一致、四类规范暂存残留可见、报告可重开、源历史及意图不变、纯缓存不产生历史 Deployment。

两次检查前后比较远端文件 SHA-256、不跟随链接的元数据及链接目标；排除读操作可能改变的 atime，确认无业务状态变更。此 Linux 测试使用预置中断证据，不代表已在每条 SSH 指令中途终止客户端。最终增加的本地数据库截断防护由专项单测覆盖，并通过最终协议与中断回归；未另跑一次实机截断场景。

运行标记 `1568b34a9a4f421d844f6b30081df4ce`；非特权容器、随机回环端口、临时身份，仅挂载测试公钥。Runner 退出码 0；额外检查确认两容器、镜像标签及临时密钥目录均已删除。普通 Docker 构建缓存可保留，未修改业务服务或默认 SSH 配置。

## 边界与后续

报告最多 256 个 Component/4 MiB，分页 100 条/8 MiB；应用证据预算 2 MiB，单 Component 180 秒、整体 600 秒。残留扫描限制 256 项、每次列表 64 KiB、32 条诊断和 30 秒；不足以证明完整时标记未知/不完整。

启动查询不创建缺失的主数据库/目录、不迁移或修改历史行；现有 WAL 数据库仍可能产生/使用 WAL/SHM 辅助文件。对账不能证明服务健康，不能自动补全旧意图或重新执行副作用。完整历史/恢复/回滚页面属于 `TUI-MGT-01`；下一工作包是 `RET-01` 保留与清理。真实 systemd、Linux 原生客户端沿用各自已记录的 M1 证据，本次不声明新增平台验收。
