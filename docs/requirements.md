# ShipForge Requirements

ShipForge 是 Windows x64 GNU 的终端部署助手，将本地应用构建产物发布到已有 Linux 服务器。产品职责由 [部署助手职责](deployment-contract.md) 定义，配置唯一入口是 [configuration-guide.md](configuration-guide.md)。

## 核心流程

用户通过 TUI 选择项目、Component、Environment 和 SSH 连接，配置既有发布目录、构建 argv、artifact 及可选服务命令。配置预览确认后保存；部署计划单独展示目标、应用版本、服务命令及检查，确认后执行。

部署执行构建、tar.gz 打包、SSH/SFTP 上传校验、上一版应用压缩、原目录发布、现有服务启停、检查。明确发布失败时恢复上一版应用压缩包并执行恢复命令；不能确认执行结果时保留现场先核实。

## 应用与运行数据

- 不强制空目录、目录迁移、releases/current、启动脚本改造、Nginx/systemd 修改或新增权限。
- 不管理数据库、附件、日志及运行数据的备份、迁移、恢复或清理。
- 每个组件仅保留上一版应用 tar.gz，成功后清除本次临时上传包；不提供多版本备份保留策略。
- 单文件产物覆盖同名程序；目录产物将内容发布到 root。首次范围按产物顶层条目确定，后续合并已知应用范围以移除过期静态资源。
- 构建产物必须只包含应用文件；不能把混有数据的目录作为应用产物。范围之外文件保持原样，不递归删除整个部署目录。
- 单个文件可用临时文件替换；全目录发布不承诺原子切换。

## 配置、连接与服务

项目配置由 TUI 管理，不提供 Agent 编辑或额外 YAML 形式。保留 `_shipforge` 中的稳定 ID、generation 和固化 root。密码/私钥正文不能进入项目文件。用户级连接可复用，但服务命令始终按 Environment/Component 独立配置。

SSH 支持密码、私钥和 SSH Agent。首次 Host Key 明确确认；密码使用 Windows DPAPI 加密保存。支持账号现有 sudo 权限和保存登录密码的规范 sudo 调用，不修改服务器权限政策。

服务统一 schema 2 argv，systemd 是同一执行器的预设。start、stop、update、restore、只读检查在既有 root 执行；update/restore 可按规范复用。不自动安装依赖或管理服务配置文件。

## 记录和交互

保留本地部署历史、日志检索/筛选、经过确认的本地导出、已记录失败命令诊断、只读状态核实，以及有证据支持的上一版恢复。旧协议历史可以查看，不自动迁移或跨协议执行恢复。

TUI 需保持键盘可用、窗口和日志内存有界；后台任务可取消且退出前 join。每个可处理退出、错误和可捕获 panic 恢复终端。没有后台脱离运行模式。

## 验证与范围

每个修复有回归测试，发布前通过 fmt、Clippy、Rust 测试及 release 构建；隔离测试禁止连接生产。客户端仅 Windows x64 GNU；不新增工具链或 GitHub 测试流程。实际服务部署需使用已确认目标并记录结果，不把模拟测试当作真实 systemd 验收。
