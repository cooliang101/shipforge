# ShipForge 开发路线图

## 目标与节奏

本路线图覆盖 `docs/requirements.md` 的 MVP：本地 Rust 单文件程序以 Ratatui TUI 作为唯一用户入口，通过统一 Deployment Driver SPI 发布，首个且唯一交付的 Driver 是 `linux-ssh`。MVP 包含稳定身份与 Component generation、可复用 Destination、统一的带版本号 `tar.gz` Component Release、多 Component 编排及补偿回滚；无头 CLI、CI 和 AI Agent 调用不进入 MVP，不预留适配器、配置、协议或工作包。以 1 名全职开发者估算，基线为 10 周，合理范围为 8～12 周。

当前状态：核心架构已收敛，M0 主体及 M1 的 `HIS-00`、`BLD-01`、`ART-01`、`SSH-02`、`REL-01`、`REL-02`、`ORC-01`、`HLT-00` 已实施。工程骨架、核心领域模型、配置加载/初始化及 generation 更新、Destination 与最近 Project 注册表、Driver SPI、应用层规划器与安全边界已有代码和测试。本地构建已覆盖 Git 预检、结构化子进程、双流排空、输出上限、超时和进程树取消；单一路径 `artifact` 会自动识别文件或目录，并由通用归档层确定性地生成一次带 manifest、大小和 SHA-256 的不可变 `tar.gz` Release。Driver `prepare` 契约只能接收核心创建且字段封装的 `ReleasePackage`。`linux-ssh` 使用 64 KiB SFTP 流式上传、单次进度、最多五次有限重试、远端 SHA-256、硬链接 no-clobber 归档、Deployment 专属暂存解压和 no-clobber 版本目录提交；随后严格观察规范 `current`，校验准备凭证与 Deployment、真实目录类型及同文件系统条件，以临时软链接加原子重命名逐 Component 激活。切换前漂移会停止且只清理本次临时链接；切换后取消或 systemd 重启失败会用独立超时恢复原版本，首次部署则移除新链接并停止服务；补偿前再次观察，拒绝覆盖外部漂移。驱动无关编排器会先持久化副作用意图并完成所有选中 Component 的 Prepare，再按所选子图拓扑顺序 Activate；失败或取消后观察失败点，将确认生效的 Component 按实际顺序逆序补偿，并把逐 Component 结果和人工恢复错误写入 SQLite。`HLT-00` 已实现 Destination 端 HTTP/HTTPS、systemd 稳定窗口和健康失败补偿。TUI 已支持最近列表、键盘目录浏览、有效 Project 直接打开、Component 发现与勾选、逐 Component 分配 Destination、滚动预览及确认写入；首次设置也已接入简单 SSH config 候选、SSH Agent/IdentityFile/文件选择、后台 Host Key 获取、显式指纹确认、严格 Host Key 下的身份认证和 Destination 自动分配。认证后会通过受限、限长、可取消的只读远端命令探测默认目录状态和 systemd unit，并在 TUI 中选择可选服务。SSH 设置通过应用服务隔离 transport 类型；root、systemd 和 Host Key 等概念明确属于 `linux-ssh`，不提升为通用 Driver 契约。真实 loopback SSH/SFTP 协议测试已覆盖握手、Host Key、公钥认证、exec、部分写入失败后的清理重试、进度、冲突、上传中取消、远端 SHA-256 成功与不匹配、完整 Release Prepare、原子激活、最终观察、远端 HTTP 健康检查、失败补偿及远端探测；剩余外部 OpenSSH 认证、传输/命令取消、Host Key 轮换及跨平台矩阵保留为 `QA-01` 发布门禁。SSH 高级配置、Shared Content 和 AI Agent 控制均已移出 MVP。工作包状态应在项目跟踪系统维护，本文只定义顺序、范围和门禁。

`HLT-00` 现已实现：`linux-ssh` 可在 Destination 端执行带重试和限时的 HTTP/HTTPS 检查，建立 systemd `NRestarts` 基线并验证稳定窗口，健康失败可使用激活凭证进入独立令牌补偿。配置采用内置默认值，不增加必填项。

