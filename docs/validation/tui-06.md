# TUI-06：帧调度与压力门禁验收

日期：2026-09-05。状态：TUI-06 通过工作包验收；实现、两轮独立代码审查、Windows/WSL Linux 最终门禁及文档复核均已完成。M3 已完成，M4 与完整 MVP 尚未完成。

## 实施范围

- `FrameSchedule` 首帧立即绘制；纯后台日志和活动耗时刷新以 50 ms 帧间隔合并，干净空闲界面只以 250 ms 等待输入/后台状态而不重绘。按键、resize、信号和任务完成页面边界立即安排下一帧。
- 事件循环每轮最多读取一个终端事件，再回到状态处理和绘制；确认页不会被同一批隐藏按键穿透。无日志的活动部署仍周期刷新耗时。
- Overview、DeploySelection、DeploymentRunning、DeploymentReview 和 DeploymentFinished 的高频滚动/选择路径改为原地更新；完整配置或计划只在确需转移所有权的页面切换时克隆。
- 运行页和完成页不再各自保存日志副本。`App.live_logs` 是跨运行/完成页面的规范有界窗口，完成边界先执行最终 drain；日志覆盖层只在打开时持有工作副本。

本工作包没有新增 YAML、数据库 schema、Driver 能力、锁、网络操作、CLI/Agent 入口或依赖。

## 显式性能门禁

父测试和子测试都标为 ignored，避免计时断言与默认约 1,000 项测试竞争。唯一支持的运行方式是独占、精确、单线程执行父测试：

```sh
cargo test --locked --lib tui::app::performance_tests::isolated_tui_stress_stays_responsive_and_memory_bounded -- --ignored --exact --nocapture --test-threads=1
```

父进程用随机 nonce 启动当前 test binary 的精确子测试，45 秒超时后终止并回收。子进程先分 6 批向 500 行 `LogView` 投递 600 行，确定性证明窗口淘汰；随后 4 个生产线程并发提交 80,000 条带 192 字节 payload 和固定消息前缀的日志，初始突发确保无需等待 UI 排空也会在 128 行 `LiveProgress` 队列发生有界淘汰。另一个有界线程提交 400 个合成按键；时延从生产者尝试向有界通道发送前打点，计到首次完成 `TestBackend` draw，因此包含输入通道背压等待。

门禁要求：生产端丢弃和视图淘汰均大于零；400 个输入全部取得样本；p99 不超过 100 ms，单个样本不超过 1 秒；子进程 RSS 不超过 256 MiB。RSS 在计时负载结束后才读取并最多重试三次，避免平台辅助命令污染时延。Windows 使用 `PeakWorkingSet64`，Linux 使用 `/proc/self/status` 的 `VmHWM`；macOS 路径使用负载后的 `/bin/ps rss` 当前值，不宣称峰值。

## 审查与回归

两名非实现者分别审查帧调度、后台事件分类、页面分派、日志所有权和隔离测试。初审发现默认并行 suite 会运行计时父测试、父进程反复启动 RSS 探针会争抢 CPU、窗口淘汰存在调度竞态，以及完成后日志缺少连续性回归。修正后两轮复审均无 P0/P1/P2。

确定性测试覆盖空闲不重绘、持续两秒 dirty 状态恰好合并为 40 帧、交互立即绘制和静默活动刷新。部署回归真实经过 running drain、worker join、finish/final drain、Finished 页面和日志覆盖层渲染，运行期与完成边界日志均保留。既有慢磁盘/队列饱和测试继续提供持久日志 writer 的独立证据。

## 质量门禁

Rust/Cargo 1.96.1；Windows 原生及 WSL Ubuntu-22.04 的隔离原生 Linux 工具链。

| 门禁 | Windows | Linux |
| --- | --- | --- |
| 库测试 | 960 通过 / 2 ignored | 981 通过 / 2 ignored |
| 可执行入口测试 | 2 通过 | 2 通过 |
| SSH/SFTP 协议测试 | 14 通过 | 14 通过 |
| 恢复测试可执行文件 | 1 通过 / 1 ignored | 1 通过 / 1 ignored |
| 格式 / 严格 Clippy | 通过 | 通过 |
| release 编译 | 通过 | 通过 |
| 显式性能门禁 | 80,000 日志 / 400 输入；p99 约 42 ms；RSS 约 13 MiB | 80,000 日志 / 400 输入；p99 约 38 ms；RSS 约 24 MiB |

默认全套两端均有 10 项 ignored：2 项 TUI-06 性能入口、7 项显式一次性 SSH/systemd 外部夹具，以及 1 项由恢复父用例启动的中断助手；均未计作通过。依赖未变化；`cargo audit` 退出码 0，保留既有 `wnaf 0.14.0` 撤回警告。

## 测量边界与分配器结论

该门禁验证合成输入生产者提交尝试到 Ratatui `TestBackend` 完成帧的 App 调度延迟，不读取真实 `crossterm` 输入，不测 PTY/终端 flush、物理显示、人工可用性、全部后台事件通道或磁盘 writer 吞吐。TUI-05 的 Linux PTY 证据验证退出与终端恢复，也不是显示延迟基准。Windows ConPTY 由 M4/QA-01 验证；现行 MVP 不要求 macOS 或最低 Rust 版本矩阵。

标准分配器在 Windows/Linux 的整进程结果分别约为 13 MiB/24 MiB，远低于 256 MiB 门槛；时延也低于阈值。没有可重复证据支持增加 `mimalloc` 的依赖、二进制和平台复杂度，因此 MVP 不引入它。
