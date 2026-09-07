# ShipForge Configuration Guide

本文是 ShipForge 项目配置的唯一规范入口。MVP 只允许通过 TUI 创建和修改配置；AI Agent 调用、AI Agent 直接编辑和自动化接口全部冻结，不设计入口、协议或兼容层。SSH Agent 只是 SSH 密钥签名方式，不属于 AI Agent 功能。

**配置版本**：当前规范为 `schemaVersion: 2`。远端服务统一使用 `service` 命令计划，systemd 是预填这套配置的内置选项；无服务的静态内容可省略 `service`。配置编辑与部署分别确认。

## 配置边界

| 文件或存储 | 内容 | 维护者 |
| --- | --- | --- |
| `shipforge.yaml` | Component、Environment 部署意图，以及 `_shipforge` 系统维护区 | TUI |
| 用户级 Destination 注册表 | 系统生成的 ID、连接类型、端点、修订及凭据引用 | TUI 连接管理页 |
| 用户级凭据注册表 | SSH Agent 指纹或私钥路径；项目文件只引用句柄 | TUI Key 选择器 |

`shipforge.yaml` 必须位于项目根目录，不得包含密码、Token、私钥正文或个人 SSH Key 路径。`_shipforge` 保存 Project/Environment ID，以及每个 Environment/Component 的 generation 和固化 root；不得绕过 TUI 手工修改。

配置文件不存在时，该目录作为新 Project 初始化并生成新身份，不从用户目录缓存或远端恢复。配置存在但 `_shipforge` 缺失或损坏时必须停止；TUI 可在校验用户配置与连接引用后生成完整 YAML 预览，只有用户明确确认“作为新项目重新初始化”后才能保存全新身份。可靠的固化 root 保留，含糊或冲突的路径拒绝猜测；保存前复核原文件和连接快照。远端 Deployment Marker 只用于目标冲突检查，不是配置备份。

## 选择优先的首次设置

1. 让用户选择项目目录；若已有 `shipforge.yaml`，立即加载并校验。
2. 从 `package.json` 的 build script、`Cargo.toml` 和 `go.mod` 推断 Component、构建命令与构建输出路径，供用户勾选或修正；Dockerfile 只提示镜像构建不在 MVP 范围，其他构建方式可手动配置。无候选或发现失败不要求先修改项目文件以通过发现。
3. 展示已有 Destination 和 SSH config 中的 Host；新建连接时优先让用户选择 SSH Agent、`IdentityFile` 或发现的 Key。
4. 首次连接前展示 Host Key 指纹；确认并认证成功后保存本机连接。用户可只读浏览目录、探测 systemd 候选，也可选择默认 root 或手动覆盖；探测结果不表示部署预检或健康已通过。
5. 用户为每个 Component 选择 Destination、独立 root 和可选服务；同一 Destination 可被任意 Project 的多个 Component 复用，不共用组件部署设置。
6. 展示规范化 YAML，确认后原子写入项目根目录的 `shipforge.yaml`；部署预检和执行另行确认。

除主机地址、特殊部署目录等无法可靠发现的值外，应使用选择、确认和默认值完成设置。Project/Environment/Destination/Deployment ID 和 Release 版本均由系统生成，用户无需命名连接。取消不保存未确认草稿；已经明确确认并保存的连接保留，不随项目向导取消而删除。写入结果不确定时要求重新加载，不宣称没有保存，也不盲目回退独立注册表。

MVP 中用户只选择 SSH 连接，不选择 Driver。TUI 根据连接记录自动使用内置 `linux-ssh` 实现；Driver 名称、能力标识符和 SFTP 参数不进入项目配置。

## TUI 配置规则

1. 按可独立构建、部署或回滚的边界识别 Component；不要把每个源码目录都定义成 Component。
2. 为每个 Component 生成可复现的 argv 构建命令和唯一 `artifact` 构建输出路径。MVP 不接受 Shell 字符串或另一套命令格式。
3. 在每个 Environment 下直接配置需要部署的 Component。每项至少选择一个 `to` Destination ID，可选设置 `root`、服务、健康检查和 `after`。
4. 只询问无法从仓库、SSH config、Destination 注册表或远端探测确定的信息，不猜测生产基础设施。
5. 生成固定格式的最小配置，再通过 TUI 预览部署计划。

