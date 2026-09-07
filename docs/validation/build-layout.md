# Windows 固定构建目录与缓存清理

日期：2026-09-07。基于提交 `39babd6`，仅调整仓库构建配置、Windows 验收入口及文档，未修改产品 Rust 源码或依赖。此记录不代替后续安全审查、安装验证或完整 MVP 验收。

## 固定规则

- `rust-toolchain.toml` 选择现有 `stable-x86_64-pc-windows-gnu`；本次仍为 Rust 1.96.1，没有安装、升级或移除工具链。
- `.cargo/config.toml` 将缓存根固定为项目的 `target/`，不设置 `build.target`。在仓库根运行 `cargo build --release`，唯一打包入口为 `target/release/shipforge.exe`；不添加 `--target` 或覆盖 Cargo 输出目录。
- Windows ReleaseGate 改为先用有界 `rustc -vV` 验证原生 GNU 编译器，再执行同目录下的 release 测试；拒绝 MSVC、未知主机和 Cargo 目标/输出目录环境覆盖。
- 开发构建仍使用标准 `target/debug/`。需要回收它时使用 `cargo clean --profile dev`，保留 release；完整 `cargo clean` 会连执行文件一起删除，之后必须重新构建。

## 清理范围与结果

删除前确认项目 `target/` 的绝对路径、祖先及子目录无 reparse point，且没有运行中的 Cargo、rustc 或 ShipForge。`cargo clean` 只针对 `D:\cdoe\shipforge\target`：删除 43,387 个文件、34.6 GiB，包含重复的默认/平台构建产物及 macOS 残留。用户级 Cargo registry/Git 下载缓存、已安装工具链和业务配置均未纳入清理。

重新打包和验证后，再用 dev-profile 清理删除本轮的 3,634 个文件、3.1 GiB；release 执行文件的 SHA-256 前后相同。最终缓存 638,933,673 bytes（约 609 MiB），只保留 release、空临时目录和 Cargo 元数据，不再有 debug、Windows triple 或 macOS triple 目录。删除的构建缓存和执行文件均可重新生成。

## 验证

| 检查 | 结果 |
| --- | --- |
| `cargo build --offline --locked --release` | 从空缓存构建通过，固定生成 `target/release/shipforge.exe`，大小 10,838,528 bytes |
| `cargo fmt --all -- --check` / 严格 Clippy | 通过 |
| 全量默认测试 | 1,000 项通过：库 969、入口 2、协议 14、两类 helper 各 7、中断恢复父用例 1；ignored 不计为通过 |
| release ConPTY smoke | 固定执行文件的 `q` 及空闲 Ctrl+C 后按 `q` 各 1 项通过，0.06 / 0.56 秒 |
| Windows runner 回归 | 有界主机检查、错误主机/环境覆盖拒绝、准确测试选择、无 `--target` 参数及既有清理回归通过 |
| 输出与清理复核 | Cargo metadata 指向项目 `target/`；清理 dev 缓存没有修改 release 文件；工具链清单未变 |

Clippy、全量测试和 smoke 仅在进程内设置 `CARGO_INCREMENTAL=0`，结束后恢复原值，避免验证期间积累增量缓存。外部 OpenSSH/systemd 本次未重跑，仍保留其原有验收范围；没有运行 GitHub 测试。

执行文件 SHA-256：`103a697497c900f6d8d224c8b134bdb74a6ba1f0200b5ffee5e983c8c2ba7852`。