`RBK-00` 现已实现：显式回滚创建关联原发布的 Rollback Deployment，在副作用前校验全部所选 Component，并按部署拓扑逆序恢复到指定 Release 或 `not_deployed`；漂移、部分失败、补偿失败和取消均保留逐 Component 事实与人工处理信息。操作类型、关联发布、可空目标版本及日志索引由后续 `HIS-01` schema v5 延续。

`RUN-01` 现已实现：TUI 会话持有唯一的内存执行门，部署与显式回滚共用；第二个操作在执行前被拒绝，完成、失败或取消均自动释放。该机制不使用文件锁、远端锁，也不提供多进程或跨机器协调。

`TUI-DEP-01` 已实现并通过自动化工作包验收：Environment/Component 选择、只读预检、含 Git 和目标信息的计划预览、显式确认、后台构建和发布、实时脱敏日志、安全取消、逐 Component 结果及人工恢复指引均已接通。构建与远端操作共用 Deployment ID；每次 Driver 写操作前复核配置，上传前检查实际解压容量及远端状态。日志故障触发安全取消，但不覆盖已知部署结果；终端错误退出也等待恢复边界。验证范围和已知限制见 [TUI-DEP-01 验收记录](validation/tui-dep-01.md)。

**M1 安全部署闭环已验收**：两台一次性 Debian/OpenSSH Destination 的发布、回滚、HTTP 失败补偿及取消已实测通过；WSL 真实 systemd 的无端口 Worker 稳定窗口、更新/首次部署失败补偿和两种显式回滚也已通过。完整范围、质量门禁及逐条 M1 证据映射见 [systemd 与 M1 验收记录](validation/systemd-acceptance.md) 和 [双 Destination 记录](validation/linux-ssh-acceptance.md)。默认协议测试不替代实机证据；M2 已验收，M3 正在推进，完整 MVP 尚未完成。

WSL Linux 客户端已通过原生单测（含 Unix 继承管道回归）、协议测试、编译及正常启动/退出冒烟；范围见 [Linux 客户端验证](validation/linux-client.md)。这不等于真实 systemd 验收或全部平台支持。

```text
TUI 骨架 → 领域与配置 → Destination 解析 → Driver SPI → Linux SSH 闭环 → 恢复与查询 → 交互加固
             └──────── 测试、安全、持久化、跨平台验证贯穿全程 ────────┘
```

## 里程碑总览

| 阶段 | 单人基线 | 核心结果 | 完成门槛 |
| --- | --- | --- | --- |
| M0 工程与模型 | 第 1～2 周 | TUI 骨架、首次设置、身份与版本字段、Driver SPI、配置 | 可在 TUI 完成无副作用项目设置；Fake Driver 和 CI 通过 |
| M1 安全部署闭环 | 第 3～6 周 | TUI 驱动的构建、Release 上传、Component 激活及补偿恢复 | 故障不会留下未记录的远端状态 |
| M2 历史与恢复 | 第 7～8 周 | TUI 查询、观测、对账、中断恢复、保留策略 | 重启及本地数据库丢失场景可解释 |
| M3 TUI 加固 | 第 9 周 | 完整、一致且高性能的交互体验 | 所有操作均可在 TUI 完成且终端可恢复 |
| M4 发布加固 | 第 10 周，最多延伸至第 12 周 | 可分发 MVP | 12 项 MVP 验收标准全部通过 |

## M0：工程与领域模型

