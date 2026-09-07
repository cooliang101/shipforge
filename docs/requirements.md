# ShipForge 产品需求文档

## 1. 文档信息

- 产品名称：ShipForge
- 产品类型：本地运行的前后端应用发布部署助手
- 目标形态：交互式终端界面（TUI）
- 当前阶段：MVP 实施 / M1、M2、M3 与 QA-01 已按原范围验收；SVC-01 已完成验收，下一工作包为 QA-02 发布加固。完整 MVP 尚未完成，早期 SSH 超时仍未定位（证据见路线图）
- 文档版本：v0.18
- 更新日期：2026-09-07

服务命令采用 schema 2 的统一 `service` 配置；systemd 由 TUI 生成同一命令计划。schema 1 只支持经 TUI 预览确认的转换，确认前不允许部署，不保留永久双格式。具体行为见配置指南。

## 2. 产品背景

前后端应用发布到普通 Linux 服务器时，开发者通常需要重复执行本地构建、产物压缩、SSH 登录、SCP/SFTP 上传、远程解压、服务重启和健康检查等操作。这类流程依赖人工记忆命令，容易出现路径错误、漏传文件、覆盖线上目录、无法快速回滚以及发布记录缺失等问题。

ShipForge 用项目配置描述 Component 和 Environment，并从用户级 Destination 注册表解析可复用的服务器连接。每个 Environment 直接配置各 Component 的 Destination ID 和部署设置。MVP 使用内置 `linux-ssh` Driver，在本地构建后通过 SSH/SFTP 部署到无需常驻服务的 Linux 服务器。用户通过 TUI 完成项目、连接、环境、组件、发布和历史 Release 操作；无头 CLI、脚本、CI 和 AI Agent 调用均不进入 MVP，也不预留配置或协议。

## 3. 产品目标

### 3.1 核心目标

1. 将一次发布收敛为“选择项目与环境、确认部署”的标准流程。
2. 支持前端、后端或二者联合构建与部署。
3. 通过统一驱动协议承载不同部署端；Linux SSH 驱动使用版本目录和软链接实现原子发布。
4. 完整记录部署步骤、命令输出、版本信息和操作结果。
5. 支持历史版本查看和一键应用回滚。
6. 在上传、启动或健康检查失败时，尽可能恢复到上一个稳定版本。
7. 保持部署编排与 SSH、供应商 API 等具体通信方式解耦。

### 3.2 非目标

MVP 阶段暂不建设完整 CI/CD 平台，不包含：

- Kubernetes 发布；
- 多服务器滚动部署；
- Docker 镜像构建与镜像仓库管理；
- 团队账号、审批和复杂权限体系；
- 数据库迁移编排；
- 远端依赖安装编排；
- 云厂商资源编排；
- 目标服务器常驻 Agent；
- 无头 CLI、脚本和 CI/CD 调用接口；
- AI Agent 调用、AI Agent 直接修改项目配置及其他自动化控制接口；该方向冻结，不预留配置、协议或路线图工作包。
- 多进程并发部署协调；MVP 假定同一项目环境一次只有一个操作者发起部署。
- Cloudflare、Vercel 等托管平台 Driver，以及同一 Component 的多服务器复制与滚动发布。
- SSH `ProxyJump`、`Include`/`Match` 完整解释和共享持久目录映射。

## 4. 目标用户与使用场景

### 4.1 目标用户

- 独立开发者；
- 小型研发团队；
- 使用一台或少量 Linux 服务器承载应用的团队；
- 希望替代手工 SSH/SCP 发布，但暂不需要大型 CI/CD 平台的用户。

### 4.2 典型场景

1. 将本地前端构建输出部署到 Nginx 静态目录。
2. 构建后端二进制或应用包，上传后执行自定义远端服务命令（如 PM2），或选择 systemd 内置预设。
3. 同时发布同一项目的前端和后端组件。
4. 查看某次失败发布的本地构建及远程执行日志。
5. 从当前线上版本回滚到某个历史稳定版本。

## 5. MVP 范围

MVP 仅支持以下范围：

- 本地运行环境：仅支持 Windows x64 客户端，使用 GNU/MinGW 构建；Linux/macOS 客户端不进入 MVP；
- Destination：用户级注册表可保存多套可复用 `linux-ssh` 连接，并可被多个项目引用；
- 部署映射：每个 Environment 直接配置需要部署的 Component；每个 Component 在该 Environment 中指向一个 Destination；
- 登录方式：SSH Key，优先使用 SSH Agent 和用户 SSH 配置；
- 服务器数量：每个 `linux-ssh` Destination 对应一台服务器；一个 Component 不支持同时部署到多台服务器；
- 并发模型：当前 TUI 会话同一时间只允许一个活动 Deployment；多个 ShipForge 进程同时部署不受支持，也不实现协调机制；
- 驱动方式：内置 `linux-ssh` 驱动，传输使用 SFTP，Release 格式为 `tar.gz`；
- 服务管理：可选自定义远端服务命令，systemd 作为同一机制上的内置预设；不是仅支持 systemd；
- 发布策略：每个 Component 一个带版本号和 SHA-256 的 `tar.gz` Release，解压后通过该 Component 的 `current` 软链接原子切换；
- 健康检查：Destination 端 HTTP/HTTPS、可选只读命令检查和 systemd 预设稳定性检查；
- 版本管理：每个 Component 默认保留最新 5 个 Release，并额外保护当前版本、上一个健康版本和未结束/待核实操作引用；
- 操作方式：Ratatui TUI；启动可执行文件后，所有用户操作均在界面内完成。

## 6. 核心用户流程

### 6.1 首次设置与再次打开

1. 用户选择项目根目录；系统规范化路径并检查根目录下的 `shipforge.yaml`。
2. 配置存在时直接加载并校验其中的用户配置和 `_shipforge` 系统维护区；除缺少本机 Destination、配置不兼容或远端状态变化外，不重复询问已保存信息。
3. 配置不存在时，该目录视为新 Project。系统扫描项目清单和构建脚本，展示推断的 Component、构建命令及构建输出路径，用户通过勾选和修改确认；没有候选或发现失败时可以手动添加，随后生成全新的稳定 ID。
4. 系统展示已有 Destination 和 SSH config Host；用户选择现有项，或新建 SSH 连接并从 SSH Agent、`IdentityFile`、标准 Key 候选或文件选择器中选择身份。
5. 私钥内容及个人 Key 路径只进入用户级凭据引用，不写入项目文件。首次连接必须展示并确认 SSH Host Key 指纹。
6. 连接成功后，用户选择 Environment、要部署的 Component、各 Component 使用的 Destination、部署目录及服务方式（不管理、自定义命令或 systemd 预设）；选择 systemd 时优先提供 unit 候选并自动填充命令和检查规则。
7. 系统展示规范化配置预览。用户确认后，在项目根目录原子写入唯一的 `shipforge.yaml`，并将项目目录登记到本机项目注册表；部署预检、计划与执行另行确认。
8. 用户取消时不得保存未确认的项目草稿或 Destination；先前已经明确确认并保存的连接保留。写入结果不确定时明确要求重新加载，不将其误报为未写入或盲目回退其他文件。

