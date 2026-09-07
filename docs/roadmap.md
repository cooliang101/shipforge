# ShipForge Roadmap

2026-09-07 产品方向调整：全面采用 [原目录发布](deployment-contract.md)，兼容既有程序发布方式。旧版本目录、迁移、远端多版本保留和备份管理工作包停止，不再作为后续路线。

## 已实现

- TUI 项目/组件/环境配置，连接管理、Host Key 确认、密码/私钥/Agent 登录。
- 本地构建与 tar.gz Release 打包、SSH/SFTP 上传校验。
- 既有 root 内应用文件发布，唯一上一版应用压缩包，已知失败恢复。
- 现有服务 argv、systemd 预设、保存密码 sudo；不修改 sudoers 或服务器配置。
- 本地历史、日志、取消、终端恢复、只读核实及有证据的恢复。
- 新驱动的临时目录测试与真实回环 SSH/SFTP 测试。见 [当前验证](validation/inplace-deployment.md)。

## 本次完成

- 已用新版 ShipForge 在 aiagent 的既有 server/website 目录完成业务发布和接口检查。
- 已完成最终 release 终端冒烟，并记录实际部署结果。

## 后续

- 扩展真实 Linux 服务故障、进程中断和文件系统持久化故障覆盖。
- 完成 Windows 发行包、安全检查和最终 MVP 验收。
- 持续改善 TUI 输入与诊断，不增加数据库/附件/运行数据管理职责。

旧 M1/M2/M3 和 QA 文档保留为历史证据，不代表新原目录驱动已获得旧协议的全部真实服务器验收覆盖。