- `FND-01`：初始化 Cargo 项目、模块边界及开发/发布 profile。
- `FND-02`：建立格式化、严格 Clippy、单测、依赖审计和平台编译 CI。
- `TUI-00`：实现 `crossterm` 终端守卫、输入循环、路由、对话框、表单、可搜索选择列表和错误边界。
- `DOM-01`：实现 Project/Environment 强类型稳定 ID、自动生成的不可变 Destination ID、Environment/Component generation、Deployment/Step 和 Component Release 模型。
- `DOM-02`：穷举 Deployment 与逐 Component 结果、Environment Observation、能力拒绝和引用保留规则测试。
- `CFG-01`：实现项目根目录自动发现、单一 `shipforge.yaml`、`_shipforge` 系统维护区、新 Project 随机身份生成、固定 YAML 结构、`after` 依赖排序、Component 单目标约束、单一路径构建输出和字段级诊断。
- `CFG-02`：建立配置契约测试，覆盖文件缺失触发新 Project、损坏的 `_shipforge` 被拒绝、确认重新初始化后生成新身份、唯一命令格式、Project/Environment 重命名保留身份与 root、Component 重命名视为新项、默认值固化、构建后自动识别输出类型、依赖环、越界路径，以及无效配置不改写文件。
- `DST-01`：实现用户级 Destination 注册表、直接 SSH config Host 与 SSH Agent/IdentityFile/Key 候选发现、自动生成的不可变 ID、修订、端点指纹、用户级凭据引用、跨项目复用及 Project 配置解析。
- `DRV-01`：定义携带 `ComponentExecutionContext` 的 Deployment Driver SPI、静态/有效能力、Component Plan/Release Receipt、Driver 注册表和 Fake Driver 契约套件。
- `APP-01`：定义只依赖 Driver SPI 的部署、回滚、恢复和查询编排，以及时钟、日志和存储端口。
- `SEC-01`：在构建和远端功能之前实现敏感值类型与脱敏、结构化参数、远端参数转义、路径约束、Host Key 策略和敏感配置检测；后续模块只能使用这些安全边界。
- `SSH-01`：技术验证 SSH Agent、用户配置、Host Key、SFTP、取消和 Windows 行为。
- `TUI-SETUP-01`：实现项目目录选择、最近项目加载、Component/构建输出发现、Destination 与 Key 选择、配置预览及确认写入；界面只调用应用服务。

交付物：可启动的 TUI、选择式首次设置、最小 Project 配置与生成状态、Fake Driver 测试夹具和 SSH 选型报告。

完成门槛：应用层不引用 SSH/SFTP；Fake Driver 可演示预检、准备、激活、观察和回滚；能力不足在计划阶段被拒绝。

## M1：安全部署闭环

- `HIS-00`：先实现最小 SQLite 操作日志、编号迁移和滚动原始日志，副作用前写入意图。
- `BLD-01`：Git 预检、结构化子进程、stdout/stderr 持续排空、超时及跨平台进程树取消。
- `ART-01`：校验每个 Component 的单一文件/目录构建输出，规范化归档路径、应用 Unix 模式，并恰好生成一次带 manifest 和 SHA-256 的 `<version>.tar.gz` Release。
- `SSH-02`：实现连接、Host Key 验证、远端命令、SFTP 进度、有限重试和远端哈希验证。
- `REL-01`：让 `linux-ssh` 接收 `ART-01` 生成的不可变 Release，上传并校验 SHA-256，原样保存压缩包并解压到对应版本目录；Driver 不参与重新打包。
- `REL-02`：由 `linux-ssh` Driver 检查同文件系统条件、原子切换每个 Component 的 `current`，并激活、检查和补偿对应服务。
- `ORC-01`：实现仅产生可记录、可清理暂存写入的全量准备，随后按所选 Component 子图的拓扑顺序激活，并按实际激活顺序逆序补偿回滚、逐 Component 观察及人工恢复指引。
- `HLT-00`：实现 Destination 端 HTTP/HTTPS 与 systemd 稳定性检查，并支持超时、间隔、重试、限长和脱敏。
- `RBK-00`：编排器创建 Rollback Deployment，由 `linux-ssh` Driver 将所选 Component 恢复到指定 Release，或首次发布前的无 `current` 状态。
- `RUN-01`：当前 TUI 会话存在活动 Deployment 时拒绝启动第二个任务；多进程并发不进入支持范围。
- `TUI-DEP-01`：实现环境检查、部署计划、安全确认、执行进度、错误指引和安全取消；所有操作通过应用服务发起。

交付物：通过 Driver SPI 在两台一次性 Linux Destination 上完成单 Component 发布、多 Component 联合发布、指定版本回滚和失败补偿。

完成门槛：每个远端副作用都有前置日志，并校验 Destination revision、端点指纹和 Component generation；当前 TUI 会话不会同时运行两个 Deployment；上传或哈希失败不改变 `current`；首次部署可只选择一个 Component；无外部端口的 Worker 可由 systemd 稳定性检查验证；激活后失败会把已操作 Component 补偿到原 Release 或未部署状态，恢复失败会产生人工指引。

## M2：历史、对账与保留

`HIS-01` 已实现本地 SQLite schema v5、冻结的所选 Component/目标/能力快照、源码和操作者信息、Release manifest/大小/SHA-256、准备回执、带时间的步骤以及明确区分未知与未部署的观察记录。部署与显式回滚已接入；查询采用有界分页，终态中的未完成意图仍可发现。验收范围见 [HIS-01 记录](validation/his-01.md)。