### 6.2 MVP `linux-ssh` 部署流程

1. 用户启动 ShipForge。
2. 系统读取 `shipforge.yaml` 的用户配置与 `_shipforge` 系统维护区，并解析用户级 Destination 注册表，完成 Project/Environment ID、Destination revision 及 Component generation 校验。
3. 用户选择项目和目标环境。
4. 用户选择部署前端、后端或全部组件；系统据此解析各 Component 的部署目标。
5. 系统检查 Git 状态、构建工具、SSH 连接和远端目录权限。
6. 系统展示本次部署计划，包括各 Component、Destination、root、执行顺序、命令和当前版本。
7. 用户确认发布。
8. 系统执行本地构建并收集 `artifact` 指向的构建输出。
9. 系统为每个所选 Component 生成 Release 版本号、`tar.gz`、SHA-256 和最小 manifest，并记录 Deployment ID。
10. 系统将各 Release 上传到对应 Component root 的临时目录并校验完整性。
11. `linux-ssh` Driver 保存带版本号的压缩包，并解压到该 Component 的版本目录。
12. 所有选中 Release 准备完成后，系统按计划逐一切换各 Component 的 `current` 软链接并重启其服务。
13. 系统执行本次所选 Component 的健康检查。
14. 全部成功且结果落盘后，按预览提示尝试清理所选 Component 的过期版本；清理失败只警告，不撤销成功部署。激活或健康检查失败时，按实际激活顺序逆序恢复已操作 Component，并记录逐 Component 结果或需人工介入。

### 6.3 MVP `linux-ssh` 回滚流程

1. 用户选择项目和环境。
2. 系统按 Component 展示当前版本及可回滚的 Release 压缩包。
3. 用户选择一个或多个 Component 及各自的目标版本，并确认 Destination。
4. 系统记录各 Component 当前版本，按计划切换到所选 Release 的解压目录。
5. 系统逐 Component 重启服务并执行健康检查。
6. 成功后记录回滚结果；失败时逆序恢复已切换 Component，并保留逐 Component 结果。

## 7. 功能需求

### 7.1 项目与环境配置

- `docs/configuration-guide.md` 是项目配置的规范入口；MVP 配置只能由 TUI 创建和修改。
- 使用项目内的 `shipforge.yaml` 描述用户部署意图；顶层 `schemaVersion` 表示配置格式版本，不得与应用版本混用。
- `shipforge.yaml` 必须位于所选项目根目录；再次打开同一目录时直接读取，不得要求用户重新选择已经持久化的部署映射。
- `shipforge.yaml` 的 `_shipforge` 系统维护区保存 Project、Environment 的稳定 ID，以及各 Environment/Component 的 generation、固化 root 和规范化摘要；它由 TUI 自动创建和更新，不得绕过 TUI 手工填写。
- `shipforge.yaml` 是 Project 身份与项目部署意图的唯一来源。文件缺失时必须按新 Project 初始化，不得从本地缓存或远端推断旧配置；文件存在但 `_shipforge` 缺失或损坏时必须停止，只有用户明确确认重新初始化后才能生成全新身份。
- 一个项目可包含多个可独立部署的组件。
- 一个项目可配置测试、预发布和生产等多个环境。
- 用户级 Destination 注册表以系统生成的不可变 `dst_…` ID 保存连接类型、主机、端口、用户、revision 和凭据引用；同一 Destination 可被多个 Project 使用。
- Environment 直接列出该环境需要部署的 Component；每项通过 Destination ID 指定连接，并配置该连接支持的部署设置。
- Environment 不保存主机、凭据或连接实现详情；这些信息只存在于用户级 Destination 注册表。同一 Destination 可供多个 Project/Component 复用。
- Project、Environment 和 Component 直接显示配置名称；Destination 由 TUI 根据 `user@host:port` 生成显示摘要，用户不用再填写名称。
- 同一 Component 在一个 Environment 中只允许一个 Destination；MVP 不支持单 Component 多目标复制。不同 Component 可引用同一 Destination。
- 顶层 Component 配置只描述工作目录、构建和 `artifact` 构建输出路径；Environment 中的 Component 条目描述部署目标与运行方式。
- Project 和 Environment 具有创建后不变的 ID，保存在 `_shipforge`。Destination ID 由系统生成且不可改名；Component 名称是其配置身份，重命名按删除旧项并新增项处理。
- YAML 映射顺序不得影响执行语义；`after` 只排列本次同时选中的 Component，不自动加入未选择的依赖项。规划器执行拓扑排序并对无依赖项采用稳定排序。
- TUI 只写入一种 YAML 结构：`build` 是按顺序执行的 argv 数组列表，`artifact` 是一个相对路径；MVP 不提供 Shell 字符串或长短格式。默认值和依赖作用域以 `docs/configuration-guide.md` 为准。`artifact` 的文件或目录类型在构建后自动识别，不提供需要用户填写的类型字段。
- 配置校验不得修改文件；只有用户在 TUI 确认变更后，系统才原子替换 `shipforge.yaml`。
- 启动部署前必须执行配置结构校验，并给出可定位到字段的错误信息。
- 配置文件不得直接保存密码、私钥正文或其他明文密钥。
- 本地构建只使用“程序 + 参数数组”，不接受 Shell 字符串；项目需提供适用于当前本地平台的命令。
- 用户无需填写 Unix 权限。单文件 `artifact` 按 `0755` 打包；目录按 `0755`、普通文件按 `0644` 打包，在 Unix 构建机上仅保留源文件“是否可执行”这一位语义。MVP 不提供任意权限映射。

当前实现的固定格式如下，完整字段、默认值及旧配置转换规则见 `docs/configuration-guide.md`：

