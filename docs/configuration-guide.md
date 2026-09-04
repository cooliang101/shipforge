# ShipForge Configuration Guide

本文是 ShipForge 项目配置的唯一规范入口。MVP 只允许通过 TUI 创建和修改配置；AI Agent 调用、AI Agent 直接编辑和自动化接口全部冻结，不设计入口、协议或兼容层。SSH Agent 只是 SSH 密钥签名方式，不属于 AI Agent 功能。

## 配置边界

| 文件或存储 | 内容 | 维护者 |
| --- | --- | --- |
| `shipforge.yaml` | Component、Environment 部署意图，以及 `_shipforge` 系统维护区 | TUI |
| 用户级 Destination 注册表 | 系统生成的 ID、连接类型、端点、修订及凭据引用 | TUI 连接管理页 |
| 用户级凭据注册表 | SSH Agent 指纹或私钥路径；项目文件只引用句柄 | TUI Key 选择器 |

`shipforge.yaml` 必须位于项目根目录，不得包含密码、Token、私钥正文或个人 SSH Key 路径。`_shipforge` 保存 Project/Environment ID，以及每个 Environment/Component 的 generation 和固化 root；不得绕过 TUI 手工修改。

配置文件不存在时，该目录作为新 Project 初始化并生成新身份，不从用户目录缓存或远端恢复。配置存在但 `_shipforge` 缺失或损坏时必须停止；只有用户在 TUI 明确确认“作为新项目重新初始化”后才能生成全新身份。远端 Deployment Marker 只用于目标冲突检查，不是配置备份。

## 选择优先的首次设置

1. 让用户选择项目目录；若已有 `shipforge.yaml`，立即加载并校验。
2. 扫描 `package.json`、`Cargo.toml`、`go.mod`、Dockerfile 和构建脚本，展示推断出的 Component、构建命令与构建输出路径，供用户勾选或修正。
3. 展示已有 Destination 和 SSH config 中的 Host；新建连接时优先让用户选择 SSH Agent、`IdentityFile` 或发现的 Key。
4. 首次连接前展示 Host Key 指纹；连接成功后展示可用目录和 systemd unit 候选。
5. 用户为每个 Component 选择 Destination；同一 Destination 可被任意 Project 的多个 Component 复用。
6. 展示规范化计划，确认后原子写入项目根目录的 `shipforge.yaml`。

除主机地址、特殊部署目录等无法可靠发现的值外，应使用选择、确认和默认值完成设置。Project/Environment/Destination/Deployment ID 和 Release 版本均由系统生成，用户无需命名连接。取消向导不得留下半成品配置或 Destination。

MVP 中用户只选择 SSH 连接，不选择 Driver。TUI 根据连接记录自动使用内置 `linux-ssh` 实现；Driver 名称、能力标识符和 SFTP 参数不进入项目配置。

## TUI 配置规则

1. 按可独立构建、部署或回滚的边界识别 Component；不要把每个源码目录都定义成 Component。
2. 为每个 Component 生成可复现的 argv 构建命令和唯一 `artifact` 构建输出路径。MVP 不接受 Shell 字符串或另一套命令格式。
3. 在每个 Environment 下直接配置需要部署的 Component。每项至少选择一个 `to` Destination ID，可选设置 `root`、服务、健康检查和 `after`。
4. 只询问无法从仓库、SSH config、Destination 注册表或远端探测确定的信息，不猜测生产基础设施。
5. 生成固定格式的最小配置，再通过 TUI 预览部署计划。

## 唯一配置格式

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

这里 `backend` 与 `worker` 复用同一个 Destination，但各自拥有独立 root、Release、服务和回滚边界。部署时可选择任意 Component 子集，不要求部署全部 Component。

## 默认值

- Component 工作目录默认为 `./<component-name>`。
- `artifact: dist` 始终只是一个构建输出路径；构建后自动识别文件或目录，不存在或为空时报错。用户无需填写产物类型。
- Destination ID 由系统生成且不可变，例如 `dst_…0002`；TUI 直接显示 `deploy@app.example.com:22` 等端点摘要，不要求填写连接名称，也不提供别名字段。
- Project、Environment、Component 使用配置名称；Destination 在项目 YAML 中只使用系统 ID，TUI 自动显示 `user@host:port` 摘要。
- 未配置 `root` 时，首次创建使用 `/srv/shipforge/<project>/<environment>/<component>`，并将结果固化到 `_shipforge.resolvedRoot`；名称变化不自动移动远端目录。
- 同一 Destination 上不同 Component 的 root 不得相同或相互嵌套。
- `systemd` 使用完整 `.service` unit 名，并自动增加一项必需的远端 systemd 稳定性检查。MVP 采用内置的 10 秒稳定窗口、1 秒间隔、5 次就绪尝试和单次 10 秒命令超时，不要求用户填写这些参数。
- URL 形式的 `health` 是必需的 Destination 端 HTTP 检查；仅接受长度受限、无凭据和 fragment 的 `http://` 或 `https://` URL。检查由 Destination 上的 `curl` 发起且只接受 2xx；无外部接口的服务使用 systemd 稳定性检查。
- `after` 只能引用同一 Environment 中已配置的 Component。它只排列本次同时选中的 Component，不会自动加入依赖项；只部署 `worker` 时，`after: [backend]` 不会部署 `backend`。未知引用、自引用和依赖环均为错误；无依赖项按名称稳定排序。
- YAML 映射顺序不表示执行顺序；发布保留数量、版本格式和失败回滚使用产品默认值。

加载阶段解析结构、默认值、身份和依赖图；构建完成后自动识别并校验构建输出类型，再冻结 Deployment Plan。任何校验错误都不得改写配置。

## 产物规则

`artifact` 始终是一个文件或目录构建输出路径，不是压缩包，也不提供类型或权限字段。ShipForge 将它恰好打包一次，生成该 Component 唯一的部署产物：不可变的 `<version>.tar.gz` **Release**，并计算 SHA-256。所有部署实现只消费这一个 Release；`linux-ssh` 原样保存压缩包并解压运行，解压目录不是第二种产物。完整示例见 [`docs/examples/shipforge.yaml`](examples/shipforge.yaml)。

## 变更与验证规则

- 修改 Destination 端点时使用 TUI 连接管理页，由系统增加 Destination revision。
- 选择 SSH Key 只更新用户级凭据引用，不得把 Key 路径复制到项目配置。
- 通过 TUI 修改某 Component 的 Destination ID 或 root 时，由系统增加该 Environment/Component 的 generation。
- Project 或 Environment 重命名应保留稳定 ID 和已固化 root。
- Component 名称就是其配置身份；重命名按删除旧 Component、增加新 Component 处理，不继承旧 Release。
- 新增 Environment 时，只配置该环境确实需要部署的 Component。
- 版本控制应保留 `_shipforge`，但绝不提交用户级 Destination 注册表或凭据。
- 计划必须展示所选 Component、Destination、root、服务、健康检查及依赖顺序，供用户在远端副作用前确认。