`HIS-02` 已完成只读远端库存与追加式审计：校验版本归档、摘要、manifest 和目录事实，显式报告不完整条目与未知 current；审计保留原始非秘密引用，不从当前配置补造历史能力或健康。副作用后的辅助审计失败保留已知结果与补偿路径。默认测试、独立审查及真实双 Linux 复跑通过，证据和资源上限见 [HIS-02 记录](validation/his-02.md)。

`REC-01` 已完成只读对账应用服务、独立不可变报告、历史修订校验、远端暂存残留诊断及 TUI 启动本地待核实提示。不重放旧意图、不改写原结果，也不从远端重建 YAML。独立审查、426 项单测、6 项协议测试、15 个真实子进程退出/模拟副作用场景和 486.50 秒双 Linux 验收通过；完整范围见 [REC-01 记录](validation/rec-01.md)。

`RET-01` 已实现并验收：成功部署后默认保留最新 5 个版本及受保护引用，以原始包证据和逐版本持久意图授权精确清理；部分失败保留路径事实，未知结果保持 pending，不补偿成功部署。独立审查、Windows/Linux 全量门禁、原生删除脚本、真实权限失败/重试及六次经正式应用流程的测试发布触发默认清理均通过；完整范围见 [RET-01 记录](validation/ret-01.md)。

`TUI-MGT-01` 已实现并验收项目配置编辑、连接管理、本地历史/日志、显式库存检查、检查报告和所选 Component 回滚入口，包含已删除 Environment 的只读历史入口。交叉审查、Windows 597 / Linux 609 项单测、各 8 项协议与 1 项中断父用例、双平台 Clippy/格式/release 构建，以及阶段诊断补强后的四轮真实 Management 用例均通过。独立百次连接用例提供额外基线，早期连接超时根因仍未知，保留为 QA-01 发行风险，不宣称已定位修复。范围与证据见 [管理功能验收记录](validation/tui-mgt-01.md)，使用方式见 [TUI 指南](tui-guide.md)。M2 已完成，M3、M4 和完整 MVP 尚未完成。

- `HIS-01`：扩展 SQLite Deployment、Step、逐 Component Release Receipt、Environment Observation、Component generation、能力快照及日志索引模型。
- `HIS-02`：实现 `linux-ssh` Release 库存重建和追加式远端 JSONL 审计记录；Release manifest 由 `ART-01` 创建，由 `REL-01` 和恢复流程读取。
- `REC-01`：在 `shipforge.yaml` 有效的前提下，通过 Driver `inventory` 中的 current、归档及残留事实对账本地意图；`linux-ssh` 以每个 Component root 的静态 Deployment Marker 与远端文件系统事实解释非终态 Deployment、部分应用和数据库丢失，独立保存观察报告，不重放旧意图或从远端重建 Project 配置。
- `RET-01`：实现基于引用的清理，自动保护每个 Component 的 current、上一健康版本及运行中操作引用的 Release。
- `TUI-MGT-01`：实现项目与 Destination 管理、Release 查询、回滚、日志和恢复操作入口。

完成门槛：进程在每个远端步骤前后中止均能在下次启动解释状态；在 `shipforge.yaml` 完整时，删除本地数据库后可重建远端发布库存，但明确提示本地日志不可恢复。

## M3：Ratatui 交互加固

`TUI-01` 已实现并通过自动化工作包验收：统一固定上下文、按稳定 ID 共享的会话内环境选择、连接展示、目标可用操作及生产环境确认；步骤元数据采用可读名称，部署控制错误保留结果和恢复信息而不回显底层字符串。独立审查、Windows 634 / Linux 646 项单测、各 8 项协议与 1 项中断父用例、双平台格式/Clippy/release 构建通过，详见 [TUI-01 记录](validation/tui-01.md)。这不等于人工终端体验、完整 M3 或最终 MVP 验收；下一工作包为 `TUI-02`。