```yaml
schemaVersion: 2

_shipforge:
  projectId: prj_01J8MALL4Y2K6M7P
  environments:
    production:
      id: env_01J8PROD8V5F3Q1N
      components:
        frontend:
          generation: 1
          resolvedRoot: /srv/shipforge/mall/production/frontend
        backend:
          generation: 1
          resolvedRoot: /srv/shipforge/mall/production/backend
        worker:
          generation: 1
          resolvedRoot: /srv/shipforge/mall/production/worker

project: mall

components:
  frontend:
    build:
      - [npm, ci]
      - [npm, run, build]
    artifact: dist

  backend:
    build:
      - [go, build, -o, build/server, ./cmd/server]
    artifact: build/server

  worker:
    build:
      - [go, build, -o, build/worker, ./cmd/worker]
    artifact: build/worker

environments:
  production:
    components:
      frontend:
        to: dst_00000000000000000000000000000001
      backend:
        to: dst_00000000000000000000000000000002
        service:
          start:
            - [systemctl, restart, --, mall-api.service]
          stop:
            - [systemctl, stop, --, mall-api.service]
          check:
            kind: systemd
            unit: mall-api.service
        health: http://127.0.0.1:8080/health
      worker:
        to: dst_00000000000000000000000000000002
        service:
          start:
            - [systemctl, restart, --, mall-worker.service]
          stop:
            - [systemctl, stop, --, mall-worker.service]
          check:
            kind: systemd
            unit: mall-worker.service
        after: [backend]
```

Destination 通过 TUI 连接管理页写入用户级注册表，系统自动生成 ID，项目配置以 `to` 引用。向导可从 SSH config 的直接 `Host` 候选中取值，或在“新建连接”表单中录入 `deploy@web.example.com`；用户无需命名连接。

### 7.2 Destination 与内部部署驱动

- 应用层只通过统一 Deployment Driver SPI 发起预检、规划、准备、激活、观察、库存查询、回滚、日志和清理操作，不得直接依赖 SSH/SFTP 或供应商客户端。
- Driver 是内部代码边界，不是用户概念。TUI 使用“SSH 连接”和“部署目标”，`shipforge.yaml` 不出现 Driver 类型、能力名、供应商对象或传输参数。内部类型和诊断步骤可标识 Driver，但普通 TUI 文案不得要求用户理解它。
- MVP Driver 只声明已经实现的准备、激活、回滚、观察、库存查询、保留清理和取消等能力；远端日志尚不声明。本地构建与标准 Release 归档属于 Driver 之前的通用流程。托管平台所需能力在对应 Driver 立项时扩展。
- 当发布策略需要 Driver 不支持的能力时，生成计划阶段必须失败并给出原因。
- 用户级 Destination 记录以内部类型标签选择实现，并由对应 Driver 执行字段级校验；Environment/Component 的项目专属字段在解析 Destination 后联合校验。内部标签不得复制到项目 YAML。
- Driver 的每个读取或副作用操作都必须接收同一个不可序列化 `ComponentExecutionContext`。通用部分只包含 Project/Environment ID、Component 名称与 generation、Destination ID/revision 和端点指纹；凭据及经 Driver 校验的非秘密目标配置以不透明句柄传入。host、port、user、Host Key、远端 root 和 systemd 等字段只由 `linux-ssh` 解释，应用层不得读取或复制这些 Driver 专属字段。
- Driver 返回的 Component Release 引用只包含 ShipForge 通用身份、版本、Destination、端点和能力快照。供应商外部 ID、SDK 对象、远端路径或 URL 不进入 MVP 通用模型。
- Driver 能力分为静态能力和预检后得到的 Component 目标有效能力；计划与 Receipt 保存有效能力快照。
- Driver 步骤必须输出统一结构化事件，但可使用命名空间保留供应商特有阶段。
- MVP Driver 编译进主程序，仅包含 `linux-ssh`；Rust 动态库和外部 Driver 进程协议不进入 MVP。

### 7.3 环境预检

部署前由应用层执行通用检查，并由 Driver 对每个解析后的 Component 目标执行特定检查。MVP `linux-ssh` Driver 必须检查：

- 配置文件是否有效；
- 本地工作目录和构建命令是否存在；
- Git 分支、提交号及工作区是否包含未提交修改；
- SSH 主机是否可连接；
- SSH Host Key 是否可信；
- 远端根目录是否存在且具备写权限；
- Component root 的 Deployment Marker 是否匹配当前 Project/Environment/Component/generation；发生冲突时必须拒绝部署；
- 新 Project 命中旧 Project 的远端目录时不得自动接管；TUI 必须要求改用其他 root，或退出并由用户自行处理旧目录；
- 远端磁盘空间是否足够；
- 发布所需命令是否可用。
- 当前 Destination 端点指纹、Component generation 和远端事实是否仍与计划一致；发生漂移时禁止继续产生副作用。

当 Git 工作区非干净状态时，默认警告并要求确认；未来可支持配置为禁止发布。

### 7.4 本地构建与 Release 打包

- MVP 对每个所选 Component 执行本地构建；`artifact` 始终且只表示单个构建输出路径。
- 选中的 Component 按名称稳定排序后依次构建；同一 Component 内的构建命令严格按列表顺序执行。
- 实时展示 stdout、stderr、步骤状态和耗时。
- 任一必需构建命令失败时立即终止后续发布。
- 配置项 `artifact` 只表示 Component 的一个构建输出路径，可指向文件或目录；类型在构建后自动识别。路径必须存在且非空，其他文件类型拒绝处理。
- 通用归档层将 `artifact` 指向的构建输出恰好打包一次，生成一个不可变的 `<version>.tar.gz` Release。归档内的最小 manifest 记录 Project、Environment、Component、generation、版本、创建时间和源码提交；最终字节生成后再计算整个 Release 的大小与 SHA-256，并保存在部署记录中，避免 manifest 自引用。后续 Driver 只消费该 Release，不得重新打包或定义另一种核心 Release 格式。
- 构建日志必须与本次 Deployment ID 关联。
- 本地命令必须支持超时、取消及子进程树清理；动态参数不得拼接进 Shell 字符串。
- 归档必须规范化路径，拒绝符号链接、特殊文件、保留路径和过量条目；所有 uid/gid、时间戳及 Unix 模式按固定规则写入，不复制主机所有权。

### 7.5 Linux SSH 驱动：连接与传输

