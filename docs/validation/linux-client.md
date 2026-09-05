# WSL Linux 客户端验证记录

日期：2026-09-04。范围：M1 本地进程回归与 Linux 客户端编译/启动证据，不代表 M1 或完整 QA-01 通过。

## 环境与隔离

- WSL Ubuntu-22.04，`Linux 6.18.33.2-microsoft-standard-WSL2 x86_64`，Rust/Cargo 1.96.1。
- Rust 工具链、Cargo 缓存、构建输出和冒烟配置均位于本次独立测试目录；通过进程级 `RUSTUP_HOME`、`CARGO_HOME`、`CARGO_TARGET_DIR`、`PATH`、`RUSTC` 和 `RUSTDOC` 指定，不更新用户原有默认 Rust 1.84.1。
- 没有安装系统软件、提升权限、创建 systemd unit 或连接真实 Destination。TUI 冒烟通过独立 `XDG_CONFIG_HOME` 加载空项目列表。
- 验证结束后，确认无本轮构建/测试进程，再删除经路径与所有者校验的独立目录（约 3.7 GiB 工具链、缓存及输出，可重新生成）。再次检查用户默认工具链仍为原有 stable / Rust 1.84.1。

## 审查修正

1. 两个 Unix 继承管道用例原先只检查超时/取消结果，可能靠两秒排空宽限截断输出而通过。现要求 stdout/stderr 都正常 EOF；受控后台 `sleep 20` 在四秒测试上限内必须结束持有管道。
2. 协议测试增加 180 秒异步期限，较大的等待 future 放到堆上，保持严格 Clippy 通过。未删减测试操作或断言。
3. 夹具 Git 初始化改为 Tokio 子进程，每条十秒限时、空 stdin、丢弃时终止。仅初始化子进程清除继承的 `GIT_*`，禁用 system/global 配置并使用空 template/hooks；临时仓库额外禁用 fsmonitor。自有 `exit 23` hook 被放入默认 hooks 目录，提交成功验证它未执行。生产 Git 行为未修改。

## 验证结果

在具备 Rust 1.96.1、Git 和 C 编译器的 Linux 开发环境中复现：

```sh
cargo test --locked --all-targets --all-features --quiet
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo build --locked --release
```

| 项目 | 结果 |
| --- | --- |
| Linux 单元测试 | 291 项通过，包含两个真实 Unix 继承管道用例 |
| Linux 协议集成测试 | 最终提交前复跑通过，耗时 85.06 秒；全部操作和断言保留 |
| Linux 严格 Clippy / 格式检查 / release 构建 | 通过 |
| Windows 回归 | 287 项单测及 1 项协议测试通过，严格 Clippy、格式检查和 release 构建通过 |
| 真实 TUI 启动/退出 | 发布构建在 PTY 显示空项目列表；按 `q` 正常退出，返回码 0，前后 `stty -g` 完全一致 |
| 依赖审计 | Windows `cargo audit` 退出码 0，无已知漏洞报告；保留 `wnaf 0.14.0` 撤回警告 |

两项外部 SSH 用例在默认套件中继续忽略；本记录不把模拟远端命令的协议测试算作真实 Linux 部署或 systemd 验收。双端真实部署证据见 [真实 Linux 部署验收](linux-ssh-acceptance.md)。

## 本记录之外的验收

- 本客户端验证不包含真实 systemd；用户随后授权准备临时环境，该部分已由 Windows 客户端驱动的 [WSL systemd 验收](systemd-acceptance.md) 补齐。
- 本记录未验证 macOS、其他 Linux 发行版、声明的最低 Rust 1.88 或完整 SSH Agent/Host Key 轮换矩阵。现行 MVP 仅支持 Windows，本记录保留为历史证据，不构成待补的平台门禁。
- 活动部署中的退出、错误和 panic 的实机终端恢复，以及全部 TUI 页面视觉交互。此处只做了空项目列表的正常退出冒烟；后续信号与退出生命周期证据见 [TUI-05 验收](tui-05.md)。