- `TUI-01`：统一项目、Environment、Component、Destination、目标可用操作和生产环境确认的呈现与导航；不向用户暴露 Driver 名称或能力标识符。
- `TUI-02`：完善选择优先的候选列表、空状态、加载状态、可恢复错误、键盘帮助和无障碍配色；补齐损坏 `_shipforge` 经明确确认后重新初始化的 TUI 入口，复用已有底层 API，不从远端恢复配置。
- `TUI-03`：完善步骤进度、耗时、有界日志窗口、历史加载、搜索、筛选、复制和导出体验。
- `TUI-04`：完善发布历史、回滚、环境检查和恢复页面的一致性。
- `TUI-05`：加固运行中退出对话框、安全取消和中断处理。
- `TUI-06`：用高吞吐合成日志验证事件背压、内存上限和输入延迟；仅在基准支持时评估 `mimalloc`。

完成门槛：全部 MVP 操作只能通过应用服务进入领域层；切换页面不取消 Deployment；安全退出等待恢复边界；正常退出、错误和 panic 均恢复 raw mode、光标及 alternate screen。

## M4：发布加固

- `QA-01`：完成确认后的 Windows、macOS、Linux 编译及 smoke test 矩阵；复核 TUI-MGT-01 早期未定位 SSH 连接超时，在阶段化诊断证据基础上给出支持条件与发行结论，不把单次复跑通过当作根因修复。
- `QA-02`：对路径穿越、Shell 注入、Host Key、凭据、日志和归档权限做安全审查。
- `QA-03`：验证 Driver SPI 契约、Project/Environment ID、Destination ID/revision、Component generation、配置兼容、远端元数据前向兼容和安装升级。
- `REL-03`：生成 ShipForge 单文件可执行程序与 SHA-256，编写安装、升级、回滚和排障文档。
- `ACC-01`：逐项执行需求文档的 12 条验收标准并保存证据。

## 质量门禁

- 每个工作包同时提交成功、失败和边界测试；安全闭环不允许“后续再补测试”。
- 每完成一个工作包，先审查代码、修复问题并通过门禁，再单独提交 Git；提交后才开始下一阶段。WIP 快照不计为阶段完成提交。
- 合并必须通过 `cargo fmt --all -- --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test`、依赖审计及平台编译。
- 领域层穷举状态和保留规则；每个 Driver 运行共享契约套件；真实 SSH 仅连接一次性环境。
- 关键操作必须可安全重试，错误必须包含阶段、目标和建议动作。
- 禁止测试发现或连接真实 Environment，禁止快照和日志包含真实密钥、Token 或主机。

## 已确定的设计默认值

- Destination 是用户级、跨 Project 复用且带系统生成不可变 ID、修订和端点指纹的连接配置；Environment 只通过 ID 引用 Destination，不包含主机和凭据。
- 项目根目录只有一个 `shipforge.yaml`，它是 Project 身份与部署配置的唯一来源；TUI 只写入固定格式，稳定身份、generation 和固化 root 由 `_shipforge` 系统维护区保存。文件缺失即初始化新 Project，系统区损坏必须经确认后以新身份重新初始化；MVP 仅由 TUI 创建和修改配置。
- 一个 Environment 直接配置任意数量的 Component；每个 Component 在该 Environment 中只指向一个 Destination，不支持单 Component 多 Destination 复制。不同 Component 可复用同一 Destination。
- 应用层只依赖 Deployment Driver SPI；Driver 以能力、计划、结构化事件和 Component Release Receipt 与编排器交互。
- Driver 的所有操作都接收 `ComponentExecutionContext`；通用部分只含稳定身份和端点指纹，凭据与 Driver 目标设置通过已校验的不透明句柄传递。应用层不读取 Linux 字段。Component Release Receipt 固化非秘密的 Destination ID/revision 与 Component generation；项目中的 Destination ID 或目标设置变化启动新 generation。
- `artifact` 永远只表示一个文件或目录构建输出路径；`Release` 永远表示由该输出生成的唯一 `<version>.tar.gz` 压缩包。通用归档层只打包一次，所有 Driver 消费同一格式且不得重复打包。
- 每个 Component 独立保存带版本号的 Release 压缩包并拥有自己的 `current`；Component 的真实依赖使用 `after` 表达并拓扑排序，失败时执行补偿回滚，不承诺跨 Component 原子性。
- 每个 Environment/Component 独立配置 Destination、root、可选 systemd 和可选 Destination 端 HTTP/HTTPS 检查。
- Prepare 只允许可记录、可重试或清理的暂存写入，不得切换版本或修改服务及业务状态；MVP Activation 只切换版本、激活所选服务并执行健康检查，不编排数据库迁移或共享持久内容。
- Git 脏工作区默认警告并确认；禁止策略留作环境配置。
- 本地 SQLite 保存执行意图和历史；实际状态由 Driver 观察。`linux-ssh` 的远端文件系统描述远端事实，JSONL 仅作审计和灾难恢复辅助。
- MVP 不支持后台脱离运行。活动 Deployment 退出时只能返回或安全取消。
- 本地构建命令只使用程序与参数数组；MVP 不提供 Shell 字符串配置。
- 当前 TUI 会话只运行一个 Deployment；MVP 不实现多进程部署协调，每个远端副作用前校验计划所依赖的远端状态。