- 可从用户 SSH 配置的直接 `Host` 块读取 host、user、port 和 `IdentityFile` 候选；MVP 不解释 `Include`、`Match`、通配 Host 或 `ProxyJump`，缺失值由 TUI 补充。
- 优先支持 SSH Agent，也可指定私钥路径。SSH Agent 仅提供密钥签名，与被冻结的 AI Agent 调用无关。
- 必须校验服务器 Host Key，禁止默认无条件信任未知主机。
- 使用 SFTP 上传到远端临时目录。
- 支持上传进度、超时和有限次数重试。
- 上传后在远端校验 Release SHA-256，校验失败不得激活版本。
- TUI 的环境检查操作必须检查远端 Shell、解压、哈希、软链接、原子重命名、磁盘及权限能力；按配置检查服务命令所需程序和 Destination 端 HTTP 客户端，并报告实际采用的命令。自定义方式不强制依赖 systemd；只读预检不得试运行启动、重启或停止命令。
- Destination ID 创建后不可修改且无需用户命名。Driver 类型、host、port 或 user 变化必须增加 Destination revision 并改变端点指纹；凭据轮换可增加 revision，但不得改变端点指纹。
- Destination 修改操作自动增加 revision；仍被 Release 引用的非秘密历史修订必须保留，以支持历史观察和清理。当前部署使用该 key 的最新 revision，历史 Release 始终使用自身记录的 revision。
- TUI 只能移除没有被已登记 Project、Deployment 或 Release 引用的 Destination；存在引用时必须拒绝并列出引用来源。

### 7.6 Linux SSH Driver：发布执行

- 每次操作生成唯一 Deployment ID；每个所选 Component 独立生成 Release 版本号，压缩包与版本目录必须使用 no-clobber 语义并在冲突时重新生成版本号。
- 修改某 Environment/Component 的 Destination ID、`linux-ssh` root 或其他影响 Driver 目标寻址的项目字段时必须增加其 generation；Destination 自身配置变化由 revision 表达。旧 Release 不得通过新 generation 执行回滚、日志或清理。
- `linux-ssh` 接收通用归档层生成的 `<version>.tar.gz` Release，并记录 Component、版本、SHA-256、大小、创建时间和源代码版本。Driver 不重新打包；远端保留该原始压缩包并将其解压为运行目录，解压目录不是另一种产物或领域版本。
- 首次及后续部署均允许选择任意 Component 子集。未选择的 Component 不构建、不上传、不切换、不启动也不检查。
- Prepare 阶段允许构建以及可安全清理的上传、校验、解压和候选 Release 组装；这些暂存写入必须记录并可重试或清理，但不得改变 `current`、服务、`shared`、数据库或业务状态。
- Activation 阶段只包含 `current` 切换、所选 Component 的服务激活和健康检查。数据库迁移与任意业务数据变更不进入 MVP。
- 每个 Component 使用自己的 `current` 软链接，并以原子方式切换。
- MVP 必须以可配置的远端服务命令为基础，systemd 仅为自动填充命令与检查规则的内置预设；两者使用同一执行、日志、超时、取消及补偿机制，不新增 PM2 Driver。
- 服务命令逐 Environment/Component 保存到项目根目录 `shipforge.yaml`，不放入共享 Destination 或本地构建脚本。TUI 覆盖首次启动、更新、恢复旧版本和恢复未部署状态时停止；可复用相同命令以减少填写，缺少必要恢复动作时在副作用前拒绝计划。
- 命令使用程序与参数数组；工作目录绑定本次激活或恢复的实际版本。命令、工作目录与检查策略必须预览确认并纳入冻结计划；PM2 不能只凭 `current` 变化或进程名重启就假设已使用新代码。
- 服务命令不自动安装远端依赖或提升权限，不允许修改 ShipForge 保留元数据；不编排数据库迁移和任意业务数据变更。有副作用命令失败或结果未知时不能盲目重试，版本观察不等于服务执行成功。
- 每一步都必须记录开始时间、结束时间、状态和输出。
- 任务被中断后，系统应能识别残留临时目录和未完成的发布状态。
- 原子性边界是单个 Component 的 `current` 切换；服务重启、健康检查和多个 Component 均不宣称原子。
- Component 服务命令已知失败时，Driver 恢复其原 current；首次部署则移除新链接并执行停止。命令超时、断线等导致退出结果未知时，不再对该 Component 发起竞争性的自动恢复命令，明确要求人工核实；其他已成功 Component 仍可独立补偿。
- 所有选中 Release 准备成功后，在本次所选 Component 形成的子图中按 `after` 拓扑顺序激活；未选择的依赖项不会自动加入。无依赖项采用稳定排序。任一失败时，对本次已激活 Component 按实际顺序逆序补偿，并持久化逐 Component 结果。

### 7.7 健康检查与自动恢复

- MVP 健康检查属于一个 Component，只对本次选择并激活的 Component 执行。
- 可选 `health` URL 由 Destination 端执行 HTTP/HTTPS 检查，因此可检查仅监听回环地址或内网地址的服务。
- `systemd` 检查在服务达到 active 后记录 `NRestarts` 等基线，并要求 Unit 在 `stableFor` 时间内保持 active 且基线不增加。
- Component 的 `service.check.kind: systemd` 自动加入必需稳定性检查；同时声明 URL `health` 时再加入必需的 Destination 端 HTTP 检查，两项必须全部通过。
- 不对外提供端口的服务可以使用只读命令检查，退出码 0 表示该次检查通过，非零、超时或未知均不通过；systemd 预设继续使用 active/NRestarts 稳定窗口，不退化为一次命令成功。
- 自定义服务不会自动附加 systemd 检查；可选 HTTP 与命令检查按显式配置执行，未配置的检查不得显示为已验证。原 `systemd` 只在 schema 1 确认转换时接受。
- 所有检查支持超时、间隔和重试次数；配置为必需的检查全部通过后，该 Component 才视为健康。
- 一个 Component 的激活和必需健康检查通过后，该 Release 才可记为该 Component 的健康版本。
- 未受本次 Deployment 影响的 Component 保持原状态，不进入本次结果。
- 激活、服务命令已知失败或健康检查失败时，恢复激活前记录的 Component current；原值不存在时恢复未部署并停止服务。结果未知时保留事实与人工指引，不盲目恢复。
- 激活前必须记录每个所选 Component 的原 `current` 目标或“不存在”。
- 若自动回滚也失败，必须输出明确的人工恢复指引和相关路径。

### 7.8 Deployment 与 Release 管理

Project、Environment、Destination 和 Deployment ID 都由 ShipForge 自动生成。Destination 以 revision 区分配置版本。Release 版本在同一 Project/Environment/Component generation 内唯一；MVP 通用模型不包含托管平台外部 ID。

