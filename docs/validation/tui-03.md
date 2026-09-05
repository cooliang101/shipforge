# TUI-03：步骤进度与日志交互验收

日期：2026-09-04。状态：TUI-03 通过自动化工作包验收，交叉审查、文档复核和双平台门禁通过。M3 其余工作包、M4 和完整 MVP 尚未完成。

## 实施范围

- 部署与回滚共用结构化事件：实际 Component/持久阶段、状态、持久化确认及单调耗时；执行线程完成时冻结计时。已知成功但历史写入失败保持成功/未确认，不伪造失败或完成旧意图。
- 日志索引升级至 SQLite schema v7，明确区分新 `jsonl_v1` 与原 `legacy_text`。编号迁移不改旧文件、原结果或 YAML；只读浏览拒绝旧 schema，不自动升级。新日志按完整 JSON 行轮转，长事件带明确分片身份。
- `l` 日志页提供有界实时跟随/暂停、完整记录、全部已有步骤与耗时、跨保留文件搜索、Component/步骤筛选和分页。旧文本不从正文推断元数据；缺失、损坏、缺片、超限和读取漂移均明确提示。
- 复制的是实际失败调用的脱敏程序/参数 JSON，不是当前 YAML 或错误文本重建出的 Shell。本地构建、远端执行及执行期间失败后观察均有事件路径；没有完整命令证据时明确不可用。
- 日志导出覆盖当前筛选匹配的全部保留文件；另有整次部署的文本摘要，不受日志筛选影响。选择已有目录、核对完整路径和冻结正文后，仅无修饰 `c` 发布且不覆盖现有文件。读取/准备不写文件，晚到取消不撤销已知发布结果。
- 日志页的后台任务跟踪取消与线程结束；打开/关闭空闲日志页不取消部署，繁忙时不能脱离线程。失败回滚保留本次实际 ID，不误用回滚来源的历史 ID。