## 服务命令与内置预设

服务配置属于每个 Environment/Component；共用 SSH 不共用服务命令。目标选择页提供不管理服务、systemd unit 候选和自定义命令；按 `c` 编辑逐阶段的程序与参数。systemd 选择 unit 后自动生成下文配置，无需填写预设 ID。

| `service` 字段 | 规则与工作目录 |
| --- | --- |
| `start` | 必填，首次部署；在新 `releases/<version>` 目录执行 |
| `update` | 可省略或为空，复用 `start`；在新版本目录执行 |
| `restore` | 可省略或为空，复用有效的 `update`；在恢复的旧版本目录执行 |
| `stop` | 必填，恢复未部署状态时停止；在刚移除 current 对应的版本目录执行 |
| `check` | 可省略；`kind: command` 加单个 `argv`，或 systemd 预设的 `kind: systemd` 加完整 `unit` |

四种动作均为按顺序执行的 argv 数组列表。单动作最多 16 条命令，每条含程序最多 128 项、每项最多 4 KiB，整套命令最多 16 KiB。每个服务动作共享 120 秒上限，补偿中的服务动作也有独立 120 秒预算（不含此前目录校验和链接恢复）；有副作用命令不自动重试。命令没有已知退出结果时，该 Component 停止自动恢复并要求人工核实，观察到 current 不代表服务已完成。

命令检查必须只读：在 `root/current` 执行，退出码 0 通过；最多 5 次、间隔 1 秒、单次 10 秒。systemd 检查保留 active/NRestarts 的 10 秒稳定窗口。配置了 HTTP 时也必须通过；未配置检查不代表执行了健康探测。

不接收 Shell 字符串；需要流程判断时将受信任脚本放进构建输出，用 `[node, service.cjs, activate]` 或 `[sh, service.sh, activate]` 调用，不用 `sh -c`。脚本须自行管理该组件的服务、提供幂等停止/恢复，不修改 ShipForge 元数据。运行时和权限需在服务器预先配置，ShipForge 不自动安装依赖或提权。程序名/绝对程序路径在预检检查；随 Release 提供的相对脚本在实际执行时校验，预检不试运行服务。

PM2 可使用自定义 argv，复杂启动/替换逻辑可交给项目脚本。不能把 `pm2 restart <name>` 的成功等同于新版本生效：它可能保留旧的程序路径。脚本必须将 PM2 的程序路径和 cwd 指向本次实际版本，停止仅处理本 Component 的唯一进程名；只读检查应验证目标进程与版本，不使用 `restart` 充当检查。无端口 Worker 同样适用。

例如项目自带 `service.cjs`（负责 PM2 的 activate/stop）与只读 `check.cjs` 时，TUI 可生成以下目标片段；脚本不是 ShipForge 内置命令，必须包含在 `artifact` 中：

```yaml
service:
  start:
    - [node, service.cjs, activate]
  stop:
    - [node, service.cjs, stop]
  check:
    kind: command
    argv: [node, check.cjs]
```

这里更新和恢复复用 activate；脚本应从自己的版本目录加载程序。不要在参数中填写秘密：可识别的密码/Token 参数会被拒绝，但不能保证发现任意位置的秘密。需要凭据时使用服务器既有安全配置。

## 旧配置与历史

schema 1 仅作为一次性读取转换入口：旧 `systemd` 在内存中转换为相同命令计划，TUI 打开配置编辑器，按 `p` 预览、`c` 确认后写入 schema 2；确认前不能部署。读取本身不改文件，等价转换保留身份、root 和 generation。schema 2 不接受旧 `systemd` 字段，也不接受字符串服务配置。

新部署冻结非秘密目标命令与检查快照。历史缺少服务快照时不借当前配置补造回滚命令，需人工恢复；文件型旧历史仍按原有身份和版本证据判断。命令或检查变更增加 generation，旧 generation 不自动接管。历史 JSON 增加可选快照和命令 cwd，旧记录仍可读取；旧版本程序不能保证读取新记录，降级前保留备份。

## 当前实现的唯一配置格式

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

这里 `backend` 与 `worker` 复用同一个 Destination，但各自拥有独立 root、Release、服务和回滚边界。部署时可选择任意 Component 子集，不要求部署全部 Component。

