# 个人使用版收尾验收

日期：2026-09-08。基线 `64388bb`，产品修复提交 `0da0663`。范围是路线图中的轻量安全复核、交付说明和代表性验收，不是完整故障矩阵认证。测试使用临时目录、回环 SSH 和普通用户 WSL，不读取保存的生产凭据或执行新的业务发布。

## 轻量安全复核

| 关注点 | 代码与代表性证据 | 结论 |
| --- | --- | --- |
| 防误删与应用范围 | `src/drivers/linux_ssh/inplace.py` 的 canonical/safe_path/snapshot/remove_files；Python 契约验证运行数据哨兵、旧静态资源、硬链接、路径遍历、外部漂移、按操作身份丢弃上传 | 删除限定在已记录应用文件集合，空目录单独移除；不递归删除部署 root。首次范围是产物顶层条目，不能在对应应用子目录混放运行数据。 |
| 凭据与日志 | `src/config/password.rs` 使用当前用户 DPAPI；connection/sudo 路径单次发送密码且不保留密码通道输出；日志流脱敏测试覆盖跨片段秘密、私钥及诊断 | 保留原防护；发现并修复下述解析器诊断透出问题。 |
| Host Key 与 argv | connection.rs 先完成 strict Host Key 握手，再认证；密码协议测试验证错误 pin 不收到密码；security.rs 对程序、每个参数和 cwd 分别引用 | 不跳过验证，不混用 argv 与 Shell。 |
| 上传完整性 | SFTP 独占创建、有限重试与清理；prepare 校验大小、SHA-256、manifest 和包路径，publish 再校验摘要 | 回环生产 Driver 与损坏包契约通过，失败不覆盖应用。 |
| 失败、取消与未知结果 | Driver 在服务命令前记录 pending，未知结果阻止竞争恢复；已知失败使用独立取消令牌恢复；WSL 验证 EIO、权限拒绝和 SIGKILL | 代表性失败通过，未知状态保留现场；不据此宣称断电持久性或所有真实服务故障已验证。 |

具体修复：原发布器把所有 `ValueError` 都当作可展示错误，导致损坏 JSON 或非法编码产生的底层诊断直接进入 RPC。新增 `ApplicationError`，仅明确编写的固定业务提示可透出，其他异常统一为 `Remote application operation failed`。不改文件替换、状态迁移或恢复规则，不改变 schema。

新增 `test_corrupt_state_reports_safe_error_without_parser_diagnostics`，通过真实 Python 子进程入口验证损坏 JSON 和非法编码两种情况：退出码 1、固定错误、recoverable=false、无 stderr，以及应用和原损坏状态字节均不变。修复前两种情况均失败，修复后通过。

## 本机门禁与交付文件

- `cargo test --all-targets --all-features` 通过。显式远端 fixture 和 release 冒烟默认忽略项没有计作通过；本轮另外运行下述 release 冒烟。
- `cargo fmt --all -- --check` 与 `cargo clippy --all-targets --all-features -- -D warnings` 通过。
- `python -B tests/inplace_contract.py`：17 项通过。
- 普通用户 Ubuntu-22.04 的 `tests/inplace_faults.py`：22 项通过（17 项基础契约加 5 项故障），无跳过。基础契约与前一行重复，不累加为独立覆盖。
- `cargo build --release` 通过。首次因用户打开旧 exe 而无法覆盖，用户正常退出后重建成功；没有强制终止部署进程。
- 使用 `SHIPFORGE_RELEASE_SMOKE_BINARY` 指向下列准确文件，两个 ConPTY 冒烟通过：`q` 退出，以及 idle Ctrl+C 字节后 `q` 退出/终端恢复。后者不证明空闲 Ctrl+C 被 UI 消费，也不等同运行中部署中断验收。

文件：`target/release/shipforge.exe`；10,642,432 字节；构建时间 2026-09-08 14:05:41 +08:00。

SHA-256：`320753f26deb1288d5a8727025ce252a5800341a7eef03c7744c57a09dee01ae`。

该 exe 为本轮产品修复后的 Windows x64 GNU 构建。随后仅同步文档，不需要为文档重新构建。

## 配置、使用与最终范围

复用全量测试中已通过的 TUI 配置流程：`reviews_and_commits_setup_only_after_confirmation` 验证按键保存项目及最近列表；`custom_commands_are_draft_only_until_yaml_confirmation_and_survive_reload` 验证自定义 argv 经 TUI 保存后重新从磁盘加载，schema 2 和其他组件配置保留；`password_connection_masks_input_and_reopens_saved_encrypted_credential` 验证密文登记后重新加载、重开编辑及取消不覆盖；`preferences_round_trip_and_unknown_language_is_not_silently_replaced` 验证语言偏好复用。结合项目打开路径的代码复核，配置不依赖上次向导的内存草稿。这里是自动化 TUI/持久化验证，不称为另一次人工业务部署。

TUI 输入、默认值、窄窗口、错误页和取消测试通过，本轮没有发现需要另改 UI 的具体缺口。交付说明统一到[个人使用指南](../personal-use.md)：直接运行、重新打开配置、正常退出后更新、Windows 文件占用及已知失败/未知结果的处理。当前 TUI/配置说明、术语和 SSH 技术说明同步原目录语义；旧 ADR 标注已被替换的布局，历史验收保留原日期及范围。44 个 Markdown 文件的本地链接目标检查通过，修改内容的空白检查通过（保留既有 CRLF 文件换行）。

实际项目证据复用 [aiagent 发布记录](inplace-deployment.md#aiagent-实测)：两个组件发布、远端摘要与状态、服务运行以及接口响应。本轮产品修改只限制错误文本，没有改变部署逻辑，未重复向业务服务器发布。

个人使用版三项收尾已完成。真实 systemd/PM2 故障、SSH 断连、客户端强制结束、多组件真实恢复及断电持久性仍不属于本次新增验证，按实际问题和相关代码改动补充，不作为个人使用版阻塞项。参见[路线图](../roadmap.md)。
