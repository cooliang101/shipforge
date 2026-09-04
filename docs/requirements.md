# ShipForge 产品需求文档

## 1. 文档信息

- 产品名称：ShipForge
- 产品类型：本地运行的前后端应用发布部署助手
- 目标形态：交互式终端界面（TUI）
- 当前阶段：需求定义 / MVP 规划
- 文档版本：v0.8
- 更新日期：2026-09-03

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
2. 构建后端二进制或应用包，上传后通过 systemd 重启服务。
3. 同时发布同一项目的前端和后端组件。
4. 查看某次失败发布的本地构建及远程执行日志。
5. 从当前线上版本回滚到某个历史稳定版本。

## 5. MVP 范围

MVP 仅支持以下范围：

- 本地运行环境：优先支持 Windows、macOS 和 Linux；
- Destination：用户级注册表可保存多套可复用 `linux-ssh` 连接，并可被多个项目引用；
- 部署映射：每个 Environment 直接配置需要部署的 Component；每个 Component 在该 Environment 中指向一个 Destination；
- 登录方式：SSH Key，优先使用 SSH Agent 和用户 SSH 配置；
- 服务器数量：每个 `linux-ssh` Destination 对应一台服务器；一个 Component 不支持同时部署到多台服务器；
- 并发模型：当前 TUI 会话同一时间只允许一个活动 Deployment；多个 ShipForge 进程同时部署不受支持，也不实现协调机制；
- 驱动方式：内置 `linux-ssh` 驱动，传输使用 SFTP，Release 格式为 `tar.gz`；
- 服务管理：可选 systemd unit；
- 发布策略：每个 Component 一个带版本号和 SHA-256 的 `tar.gz` Release，解压后通过该 Component 的 `current` 软链接原子切换；
- 健康检查：Destination 端 HTTP/HTTPS 和 systemd 稳定性检查；
- 版本管理：每个 Component 保留最近 N 个 Release，并保护当前版本和上一个健康版本；
- 操作方式：Ratatui TUI；启动可执行文件后，所有用户操作均在界面内完成。

## 6. 核心用户流程

### 6.1 首次设置与再次打开

1. 用户选择项目根目录；系统规范化路径并检查根目录下的 `shipforge.yaml`。
2. 配置存在时直接加载并校验其中的用户配置和 `_shipforge` 系统维护区；除缺少本机 Destination、配置不兼容或远端状态变化外，不重复询问已保存信息。
3. 配置不存在时，该目录视为新 Project。系统扫描项目清单和构建脚本，展示推断的 Component、构建命令及构建输出路径，用户通过勾选和修改确认，随后生成全新的稳定 ID。
4. 系统展示已有 Destination 和 SSH config Host；用户选择现有项，或新建 SSH 连接并从 SSH Agent、`IdentityFile`、标准 Key 候选或文件选择器中选择身份。
5. 私钥内容及个人 Key 路径只进入用户级凭据引用，不写入项目文件。首次连接必须展示并确认 SSH Host Key 指纹。
6. 连接成功后，用户选择 Environment、要部署的 Component、各 Component 使用的 Destination、部署目录及可选 systemd unit；系统优先提供探测候选和默认值。
7. 系统展示规范化配置和部署计划。用户确认后，在项目根目录原子写入唯一的 `shipforge.yaml`，并将项目目录登记到本机项目注册表。
8. 用户取消时不得留下半成品项目配置或未确认的 Destination。

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
14. 全部成功时记录各 Component Release 结果并清理过期版本；失败时按实际激活顺序逆序恢复已操作 Component，并记录逐 Component 结果或需人工介入。

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

固定格式如下；完整规则见 `docs/configuration-guide.md`：

```yaml
schemaVersion: 1

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
        systemd: mall-api.service
        health: http://127.0.0.1:8080/health
      worker:
        to: dst_00000000000000000000000000000002
        systemd: mall-worker.service
        after: [backend]
```