## 默认值

- YAML 省略 Component 工作目录时默认为 `./<component-name>`；`artifact` 相对于该工作目录，而非固定相对于 Project 根目录。首次设置的手动添加表单预填显式 `.`，表示使用所选项目根目录，不改变 YAML 的省略规则。
- `artifact: dist` 始终只是一个构建输出路径；构建后自动识别文件或目录，不存在或为空时报错。用户无需填写产物类型。
- Destination ID 由系统生成且不可变，例如 `dst_…0002`；TUI 直接显示 `deploy@app.example.com:22` 等端点摘要，不要求填写连接名称，也不提供别名字段。
- Project、Environment、Component 使用配置名称；Destination 在项目 YAML 中只使用系统 ID，TUI 自动显示 `user@host:port` 摘要。
- 未配置 `root` 时，首次创建使用 `/srv/shipforge/<project>/<environment>/<component>`，并将结果固化到 `_shipforge.resolvedRoot`；名称变化不自动移动远端目录。
- 同一 Destination 上不同 Component 的 root 不得相同或相互嵌套。
- systemd 预设使用完整 `.service` unit 名，填充 start/stop 和必需的 `service.check` 稳定性检查；更新与恢复默认复用 start，无需重复填写。
- URL 形式的 `health` 是必需的 Destination 端 HTTP 检查；仅接受长度受限、无凭据和 fragment 的 `http://` 或 `https://` URL。检查由 Destination 上的 `curl` 发起且只接受 2xx；无外部接口可用只读命令或 systemd 稳定性检查。
- `after` 只能引用同一 Environment 中已配置的 Component。它只排列本次同时选中的 Component，不会自动加入依赖项；只部署 `worker` 时，`after: [backend]` 不会部署 `backend`。未知引用、自引用和依赖环均为错误；无依赖项按名称稳定排序。
- YAML 映射顺序不表示执行顺序；版本格式和失败回滚使用产品默认值。
- 成功部署后默认保留每个所选 Component 最新 5 个 Release，并额外保护 current、上一健康版本及未结束/待核实操作的引用。无需填写保留数量；计划预览会提示自动清理，证据不足则保留，清理失败只警告、不回滚成功部署。

加载阶段解析结构、默认值、身份和依赖图；构建完成后自动识别并校验构建输出类型，再冻结 Deployment Plan。任何校验错误都不得改写配置。

## 产物规则

`artifact` 始终是一个文件或目录构建输出路径，不是压缩包，也不提供类型或权限字段。ShipForge 将它恰好打包一次，生成该 Component 唯一的部署产物：不可变的 `<version>.tar.gz` **Release**，并计算 SHA-256。所有部署实现只消费这一个 Release；`linux-ssh` 原样保存压缩包并解压运行，解压目录不是第二种产物。完整示例见 [`docs/examples/shipforge.yaml`](examples/shipforge.yaml)。

## 变更与验证规则

- 修改 Destination 端点时使用 TUI 连接管理页，由系统增加 Destination revision。
- 选择 SSH Key 只更新用户级凭据引用，不得把 Key 路径复制到项目配置。
- 通过 TUI 修改某 Component 的 Destination ID、root、服务命令或检查时，由系统增加该 Environment/Component 的 generation。
- Project 或 Environment 重命名应保留稳定 ID 和已固化 root。
- 编辑已有目标时，root 留空表示继续使用原固化 root；只有新增目标使用默认 root。要迁移部署目录必须显式填写新 root，并由系统更新 generation。
- Component 名称就是其配置身份；重命名按删除旧 Component、增加新 Component 处理，不继承旧 Release。
- 新增 Environment 时，只配置该环境确实需要部署的 Component。
- 版本控制应保留 `_shipforge`，但绝不提交用户级 Destination 注册表或凭据。
- 计划必须展示所选 Component、Destination、root、服务、健康检查及依赖顺序，供用户在远端副作用前确认。

已有项目从概览的 `e` 进入草稿编辑；逐层 Apply 后按 `p` 预览完整 YAML，再以 `c` 确认保存。保存前会复核原文件和连接快照，检测到变化则停止。保存期间避免其他编辑器同时写入，不承诺外部并发写入的原子隔离。操作与取消说明见 [TUI 使用指南](tui-guide.md)。