## 已延期能力

Shared Content、SSH `ProxyJump`/`Include`/`Match`、TCP 与日志关键字健康检查均不进入 MVP。Windows、macOS 和 Linux 的发布等级在 `QA-01` 依据实际 smoke test 结果标注，不阻塞当前接口设计。

## MVP 验收追踪

| 验收能力 | 负责工作包 |
| --- | --- |
| 初始化、Project/Environment ID、Destination ID/revision 与配置模板 | `DOM-01`、`CFG-01`、`DST-01` |
| Deployment Driver SPI 与能力验证 | `DRV-01`、`DOM-02` |
| TUI 首次设置、选择 Component 并发起部署 | `TUI-SETUP-01`、`TUI-DEP-01`、`TUI-01` |
| 构建、压缩、权限与 SHA-256 | `BLD-01`、`ART-01` |
| `linux-ssh`、SFTP 与 Component Release 压缩包 | `SSH-02`、`REL-01` |
| Component Release 与 Component 级原子文件系统切换 | `REL-01`、`REL-02` |
| 多 Component 编排与补偿回滚 | `ORC-01`、`RBK-00` |
| Component 级本机/远端健康检查与自动恢复 | `HLT-00`、`REL-02`、`RBK-00` |
| 单会话任务约束、目标版本校验与远端状态变化拒绝 | `RUN-01`、`DST-01`、`REC-01` |
| 历史、手动回滚与重启持久化 | `HIS-00`、`HIS-01`、`REC-01`、`TUI-MGT-01` |
| 密钥和日志安全 | `SSH-01`、`SEC-01` |
| 引用保护与安全清理 | `RET-01`、`TUI-MGT-01`、`TUI-04` |

## 首批开发顺序

1. 完成 `FND-01`、`FND-02`，建立可持续提交的主干。
2. 建立 `TUI-00` 后，并行推进 `DOM-01`、`CFG-01`、`DST-01`、`SSH-01` 和 `TUI-SETUP-01`，用身份、revision、generation 和连接结果完善 `DRV-01`。
3. 完成 `DOM-02`、`DRV-01`、`APP-01`、`SEC-01`、`HIS-00` 后，才允许引入构建执行和远端副作用。
4. 完成 `RUN-01` 后，依次打通首次发布、单 Component 更新、跨 Destination 的多 Component 联合更新、健康失败、远端状态变化拒绝和补偿回滚。
5. M1 安全闭环通过后，再完善高级历史、恢复界面、交互加固和发布包。

## MVP 之后

按以下顺序扩展部署端，每个 Driver 必须先通过 `DRV-01` 定义的共享契约：

1. `E1 cloudflare-pages`：预构建内容上传、预览 URL、状态轮询、生产激活、日志和供应商回滚。
2. `E2 vercel`：Release 内容上传、Deployment 创建、Preview 验证、Promote、日志和符合资格限制的回滚。
3. `E3 cloudflare-workers`：Worker Version 上传、Deployment 激活、bindings 快照和回滚限制；流量拆分另行设计。
4. `E4 replicated-component`：同一 Component 的多 Destination 副本、滚动发布、批次健康聚合与流量调度。
5. 外部 Driver、无头 CLI 与 CI 接口继续冻结；恢复评估前不设计协议、配置或兼容层。AI Agent 调用不列为扩展项，除非未来单独重新立项。

Docker、多服务器滚动部署、蓝绿/金丝雀、后台守护进程、无头 CLI/CI、多进程并发部署协调、数据库迁移编排、Web 管理、团队审批和通知集成仍不进入 MVP。