Destination 通过 TUI 连接管理页写入用户级注册表，系统自动生成 ID，项目配置以 `to` 引用。向导可从 SSH config 的直接 `Host` 候选中取值，或在“新建连接”表单中录入 `deploy@web.example.com`；用户无需命名连接。

### 7.2 Destination 与内部部署驱动

- 应用层只通过统一 Deployment Driver SPI 发起预检、规划、准备、激活、观察、回滚、日志和清理操作，不得直接依赖 SSH/SFTP 或供应商客户端。
- Driver 是内部代码边界，不是用户概念。TUI 使用“SSH 连接”和“部署目标”，`shipforge.yaml` 不出现 Driver 类型、能力名、供应商对象或传输参数。内部类型和诊断步骤可标识 Driver，但普通 TUI 文案不得要求用户理解它。
- MVP Driver 只声明当前编排实际使用的准备、激活、回滚、观察、远端日志、清理和取消能力；本地构建与标准 Release 归档属于 Driver 之前的通用流程。托管平台所需能力在对应 Driver 立项时扩展。
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
- TUI 的环境检查操作必须检查远端 Shell、解压、哈希、软链接、原子重命名、磁盘及权限能力；按配置检查 systemd 和 Destination 端 HTTP 客户端，并报告实际采用的命令。
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
- MVP 只支持可选的 systemd unit 激活；自定义服务命令延后设计。
- 每一步都必须记录开始时间、结束时间、状态和输出。
- 任务被中断后，系统应能识别残留临时目录和未完成的发布状态。
- 原子性边界是单个 Component 的 `current` 切换；服务重启、健康检查和多个 Component 均不宣称原子。
- Component 服务失败时，Driver 必须恢复该 Component 原 `current`；若原来没有版本，则移除新链接并停止本次启动的服务。未成功恢复时标记需人工介入。
- 所有选中 Release 准备成功后，在本次所选 Component 形成的子图中按 `after` 拓扑顺序激活；未选择的依赖项不会自动加入。无依赖项采用稳定排序。任一失败时，对本次已激活 Component 按实际顺序逆序补偿，并持久化逐 Component 结果。

### 7.7 健康检查与自动恢复

- MVP 健康检查属于一个 Component，只对本次选择并激活的 Component 执行。
- 可选 `health` URL 由 Destination 端执行 HTTP/HTTPS 检查，因此可检查仅监听回环地址或内网地址的服务。
- `systemd` 检查在服务达到 active 后记录 `NRestarts` 等基线，并要求 Unit 在 `stableFor` 时间内保持 active 且基线不增加。
- Component 声明 `systemd` 时自动加入必需的 systemd 检查；同时声明 URL `health` 时再加入必需的 Destination 端 HTTP 检查，两项必须全部通过。
- 不对外提供端口的服务使用 systemd 稳定性检查；自定义命令检查不进入 MVP。
- 所有检查支持超时、间隔和重试次数；配置为必需的检查全部通过后，该 Component 才视为健康。
- 一个 Component 的激活和必需健康检查通过后，该 Release 才可记为该 Component 的健康版本。
- 未受本次 Deployment 影响的 Component 保持原状态，不进入本次结果。
- 激活、重启或健康检查失败时，若已启用自动回滚，应恢复激活前记录的 Component `current`；原值不存在时恢复为未部署状态。
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
- 操作者和时间；
- 部署组件；
- Release SHA-256；
- Deployment、步骤及各 Component 的准备、激活、健康和补偿结果；
- 各 Component 激活前的 Release；
- 各步骤执行结果。

Release 只在 Driver 声明支持清理时处理。`linux-ssh` 清理必须保护：

- 每个 Component 的当前 Release；
- 每个 Component 的上一个健康 Release；
- 被运行中 Deployment 或 Rollback Deployment 引用的 Release。

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

