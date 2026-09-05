# TUI-02：选择式设置与可恢复状态验收

日期：2026-09-04。状态：TUI-02 通过自动化工作包验收；M3 其余工作包、M4 和完整 MVP 尚未完成。

## 实施范围

- 共享 `F4` 候选搜索与 `F1` 页面帮助，覆盖项目/目录、Environment/Component、连接/身份和远端目录/服务等已有候选。搜索回车只选择原页面候选，不穿透到连接、部署或保存；应用前复核候选快照。历史 Environment ID 选择仅搜索当前页，不提供历史记录或日志搜索。
- 列表保持当前项可见，以反显、粗体和文字标识提供非颜色线索；区分空、加载、错误与未知。目录/Key 枚举有限长，渲染不做文件系统读取。最近项目读取失败保留警告并禁用旧缓存，不覆盖损坏注册表。
- 首次 Component 发现失败或无候选时仍可手动添加，程序和参数分别编辑；表单只改草稿，最终 YAML 确认才保存。首次 SSH 身份、Host Key 与认证转为可取消的后台任务，匹配请求 ID 并等待线程退出后导航；取消后的迟到成功不能继续注册连接。
- 已保存连接可用于只读远端目录浏览、root/systemd 探测和选择。目录仅列直接子项，不创建路径或修改服务；失败/取消保持未知并可重试。root 与服务逐 Environment/Component 设置，初次设置和项目编辑共用选择页，不增加全局部署设置。
- `_shipforge` 缺失/损坏时提供新身份重新初始化：保留可验证的用户配置和 root，拒绝含糊路径、无效引用及源文件漂移。预览完整 YAML，明确确认后才原子替换；不读取远端或重建历史。成功写入 YAML 后最近列表登记失败，不改报为配置未保存。
- 本机凭据和连接分别保存时，区分未确认写入与已知结果，只有证据足够才撤回本次新凭据，不假定跨文件事务。原始解析/存储/SSH 错误不直接进入设置页；保留可行动的阶段诊断。

没有新增 YAML 形式、用户填写 ID、别名、Driver 字段、AI/CLI 接口、数据库迁移或锁；没有修改部署、健康、回滚和保留清理算法。

## 审查与回归

实现按界面状态、选择适配器、重新初始化服务和 SSH 协议夹具拆分；根代理集成复审，不同实现者交叉检查确认、取消、文件证据及远端读取边界。最后的函数拆分也经独立复核，无 Clippy 豁免。

审查和测试修正包括：取消后迟到成功推进页面、旧连接的探测结果用于新 Component root、失败浏览回显旧目录、长列表隐藏固定上下文、重新初始化预览截断长行/超过 65,535 行，以及 YAML 成功但最近列表失败仍显示旧缓存可用。实际键盘流程覆盖多 Component 独立设置、手动首次设置及项目编辑的逐层 Apply/最终确认边界。

初轮协议测试暴露夹具临时目录在客户端使用密钥前释放；改由运行夹具持有到客户端及清理结束，未放宽公钥或 Host Key 检查，随后六项新增协议测试及全套回归通过。取消测试使用明确的 worker-start 信号消除时序竞争；没有把失败用例忽略或降低通过条件。

| 证据位置 | 主要覆盖 |
| --- | --- |
| `src/tui/picker.rs`、`src/tui/app/search/` | 查询/列表边界、快照变化拒绝、只聚焦、帮助隔离、长列表和空状态 |
| `src/tui/app/setup_{async,save,diagnostics}/`、`setup_flow_tests.rs` | 真实后台 worker 取消/迟到结果、注册结果不确定、字段诊断、无发现候选的完整设置流程 |
| `src/tui/app/remote_target/`、`project_edit/tests/remote_targets.rs` | 独立 root/服务、嵌套草稿保存、80×10 视口、未知/重试、初次与后续探测共用候选校验 |
| `src/application/connection_management/target_setup/` | 保存连接的准确 revision/凭据快照、取消、时限、只读和候选校验 |
| `src/config/reinitialize.rs`、`src/application/project_reinitialize/`、`src/tui/app/reinitialize/` | 新身份、可靠路径保留、完整且可横向滚动的 YAML、原文件/连接变化、明确确认与持久化结果 |
| `src/drivers/linux_ssh/setup_probe/directories/`、`tests/support/remote_directory_protocol.rs` | 路径/响应边界、Linux 原生命令、真实 SSH 认证/输出/取消/超时和断连 |

测试使用临时项目/注册表、Fake Driver/设置适配器、Ratatui `TestBackend` 和回环协议服务器，不读取真实 Project/Destination。命令及夹具边界见 [测试指南](../../tests/README.md)。

## 质量门禁

冻结 Rust 源码使用 Rust/Cargo 1.96.1，Windows 原生与 WSL Ubuntu-22.04 分别执行；Linux 使用既有隔离工具链和 target，不修改默认工具链。

```sh
cargo test --locked --all-targets --all-features --quiet
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo build --locked --release
```

| 门禁 | Windows | Linux |
| --- | --- | --- |
| 库测试 | 749 通过 | 765 通过 |
| SSH/SFTP 协议测试 | 14 通过 | 14 通过 |
| 中断恢复父用例 | 1 通过（内部 15 个场景） | 1 通过（内部 15 个场景） |
| 格式 / 严格 Clippy | 全部通过 | 全部通过 |
| release 编译 | 通过 | 通过 |

两端默认套件各有 8 项 ignored：6 项外部 SSH/systemd 用例、1 项独立连接诊断、1 项由父用例显式启动的中断助手，未计作通过。Windows `cargo audit` 更新 RustSec 数据库后退出码为 0；保留已有 `wnaf 0.14.0` 撤回警告，本阶段未改变依赖。

## 范围限制与后续

新增六项目录协议用例使用真实 loopback SSH 握手、签名公钥认证和字节传输，远端命令/输出由夹具模拟；另有 Linux 单测对临时目录执行真实只读命令。它们不等于外部 OpenSSH 验收。本阶段没有重跑两 Destination 或 WSL systemd 实机部署；原 M1/M2 证据保持其原范围。早期未定位 SSH 超时/unknown current 继续由 `QA-01` 复核，不能由本阶段通过推断已修复。

远端候选不是部署权限、目录隔离或服务健康证明；最终部署仍需预检和确认。新身份不会接管旧远端目录。最近项目损坏不会自动重建，登记失败可能阻止普通项目打开，需处理注册表后重试。

重新初始化仅适用于现存且用户配置有效的 YAML。整个 `_shipforge` 缺失/null 且未指定 root 时采用新 Project 默认值；系统区部分损坏又没有可靠固化路径或显式 root 时拒绝推断。整份 YAML 缺失走普通新项目设置，不进入恢复流程。

`TUI-03` 继续步骤/日志搜索、筛选、复制与导出；`TUI-04` 统一管理页面；`TUI-05` 审查所有运行中退出和中断路径（包括现有部署规划线程的关闭跟踪）；后续 [TUI-06](tui-06.md) 验证与 UI 排空解耦的有界淘汰、内存及合成输入延迟。本阶段自动化渲染不等于人工终端体验、完整退出恢复或跨平台发布验收。