推荐 `linux-ssh` Release 版本格式：

```text
v1.4.2-20260903.143520-8f21ac-a7c91d2e
```

该版本由可选发布标签或 Git Tag、UTC 时间、Git 短提交号和随机后缀组成；通过 no-clobber 文件与目录创建保证最终唯一。无法获取语义化标签时退化为：

```text
20260903.143520-8f21ac-a7c91d2e
```

Deployment 状态包括：

- `created`
- `running`
- `succeeded`
- `failed`
- `cancelled`

Deployment 的步骤具有 `pending`、`running`、`succeeded`、`failed` 或 `skipped` 状态。内部诊断使用稳定的命名空间步骤，例如 `build.packaging` 或 `linux-ssh.uploading`；TUI 显示“打包”“上传”等用户文案，不把 Driver 标识符作为配置概念。发布失败后的自动补偿记录在原 Deployment；用户发起的显式回滚记录为关联原 Deployment 的 Rollback Deployment。

Release 本身是不可变的 `<version>.tar.gz` 压缩包及其中的 manifest，不承担 Deployment 状态机。Driver 对每个 Component 观察当前 Release 版本或 `not_deployed`；通用记录只保存 Destination revision、Component generation 和能力快照。多 Component Deployment 的成功、失败、补偿和人工介入状态记录在 Deployment 及其逐 Component 结果中。

每次 Deployment 至少记录：

- Deployment ID 和各所选 Component 的 Release 版本；
- Project/Environment ID、Component 名称、Destination ID/revision、端点指纹及 Component generation；
- Git 分支、提交号及工作区状态；
- 操作者和时间；操作者自动取本机账号环境信息，无法获取时明确未知，不引入账号表单或身份认证体系；
- 部署组件；
- Release SHA-256；
- Deployment、步骤及各 Component 的准备、激活、健康和补偿结果；
- 各 Component 激活前的 Release；
- 各步骤执行结果。

历史查询必须区分“观察到 Release”“确认未部署”和“观察失败、状态未知”；不能把旧记录的空版本字段补写成未部署。计划和准备成功不代表版本健康。副作用之后的本地持久化写入失败必须保留已知远端结果、停止后续前进并尝试有持久化意图的安全补偿；仍未完成的意图即使所属 Deployment 已终态，也必须能被查询到。旧数据库缺少的快照和源码信息保持未知。

远端库存只读取已有身份对应的版本压缩包、manifest 和文件系统事实；查询不得创建目录、修复配置或自动恢复服务。缺失解压目录、缺失归档、损坏和未知 current 必须明确区分；扫描超限不能静默省略。manifest 不包含历史 Destination revision、端点或能力，审计缺失时不得使用当前连接配置补造这些历史字段，也不得由 current 推断健康。

远端 JSONL 只追加逐 Component 阶段证据，不表示整个 Deployment 成功。损坏、截断、重复冲突、未知格式或读取上限必须产生不完整提示；缺失审计不妨碍重建可验证的归档库存。Prepare 审计失败停止后续激活；激活或回滚生效后的辅助审计失败只附加警告，不覆盖已知结果，不阻止安全补偿。审计中不保存凭据、连接设置、路径、URL 或原始输出。

Release 只在目标有效能力支持清理时处理。全部所选 Component 成功且结果落盘后，系统默认保留每个 Component 最新 5 个 Release，并额外保护：

- 每个 Component 的当前 Release；
- 每个 Component 的上一个健康 Release；
- 被未结束 Deployment、Rollback Deployment 或待核实意图引用的 Release，包括所属 Deployment 已终态的 pending 意图。

“最新”按 manifest 创建时间、版本号降序稳定排列；上一健康版本按本地健康观察的写入顺序确定，必须属于相同 Component generation、Destination 和端点，不能由 current 或准备成功推断。原历史修订和能力快照保持不变；自动清理不借用当前连接处理其他 revision 的旧版本。

每个删除候选必须有本地原始包记录与远端 manifest、SHA-256、大小一致的证据，以及新持久化意图。删除前复核保护引用、配置与预期 current；本地历史缺失/损坏/超限、库存冲突或未知、临时残留不明确时不删除。数据库丢失后的库存缓存不是历史删除授权；辅助 JSONL 只能增加保护或否决冲突。

`linux-ssh` 逐版本先删解压目录、再删压缩包，仅操作准确候选，不修改 current、服务、Marker、审计或临时文件。路径、元数据、链接和挂载检查失败即停止。部分失败分别记录两条路径的已知/未知状态；未知结果保留 pending 意图，后续只读对账不能完成或重放它。清理取消、超时或写入结果失败均停止后续清理，不补偿已成功部署；新的清理操作须重新规划和记录意图。

### 7.9 日志

日志分为三层：

1. Deployment 日志：一次完整操作的总体过程；
2. 步骤日志：构建、上传、解压、切换、重启和健康检查结果；
3. 原始日志：命令的 stdout 和 stderr。

TUI 应支持：

- 实时滚动；
- 按步骤筛选；
- 文本搜索；
- 复制失败命令；
- 查看历史发布日志；
- 导出日志；
- 对密码、Token 和私钥等敏感信息进行脱敏。

日志筛选范围固定为实际 Project/Environment/Deployment，Component 和步骤可独立选择；文本在脱敏后按不区分大小写的字面量搜索。历史搜索与导出覆盖全部仍保留的轮转文件，不只当前显示页；缺失、损坏、缺片、超限和轮转变化必须明确提示，不宣称生命周期全文完整。实时窗口可暂停、恢复跟随和查看完整记录，生产端遗漏与窗口淘汰分别报告。

步骤的已知执行结果与持久化状态分开显示；单调耗时在执行完成时冻结，缺少历史时间则保持未知。复制只使用实际失败调用时保存并脱敏的程序/参数快照，作为诊断 JSON；不得从当前 YAML 或错误文本重建。旧文本日志没有可信步骤或命令时明确不可用，不靠 JSON 外观推断结构化格式。

导出先选择本机目录并预览冻结的完整内容，以无修饰确认键保存且不覆盖已有文件；部署摘要覆盖整次记录，不受日志筛选影响，不代表当前远端健康。剪贴板请求需显式触发，终端未确认接收时不得声称已成功复制。日志浏览不创建/迁移数据库，不访问远端；新结构化格式仅通过编号的本地写入迁移登记，不修改项目 YAML 或旧部署结果。

