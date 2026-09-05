# TUI-05：退出生命周期验收

日期：2026-09-05。状态：TUI-05 通过工作包验收；实现、两轮独立审查、文档复核、双平台最终门禁及 Linux 真实 PTY smoke 均已完成。TUI-06、M4 和完整 MVP 尚未完成。

## 实施范围

- `ExitState` 明确区分运行、退出确认和安全等待。活动工作按 `q` 只显示确认；`Esc`/`r` 返回，`c`/`Ctrl+C` 才取消全部受跟踪任务。等待中不接受继续操作或强制脱离。
- 部署规划/执行、Management、Connections、ProjectEdit、Reinitialize、远端目标选择、SSH Setup 和 Attention 的事件型任务由 App 保存请求 ID、取消令牌和线程句柄；匹配完成事件先 join，迟到 ID 不丢失当前任务。本地日志/导出不走完成事件，由 App 单一持有取消令牌和线程句柄并直接检测、join。
- 取消后的读取、预览和计划恢复原页，不生成新确认；保存、移除、库存检查和执行的已知结果不被迟到取消或线程尾部错误覆盖。项目/Destination 移除在原子发布前再次检查取消，发布后的取消不撤销成功。
- Unix `INT`/`TERM`/`HUP` 与 Windows Ctrl+C/Break/Close/Shutdown 进入同一安全退出路径。终端初始化部分失败、正常退出、I/O 错误和可捕获 panic 都逐项尝试关闭 raw mode、离开 alternate screen 并显示光标。
- 默认 panic hook 不在 raw/alternate screen 中输出；恢复后只写固定诊断，不读取 panic payload 或底层错误正文。

本工作包没有新增 YAML 字段、存储迁移、Driver 能力、锁、后台脱离或自动化入口。

## 审查与回归

两名非实现者分别审查全部 App 任务闭环及跨模块退出状态机，最终未发现 P0/P1/P2。审查发现普通 `q` 会在 Help/Picker/Log 覆盖层之前被截获；现已调整为退出对话框打开时最高优先，其他情况下覆盖层先消费按键，并增加日志页及 20×3 短终端回归。

受控真实线程测试证明：`q` 不取消活动任务；确认取消后 `exit_ready` 在 worker 结束前保持 false；matching UUID 必须 join，stale UUID 保留句柄；shutdown 会等待尚未取得 `DeploymentSession` 的线程。worker panic 只产生固定错误，已知写入/执行结果仍保留。终端清理使用可失败的 fake，验证三步全部尝试、幂等和主错误/恢复错误合并。

## 质量门禁

Rust/Cargo 1.96.1；Windows 原生及 WSL Ubuntu-22.04 隔离工具链。Linux 的 Rust、Cargo 和 target 目录位于独立 `/var/tmp`，未替换用户默认工具链。

```sh
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets --all-features -- -D warnings
cargo test --offline --locked --all-targets --all-features --quiet
cargo build --offline --locked --release
```

| 门禁 | Windows | Linux |
| --- | --- | --- |
| 库测试 | 955 通过 | 976 通过 |
| 可执行入口测试 | 2 通过 | 2 通过 |
| SSH/SFTP 协议测试 | 14 通过 | 14 通过 |
| 恢复测试可执行文件 | 1 通过 / 1 ignored；父用例内部 15 个场景 | 1 通过 / 1 ignored；父用例内部 15 个场景 |
| 格式 / 严格 Clippy | 通过 | 通过 |
| release 编译 | 通过，退出码 0 | 通过，退出码 0 |

两端默认套件各有 8 项 ignored：7 项需显式一次性 SSH/systemd 外部夹具，另 1 项是由恢复父用例启动的中断助手；均未计作通过。Windows `cargo audit` 更新 1,239 条 RustSec 公告并检查 355 项依赖，退出码 0；仍有既有 `wnaf 0.14.0` 撤回警告，本阶段未改依赖。32 个 Markdown 文件的相对文件链接检查无缺失。

## Linux PTY 证据

隔离的 Linux release 构建先在 `script` PTY 中按 `q` 正常退出：返回码 0，前后 `stty -g` 相同，并捕获到 alternate screen 的 `1049h/1049l` 与光标的 `25l/25h` 成对序列。

信号 smoke 使用 Python `openpty` 建立控制终端，持续排空输出并直接向 ShipForge PID 分别发送 `INT`、`TERM`、`HUP`。三次均返回 0、未强杀、termios 完全恢复，且上述两组控制序列均成对。早先不带 `--foreground` 的 `script + timeout + 复合 shell` 包装使被测进程组因 `SIGTTOU` 暂停并得到 137；`strace` 定位后该假阴性未计入验收。直接 PID 驱动及 `timeout --foreground` 均通过。

## 范围限制

所有 App 级操作 worker 会在退出前 join，部署/回滚等待安全恢复边界。内部部署日志 writer 关闭通道后最多等待五秒；超时会明确标记日志不完整，但不改变已知远端执行或补偿结论，因此不能声称每个进程内部辅助线程都必然 join。

App 安全退出有意不设超时；若系统调用永久不返回，界面会继续等待而不假装任务已结束。`SIGKILL`、OOM、`panic=abort` 及宿主强制关闭期限不可恢复。真实 Windows ConPTY 的 Break/Close/Shutdown、macOS、最低 Rust 1.88、人工全页面体验及全进程背压/内存/输入延迟分别留给 M4 和 TUI-06。
