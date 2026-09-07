//! Explicit UI messages only. Never translate project values, argv, YAML or process output.
use serde::{Deserialize, Serialize};
use std::{cell::Cell, io::Write, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    Chinese,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    language: Language,
}
impl Language {
    pub(super) const fn choose(self, english: &'static str, chinese: &'static str) -> &'static str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }
    pub(super) const fn other(self) -> Self {
        match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        }
    }
    pub(super) fn load(path: &Path) -> Result<Self, ()> {
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Ok(m) if m.len() <= 4096 => (),
            _ => return Err(()),
        }
        let contents = std::fs::read(path).map_err(|_| ())?;
        serde_json::from_slice::<Preferences>(&contents)
            .map(|p| p.language)
            .map_err(|_| ())
    }
    pub(super) fn save(self, path: &Path) -> Result<(), ()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| ())?;
        }
        let contents =
            serde_json::to_vec_pretty(&Preferences { language: self }).map_err(|_| ())?;
        let mut file = atomic_write_file::AtomicWriteFile::open(path).map_err(|_| ())?;
        file.write_all(&contents)
            .and_then(|()| file.commit())
            .map_err(|_| ())
    }
}
thread_local! {
    // Clippy reports the generated Windows GNU storage despite this const initializer.
    #[allow(clippy::missing_const_for_thread_local)]
    static FRAME_LANGUAGE: Cell<Language> = const { Cell::new(Language::English) };
}
/// A synchronous frame scope, restored even on unwinding; workers and persisted text are unaffected.
pub(super) fn with_language<T>(language: Language, render: impl FnOnce() -> T) -> T {
    struct Restore(Language);
    impl Drop for Restore {
        fn drop(&mut self) {
            FRAME_LANGUAGE.set(self.0);
        }
    }
    let _restore = Restore(FRAME_LANGUAGE.replace(language));
    render()
}
pub(super) fn choose(english: &'static str, chinese: &'static str) -> &'static str {
    FRAME_LANGUAGE.get().choose(english, chinese)
}
pub(super) fn tr(english: &str) -> &str {
    if FRAME_LANGUAGE.get() == Language::English {
        return english;
    }
    catalog(english).unwrap_or(english)
}
const MESSAGES: &[(&str, &str)] = &[
    (
        "↑↓ select · Enter open · o browse · f refresh · c connections · x unregister · q quit",
        "↑↓ 选择 · Enter 打开 · o 选目录 · f 刷新 · c 连接 · x 移除记录 · q 退出",
    ),
    (
        "↑/↓ select   Enter enter directory   Backspace parent   s select root   Esc back",
        "↑↓ 选择 · Enter 进入目录 · Backspace 上级 · s 选为项目 · Esc 返回",
    ),
    (
        "←/→ env   ↑/↓ scroll   d deploy   m manage   e edit   Esc projects   q quit",
        "←→ 环境 · ↑↓ 滚动 · d 发布 · m 历史 · e 配置 · Esc 项目 · q 退出",
    ),
    (
        "↑/↓ Component   Space select (required)   ←/→ Environment   Esc overview",
        "↑↓ 组件 · Space 选择（必选）· ←→ 环境 · Esc 概览",
    ),
    (
        "←/→ Environment   ↑/↓ Component   Space toggle   Enter check   Esc overview",
        "←→ 环境 · ↑↓ 组件 · Space 勾选 · Enter 预检 · Esc 概览",
    ),
    (
        "↑/↓ or PgUp/PgDn scroll   c confirm Deployment   Esc change selection",
        "↑↓ / PgUp/PgDn 滚动 · c 确认发布 · Esc 修改选择",
    ),
    (
        "Safe cancellation requested; recovery may still be running",
        "已请求安全取消，恢复操作可能仍在进行",
    ),
    (
        "↑/↓ scroll   l logs/search/export   Enter/Esc project overview",
        "↑↓ 滚动 · l 日志/搜索/导出 · Enter/Esc 项目概览",
    ),
    (
        "Esc projects   a add manually   ↑/↓ move   Space select (required)",
        "Esc 项目 · a 手动添加 · ↑↓ 移动 · Space 选择（必选）",
    ),
    (
        "Esc projects   ↑/↓ move   Space toggle   Enter next   a add manually",
        "Esc 项目 · ↑↓ 移动 · Space 勾选 · Enter 下一步 · a 手动添加",
    ),
    (
        "←→ Component · ↑↓ connection · Space assign · e target · a SSH · n review · Esc back",
        "←→ 组件 · ↑↓ 连接 · Space 分配 · e 目标 · a SSH · n 预览 · Esc 返回",
    ),
    (
        "Tab field   Backspace erase   Delete clear   F3 key   Enter next   Esc cancel",
        "Tab 字段 · Backspace 删除 · Delete 清空 · F3 私钥 · Enter 下一步 · Esc 取消",
    ),
    (
        "Tab field   ↑/↓ identity   F3 key   F5 password   Enter probe   Esc cancel",
        "Tab 字段 · ↑↓ 身份 · F3 私钥 · F5 密码 · Enter 探测 · Esc 取消",
    ),
    (
        "↑/↓ select   Enter directory   Backspace parent   s choose file   Esc back",
        "↑↓ 选择 · Enter 进入目录 · Backspace 上级 · s 选择文件 · Esc 返回",
    ),
    (
        "↑/↓ or PgUp/PgDn scroll   c confirm and save   Esc Destinations",
        "↑↓ / PgUp/PgDn 滚动 · c 确认保存 · Esc 连接目标",
    ),
    (
        "Log viewer open · F1 full keys / current message · Ctrl+C safe cancellation",
        "日志已打开 · F1 完整快捷键/当前提示 · Ctrl+C 安全取消",
    ),
    (
        "SSH setup cancellation requested; waiting for the worker to stop",
        "已请求取消 SSH 设置，正在等待任务停止",
    ),
    (
        "SSH setup working; Esc requests cancellation and waits before retry",
        "SSH 设置进行中；Esc 取消，任务停止后可重试",
    ),
    (
        "Deployment active   return to progress or cancel safely",
        "发布进行中 · 返回进度页或安全取消",
    ),
    (" Exit? c / Esc ", " 退出？c / Esc "),
    (" Exiting safely ", " 正在安全退出 "),
    (
        "c / Ctrl+C: cancel tracked work and exit safely.\nEsc / r: resume the task.\n\nWork is still active.",
        "c / Ctrl+C：取消正在执行的任务并安全退出。\nEsc / r：继续任务。\n\n当前仍有任务运行。",
    ),
    (
        "Waiting for tracked workers to stop.\nCancellation requested; recovery may still be running.\n\nThe terminal will be restored before the process exits.",
        "正在等待任务停止。\n已请求取消，恢复操作可能仍在执行。\n\n程序退出前会恢复终端。",
    ),
    (
        "Build order: Component name order. Only selected Components will be deployed.\n\n",
        "按组件名称顺序构建，仅发布已选择的组件。\n\n",
    ),
    (
        "\nConfirm to build, package, upload, activate, and check health.",
        "\n确认后将构建、打包、上传、发布并检查健康状态。",
    ),
    ("clean", "干净"),
    (
        "dirty — confirmation deploys these local changes",
        "有未提交修改 — 确认后将发布这些本地修改",
    ),
    ("not a Git repository", "不是 Git 仓库"),
    ("none; files only", "无，仅发布文件"),
    ("custom commands", "自定义命令"),
    (
        "configured service check and remote HTTP/HTTPS",
        "服务检查与远端 HTTP/HTTPS 检查",
    ),
    ("configured service check", "已配置的服务检查"),
    ("remote HTTP/HTTPS", "远端 HTTP/HTTPS"),
    (
        "none configured; application health will not be verified",
        "未配置，不验证应用健康状态",
    ),
    (
        "Standalone saved connections · no Project is required\n\n",
        "独立管理已保存连接，无需打开项目\n\n",
    ),
    (
        "No saved connections.\nPress a to add one, f to refresh, or Esc to return.\n",
        "暂无已保存连接。\n按 a 添加，f 刷新，Esc 返回。\n",
    ),
    (
        "No identity found. F3: private key; F5: password.\n",
        "未找到认证身份。F3：私钥；F5：密码。\n",
    ),
    (
        "No suggested hosts. Type a hostname in the Host field.\n",
        "暂无主机建议，请直接填写主机地址。\n",
    ),
    (
        "Host field: F4 searches suggested hosts; F2 cycles them.\n",
        "主机字段：F4 搜索建议，F2 切换建议。\n",
    ),
    (
        "F5: enter/change password (hidden); Backspace: erase last; Delete: clear.\nPassword is saved encrypted for this Windows user after authentication.\nEnter captures the host-key fingerprint only. Review it before plain y saves.\n",
        "F5：输入/修改密码（隐藏）；Backspace：删除末位；Delete：清空。\n认证成功后，将为当前 Windows 用户加密保存密码。\nEnter 获取主机密钥指纹，核对后按 y 保存。\n",
    ),
    (
        "Edit connection: append a revision after Host Key confirmation",
        "编辑连接：核对主机密钥后保存新修订",
    ),
    (
        "New connection: ID generated automatically on preview",
        "新建连接：预览时自动生成 ID",
    ),
    (
        "WARNING: host-key fingerprint differs from the saved connection.\n",
        "警告：主机密钥指纹与已保存连接不一致。\n",
    ),
    ("Project editor / Components", "项目配置 / 组件"),
    ("Project editor / Component", "项目配置 / 组件编辑"),
    ("Project editor / Environments", "项目配置 / 环境"),
    ("Project editor / Environment", "项目配置 / 环境编辑"),
    ("Project editor / Choose connection", "项目配置 / 选择连接"),
    ("Project editor / Dependencies", "项目配置 / 发布顺序"),
    ("Project editor / Target", "项目配置 / 部署目标"),
    ("Project editor / Remove Component", "项目配置 / 移除组件"),
    ("Project editor / Remove Environment", "项目配置 / 移除环境"),
    ("Project editor / Discovery", "项目配置 / 发现组件"),
    ("Project editor / Confirm YAML", "项目配置 / 确认 YAML"),
    ("Project editor / Discard draft", "项目配置 / 放弃草稿"),
    ("Discard unsaved draft?", "放弃未保存的草稿？"),
    ("Confirm draft removal", "确认从草稿移除"),
    ("Choose discovered Component", "选择发现的组件"),
    ("Environment / Component subset", "环境 / 选择组件"),
    ("Component deployment target", "组件部署目标"),
    (
        "Choose saved connection (no manual ID entry)",
        "选择已保存连接（无需输入 ID）",
    ),
    ("Choose ordering dependencies", "选择发布顺序依赖"),
    (
        "No file has been saved. Press c to discard all in-memory edits; Esc keeps editing.",
        "尚未保存文件。按 c 放弃所有草稿修改，Esc 继续编辑。",
    ),
    (
        "Configuration is unavailable. Press r to retry, or Esc to return. No file was created.",
        "无法读取配置。按 r 重试，Esc 返回。尚未创建文件。",
    ),
    ("UNSAVED DRAFT", "未保存的草稿"),
    (
        "Saved configuration loaded; no changes yet",
        "已读取配置，尚未修改",
    ),
    (
        "Choose a Component; prefer f discovery for a new build setup.\n\n",
        "选择组件；可按 f 发现新的构建配置。\n\n",
    ),
    (
        "Choose an Environment; rename is explicit and retains its identity.\n\n",
        "选择环境；重命名会保留环境标识。\n\n",
    ),
    (
        "No Components in the draft.\nPress f to discover candidates, a to add one, or Esc to return.\n",
        "草稿中没有组件。\n按 f 发现，a 添加，Esc 返回。\n",
    ),
    (
        "No Environments in the draft.\nPress a to add one or Esc to return.\n",
        "草稿中没有环境。\n按 a 添加，Esc 返回。\n",
    ),
    (
        "Apply Component to draft (not YAML save)",
        "应用组件到草稿（尚不保存 YAML）",
    ),
    (
        "Apply Environment to draft (not YAML save)",
        "应用环境到草稿（尚不保存 YAML）",
    ),
    ("Apply target to Environment form", "应用部署目标到环境表单"),
    (
        "\nThe last Component cannot be deleted. Removing one also removes its Environment assignments and ordering links in the draft.",
        "\n至少保留一个组件。移除组件也会移除草稿中对应的环境分配与顺序依赖。",
    ),
    (
        "\nThe last Environment cannot be deleted. Config removal never removes remote files or stops services.",
        "\n至少保留一个环境。移除配置不会删除远端文件或停止服务。",
    ),
    ("Manage", "历史与恢复"),
    ("Historical Environments", "历史环境"),
    ("Deployment history", "发布历史"),
    ("Inspections", "检查报告"),
    ("Choose inspection targets", "选择检查目标"),
    ("Choose rollback targets", "选择回退目标"),
    ("Confirm rollback", "确认回退"),
    ("Rollback result", "回退结果"),
    ("Rollback did not complete normally", "回退未正常完成"),
    (
        "Deployment history · local · created time",
        "发布历史 · 本地 · 创建时间",
    ),
    (
        "Deployment details · original record",
        "发布详情 · 原始记录",
    ),
    (
        "Saved inspections · local · checked time",
        "已保存检查 · 本地 · 检查时间",
    ),
    ("Rollback progress", "回退进度"),
    (
        "Esc return   r retry loading configuration",
        "Esc 返回 · r 重试读取配置",
    ),
    (
        "Esc leave/discard   n name   c Components   e Environments   p YAML preview",
        "Esc 返回/放弃 · n 名称 · c 组件 · e 环境 · p YAML 预览",
    ),
    (
        "Esc back   f discover candidates   a add Component",
        "Esc 返回 · f 发现组件 · a 添加组件",
    ),
    (
        "Esc back   F4 search   ↑/↓ choose   Enter edit   f discover   a add",
        "Esc 返回 · F4 搜索 · ↑↓ 选择 · Enter 编辑 · f 发现 · a 添加",
    ),
    (
        "Esc back   F4 search   ↑/↓ choose   Enter edit   f discover   a add   d remove",
        "Esc 返回 · F4 搜索 · ↑↓ 选择 · Enter 编辑 · f 发现 · a 添加 · d 移除",
    ),
    ("Esc back   a add Environment", "Esc 返回 · a 添加环境"),
    (
        "Esc back   F4 search   ↑/↓ choose   Enter edit/rename   a add",
        "Esc 返回 · F4 搜索 · ↑↓ 选择 · Enter 编辑/重命名 · a 添加",
    ),
    (
        "Esc back   F4 search   ↑/↓ choose   Enter edit/rename   a add   d remove",
        "Esc 返回 · F4 搜索 · ↑↓ 选择 · Enter 编辑/重命名 · a 添加 · d 移除",
    ),
    (
        "Esc discard form   F4 search   ↑/↓ field   Enter edit / apply to draft",
        "Esc 放弃表单 · F4 搜索 · ↑↓ 字段 · Enter 编辑/应用到草稿",
    ),
    (
        "Esc discard form   F4 search   ↑/↓ field   Space toggle   Enter edit/apply",
        "Esc 放弃表单 · F4 搜索 · ↑↓ 字段 · Space 勾选 · Enter 编辑/应用",
    ),
    (
        "F4 search · ↑↓ field · Enter edit/apply · b remote choices · Esc back",
        "F4 搜索 · ↑↓ 字段 · Enter 编辑/应用 · b 远端选项 · Esc 返回",
    ),
    (
        "Esc cancel field   Type value   Backspace erase   Delete clear   Enter apply",
        "Esc 取消 · 输入值 · Backspace 删除 · Delete 清空 · Enter 应用",
    ),
    (
        "Esc reject preview   ↑/↓ PgUp/PgDn scroll   c confirm exact YAML save",
        "Esc 放弃预览 · ↑↓ PgUp/PgDn 滚动 · c 确认保存此 YAML",
    ),
    (
        "c discard unsaved draft and leave   Esc keep editing",
        "c 放弃草稿并返回 · Esc 继续编辑",
    ),
    (
        "Esc cancel   c remove from draft only (remote resources untouched)",
        "Esc 取消 · c 仅从草稿移除（不修改远端资源）",
    ),
    (
        "Esc projects   F4 search   ↑/↓ select   Enter details   a add   f refresh",
        "Esc 项目 · F4 搜索 · ↑↓ 选择 · Enter 详情 · a 添加 · f 刷新",
    ),
    (
        "Esc list   e edit   v verify (read-only)   x remove registration",
        "Esc 列表 · e 编辑 · v 验证（只读）· x 移除记录",
    ),
    (
        "Esc list   Tab field   F4 hosts   F2 next host   F3 keys   Enter capture",
        "Esc 列表 · Tab 字段 · F4 主机 · F2 下一主机 · F3 私钥 · Enter 获取指纹",
    ),
    (
        "Esc list   Tab field   ↑/↓ identity   F4 search   F3 keys   Enter capture",
        "Esc 列表 · Tab 字段 · ↑↓ 身份 · F4 搜索 · F3 私钥 · Enter 获取指纹",
    ),
    (
        "Esc list   Tab field   type edit   F2 next host   F3 keys   Enter capture",
        "Esc 列表 · Tab 字段 · 输入编辑 · F2 下一主机 · F3 私钥 · Enter 获取指纹",
    ),
    (
        "Esc / n reject   y trust fingerprint, authenticate and save",
        "Esc / n 拒绝 · y 信任指纹、认证并保存",
    ),
    (
        "Esc overview  ←/→ env  h history  i inspect  p reports  a old envs",
        "Esc 概览 · ←→ 环境 · h 历史 · i 检查 · p 报告 · a 历史环境",
    ),
    (
        "Esc back  ↑/↓ select  Enter details  n/b page  f refresh  [/] pan",
        "Esc 返回 · ↑↓ 选择 · Enter 详情 · n/b 翻页 · f 刷新 · [/] 横移",
    ),
    (
        "Esc back  ↑/↓ PgUp/PgDn scroll  [/] pan  l logs  r rollback  i inspect",
        "Esc 返回 · ↑↓ 翻页 滚动 · [/] 横移 · l 日志 · r 回退 · i 检查",
    ),
    (
        "Esc back  ↑/↓ Component  Space toggle  Enter inspect selected (read-only)",
        "Esc 返回 · ↑↓ 组件 · Space 勾选 · Enter 检查所选组件（只读）",
    ),
    (
        "Esc back  ↑/↓ Component  ←/→ version  Space select  Enter check  d details",
        "Esc 返回 · ↑↓ 组件 · ←→ 版本 · Space 选择 · Enter 预检 · d 详情",
    ),
    (
        "Esc reject  ↑/↓ PgUp/PgDn scroll  [/] pan  c confirm rollback",
        "Esc 放弃 · ↑↓ 翻页 滚动 · [/] 横移 · c 确认回退",
    ),
    ("Projects", "项目"),
    ("Projects / Choose directory", "项目 / 选择目录"),
    ("Overview", "概览"),
    ("Browse directories…", "选择目录…"),
    ("Message: ", "提示："),
    ("Deploy / Select Components", "发布 / 选择组件"),
    ("Deploy / Check", "发布 / 预检"),
    ("Deploy / Confirm", "发布 / 确认"),
    ("Deploy / Progress", "发布 / 进度"),
    ("Deploy / Result", "发布 / 结果"),
    ("Setup / Components", "设置 / 组件"),
    (
        "Setup / Confirm YAML (local only)",
        "设置 / 确认 YAML（仅本地）",
    ),
    ("Setup / Assign connection", "设置 / 分配连接"),
    ("Setup / Capture Host Key", "设置 / 获取主机密钥"),
    ("Setup / Confirm Host Key", "设置 / 确认主机密钥"),
    ("Setup / Authenticate", "设置 / 认证"),
    ("Setup / Choose identity", "设置 / 选择身份"),
    ("Setup / Connection", "设置 / 连接"),
    ("Connections", "连接"),
    ("Connections / Saved connections", "连接 / 已保存连接"),
    (
        "Connections / Unavailable saved connections",
        "连接 / 无法读取记录",
    ),
    ("Connections / Details", "连接 / 详情"),
    ("Project editor", "项目配置"),
    ("Project editor / Edit value", "项目配置 / 编辑值"),
    ("Host", "主机"),
    ("User", "用户"),
    ("Port", "端口"),
    ("Password", "密码"),
    ("Identity:", "认证身份："),
    ("Environment", "环境"),
    ("Root", "目录"),
    ("Project", "项目"),
    (
        "Enter: review host key before connecting",
        "Enter：连接前核对主机密钥",
    ),
    ("Project overview", "项目概览"),
    ("Deployment progress", "发布进度"),
    ("Deployment result", "发布结果"),
    ("Recent output", "最近输出"),
    ("Recent events", "最近事件"),
    ("New Deployment · Environment check", "新建发布 · 环境预检"),
    ("Confirm deployment", "确认发布"),
    ("[PRODUCTION] Confirm deployment", "[PRODUCTION] 确认发布"),
    ("Select Components", "选择组件"),
    ("[PRODUCTION] Select Components", "[PRODUCTION] 选择组件"),
    ("Choose project directory", "选择项目目录"),
    ("Choose a project directory", "选择项目目录"),
    ("SSH connection · Password", "SSH 连接 · 密码"),
    ("New SSH Destination", "新建 SSH 连接"),
    ("New SSH Destination · Host Key", "新建 SSH 连接 · 主机密钥"),
    ("Confirm Host Key", "确认主机密钥"),
    ("New SSH Destination · Authenticate", "新建 SSH 连接 · 认证"),
    (
        "First-time setup · Review shipforge.yaml",
        "首次设置 · 确认 shipforge.yaml",
    ),
    ("Select SSH identity", "选择 SSH 身份"),
    ("Saved connections unavailable", "无法读取已保存连接"),
    ("Connection details", "连接详情"),
    ("Connection setup", "连接设置"),
    ("Explicit Host Key confirmation", "确认主机密钥"),
    ("Confirm connection removal", "确认移除连接"),
    ("Remove Project from recents", "移除最近项目记录"),
    ("Project removed from recents", "已移除最近项目记录"),
    ("Working", "处理中"),
    ("Edit project configuration", "编辑项目配置"),
    ("Components", "组件"),
    ("Environments", "环境"),
    ("Component draft", "组件草稿"),
    ("Build commands", "构建命令"),
    ("One command · literal argv", "单条命令 · 独立参数"),
    ("Edit one value", "编辑值"),
    ("Confirm exact shipforge.yaml", "确认 shipforge.yaml"),
    ("Language saved.", "语言设置已保存。"),
    ("No current message.", "暂无提示。"),
    ("Keyboard help and current message", "快捷键帮助与当前提示"),
    ("Log viewer help and current message", "日志帮助与当前提示"),
    (
        "Checking local and remote state…   Esc cancel",
        "正在检查本地和远端状态…   Esc 取消",
    ),
    (
        "Esc request safe cancellation   l logs/search/export",
        "Esc 安全取消   l 日志/搜索/导出",
    ),
    (
        "Fetching Host Key…   Esc cancel",
        "正在获取主机密钥…   Esc 取消",
    ),
    (
        "y trust fingerprint and authenticate   n/Esc reject",
        "y 信任指纹并认证   n/Esc 拒绝",
    ),
    (
        "Authenticating and probing…   Esc cancel",
        "正在认证和探测…   Esc 取消",
    ),
    (
        "Esc projects   f retry reading saved connections",
        "Esc 项目   f 重试读取连接",
    ),
    (
        "Esc projects   a add SSH connection   f refresh",
        "Esc 项目   a 添加 SSH 连接   f 刷新",
    ),
    ("Build and package", "构建与打包"),
    ("Preparing Release", "准备发布包"),
    ("Activating", "发布与检查"),
    ("Building", "构建中"),
    ("Packaging", "打包中"),
    ("Uploading", "上传中"),
    ("Deployment started", "发布开始"),
    ("Deployment finished", "发布完成"),
    ("Remote operation", "远端操作"),
    ("Succeeded", "成功"),
    ("Failed", "失败"),
    ("Cancelled", "已取消"),
    ("Started", "进行中"),
    ("Recorded", "已记录"),
    (
        "Project files and local history remain unchanged.",
        "项目文件和本地历史保持不变。",
    ),
    (" Projects ", " 项目 "),
    (" Projects / Choose directory ", " 项目 / 选择目录 "),
    (" Overview ", " 概览 "),
    (" Browse directories… ", " 选择目录… "),
    (" Deploy / Select Components ", " 发布 / 选择组件 "),
    (" Deploy / Check ", " 发布 / 预检 "),
    (" Deploy / Confirm ", " 发布 / 确认 "),
    (" Deploy / Progress ", " 发布 / 进度 "),
    (" Deploy / Result ", " 发布 / 结果 "),
    (" Setup / Components ", " 设置 / 组件 "),
    (
        " Setup / Confirm YAML (local only) ",
        " 设置 / 确认 YAML（仅本地） ",
    ),
    (" Setup / Assign connection ", " 设置 / 分配连接 "),
    (" Setup / Capture Host Key ", " 设置 / 获取主机密钥 "),
    (" Setup / Confirm Host Key ", " 设置 / 确认主机密钥 "),
    (" Setup / Authenticate ", " 设置 / 认证 "),
    (" Setup / Choose identity ", " 设置 / 选择身份 "),
    (" Setup / Connection ", " 设置 / 连接 "),
    (" Connections ", " 连接 "),
    (" Connections / Saved connections ", " 连接 / 已保存连接 "),
    (
        " Connections / Unavailable saved connections ",
        " 连接 / 无法读取记录 ",
    ),
    (" Connections / Details ", " 连接 / 详情 "),
    (" Project editor ", " 项目配置 "),
    (" Project editor / Edit value ", " 项目配置 / 编辑值 "),
    (" Host ", " 主机 "),
    (" User ", " 用户 "),
    (" Port ", " 端口 "),
    (" Password ", " 密码 "),
    (" Identity: ", " 认证身份： "),
    (" Environment ", " 环境 "),
    (" Root ", " 目录 "),
    (" Project ", " 项目 "),
    (
        " Enter: review host key before connecting ",
        " Enter：连接前核对主机密钥 ",
    ),
    (" Project overview ", " 项目概览 "),
    (" Deployment progress ", " 发布进度 "),
    (" Deployment result ", " 发布结果 "),
    (" Recent output ", " 最近输出 "),
    (" Recent events ", " 最近事件 "),
    (
        " New Deployment · Environment check ",
        " 新建发布 · 环境预检 ",
    ),
    (" Confirm deployment ", " 确认发布 "),
    (
        " [PRODUCTION] Confirm deployment ",
        " [PRODUCTION] 确认发布 ",
    ),
    (" Select Components ", " 选择组件 "),
    (
        " [PRODUCTION] Select Components ",
        " [PRODUCTION] 选择组件 ",
    ),
    (" Choose project directory ", " 选择项目目录 "),
    (" Choose a project directory ", " 选择项目目录 "),
    (" SSH connection · Password ", " SSH 连接 · 密码 "),
    (" New SSH Destination ", " 新建 SSH 连接 "),
    (
        " New SSH Destination · Host Key ",
        " 新建 SSH 连接 · 主机密钥 ",
    ),
    (" Confirm Host Key ", " 确认主机密钥 "),
    (
        " New SSH Destination · Authenticate ",
        " 新建 SSH 连接 · 认证 ",
    ),
    (
        " First-time setup · Review shipforge.yaml ",
        " 首次设置 · 确认 shipforge.yaml ",
    ),
    (" Select SSH identity ", " 选择 SSH 身份 "),
    (" Saved connections unavailable ", " 无法读取已保存连接 "),
    (" Connection details ", " 连接详情 "),
    (" Connection setup ", " 连接设置 "),
    (" Explicit Host Key confirmation ", " 确认主机密钥 "),
    (" Confirm connection removal ", " 确认移除连接 "),
    (" Remove Project from recents ", " 移除最近项目记录 "),
    (" Project removed from recents ", " 已移除最近项目记录 "),
    (" Working ", " 处理中 "),
    (" Edit project configuration ", " 编辑项目配置 "),
    (" Components ", " 组件 "),
    (" Environments ", " 环境 "),
    (" Component draft ", " 组件草稿 "),
    (" Build commands ", " 构建命令 "),
    (" One command · literal argv ", " 单条命令 · 独立参数 "),
    (" Edit one value ", " 编辑值 "),
    (" Confirm exact shipforge.yaml ", " 确认 shipforge.yaml "),
    (" Language saved. ", " 语言设置已保存。 "),
    (" No current message. ", " 暂无提示。 "),
    (
        " Keyboard help and current message ",
        " 快捷键帮助与当前提示 ",
    ),
    (
        " Log viewer help and current message ",
        " 日志帮助与当前提示 ",
    ),
    (
        " Fetching Host Key…   Esc cancel ",
        " 正在获取主机密钥…   Esc 取消 ",
    ),
    (
        " Authenticating and probing…   Esc cancel ",
        " 正在认证和探测…   Esc 取消 ",
    ),
    (
        " Esc projects   f retry reading saved connections ",
        " Esc 项目   f 重试读取连接 ",
    ),
    (" Build and package ", " 构建与打包 "),
    (" Preparing Release ", " 准备发布包 "),
    (" Activating ", " 发布与检查 "),
    (" Building ", " 构建中 "),
    (" Packaging ", " 打包中 "),
    (" Uploading ", " 上传中 "),
    (" Deployment started ", " 发布开始 "),
    (" Deployment finished ", " 发布完成 "),
    (" Remote operation ", " 远端操作 "),
    (" Succeeded ", " 成功 "),
    (" Failed ", " 失败 "),
    (" Cancelled ", " 已取消 "),
    (" Started ", " 进行中 "),
    (" Recorded ", " 已记录 "),
    (
        " Project files and local history remain unchanged. ",
        " 项目文件和本地历史保持不变。 ",
    ),
];
fn catalog(english: &str) -> Option<&'static str> {
    MESSAGES
        .iter()
        .find_map(|(key, value)| (*key == english).then_some(*value))
}
pub(super) fn is_chinese() -> bool {
    FRAME_LANGUAGE.get() == Language::Chinese
}
macro_rules! localized_format {
    ($english:literal,$chinese:literal $(,$argument:expr)* $(,)?) => {
        if $crate::tui::i18n::is_chinese() {format!($chinese $(,$argument)*)} else {format!($english $(,$argument)*)}
    };
}
macro_rules! localized_write {
    ($destination:expr,$english:literal,$chinese:literal $(,$argument:expr)* $(,)?) => {
        if $crate::tui::i18n::is_chinese() {write!($destination,$chinese $(,$argument)*)} else {write!($destination,$english $(,$argument)*)}
    };
}
macro_rules! localized_writeln {
    ($destination:expr,$english:literal,$chinese:literal $(,$argument:expr)* $(,)?) => {
        if $crate::tui::i18n::is_chinese() {writeln!($destination,$chinese $(,$argument)*)} else {writeln!($destination,$english $(,$argument)*)}
    };
}
pub(super) use localized_format as format;
pub(super) use localized_write as write;
pub(super) use localized_writeln as writeln;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_round_trip_and_unknown_language_is_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.json");
        assert_eq!(Language::load(&path), Ok(Language::English));
        Language::Chinese.save(&path).unwrap();
        assert_eq!(Language::load(&path), Ok(Language::Chinese));
        std::fs::write(&path, br#"{"language":"unsupported"}"#).unwrap();
        assert!(Language::load(&path).is_err());
    }
    #[test]
    fn frame_language_does_not_translate_user_values_or_leak_to_next_frame() {
        with_language(Language::Chinese, || {
            assert_eq!(tr("Projects"), "项目");
            assert_eq!(tr("D:/project/Projects"), "D:/project/Projects");
            assert_eq!(tr("user-supplied command"), "user-supplied command");
        });
        assert_eq!(tr("Projects"), "Projects");
    }
}