从首次产生副作用前开始，ShipForge 必须将 Deployment、步骤和意图写入本地 SQLite。原始脱敏日志写入有界滚动文件并由 SQLite 索引。实际状态由 Driver 观察；对 `linux-ssh`，每个 Component 的 `current` 软链接、版本压缩包、解压目录和 manifest 描述远端事实，JSONL 作为追加式审计记录。对账时以 Driver 观察结果修正本地缓存，同时保留差异记录。该过程要求有效的 `shipforge.yaml`，只重建 Release 库存和观察结果，不重建 Project 身份或部署配置。

对账以独立、带时间和检查范围的报告保存事实，不修改原 Deployment 状态、不完成旧意图、不重放命令。目标版本、原版本、其他版本与未知分别呈现；归档另行比对冻结的 manifest、SHA-256、大小及辅助准备审计。版本一致不证明重启、健康检查或原操作成功，多个 Component 状态混合只提示可能部分应用。

启动和打开项目只查询本地 `created/running` 或仍有 pending 意图的记录，包含终态残留意图；当前会话活动操作不被误判为中断，也不自动连接远端。用户主动检查时使用准确的历史 Destination revision，旧 generation 或缺失上下文不能借用新配置。只识别临时残留，不清理或修复。

数据库确实缺失时可创建库存缓存，但不伪造历史 Deployment、意图、执行结果和本地日志；已有损坏或未初始化数据库必须报错，不可当作缺失覆盖。检查期间配置或关联的源 Deployment 历史改变时不得更新缓存；无源 Deployment 的库存检查不比较历史修订。缓存保存失败仍须保留已观察结果与警告。较新的未知观察不能回退为旧的成功缓存。

### 7.10 TUI 页面

MVP 包含以下页面：

1. 项目目录选择与最近项目；
2. 首次设置向导；
3. 部署向导；
4. 部署执行与实时进度；
5. 发布历史；
6. 版本回滚；
7. Environment、Component 部署目标及 Destination 检查（MVP 展示 SSH 详情）。

交互要求：

- 支持方向键、回车、Esc 和页面快捷键；
- 顶部固定展示当前项目、环境与页面范围，适用时包含 Component；概览、部署和管理页按稳定 ID 共享会话内环境选择，同名重建或切换项目不得借用旧选择，不增加配置项。
- 连接端点与系统 ID 只作显示和选择，本地登记不等于连接可用或服务健康；历史目标只能使用冻结的原始引用，不借当前配置补造旧端点。步骤元数据转为可读操作名称，不暴露 Driver 名称或能力标识符，也不改写用户日志正文。
- 对项目、Component、Environment、Destination、SSH config Host、SSH Key、远端目录和 systemd unit 优先提供可搜索的选择列表；没有可靠候选或用户需要覆盖时，允许主动进入手动输入。搜索选择不启动连接、部署或保存；SSH Host 可填入表单草稿，Environment 可切换当前范围。
- 帮助浮层不得穿透确认键；空列表、读取失败、加载和取消分别显示，不能把未知当作空或未部署。当前项使用反显或文字标记，不仅依赖颜色；长列表保持当前候选可见。
- 管理详情逐层返回原范围、页码与选择；同页刷新按稳定 ID 保留焦点。失败必须有持续状态与准确的安全重试入口，返回的旧快照不得伪装为新检查结果。
- 取消的本地查询、回滚候选和计划丢弃迟到结果；已执行操作和库存检查保留已知结果及保存警告。实际回滚开始后的返回路径不得再次确认旧计划，重试须重新检查。
- 历史、报告及候选详情须能查看长字段和完整有界证据，超过 65,535 行仍可到达末尾；保存状态、主结果和警告数量优先展示。创建/更新时间不冒充执行起止，缺失或反向耗时保持未知，历史运行状态不证明当前进程活跃。
- 库存/恢复观察与环境预检明确区分；准备回执、阶段审计与临时残留不作为健康、整体成功、所有权或删除授权。
- 远端目录选择只读、有界、不递归；根目录和可选服务逐 Environment/Component 保存。探测失败可重试或改路径，探测结果不能替代部署预检、授权或健康检查。
- 系统区重新初始化必须完整预览将写入的 YAML 与新身份，并在保存前复核源文件和连接快照；可靠的固化 root 保留，无法确定时拒绝猜测。不读远端来恢复配置，也不重建历史。
- 选择已有项目目录后，若根目录配置有效，应直接进入项目概览或部署页，不重复显示首次设置步骤。
- 向导每一步显示自动推断来源，允许返回修改，并在最终确认前不产生远端部署副作用。
- 长时间运行任务必须持续显示当前步骤、耗时和日志；
- 危险操作必须展示项目、环境和目标服务器并二次确认；
- 环境名包含 `prod`（ASCII 不区分大小写）时，在固定上下文和确认页显示醒目标识；这是名称提示，不是安全分类，未命中也不得跳过服务器核对和显式确认。部署/回滚预览只接受无修饰 `c`，SSH 指纹确认只接受无修饰 `y`，回车不代替这些确认。
- 首次设置向导在用户选择的项目根目录生成配置，并将目录规范路径登记到平台配置目录中的项目注册表；项目列表读取该注册表并标记失效路径。
- 切换页面不得取消 Deployment。存在活动后台操作时，`q` 只打开退出确认；`Esc`/`r` 返回，只有 `c`/`Ctrl+C` 才请求取消全部受跟踪工作并等待安全边界。帮助、候选搜索和日志覆盖层打开时，其普通按键优先，不得被退出处理截获。MVP 不允许后台脱离。
- 所有 App 级后台操作必须由 App 保留取消与 join 所有权。通过事件投递结果的任务还须保留请求 ID，只接受匹配事件后 join；直接轮询的本地日志/导出任务由 App 单一持有取消令牌和线程句柄，并在完成或退出时 join。取消后的只读/计划结果不得打开新确认页；副作用或保存的已知结果不得被迟到取消或线程尾部错误改写。
- `INT`、`TERM`、`HUP` 及受支持的 Windows 控制事件直接请求安全退出。不可拦截的强制终止由持久化日志和下次启动对账解释，不能承诺进程内清理。
- 日志通知必须使用有界通道和批量刷新；原始输出持续落盘，界面仅保留有界窗口。

### 7.11 TUI 操作入口

启动 `shipforge` 后直接进入 TUI。MVP 必须在界面内提供以下操作，不要求用户记忆子命令：

