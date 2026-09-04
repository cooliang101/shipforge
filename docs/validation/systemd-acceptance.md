# 真实 systemd 验收与 M1 门禁闭合

日期：2026-09-04。结论：真实 systemd 工作包验收通过；结合既有双 Destination 与本地回归证据，M1 安全部署闭环通过。M2/M3/M4 和生产发布验收仍未完成。

## 环境与复现

- Windows / PowerShell 7.6.5 / Rust 1.96.1 客户端，调用生产 `DeploymentService`、编排器与 `LinuxSshDriver`，没有修改生产代码。
- WSL Ubuntu-22.04，OpenSSH `8.9p1-3ubuntu0.16`，systemd `249.11-0ubuntu3.22`。用户授权安装 SSH 服务端并继续测试；本夹具不启用默认 `ssh.service`、`ssh.socket`，测试前后均确认两者关闭和禁用。
- 执行 `./tests/run-systemd-acceptance.ps1 -Distribution Ubuntu-22.04`；权限、前置条件和清理边界见 [测试指南](../../tests/README.md)。这是开发测试夹具，不是产品 CLI。
- 独立回环 SSH 端口、临时 Ed25519 身份和固定 Host Key；控制连接使用 root，Worker 使用 `nobody:nogroup`，无 HTTP 检查或网络端口。未解锁 root、修改 PAM/polkit 或现有业务服务。

## 实测结果

完整用例通过，耗时 **257.96 秒**；所有临时服务、单独授权公钥、测试 Host Key、Release 目录和 Windows 身份目录均已清理。

| 场景 | 实际检查 |
| --- | --- |
| 首次仅发布 worker | current 与 manifest 正确，另一 Component 未部署，服务通过默认 10 秒稳定窗口 |
| 健康更新 | 新版本实际运行，启动证据包含该 payload 的唯一标记 |
| 显式回滚旧版本 | current 恢复原 Release，旧 payload 重新运行并独立通过稳定检查 |
| 不稳定更新 | 新脚本主动退出；健康阶段判定不稳定并补偿，旧版本与旧进程恢复 |
| 首次不稳定部署 | 恢复无 current，服务 inactive、MainPID=0，等待后仍未重启 |
| 显式回滚未部署 | 正常运行的 worker 被停止，current 移除，无残留主进程 |
| 持久化 | 4 次部署、2 次关联回滚；2 条 failed、4 条 succeeded，无 pending 意图或未结束 Deployment |

本轮更新失败捕获 `active=false`；首次失败捕获 `active=true` 且 `NRestarts` 从 0 增至 1。两次均有独立证据证明不稳定脚本启动两次、主动退出一次。失败断言限定在 Activate/health 和 systemd 不稳定诊断，不接受认证、上传或启动命令错误作为替代。

自动补偿本身恢复链接并重启服务，不再次运行健康窗口；测试在补偿返回后额外执行完整稳定检查，并核对实际进程及 payload 标记。构建命令为 `rustc --version`，payload 是测试生成的 Shell 脚本，不代表业务应用构建验收。

## 审查修正与安全清理

独立审查补强了临时目录创建归属、错误聚合、进程环境恢复、PAM 会话身份核验、监听消失检查及部分初始化失败清理。实测修正了 systemd 249 的 `loginctl` 多属性参数、StrictModes 所需的 `/run` 公钥路径，以及会隐藏测试 Release 的 `PrivateTmp` 设置；没有放宽 StrictModes、root 账号或宿主目录权限。

每轮只创建带随机 run ID 的临时系统单元；Worker 绑定到最多运行 15 分钟且不自动重启的 SSH 测试单元。清理前核对 marker、单元内容/来源和会话身份；无法归属的对象保留现场并报错，不停止其他服务。结束后再次确认无测试监听、sshd 进程、运行时单元或私钥目录，root 仍锁定。OpenSSH 安装、共享运行时目录 `/run/sshd` 及正常认证/系统日志保留。

## 质量门禁

| 检查 | 结果 |
| --- | --- |
| 真实 WSL systemd 用例 | 1 项通过，清理成功 |
| Python 夹具安全回归 | 32 项通过；模拟系统操作，仅使用自有临时文件 |
| PowerShell systemd 清理回归 | 原生命令检查及 22 个内存场景通过 |
| 既有双端清理回归 | 原生命令检查及 6 个内存场景通过 |
| `cargo test --locked --all-targets --all-features --quiet` | Windows 287 项单测及 1 项协议测试通过；3 项外部用例默认忽略，本 systemd 用例单独实测 |
| rustfmt / 严格 Clippy / Windows release 构建 | 通过 |
| `cargo audit` | 退出码 0，无已知漏洞报告；仍有 `wnaf 0.14.0` 撤回警告 |
| `git diff --check` / Markdown 相对链接 | 通过 |

安全回归已接入 CI，但本记录不宣称远端 CI 已运行。此前通过的 [双 Destination 实测](linux-ssh-acceptance.md) 与 [Linux 客户端验证](linux-client.md) 保持各自历史范围，本次未重跑其完整实机矩阵。

## M1 门槛对应证据

| 路线图门槛 | 验证入口 |
| --- | --- |
| 副作用前持久化意图 | `history/store.rs::pending_intent_survives_reopen`；`application/deployment.rs::failing_build_has_a_durable_intent_and_terminal_deployment`；编排/回滚路径先记录意图再调用 Driver |
| revision、端点与 generation 校验 | `application/execution_guard/tests.rs`：变更后底层写调用被拒绝，补偿漂移保留人工恢复结果 |
| 同一 TUI 会话单任务 | `application/session.rs::rejects_a_second_operation_without_polling_it` 及完成、错误、取消释放许可回归 |
| 上传/哈希失败不切换 current | `src/drivers/linux_ssh/prepare.rs` 暂存清理回归、真实协议故障注入和 `prepare_failure_prevents_every_activation` |
| 首次可选 Component 子集 | 双 Destination 实测及生产应用服务协议夹具验证未选 Component 不构建、不发布 |
| 无端口 Worker 稳定性 | 本记录的真实 systemd 激活、稳定窗口和运行版本证据 |
| 失败补偿及人工指引 | 双端 HTTP/取消实测、本 systemd 实测，以及 `failed_compensation_reports_manual_action_without_hiding_actual_current` 等失败恢复回归 |

下一阶段是 M2 历史、对账与保留。普通非 root 部署账号授权、macOS/完整平台矩阵、最低 Rust 版本、SSH Agent/Host Key 轮换、全部 TUI 实机交互和异常退出仍需后续验证；不能将 M1 通过等同于 MVP 可发布。
