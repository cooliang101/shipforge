# 原目录发布替换验证

2026-09-07。取代旧 releases/current 驱动，不新增发布模式或迁移选项。

## 代码与测试

- 已删除旧 activation、prepare、Marker、audit、inventory、remnants、retention、space/preflight 实现及依赖旧布局的验收入口。
- 新 linux-ssh 驱动通过 SSH 执行内置 Python 3 标准库发布器，SFTP 上传并校验产物。
- 保留项目/环境/组件身份、固定 Host Key、密码/密钥认证、保存密码 sudo、冻结命令、历史记录、取消及未知结果保护。
- 原目录逐文件替换，运行数据范围之外不读取内容；只压缩应用产物对应的上一版范围。首次范围由产物顶层条目决定，后续合并已记录范围。
- 回环 SSH/SFTP 测试执行生产驱动和真实 Python 文件操作，验证正常发布、启动明确失败后的恢复，以及服务无退出状态时阻断竞争恢复。密码与 sudo 回归继续运行。
- Python 隔离测试覆盖上传损坏、旧包损坏、身份/版本错配、硬链接、路径遍历、外部修改、服务阶段恢复、单个上一版、旧静态资源移除、操作中断及按操作身份丢弃上传。
- 全量默认 Rust 测试通过（库与集成测试分别运行，需显式环境的用例按原约定忽略），Clippy 无警告；release 构建成功；两个 release ConPTY 退出/终端恢复冒烟均通过。补充回归验证多组件准备失败或取消后以独立令牌丢弃未应用上传。

## 边界

2026-09-08 的重新打包与 WSL 文件/中断故障增量验证见 [WSL 异常验收](wsl-inplace-faults.md)。其中发布器子进程测试不作为真实服务或 SSH 断连验收。

不管理数据库、附件、日志、Nginx/systemd 配置或权限策略；不执行多版本远端保留。产物必须仅含应用文件。逐文件替换不等于整个目录原子切换，不保证同权限恶意进程并发替换目录时的隔离。未知远端执行结果保留现场，需先核实再处理；旧协议历史仅可查看，不自动迁移或跨协议恢复。

## aiagent 实测

目标为 tomato@10.0.0.206 的 `/home/tomato/aigenvideo/server` 和 `/home/tomato/aigenvideo/website`。通过 TUI 保存前端 PowerShell/npm.cmd 构建配置，两个组件均使用现有目录。实际部署 `dep_01a07b4fa9e17f3093b8d8d2900a4c57` 成功：

- server：`1788775190-01a07b4f4ef97b018e87562690012bc9`，一个程序文件。
- website：`1788775190-01a07b4f51567002abd4c3f56113e7ee`，429 个静态应用文件。
- 远端全部已发布文件摘要与发布状态一致；后端二进制和前端 index.html 的 SHA-256 另与本地构建输出逐项一致。
- `aigenvideo.service` active/running，PID 2611933；无 DropInPaths，WorkingDirectory 保持原空值，仍由原启动脚本进入原目录。
- 9901/9902 首页均 HTTP 200；8120 `/v1/user/balance` 未登录返回预期 403。未执行登录或业务数据写入测试。
- 两个组件均为 stable，incoming 包已清除。上一版包分别为 server/.shipforge-deploy/previous.tar.gz（18,219,341 字节）和 website/.shipforge-deploy/previous.tar.gz（4,279,938 字节）。后端包仅含二进制，前端包仅含应用静态文件。
- 在逐文件校验旧手工应用压缩包与新上一版包内容完全一致后，移除 shipforge-backups 中的重复应用包和空目录。每个组件只留一个上一版应用包。
- 本次未修改数据库、附件、日志、Nginx/systemd 配置或权限政策。