- 选择、登记和移除项目目录；
- 创建、修改、检查和移除 Destination；
- 校验配置并预览部署计划；
- 选择 Environment 和 Component，查看计划并确认部署；
- 运行环境检查并查看建议处理动作；
- 查看 Release、执行回滚；
- 查看、搜索、复制和导出日志。

MVP 不提供用于部署的无头 CLI、`--yes`、JSON 命令输出、AI Agent 调用或 CI 接口，也不为它们预留协议。内部应用服务仍与界面解耦，但只服务当前 TUI。

## 8. `linux-ssh` 远端目录规范

```text
<component.root>/
├── .shipforge-project.json
├── current -> releases/<version>/
├── archives/<version>.tar.gz
├── releases/
│   └── <version>/
│       ├── manifest.json
│       └── <extracted payload>
├── temporary/<deployment-id>.tar.gz
└── metadata/
    ├── deployments.jsonl
    └── releases.jsonl
```

`.shipforge-project.json` 是每个 Component root 内的静态 Deployment Marker，仅记录 Project ID、Environment ID、Component 名称和 generation。后续 ShipForge 部署、回滚或恢复用它检查冲突；应用、systemd unit 和代理服务均不依赖它。ShipForge 不得直接覆盖 `current` 指向目录中的文件，也不得在标记字段不匹配时继续操作。

Component root 必须是规范化绝对路径；同一 Destination 上不同 Component 的 root 不得相同或相互嵌套，并须避开 ShipForge 保留目录。

激活与健康校验完成、记录结果并结束本次可选清理后，ShipForge 关闭 SSH 连接，不保留任何常驻控制进程；已部署服务独立运行。

## 9. 非功能需求

### 9.1 安全性

- 不在配置文件或日志中存储明文密码和私钥正文；
- 使用 SSH Agent、系统 SSH 配置或系统凭据管理器；
- 严格校验 SSH Host Key；
- 日志默认对敏感环境变量和命令参数脱敏；
- 执行远程命令时避免直接拼接未经转义的用户输入；
- 默认使用最小权限的部署账号。

### 9.2 可靠性

- 所有发布步骤必须具备明确状态；
- 当前 TUI 会话存在活动 Deployment 时不得启动第二个 Deployment；不同进程间不实现部署协调。
- 关键远程操作应尽量幂等；
- 网络中断后不得误把不完整版本标记为成功；
- 软链接切换必须为原子操作；
- 上传和激活过程使用临时状态，成功后再提交最终状态；
- 本地进程异常退出后能够识别上次未完成任务。
- 在构建、上传或激活等副作用前持久化操作意图；恢复时先探测远端事实再决定动作。
- 每个远端副作用前重新验证 Destination 端点指纹、Component generation 和预期当前版本；不一致时停止并提示重新检查。

### 9.3 可移植性

- ShipForge 使用 Rust 实现，并发布单文件可执行程序；
- 本地仅支持 Windows x64，使用现有 Rust + MinGW 工具链进行本机编译、审查和测试，不在 GitHub 执行测试；推送只同步仓库内容；
- 目标服务器首版仅支持 Linux；
- 目标服务器除 SSH、基础 Shell 和解压工具外不应依赖专用运行时。
- Windows 本地构建命令、路径与 Linux 目标归档权限必须有明确语义；项目负责提供能在 Windows 构建并为目标 Linux 生成有效输出的命令。

### 9.4 可观测性

- 每次任务使用唯一 ID；
- 每个步骤具有开始时间、结束时间、耗时和结果；
- 错误信息必须包含失败阶段、执行目标和建议处理动作；
- 支持将部署摘要导出为文本或 JSON。

## 10. 建议技术方案

- 开发语言：Rust；
- TUI：使用 `ratatui`，终端后端使用 `crossterm`；
- 部署端：内部 Deployment Driver SPI + 能力模型 + 编译期 Driver 注册表；
- MVP 驱动：`linux-ssh`，使用成熟的 Rust SSH/SFTP 库实现；
- 后续驱动：通过各供应商 REST API 或必要的官方 CLI 实现，不向应用层暴露协议和 Linux 文件系统字段；
- 本地状态：SQLite；
- 配置格式：YAML；
- `linux-ssh` 远端元数据：JSONL；
- MVP Release 格式：单个 `tar.gz`；
- 校验算法：SHA-256。

TUI 渲染要求：

- 使用 `ratatui` 的离屏缓冲构建完整帧，并通过差异化批量刷新减少终端写入和闪烁；
- 终端输出使用缓冲写入器，在帧边界统一刷新，禁止逐单元格直接写终端；
- 输入事件、应用状态和渲染逻辑分离；无状态变化时不得持续重绘，后台日志帧最多每 50 ms 合并刷新一次，真实交互与页面完成事件立即安排下一帧；
- 实时日志生产队列、步骤表和展示窗口必须各自有行数与字节上限，生产端遗漏和展示窗口淘汰分别报告；运行页和完成页读取 `App` 的规范实时窗口，日志覆盖层打开时使用持续同步新行的临时工作副本；
- 显式、串行的全进程压力门禁必须验证高吞吐日志下与 UI 排空解耦的生产端有界淘汰、展示窗口淘汰、进程内存上限及合成输入提交尝试到完成帧的延迟；该门禁不得冒充 `crossterm`、PTY 或物理终端端到端测试；
- 终端初始化的部分失败，以及正常退出、I/O 错误和可捕获 panic，都必须逐项尝试恢复 raw mode、alternate screen 和光标；恢复诊断不得输出 panic payload 或底层私密错误；
- `mimalloc` 等全局分配器只有在可重复基准证明收益时才可引入；当前有界内存与延迟门禁已有充足余量，MVP 保留标准分配器。

建议模块划分：

```text
shipforge
├── src/main.rs         # TUI 入口与运行时初始化
├── src/application     # 驱动无关的部署编排、回滚、恢复和查询
├── src/domain          # Deployment、Destination、Component 与 Release 规则
├── src/drivers
│   └── linux_ssh       # MVP Deployment Driver
├── src/adapters        # 进程、归档、HTTP 和凭据等底层能力
├── src/projects        # 本地项目注册表
├── src/config          # 项目配置读取与校验
├── src/history         # 本地记录和日志
├── src/telemetry       # 结构化事件与脱敏
└── src/tui             # ratatui 视图、状态、输入和事件投影
```

## 11. Deployment 与 Release 生命周期

```text
Deployment: created → running → succeeded
                         ├────→ failed
                         └────→ cancelled
```

