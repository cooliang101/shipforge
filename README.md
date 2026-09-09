# ShipForge

Windows 上的个人部署助手：在终端中构建本地项目，通过 SSH/SFTP 发布到已有 Linux 应用目录。

A personal deployment assistant for Windows: build local projects in a terminal UI and publish them to existing Linux application directories over SSH/SFTP.

[简体中文](#简体中文) · [English](#english)

## 简体中文

### 功能与部署方式

一个 Project（项目）可以包含多个独立构建、发布和恢复的 Component（组件），例如后端和静态前端。每个 Environment（环境）为组件配置 SSH 连接、远端目录和可选服务命令；多个组件可以复用同一连接。

部署流程：本机构建 → 打包为带版本的 `tar.gz` Release → 上传校验 → 保存上一版应用 → 发布到原目录 → 执行服务命令和已配置的检查。明确发布失败时尝试恢复上一版应用；服务执行结果未知时保留现场，先核实再处理。

ShipForge 沿用已有应用目录、systemd/Nginx 配置和权限，不安装远端运行环境，不管理数据库、附件或日志备份。发布逐文件完成，不保证整个目录原子切换；每个组件只保留一个上一版应用压缩包，不是多版本备份系统。

### 1. 准备和启动

| 位置 | 需要准备 |
| --- | --- |
| 本机 | Windows x64；支持 ConPTY 的交互式终端，建议至少 100×28；项目自身的构建工具 |
| Linux 服务器 | SSH/SFTP、Python 3 标准库、应用运行时与服务工具，以及已有部署目录的操作权限 |
| 可选工具 | systemd 预设需要 systemctl；HTTP 健康检查需要 curl；Node.js/PM2 等自定义服务需预先安装相应工具 |

已有 `shipforge.exe` 时，在交互式终端中运行它即可。例如在 exe 所在目录执行：

```powershell
.\shipforge.exe
```

从源码构建时，在 ShipForge 仓库根目录执行，复用已有 Rust/MinGW 工具链：

```powershell
cargo build --release
& .\target\release\shipforge.exe
```

客户端仅支持 Windows x64 GNU。遵循 `rust-toolchain.toml` 和 `.cargo/config.toml`，不要添加 `--target` 或覆盖输出目录；可执行文件固定为 `target/release/shipforge.exe`。开发时可用 `cargo run`。所有部署操作都在 TUI 内完成，没有部署子命令。

### 2. 首次配置项目

1. 按 `F6` 选择 `简体中文`，按 `F1` 查看当前页帮助。
2. 项目列表按 `o` 浏览目录，进入项目根目录后按 `s` 选择。已有有效 `shipforge.yaml` 会直接打开概览；没有配置则进入首次设置。
3. 用空格勾选发现的组件，核对构建命令、工作目录和 `artifact`。没有合适候选时按 `a` 手动添加；工作目录填 `.` 表示项目根目录。`artifact` 相对于组件工作目录，必须是构建后的文件或目录，不能是预先制作的压缩包。
4. 为组件选择已保存 SSH 连接，或按 `a` 新建。支持密码、私钥和 SSH Agent。密码方式按 `F5` 输入遮蔽密码；回车获取 Host Key，通过可信渠道核对指纹后按 `y` 确认，认证成功才保存。
5. 连接分配页用左右键切换组件，空格或回车分配连接，按 `e` 设置远端 `root` 和服务。选择服务器原有应用目录；静态文件可选不管理服务，后端可选 systemd 或自定义命令。同一连接上不同组件的目录不能相同或相互嵌套。
6. 按 `n` 预览 YAML，核对后按 `c` 保存到项目根目录的 `shipforge.yaml`。**保存配置不会部署。**

配置由 TUI 创建和修改，不要手工编辑 YAML 或 `_shipforge` 系统维护区。项目文件不存密码、Token、私钥正文或个人私钥路径。连接和凭据单独保存在本机，密码使用 Windows 当前用户 DPAPI 加密；标准密码认证不包含 keyboard-interactive/MFA。

### 让 AI Agent 协助配置（可复制）

在需要部署的项目中打开 AI Agent，复制下面整段内容发送。提示词会引导 Agent 先阅读 [AI Agent 配置接入指南](https://github.com/cooliang101/shipforge/blob/codex/qa-01/docs/ai-agent-configuration.md)，再根据当前项目整理和保存配置。

```text
请为当前项目接入 ShipForge 部署配置。先完整阅读以下指南，并按需阅读其中链接的配置规范和 TUI 使用指南，再开始配置：
https://github.com/cooliang101/shipforge/blob/codex/qa-01/docs/ai-agent-configuration.md

请检查当前项目已有的 shipforge.yaml、README、构建与发布脚本，以及 package.json、Cargo.toml、go.mod 等相关文件，沿用实际构建和部署方式。指南中的项目名、连接、路径和服务名只是示例，不要复制到当前项目。

请按组件整理：依据文件、构建工作目录、逐项 argv 构建命令、artifact、环境、SSH 连接、远端 root、启动/更新/恢复/停止命令、只读检查或健康 URL，以及必要的 after。只询问无法从现有资料确定的信息，不猜测服务器设置。产物仅包含应用文件，不混入数据库、附件或日志。

配置通过 ShipForge TUI 预览并保存，不直接生成或改写 shipforge.yaml，不手工修改 _shipforge 或连接/凭据注册表。有交互终端能力时按指南完成 TUI 配置并重新打开核对；没有该能力时，交付可逐项填写的配置清单和操作步骤，明确说明尚未保存。

不要读取或输出密码及私钥内容；需要密码时让我在 TUI 的遮蔽输入框中输入。此次任务仅配置，不实际发布；构建测试或远端探测按我已授权的范围执行。最后列出配置依据、待确认项和当前状态，区分已整理、已保存、已验证和已发布。
```

### 3. 发布和后续更新

1. 打开已保存项目，在概览中用左右键选择环境，按 `d` 选择本次要发布的组件。
2. 按页面提示进入检查和计划预览。本机构建及产物校验完成后，核对连接端点、远端目录、版本、应用文件范围、服务命令和检查。
3. 在执行确认页按 `c` 发布；回车不能代替执行确认。
4. 完成后查看每个组件的结果、恢复结果和记录保存告警。按 `l` 查看日志。未配置健康检查时，成功不代表已验证业务接口。

下次更新代码后，重新打开项目并重复发布流程即可。修改配置时从概览按 `e`，逐层选择 `Apply`，回到编辑首页按 `p` 预览、`c` 保存。`after` 只排列本次已选组件，不会自动选中或部署依赖组件。

### 4. 两类配置示例

以下名称、路径、程序和服务名均为虚构示例，用于说明 TUI 填写关系，不是可直接套用的配置。请按已有构建脚本和服务器设置替换。程序和参数逐项输入，不把 `cd ... && ...` 当作一条命令。

**示例 A：后端程序 + 静态前端**

假设项目有 `api`、`web` 子目录，已有后端脚本能在 Windows 上生成 Linux 程序，前端生成纯静态文件：

| TUI 配置项 | 后端组件 `api` | 前端组件 `web` |
| --- | --- | --- |
| 构建工作目录 | `api` | `web` |
| 构建 argv，按顺序 | `[powershell.exe, -NoProfile, -File, build-linux.ps1]` | `[npm, ci]`，然后 `[npm, run, build]` |
| `artifact` | `out/sample-api` | `dist` |
| Environment | `production` | `production` |
| SSH 连接 | 在 TUI 选择已保存连接 | 可复用后端连接 |
| 远端 `root` | `/srv/sample-suite/api` | `/srv/sample-suite/web` |
| 服务 | 选择已有 `sample-api.service` 的 systemd 预设 | 不管理服务，沿用现有静态站点配置 |
| 检查 | systemd 稳定性检查；若确有健康端点，可加 `http://127.0.0.1:8080/health` | 按实际情况配置 HTTP 检查 |

构建脚本不是 ShipForge 内置功能。后端产物必须是 Linux 程序，不能直接发布 Windows exe。文件产物覆盖 root 内的同名文件；目录产物发布其内容，因此 `dist` 的内容会放入 `/srv/sample-suite/web`，不会额外创建一层 `dist`。

产物只能包含应用文件，不要选择混有数据库、附件或日志的目录。应用子目录会纳入替换范围，不能在这些子目录里混放运行数据。

**示例 B：使用 Node.js/PM2 的后台服务**

假设另一项目已有构建脚本生成 `bundle`，包含应用代码、服务控制脚本 `service.cjs` 和只读检查脚本 `check.cjs`；依赖按项目原有方式准备：

| TUI 配置项 | 示例值 |
| --- | --- |
| 组件 / 构建工作目录 | `worker` / `.` |
| 构建 argv，按顺序 | `[npm, ci]`，然后 `[npm, run, build]` |
| `artifact` / 远端 `root` | `bundle` / `/srv/sample-worker` |
| 自定义服务 `start` | `[node, service.cjs, activate]` |
| 自定义服务 `stop` | `[node, service.cjs, stop]` |
| `update` / `restore` | 留空时，更新复用 start，恢复复用有效 update |
| 只读命令检查 | `[node, check.cjs]` |

在目标选择页按 `c` 编辑服务阶段，分别填写程序和参数，再逐层 Apply。所有服务命令和命令检查都在远端 `root` 执行。这些脚本由项目自行提供，负责沿用原应用目录、只操作本组件进程，并支持停止和恢复。检查应验证目标进程及版本，不能用重启命令代替检查；没有 HTTP 端口的 Worker 可以使用只读命令检查。

账号已有 sudo 权限且 SSH 与 sudo 密码相同时，可在服务动作编辑页按 `p` 启用保存密码的 sudo；不在参数中填写密码，也无需修改 sudoers。详细限制见[配置指南](docs/configuration-guide.md#ssh-密码登录)。

### 5. 日志、失败处理和退出

| 入口或现象 | 操作 |
| --- | --- |
| 查看历史 | 概览按 `m`，再按 `h`；回车打开记录，`l` 查看日志 |
| 搜索和导出日志 | 日志页 `/` 搜索保留文件，`f`/`t` 筛选组件/步骤，`p` 查看进度；`e`/`s` 预览并确认导出日志/摘要 |
| 构建失败、产物不存在或为空 | 核对失败步骤、工作目录和 artifact，修正构建或 TUI 配置后重新检查 |
| SSH、指纹或密码错误 | 项目列表按 `c` 管理连接，核实主机和指纹；密码变化按 `F5` 重新输入 |
| 明确失败且已恢复上一版 | 分别核对失败原因与恢复结果；恢复成功不代表此次部署成功 |
| 结果未知或恢复失败 | 保留 Deployment ID，查看历史和日志，在服务器只读核对文件及进程；查明前不要重复发布、启停或删除 `.shipforge-deploy` |
| 执行成功但记录保存告警 | 处理本机历史/日志存储问题，不能据此认为远端没有执行 |

有足够历史证据且远端上一版可用时，可从历史详情按 `r` 选择组件、检查回滚计划，再按 `c` 执行。历史列表不意味着每个旧版本都能恢复。

空闲顶层页面按 `q` 退出。任务活动时，`q` 打开退出确认：`Esc` 或 `r` 返回，`c` 或 `Ctrl+C` 请求取消并等待安全结束。不要强制结束正在发布的进程；关闭宿主或断电可能留下未知结果。

### 更新 ShipForge 与保存配置

先正常退出所有运行中的 ShipForge，再替换 exe 或执行 `cargo build --release`。若构建提示拒绝访问，检查是否仍有 exe 在运行；构建失败后仍存在的旧 exe 不代表更新成功。

保留项目的 `shipforge.yaml` 和当前 Windows 用户的 `%APPDATA%\ShipForge`（连接、加密凭据、历史、日志等）。更新后无需重新建项目；换机器或 Windows 用户后，应在连接管理中重新输入密码。备份应在退出程序后进行并按私有资料保管；旧版本不保证兼容新配置或历史。

## English

### Features and deployment behavior

A Project contains independently built, deployed and restored Components, such as an API and a static frontend. Each Environment assigns SSH connections, remote directories and optional service commands to its Components. Multiple Components can share a connection.

Deployment follows: local build → versioned `tar.gz` Release → upload and verification → previous application archive → publish into the existing directory → service commands and configured checks. Known publish failures trigger an attempt to restore the previous application; unknown service outcomes preserve the state for investigation.

ShipForge uses existing application directories, systemd/Nginx configuration and permissions. It does not install remote runtimes or manage database, upload or log backups. Publishing replaces files individually, without an atomic directory switch. Each Component retains only one previous application archive, not a multi-version backup library.

### 1. Prepare and launch

| Location | Requirements |
| --- | --- |
| Local machine | Windows x64; an interactive terminal with ConPTY support, preferably at least 100×28; the project's build tools |
| Linux server | SSH/SFTP, Python 3 standard library, application runtimes and service tools, and existing permissions for the deployment directory |
| Optional tools | systemctl for the systemd preset; curl for HTTP checks; preinstalled tools for custom services such as Node.js/PM2 |

If you already have `shipforge.exe`, run it in an interactive terminal. From its containing directory:

```powershell
.\shipforge.exe
```

To build from source, run these commands in the ShipForge repository root using the existing Rust/MinGW toolchain:

```powershell
cargo build --release
& .\target\release\shipforge.exe
```

The client supports Windows x64 GNU only. Respect `rust-toolchain.toml` and `.cargo/config.toml`; do not add `--target` or override the output directory. The executable is always `target/release/shipforge.exe`. Use `cargo run` for development. All deployment operations happen in the TUI; there are no deployment subcommands.

### 2. Configure your first project

1. Press `F6` to select `English`, and `F1` for help on the current page.
2. From the project list, press `o` to browse, enter the project root and press `s` to select it. A valid existing `shipforge.yaml` opens the overview; otherwise, first-time setup begins.
3. Use Space to select discovered Components. Review build commands, working directories and `artifact` paths. Press `a` to add one manually if needed; use `.` for the project root. The artifact path is relative to the Component's working directory and must identify a built file or directory, not a prebuilt archive.
4. Select a saved SSH connection or press `a` to create one using a password, private key or SSH Agent. For passwords, press `F5` and enter the masked value. Press Enter to fetch the Host Key, verify its fingerprint through a trusted channel, then press `y` to trust it. The connection is saved only after successful authentication.
5. On the connection assignment page, use Left/Right to switch Components, Space or Enter to assign a connection, and `e` to configure the remote `root` and service. Select the existing application directory. Static files can use no service; backends can use systemd or custom commands. Component roots on the same connection must not be identical or nested within one another.
6. Press `n` to preview the YAML, review it, then press `c` to save `shipforge.yaml` in the project root. **Saving configuration does not deploy.**

Create and edit configuration through the TUI. Do not manually edit the YAML or its `_shipforge` system section. Project files must not contain passwords, tokens, private-key contents or personal private-key paths. Connections and credentials are stored separately on this machine; passwords use Windows current-user DPAPI encryption. Standard password authentication does not include keyboard-interactive/MFA.

### Configure with an AI Agent (copyable prompt)

Open your AI Agent in the project you want to deploy and send the entire prompt below. It directs the Agent to read the [AI Agent configuration guide](https://github.com/cooliang101/shipforge/blob/codex/qa-01/docs/ai-agent-configuration.md) before preparing and saving configuration for your project.

```text
Set up ShipForge deployment configuration for the current project. First read this guide in full, and follow its links to the configuration specification and TUI guide as needed:
https://github.com/cooliang101/shipforge/blob/codex/qa-01/docs/ai-agent-configuration.md

Inspect the project's existing shipforge.yaml, README, build/deployment scripts and relevant manifests such as package.json, Cargo.toml or go.mod. Preserve the existing build and deployment approach. Treat project names, connections, paths and service names in the guide as examples; do not copy them into this project.

For each Component, identify supporting files, build working directory, build commands as separate argv entries, artifact, Environment, SSH connection, remote root, start/update/restore/stop commands, read-only checks or health URL, and any necessary after dependencies. Ask only for information unavailable from existing sources; do not guess server settings. Artifacts must contain application files only, excluding databases, uploads and logs.

Preview and save configuration through the ShipForge TUI. Do not directly generate or edit shipforge.yaml, its _shipforge section, or connection/credential registries. If interactive terminal control is available, complete the TUI setup and reopen it to verify. Otherwise, provide a field-by-field configuration checklist and TUI instructions, clearly stating that nothing has been saved.

Do not read or disclose passwords or private-key contents. Have me enter passwords in the TUI's masked input. This task is configuration only, without deployment; run build tests or remote probes only within my existing authorization. Finish with supporting evidence, unresolved inputs and the current status, distinguishing preparation, saving, verification and deployment.
```

### 3. Deploy and update an application

1. Open the saved project, select an Environment with Left/Right in the overview, and press `d` to select only the Components you want to publish.
2. Follow the prompts for checks and plan preview. After local building and artifact validation, review the connection endpoint, remote directory, version, application file scope, service commands and checks.
3. Press `c` on the execution confirmation page to deploy. Enter does not substitute for execution confirmation.
4. Review each Component's outcome, recovery result and persistence warnings. Press `l` for logs. Success without a configured health check does not mean a business endpoint was verified.

For later code updates, reopen the project and repeat the deployment flow. To change configuration, press `e` from the overview, Apply each form, then press `p` on the editor home page to preview and `c` to save. `after` orders only Components selected for this deployment; it does not automatically select or deploy dependencies.

### 4. Two configuration examples

All names, paths, programs and service names below are fictional. They illustrate TUI fields rather than ready-to-use configurations. Replace them with values from your existing build scripts and server setup. Enter programs and arguments separately; do not enter `cd ... && ...` as one command.

**Example A: backend binary and static frontend**

Assume the project has `api` and `web` subdirectories, an existing backend script that produces a Linux binary from Windows, and a frontend build that produces only static files:

| TUI field | Backend Component `api` | Frontend Component `web` |
| --- | --- | --- |
| Build working directory | `api` | `web` |
| Build argv, in order | `[powershell.exe, -NoProfile, -File, build-linux.ps1]` | `[npm, ci]`, then `[npm, run, build]` |
| `artifact` | `out/sample-api` | `dist` |
| Environment | `production` | `production` |
| SSH connection | Select a saved connection in the TUI | May share the backend connection |
| Remote `root` | `/srv/sample-suite/api` | `/srv/sample-suite/web` |
| Service | Select the systemd preset for an existing `sample-api.service` | No service management; keep the existing static-site configuration |
| Checks | systemd stability check; optionally `http://127.0.0.1:8080/health` if that endpoint actually exists | Configure an HTTP check if appropriate |

Build scripts are not built into ShipForge. The backend output must run on Linux; do not publish a Windows exe. A file artifact replaces the same filename inside root. A directory artifact publishes its contents: files from `dist` go directly into `/srv/sample-suite/web`, without an extra `dist` directory.

Artifacts must contain only application files, excluding databases, uploads and logs. Application subdirectories fall within the replacement scope, so runtime data must not be mixed into them.

**Example B: a Node.js/PM2 background service**

Assume another project's existing build produces `bundle`, containing application code, a `service.cjs` control script and a read-only `check.cjs` script. Dependencies are prepared using the project's existing process:

| TUI field | Example value |
| --- | --- |
| Component / build working directory | `worker` / `.` |
| Build argv, in order | `[npm, ci]`, then `[npm, run, build]` |
| `artifact` / remote `root` | `bundle` / `/srv/sample-worker` |
| Custom service `start` | `[node, service.cjs, activate]` |
| Custom service `stop` | `[node, service.cjs, stop]` |
| `update` / `restore` | Leave empty to reuse start for updates and the effective update action for restoration |
| Read-only command check | `[node, check.cjs]` |

Press `c` on the target selection page to edit service stages, enter programs and arguments separately, and Apply each form. All service commands and command checks run in the remote `root`. The project must supply these scripts. They should use the existing application directory, operate only on this Component's process, and support stopping and restoration. Checks should verify the target process and version, not restart it. Workers without an HTTP port can use read-only command checks.

If the account already has sudo permission and SSH and sudo use the same password, press `p` in the service action editor to enable saved-password sudo. Never put the password in arguments; no sudoers change is required. See the [configuration guide](docs/configuration-guide.md#ssh-密码登录) for limitations.

### 5. Logs, failures and exit

| Entry point or situation | Action |
| --- | --- |
| View history | Press `m` from the overview, then `h`; Enter opens a record and `l` opens logs |
| Search and export logs | In logs, `/` searches retained files, `f`/`t` filters Components/steps, and `p` shows progress; `e`/`s` previews log/summary export before confirmation |
| Build fails, artifact missing or empty | Check the failed step, working directory and artifact; fix the build or TUI configuration and run checks again |
| SSH, fingerprint or password error | Press `c` from the project list to manage connections; verify the host and fingerprint, and use `F5` to replace a changed password |
| Known failure with previous application restored | Review the failure and recovery result separately; successful recovery does not make the deployment successful |
| Unknown outcome or failed recovery | Keep the Deployment ID, inspect history/logs, and verify server files and processes read-only. Do not repeat publishing/service actions or delete `.shipforge-deploy` before resolving the uncertainty |
| Execution succeeded with a recording warning | Address local history/log storage problems; the warning does not mean the remote action never happened |

When sufficient historical evidence and the remote previous archive are available, press `r` in history details, select Components, check the rollback plan, and press `c` to execute. A version appearing in history does not guarantee it can be restored.

Press `q` on an idle top-level page to exit. With active work, `q` opens exit confirmation: `Esc` or `r` returns; `c` or `Ctrl+C` requests cancellation and waits for a safe end. Do not forcibly terminate an active deployment. Closing the terminal host or losing power can leave an unknown outcome.

### Updating ShipForge and preserving configuration

Exit all running ShipForge instances normally before replacing the exe or running `cargo build --release`. If the build reports access denied, check for a running exe. An old executable remaining after a failed build does not mean the update succeeded.

Keep each project's `shipforge.yaml` and the current Windows user's `%APPDATA%\ShipForge` directory, which stores connections, encrypted credentials, history and logs. Updates do not require recreating projects. On another machine or Windows account, re-enter passwords through connection management. Make backups after exiting and treat them as private data. Older versions may not understand newer configuration or history.

## 开发验证 / Development checks

```powershell
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

验证在本机 Windows 上运行，不使用 GitHub Actions 测试流程。纯文档变更只需一致性和链接检查，无需重新构建。默认测试不连接生产服务器，真实 SSH/服务检查使用隔离环境。可用现有 cargo-audit 单独运行 `cargo audit`。

Checks run locally on Windows, without GitHub Actions test workflows. Documentation-only changes need consistency and link checks, not a fresh build. Default tests never contact production; real SSH/service checks use isolated fixtures. Run `cargo audit` separately with the existing cargo-audit tool.

`cargo clean --profile dev` 只清理开发构建缓存并保留 release exe；完整 `cargo clean` 会删除 exe，需要重新构建。

Use `cargo clean --profile dev` to reclaim development build caches while preserving the release exe. A full `cargo clean` removes the executable and requires rebuilding it.

## 文档 / Documentation

详细使用文档目前以中文为主。Detailed usage documents are currently primarily in Chinese.

- [个人使用指南 / Personal-use guide](docs/personal-use.md)
- [配置规范 / Configuration guide](docs/configuration-guide.md)
- [完整按键与操作 / TUI guide](docs/tui-guide.md)
- [部署范围与恢复边界 / Deployment contract](docs/deployment-contract.md)
- [需求 / Requirements](docs/requirements.md) · [架构 / Architecture](docs/architecture.md)
- [路线图 / Roadmap](docs/roadmap.md)
- [测试说明 / Test instructions](tests/README.md)
- [构建目录说明 / Build layout](docs/validation/build-layout.md)
- [贡献约定 / Contributor guide](AGENTS.md)