使用说明见 [TUI 指南](../tui-guide.md#步骤与日志)。本阶段没有新增用户填写的 ID、别名、Driver 字段、CLI/AI 接口、文件锁或远端锁，也没有改变 Release 格式、健康策略、补偿或保留算法。

## 审查与回归

结构化读取/脱敏、核心事件、远端诊断、本地导出与 TUI 分工实施；由非实现者交叉检查数据来源、取消、确认和资源边界，根代理复核集成及最终差异。

审查修正了实际问题：长日志先分片后脱敏造成的边界风险、旧分页先读内容后用另一快照验证、重分片错误声称原始来源、规范化秘密替换顺序、失败回滚误用旧入口、生产 writer 对实时正文的截断/去换行，以及 80×10 终端中详情和导出正文被固定标题挤没。最终小视口测试通过完整 App 渲染与键盘处理，不只测试孤立组件。失败后的独立远端观察也补齐事件转发，保持原 fresh token、超时与 Unknown 结果。

| 证据位置 | 主要覆盖 |
| --- | --- |
| `src/application/step_events/`、`build/`、`deployment/`、`orchestrator/tests/`、`rollback/tests/`、`retention/tests/` | 意图先于 Started、真实失败 argv、已知结果与持久化分离、取消/补偿/清理事件 |
| `src/application/deployment_logs/tests.rs`、`src/history/logs.rs`、`src/history/store/log_index_tests.rs` | 完整 Unicode/换行在磁盘与实时入口一致、整条轮转、schema 6→7、旧格式不重标 |
| `src/telemetry/log_record/`、`src/application/history_query/logs/` | 原始日志跨片/跨代脱敏、完整保留集搜索、过滤、内容摘要游标、旧分页同快照、超限与不完整覆盖 |
| `src/drivers/linux_ssh/command_events/`、`connection/`、`src/application/execution_guard/tests/` | 实际 argv 而非渲染 Shell、敏感参数、队列上限、观察事件转发与原结果保持 |
| `src/tui/live_progress/`、`log_view/`、`app/logs/`、`app/management/rollback_failure_tests.rs` | 时钟冻结、有界内存投影、独立筛选、失败回滚 ID、长内容、小视口、帮助/确认隔离及 worker join |
| `src/application/local_export.rs`、`src/tui/clipboard.rs` | 无副作用预览、准确字节、路径变化、no-clobber、取消/发布结果，以及单次 OSC 52 写入和失败行为 |

关键原始文件回归包括 `raw_indexed_private_key_fragments_are_joined_before_any_body_redaction`、`legacy_extra_hyphen_orphan_end_never_exposes_prefix_body` 和 `compatibility_legacy_page_returns_safe_bytes_from_its_own_verified_scan`。`same_size_content_change_with_restored_time_invalidates_cursor` 证明游标不只依赖大小/mtime。`event_writer_preserves_full_utf8_text_and_scope_as_complete_json_lines` 同时验证磁盘与生产实时入口，不由直接 UI 注入替代。

## 质量门禁

使用 Rust/Cargo 1.96.1；Windows 原生与 WSL Ubuntu-22.04 隔离工具链分别执行，Linux target 不在 Windows 映射盘上。

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --all-targets --all-features --quiet
cargo build --offline --locked --release
```

| 门禁 | Windows | Linux |
| --- | --- | --- |
| 库测试 | 887 通过 | 908 通过 |
| SSH/SFTP 协议测试 | 14 通过 | 14 通过 |
| 中断恢复父用例 | 1 通过（内部 15 个场景） | 1 通过（内部 15 个场景） |
| 格式 / 严格 Clippy | 全部通过 | 全部通过 |
| release 编译 | 通过 | 通过 |

默认套件各有 8 项 ignored：6 项外部 SSH/systemd 用例、1 项独立连接诊断、1 项由父用例显式启动的中断助手，未计作通过。初轮完整协议测试发现旧纯文本格式断言；改为严格解码新记录，保留原意图/生命周期检查，并增加格式索引、实际 Deployment ID 和 stderr 的准确 Component/步骤归属断言，经过独立复核后两端全套重跑通过。

Windows `cargo audit` 更新 RustSec 数据库后退出码 0（355 项依赖）；仍有既有 `wnaf 0.14.0` 撤回警告。本阶段仅启用 `crossterm` 的 `osc52` 特性，并在锁文件增加已有 `base64` 的依赖关系，没有批量更新版本。所有 Markdown 相对文件链接检查通过。

## 资源与证据边界

- 磁盘日志：当前文件 1 MiB，加最多三个轮转文件；writer 队列 128 条，收尾等待最多 5 秒。结构化记录上限 128 KiB，消息分片和完整 argv 各 16 KiB，argv 最多 128 个参数。
- 实时生产队列：128 行/1 MiB；步骤表 1,024 项。日志窗口：500 行/2 MiB，单行上限 64 KiB；生产端遗漏与窗口淘汰独立报告。这不是全进程内存上限或吞吐基准。
- 历史读取：单文件 8 MiB、合计扫描/安全投影各 16 MiB、32,768 个安全视图片段、每页最多 200 条/256 KiB。查询最多 128 字符；历史日志/摘要导出上限 8 MiB，通用落盘服务另有 16 MiB 上限。导出预览正文每次只渲染最多 4 KiB 的可滚动块。
- 搜索/导出仅对仍保留的文件负责，不恢复已删除轮转文件。旧日志无标记、无可见缺代的私钥中段无法总是与普通文本区分；不能宣称绝对秘密检测。可识别 PEM、孤结束标记、已知缺代和不完整结构化分片按保守规则隐藏。
- Unix 导出为 `0600`，Windows 继承目录 ACL；无法检查祖先目录时拒绝。路径和文件证据复核不是跨进程隔离。新历史 schema 不保证旧可执行程序兼容读取；降级前保留数据库备份，不为浏览而触发部署来升级。

## 未覆盖与后续

默认测试只使用临时项目/注册表、Fake Driver、Ratatui `TestBackend`、内存 SSH 流及回环协议服务器。新增失败后观察测试经过真实 core 分支，但远端结果由 Fake Driver 提供；SSH 字节传输用例也不等于外部 OpenSSH 逐阶段故障注入。本阶段未重新执行双 Destination 或 WSL systemd 实机部署，旧阶段证据仍保留原范围。

OSC 52 测试使用注入的输出 writer，未修改或读取用户剪贴板。真实终端/复用器可能拒绝请求，发送后需粘贴验证；非 ANSI Windows 控制台不支持该路径。自动化 80×10 渲染不等于人工终端体验。

TUI-04 继续管理页面一致性；TUI-05 审查全部运行/退出路径，包含既有部署规划线程的关闭跟踪；后续 [TUI-06](tui-06.md) 测量高吞吐下与 UI 排空解耦的有界淘汰、整进程内存与合成输入延迟。macOS、最低 Rust 版本、完整平台发布矩阵和早期未定位 SSH 超时仍归 M4/QA-01，本阶段不宣称已解决。