Release 是不可变的 Component 版本压缩包，不使用业务状态机。是否为当前版本、健康版本或可删除版本来自 Driver 观察、Deployment 结果和保留规则。若激活后失败，原 Deployment 内的补偿流程让 Driver 恢复每个已操作 Component 激活前的 Release：

```text
failed deployment → compensation → restored | manual intervention required
```

用户发起显式回滚时，系统创建关联原 Deployment 的 Rollback Deployment；全部选中 Component 预检通过后，按部署拓扑逆序切换到指定历史健康 Release。恢复到首次部署前状态时，目标可为 `not_deployed`。当前状态漂移时不得产生副作用；部分失败时观察事实并逆序补偿本次已确认的变更。

恢复失败时，原 Deployment 保持 `failed` 并标记 `manual_intervention_required`；逐 Component 结果保留实际当前版本。ShipForge 不合成 Environment Release，也不把部分成功伪装成整体成功。

## 12. MVP 验收标准

1. 用户启动 TUI 并选择项目目录，通过候选列表完成组件识别、SSH Host/Key、部署位置和服务设置；向导在项目根目录创建包含自动生成稳定 ID 的单一 `shipforge.yaml`。
2. 再次选择已有项目目录时，系统直接加载根目录配置；用户可从 TUI 选择 Environment 及 Component，一个 Environment 可将不同 Component 发布到至少两个 SSH Destination。
3. 系统能接受文件或目录构建输出，并统一生成带 SHA-256 的 `<version>.tar.gz` Release。
4. 应用层通过 Deployment Driver SPI 完成发布；`linux-ssh` Driver 能通过 SSH/SFTP 保存带版本号和 SHA-256 的 Component Release 压缩包并解压运行。
5. 系统能按 Component 原子切换 `current`，执行 TUI 保存的自定义远端服务命令；systemd 作为同一机制上的内置预设。覆盖 PM2 首次启动、更新、恢复旧版本和恢复未部署状态，不影响共用 SSH 的未选组件。
6. 系统能按 Component 执行 Destination 端 HTTP/HTTPS、只读命令检查和 systemd 预设稳定性检查，并正确判断发布成功或失败；自定义服务不依赖 systemd。
7. 发布失败后，系统能将已操作 Component 恢复到各自激活前的版本；补偿失败时准确记录逐 Component 实际状态。
8. 用户可查看历史版本、发布时间、Git 提交和发布结果。
9. 用户可为一个或多个 Component 选择历史健康 Release 完成回滚。
10. 本地和远程发布记录在应用重启后仍然可查询；当前 TUI 会话不会同时启动两个 Deployment，远端状态与计划不一致时停止执行并提示重新检查。
11. SSH 支持私钥、SSH Agent 与标准密码认证；密码通过 TUI 遮蔽输入并以 Windows 当前用户 DPAPI 密文保存在用户级凭据注册表。SSH 密钥、密码和 Token 不以明文形式写入项目配置或普通日志；密码认证不能绕过 Host Key 校验，不包含 keyboard-interactive/MFA。
12. 每个 Component 的当前 Release、上一个健康 Release 及运行中操作引用的 Release 不会被自动清理。

## 13. 开发阶段建议

详细工作包、依赖和质量门禁以 `docs/roadmap.md` 为准。本节只描述阶段范围。

### 第一阶段：工程基础与安全部署闭环

- TUI 运行骨架、项目目录选择和首次设置向导；
- 配置解析与校验；
- 敏感值与日志脱敏、结构化参数、路径约束、远端参数转义和 Host Key 安全边界；
- Deployment、Destination、Component Release 与 Driver 能力模型；
- Deployment Driver SPI、注册表、Fake Driver 契约测试；
- 最小 SQLite 操作日志和滚动原始日志；
- 本地构建与打包；
- `linux-ssh` 驱动的 SSH/SFTP；
- 带版本号的 Component Release 压缩包、解压目录和 Component 级原子软链接切换；
- Component 级 Destination 端 HTTP/HTTPS、systemd 稳定性检查和自动回滚；
- TUI 部署计划、环境检查、执行进度和安全取消页面。

预计工作量：5～7 周，包含稳定身份与 Component generation、Destination 解析、最小持久化、健康检查和多 Component 补偿回滚闭环。

### 第二阶段：历史、恢复与保留

- 完整 SQLite 查询模型和远端 JSONL 审计；
- 中断识别、远端事实对账与恢复；
- 基于引用保护的安全清理；
- TUI 项目、Release、回滚和日志页面。

预计工作量：1.5～2 周。

### 第三阶段：TUI 交互与发布加固

- `SVC-01` 已完成统一远端服务命令、systemd 预设、配置转换、TUI 与 PM2/恢复验收；后续继续 `QA-02` 等发布门禁；
- 统一项目、环境、组件和 Destination 的导航与呈现；
- 完善可搜索选择、部署确认、实时步骤和有界日志；
- 完善发布历史、回滚和恢复体验；
- 安全取消和终端状态恢复；
- Windows 本机 smoke test、安全审查和 ShipForge 可执行文件打包。

预计工作量：1.5～3 周。

单人开发一个可供个人或小团队稳定使用的首版，整体预计需要 8～12 周；正式排期以 SSH 技术验证、故障注入结果和正式支持平台范围为准。

## 14. 后续规划

- 无头 CLI、JSON 输出、脚本和通用 CI 自动化接口；
- Docker 与容器化发布；
- Cloudflare Pages Deployment Driver；
- Vercel Deployment Driver；
- Cloudflare Workers Deployment Driver；
- 同一 Component 的多 Destination 复制、滚动发布与流量调度；
- 外部 Driver 插件或进程协议；
- 多服务器滚动发布；
- 蓝绿发布和金丝雀发布；
- Web 管理界面；
- 团队权限、审批和审计；
- Slack、钉钉、飞书等通知；
- GitHub Actions 等 CI 供应商集成、流水线模板和状态回写；
- 插件化构建器、传输器和健康检查器；
- 服务器应用日志查看；
- 发布模板与项目脚手架。

## 15. 平台与延期范围

客户端平台已限定为 Windows x64 GNU；`QA-01` 只要求本机编译、测试、ConPTY smoke 和真实 SSH 验收，不再要求三平台或 GitHub CI。实测工具链为既有 Rust 1.96.1 + MinGW；`Cargo.toml` 的 Rust 1.88 声明尚无独立最低版本实测，不作为已验证兼容性承诺。Linux/macOS 客户端、Shared Content、TCP/日志关键字健康检查、跳板机和 `ProxyJump` 均不进入 MVP。
