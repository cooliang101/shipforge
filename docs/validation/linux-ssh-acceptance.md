# 真实 Linux 部署验收记录

日期：2026-09-04。范围：M1 的双 Destination 发布、回滚和补偿实测；**不代表 M1 或 MVP 整体完成**。

## 环境与复现

- 客户端：Windows / PowerShell 7.6.5 / Rust 1.96.1，运行生产 `DeploymentService`、编排器和 `LinuxSshDriver`。
- 目标：WSL Ubuntu-22.04 中本地 Docker 提供两个一次性 Debian bookworm / OpenSSH 9.2p1 容器；独立 Host Key、仅回环地址的随机 SSH 端口，无特权模式。
- 临时身份仅用于此次测试；不读取真实 Destination 或凭据注册表。每个端点须通过独立固定指纹认证和夹具标记检查后才能写入。
- 执行 `./tests/run-linux-acceptance.ps1`；详情见 [测试指南](../../tests/README.md)。默认 `cargo test` 忽略外部环境测试。

## 实测范围

| 场景 | 检查内容 |
| --- | --- |
| 首次仅发布 frontend | 仅一项结果，backend 仍为未部署 |
| 跨 Destination 联合发布 | 两项成功，真实 SFTP、SHA-256、解压、manifest 与 current 观察 |
| 显式回滚 | frontend 恢复历史 Release，backend 恢复未部署；保存关联 Rollback Deployment |
| backend HTTP 健康失败 | frontend 已完成激活；backend 返回 HTTP 503，两者恢复到激活前版本 |
| 后一组件激活前取消 | frontend 完成激活后在 backend 开始事件取消；frontend 补偿，最终版本恢复 |
| 持久化终态 | 无 pending 意图、无 created/running Deployment，保留回滚关联 |

HTTP 由目标内的真实 Python 服务提供，根据实际 `current/health.txt` 返回状态；SSH 命令、SFTP 与远端文件系统均不是模拟实现。本地 payload 预先创建，构建命令为 `rustc --version`，因此不将此用例视为应用二进制构建验收。

## 审查与门禁

首次完整实测通过，耗时 242.53 秒。独立复审后补强了 backend 的 Activate/health/HTTP 503 原因、已补偿诊断和取消事件顺序断言；清理改为逐资源继续尝试并聚合错误，保留原操作错误且保证释放 WSL 保活进程。补强后的完整复跑通过，耗时 **239.94 秒**。

| 门禁 | 结果 |
| --- | --- |
| `./tests/run-linux-acceptance.ps1` | 1 项真实双端部署验收通过，清理成功 |
| `./tests/run-linux-acceptance-cleanup-tests.ps1` | 原生命令退出码及 6 个隔离清理场景通过；接入 Windows CI |
| `cargo test --all-targets --all-features --quiet` | 287 项单测和 1 项协议集成测试通过；2 项外部环境测试默认忽略，双端用例已单独显式执行 |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --all-targets --all-features -- -D warnings` | 通过 |
| `cargo build --release` | Windows 构建通过 |
| `cargo audit` | 退出码 0，未报告已知漏洞；仍有 `wnaf 0.14.0` 撤回警告，须在发布前跟进 |
| `git diff --check` / 文档相对链接检查 | 通过 |

上述命令均在本机执行；CI 步骤已配置，但本记录不宣称远端 CI 或其他平台执行通过。夹具文件固定 LF 行尾，避免 Windows checkout 改成 CRLF 后导致 Linux 启动脚本失败。

WSL 的前台输入管道只用于保证 Windows 测试运行期间实例不退出，不修改 WSL 或 Docker 配置。清理限于本轮带匹配标签的容器、唯一镜像标签和精确临时密钥目录；Docker 构建缓存可能保留。

## 未完成的验收

- 容器不运行 systemd。本次只证明 HTTP 健康检查；真实服务重启、无端口 Worker 稳定窗口和不稳定服务的恢复仍待执行。
- WSL 已有运行中的 systemd，但缺少可用 SSH 服务端，且非交互 sudo 不可用。安装及临时系统服务变更须先获用户确认，不能以特权容器替代授权。
- Unix 专用子进程回归与 WSL Linux 的编译/正常退出冒烟已由后续 [Linux 客户端验证](linux-client.md) 补齐；macOS、其他 Linux 环境、SSH Agent 和 Host Key 轮换等完整平台矩阵仍属 QA-01。
- TUI 视觉交互、M2 历史/恢复/保留及 M3/M4 工作不由此测试证明完成。