从首次产生副作用前开始，ShipForge 必须将 Deployment、步骤和意图写入本地 SQLite。原始脱敏日志写入有界滚动文件并由 SQLite 索引。实际状态由 Driver 观察；对 `linux-ssh`，每个 Component 的 `current` 软链接、版本压缩包、解压目录和 manifest 描述远端事实，JSONL 作为追加式审计记录。对账时以 Driver 观察结果修正本地缓存，同时保留差异记录。该过程要求有效的 `shipforge.yaml`，只重建 Release 库存和观察结果，不重建 Project 身份或部署配置。

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
- 对项目、Component、Environment、Destination、SSH config Host、SSH Key、远端目录和 systemd unit 优先提供可搜索的选择列表；只有无法发现时才显示自由输入。
- 选择已有项目目录后，若根目录配置有效，应直接进入项目概览或部署页，不重复显示首次设置步骤。
- 向导每一步显示自动推断来源，允许返回修改，并在最终确认前不产生远端部署副作用。
- 长时间运行任务必须持续显示当前步骤、耗时和日志；
- 危险操作必须展示项目、环境和目标服务器并二次确认；
- 生产环境应使用醒目标识，避免与测试环境混淆；
- 首次设置向导在用户选择的项目根目录生成配置，并将目录规范路径登记到平台配置目录中的项目注册表；项目列表读取该注册表并标记失效路径。
- 切换页面不得取消 Deployment。存在运行中 Deployment 时，正常退出必须要求返回任务或执行安全取消；安全取消到达可恢复边界并完成必要回滚后才能退出。
- 首次中断信号触发安全取消；无法拦截的强制终止由持久化日志和下次启动对账恢复。
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

激活与健康校验完成并记录结果后，ShipForge 关闭 SSH 连接，不保留任何常驻控制进程；已部署服务独立运行。

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
- 本地优先覆盖 Windows、macOS 和 Linux；
- 目标服务器首版仅支持 Linux；
- 目标服务器除 SSH、基础 Shell 和解压工具外不应依赖专用运行时。
- 本地构建命令、路径和归档权限必须有明确的跨平台语义；项目负责提供能为目标 Linux 生成有效构建输出的命令。

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
- 输入事件、应用状态和渲染逻辑分离，限制无状态变化时的无效重绘；
- 所有退出和异常路径必须恢复 raw mode、光标及 alternate screen；
- 可评估 `mimalloc` 等全局分配器，但引入前必须通过基准测试证明收益。

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
5. 系统能按 Component 原子切换 `current` 软链接并执行服务重启命令。
6. 系统能按 Component 执行 Destination 端 HTTP/HTTPS 和 systemd 稳定性检查，并正确判断发布成功或失败。
7. 发布失败后，系统能将已操作 Component 恢复到各自激活前的版本；补偿失败时准确记录逐 Component 实际状态。
8. 用户可查看历史版本、发布时间、Git 提交和发布结果。
9. 用户可为一个或多个 Component 选择历史健康 Release 完成回滚。
10. 本地和远程发布记录在应用重启后仍然可查询；当前 TUI 会话不会同时启动两个 Deployment，远端状态与计划不一致时停止执行并提示重新检查。
11. SSH 密钥、密码和 Token 不以明文形式写入项目配置或普通日志。
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

- 统一项目、环境、组件和 Destination 的导航与呈现；
- 完善可搜索选择、部署确认、实时步骤和有界日志；
- 完善发布历史、回滚和恢复体验；
- 安全取消和终端状态恢复；
- 跨平台 smoke test、安全审查和 ShipForge 可执行文件发布。

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

## 15. 待确认事项

正式发布前，根据 `QA-01` 在 Windows、macOS 和 Linux 上的实际编译与 smoke test 结果标注支持等级。Shared Content、TCP/日志关键字健康检查、跳板机和 `ProxyJump` 已明确延期，不再作为 MVP 待确认项。
