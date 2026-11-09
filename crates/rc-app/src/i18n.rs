//! UI-text localization: Simplified Chinese with English as the source and
//! the fallback. `Language` is what settings.json stores (including
//! `System`, resolved from the OS at runtime); `Lang` is the resolved
//! choice every render reads.
//!
//! The English source text is the key: [`tr`] returns the key itself in
//! English and the Chinese string in Chinese, so a missing table entry can
//! only ever fall back to English. Dynamic lines translate whole format
//! templates through [`tr_fmt!`] — never word by word:
//!
//! `tr_fmt!(lang, "Generation {}, acked {} ago{}", gen, age, stale)`
//!
//! UI-visible strings only: protocol lines, helper/app logs and bridge wire
//! messages stay English, as do the brand ("Remote Mic Rune"), the version
//! string, shortcut syntax (`Ctrl+Alt+F5`) and usage hex values.

use serde::{Deserialize, Serialize};

/// The language preference settings.json stores. `System` follows the OS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    System,
    En,
    Zh,
}

impl Language {
    /// The concrete language to render in: explicit choices win, `System`
    /// asks the OS (its answer may change between runs, not mid-run).
    pub fn resolve(self) -> Lang {
        match self {
            Language::En => Lang::En,
            Language::Zh => Lang::Zh,
            Language::System => system_lang(),
        }
    }
}

/// A resolved render language (what [`tr`] takes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

/// Translate one UI string. `key` is the English source text; English
/// returns it unchanged and Chinese looks it up in [`ZH_TABLE`], falling
/// back to the key when the entry is missing. Lookup collapses whitespace
/// runs, so call sites may wrap long literals across lines.
pub fn tr(lang: Lang, key: &'static str) -> &'static str {
    match lang {
        Lang::En => key,
        Lang::Zh => {
            let norm = collapse_ws(key);
            match ZH_TABLE.binary_search_by_key(&norm.as_str(), |(k, _)| *k) {
                Ok(i) => ZH_TABLE[i].1,
                Err(_) => key,
            }
        }
    }
}

/// Collapse runs of whitespace into single spaces (table keys are
/// single-spaced).
fn collapse_ws(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut in_ws = false;
    for c in key.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// [`tr`] for dynamic lines: translates the whole template, then fills its
/// positional `{}` placeholders in order. Templates only ever use plain
/// `{}` placeholders — call sites pre-format any precision or debug specs
/// (e.g. `format!("{:.1}", value)`) before passing them as arguments.
macro_rules! tr_fmt {
    ($lang:expr, $key:expr $(, $arg:expr)* $(,)?) => {{
        let template = crate::i18n::tr($lang, $key);
        crate::i18n::fill(template, &[$(&$arg as &dyn std::fmt::Display),*])
    }};
}

/// Fill a translated template's `{}` placeholders, in order, with the given
/// arguments. Leftover `{}` (an arity bug) is kept visible instead of
/// hiding the mistake.
pub(crate) fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut parts = template.split("{}");
    out.push_str(parts.next().unwrap_or(""));
    for (i, piece) in parts.enumerate() {
        match args.get(i) {
            Some(arg) => out.push_str(&arg.to_string()),
            None => out.push_str("{}"),
        }
        out.push_str(piece);
    }
    out
}

pub(crate) use tr_fmt;

/// The system's language: Chinese when the OS UI language (Windows) or the
/// `LC_ALL`/`LANG` locale (Linux, macOS) is Chinese, English otherwise.
fn system_lang() -> Lang {
    #[cfg(windows)]
    {
        system_lang_windows()
    }
    #[cfg(not(windows))]
    {
        let lc_all = std::env::var("LC_ALL").ok();
        let lang = std::env::var("LANG").ok();
        lang_from_env(lc_all.as_deref(), lang.as_deref())
    }
}

/// Windows: `GetUserDefaultUILanguage` returns a LANGID whose low 10 bits
/// are the primary language; 0x04 is Chinese.
#[cfg(windows)]
fn system_lang_windows() -> Lang {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // SAFETY: no parameters; the call reads the user's UI language preference.
    let langid = unsafe { GetUserDefaultUILanguage() };
    lang_from_windows_primary(langid)
}

/// The pure half of the Windows detection, so tests can pin it on any host.
/// Chinese when the LANGID's primary language is 0x04 (any zh-* locale).
fn lang_from_windows_primary(langid: u16) -> Lang {
    let primary = langid & 0x03FF;
    if primary == 0x0004 {
        Lang::Zh
    } else {
        Lang::En
    }
}

/// Linux/macOS: the first non-empty of `LC_ALL` and `LANG`, Chinese when it
/// starts with `zh` (`zh_CN.UTF-8`), English otherwise.
#[cfg(not(windows))]
fn lang_from_env(lc_all: Option<&str>, lang: Option<&str>) -> Lang {
    let first = [lc_all, lang]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty());
    match first {
        Some(v) if v.to_ascii_lowercase().starts_with("zh") => Lang::Zh,
        _ => Lang::En,
    }
}

/// The Chinese table, sorted by key (English source text) — [`tr`] binary
/// searches it, and a unit test keeps it sorted, duplicate-free and complete
/// against every `tr(` call site.
static ZH_TABLE: &[(&str, &str)] = &[
    (" (stale)", "（已过期）"),
    ("(no output)", "（无输出）"),
    ("Agent acked", "代理已确认"),
    ("Also on this remote", "此遥控器上还有"),
    ("Another instance of Remote Mic Rune is already running. Only one app can own the bridge pipe at a time.", "Remote Mic Rune 的另一个实例已在运行。同一时间只能有一个应用拥有桥接管道。"),
    ("Appearance", "外观"),
    ("Auto", "自动"),
    ("Back", "返回"),
    ("Bridge phase", "桥接阶段"),
    ("Bridge stopped", "桥接已停止"),
    ("Buttons", "按键"),
    ("Buttons configured", "已配置按键"),
    ("Cannot start", "无法启动"),
    ("Capture", "捕获"),
    ("Capture set", "捕获集合"),
    ("Check status", "检查状态"),
    ("Connected", "已连接"),
    ("Counters", "计数器"),
    ("Counters update every 500 ms from the bridge snapshot.", "计数器每 500 毫秒从桥接快照更新一次。"),
    ("Coverage", "覆盖范围"),
    ("Dark", "深色"),
    ("Device-node recovery in progress", "正在恢复设备节点"),
    ("Disabled", "禁用"),
    ("Double", "双击"),
    ("Down", "下"),
    ("Edges {} · ReleaseAll {} · Retargets {} · Pings {} · Denies {}", "边沿 {} · 全部释放 {} · 目标更新 {} · 心跳 {} · 拒绝 {}"),
    ("English", "English"),
    ("Enhanced capture", "增强捕获"),
    ("Enhanced capture off", "增强捕获已关闭"),
    ("Enhanced capture on", "增强捕获已开启"),
    ("Enhanced capture owns the remote's keys at the HID report layer. Turn it off and the remote behaves like a plain keyboard.", "增强捕获在 HID 报告层接管遥控器按键。关闭后，遥控器的表现与普通键盘相同。"),
    ("Exit", "退出"),
    ("Generation {}, acked {} ago{}", "代次 {}，{}前已确认{}"),
    ("Generation {}, not acked yet", "代次 {}，尚未确认"),
    ("Getting started", "开始使用"),
    ("Helper", "辅助程序"),
    ("Helper pid", "辅助程序 PID"),
    ("Helper start requested", "已请求启动辅助程序"),
    ("Helper stop requested (it exits on disconnect by design; the bridge keeps running)", "已请求停止辅助程序（它按设计在断开连接时退出；桥接继续运行）"),
    ("Helper task installed (UAC answered)", "辅助任务已安装（UAC 已应答）"),
    ("Helper task removed", "辅助任务已删除"),
    ("Home", "主页"),
    ("Idle", "空闲"),
    ("Install helper task (UAC once)", "安装辅助任务（一次 UAC）"),
    ("Install the helper, then start it — Helper tab.", "安装辅助程序并启动它 —「辅助程序」标签页。"),
    ("L-Click", "左键"),
    ("Language", "语言"),
    ("Left", "左"),
    ("Left Alt (0x00E2)", "左 Alt（0x00E2）"),
    ("Light", "浅色"),
    ("Link", "连接"),
    ("Listening", "监听中"),
    ("Listening for helper", "等待辅助程序"),
    ("Long", "长按"),
    ("M-Click", "中键"),
    ("Map what each button press does — Buttons tab.", "为按键的按压方式设置动作 —「按键」标签页。"),
    ("Mapped buttons", "已映射的按键"),
    ("Media: Mute", "媒体：静音"),
    ("Media: Next", "媒体：下一曲"),
    ("Media: Play", "媒体：播放"),
    ("Media: Prev", "媒体：上一曲"),
    ("Media: Vol +", "媒体：音量 +"),
    ("Media: Vol -", "媒体：音量 -"),
    ("Menu", "菜单"),
    ("Mouse: Left", "鼠标：左键"),
    ("Mouse: Middle", "鼠标：中键"),
    ("Mouse: Right", "鼠标：右键"),
    ("Move", "移动"),
    ("Mute", "静音"),
    ("Next", "下一曲"),
    ("No UI handle: helper commands need the windowed app", "没有 UI 句柄：辅助程序命令需要窗口化的应用"),
    ("No capture generation issued yet", "尚未下发捕获代次"),
    ("None", "无"),
    ("Not checked yet", "尚未检查"),
    ("OK", "OK"),
    ("Off", "关闭"),
    ("Ownership", "所有权"),
    ("Pick what each press, double press and long press does. Changes persist immediately and update the capture set.", "为单击、双击和长按选择各自的动作。更改立即保存并更新捕获集合。"),
    ("Play", "播放"),
    ("Power", "电源"),
    ("Pressed: {}", "已按下：{}"),
    ("Prev", "上一曲"),
    ("Quit", "退出"),
    ("R-Click", "右键"),
    ("Recovery", "恢复"),
    ("Reload from disk", "从磁盘重新加载"),
    ("Remove task", "删除任务"),
    ("Repeat", "连发"),
    ("Report-layer replacement of usage 0x003E", "在报告层替换用法 0x003E"),
    ("Requested set", "请求集合"),
    ("Right", "右"),
    ("Right Alt (0x00E6)", "右 Alt（0x00E6）"),
    ("Save now", "立即保存"),
    ("Scheduled task", "计划任务"),
    ("Scroll", "滚动"),
    ("Settings", "设置"),
    ("Settings file", "设置文件"),
    ("Shortcut", "快捷键"),
    ("Show Remote Mic Rune", "显示 Remote Mic Rune"),
    ("Single", "单击"),
    ("Start helper", "启动辅助程序"),
    ("Status", "状态"),
    ("Stop helper", "停止辅助程序"),
    ("Stopped", "已停止"),
    ("Storage", "存储"),
    ("Synthesizes the voice key (0x003E) into an Alt usage at the report layer; targets are hardware-validated.", "在报告层将语音键（0x003E）合成为 Alt 用法；目标均通过硬件验证。"),
    ("System", "跟随系统"),
    ("TV", "TV"),
    ("Task installed, helper not running", "任务已安装，辅助程序未运行"),
    ("Task not installed", "任务未安装"),
    ("Task running", "任务运行中"),
    ("Task status", "任务状态"),
    ("Task status: {}", "任务状态：{}"),
    ("The bridge could not start: {}", "桥接无法启动：{}"),
    ("The bridge event channel closed", "桥接事件通道已关闭"),
    ("The helper runs elevated via one scheduled task: install it once (UAC), then start and stop it without prompts. It connects to the bridge pipe and does all elevated work. It exits on app disconnect by design; the bridge keeps running.", "辅助程序通过一个计划任务以提升的权限运行：安装一次（UAC），之后无需提示即可启动和停止。它连接桥接管道并完成所有需要提升权限的工作。应用断开时它会退出（设计如此）；桥接继续运行。"),
    ("Turn on enhanced capture — Capture tab.", "开启增强捕获 —「捕获」标签页。"),
    ("Up", "上"),
    ("Voice key synthesis", "语音键合成"),
    ("Voice synthesis target in settings.json is not in the validated list; it has been turned off.", "settings.json 中的语音合成目标不在已验证列表内；已将其关闭。"),
    ("Vol +", "音量 +"),
    ("Vol -", "音量 -"),
    ("Vol+", "音量+"),
    ("Vol-", "音量-"),
    ("While enabled, {} are always captured, even with no mappings configured.", "启用期间，即使未配置任何映射，{} 也始终处于捕获之下。"),
    ("Working…", "处理中…"),
    ("cannot create {}: {}", "无法创建 {}：{}"),
    ("cannot locate the app executable: {}", "无法定位应用可执行文件：{}"),
    ("cannot move it aside: {}", "无法将其移开：{}"),
    ("cannot replace {}: {}", "无法替换 {}：{}"),
    ("cannot serialize settings: {}", "无法序列化设置：{}"),
    ("cannot start {}: {}", "无法启动 {}：{}"),
    ("cannot write {}: {}", "无法写入 {}：{}"),
    ("elevated launch failed (ShellExecuteW returned {}); the UAC prompt may have been cancelled", "提权启动失败（ShellExecuteW 返回 {}）；UAC 提示可能已被取消"),
    ("elevation (UAC) is only available on Windows", "提权（UAC）仅在 Windows 上可用"),
    ("no output", "无输出"),
    ("none", "无"),
    ("pressed", "按下"),
    ("schtasks /end failed: {}", "schtasks /end 失败：{}"),
    ("schtasks /run failed: {}", "schtasks /run 失败：{}"),
    ("settings.json (LOCALAPPDATA unavailable: not persisted this run)", "settings.json（LOCALAPPDATA 不可用：本次运行不会保存设置）"),
    ("settings.json is malformed ({}) and backing it up failed: {}", "settings.json 格式错误（{}），且备份失败：{}"),
    ("settings.json was malformed ({}); it has been backed up to settings.json.bak and defaults are in effect.", "settings.json 格式错误（{}）；已备份为 settings.json.bak，当前使用默认设置。"),
    ("the app executable has no parent directory", "应用可执行文件没有父目录"),
    ("{} failed: {}", "{} 执行失败：{}"),
    ("{} has an empty key before or after '+'", "{}：'+' 前后存在空的按键"),
    ("{} has more than one non-modifier key", "{}：包含多个非修饰键"),
    ("{} has no key, only modifiers", "{}：只有修饰键，没有按键"),
    ("{} has too many '+' groups (max {})", "{}：'+' 分组过多（最多 {} 个）"),
    ("{} is not a key this app knows", "{}：本应用不认识此按键"),
    ("{} is out of range (F1-F24)", "{}：超出范围（F1-F24）"),
    ("{} min", "{} 分钟"),
    ("{} ms", "{} 毫秒"),
    ("{} not found next to the app; install the helper build into the same directory", "应用旁未找到 {}；请将辅助程序构建安装到同一目录"),
    ("{} of {}", "{} / {}"),
    ("{} s", "{} 秒"),
    ("{} usages (unrecognized mix)", "{} 个用法（无法识别的组合）"),
    ("{}: modifier {} must come before the key", "{}：修饰键 {} 必须放在按键之前"),
    ("简体中文", "简体中文"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_has_no_duplicate_keys() {
        assert!(
            ZH_TABLE.is_sorted_by(|a, b| a.0 <= b.0),
            "ZH_TABLE must stay sorted (tr binary-searches it)"
        );
        for pair in ZH_TABLE.windows(2) {
            assert_ne!(pair[0].0, pair[1].0, "duplicate key: {:?}", pair[0].0);
        }
        assert!(
            !ZH_TABLE.iter().any(|(k, v)| k.is_empty() || v.is_empty()),
            "no empty entries"
        );
    }

    /// Every key any `tr(`/`tr_fmt!(` call site passes must have a Chinese
    /// entry: the scan reads the crate's own sources, so adding a new
    /// English key without a translation fails this test.
    #[test]
    fn every_tr_call_site_key_has_a_chinese_entry() {
        let scanned = scanned_keys();
        let missing: Vec<&str> = scanned
            .iter()
            .filter(|key| !ZH_TABLE.iter().any(|(k, _)| k == *key))
            .map(|s| s.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "keys without a Chinese entry: {missing:?}"
        );
    }

    #[test]
    fn english_returns_the_key_and_zh_returns_translations() {
        for lang in [Lang::En, Lang::Zh] {
            assert_eq!(
                tr(lang, "Status"),
                if lang == Lang::Zh { "状态" } else { "Status" }
            );
        }
        assert_eq!(
            tr(Lang::Zh, "Install helper task (UAC once)"),
            "安装辅助任务（一次 UAC）"
        );
        // Unknown keys fall back to English in both languages.
        assert_eq!(tr(Lang::Zh, "not a real key"), "not a real key");
        assert_eq!(tr(Lang::En, "not a real key"), "not a real key");
    }

    #[test]
    fn tr_fmt_translates_the_whole_template() {
        let zh = tr_fmt!(Lang::Zh, "Generation {}, acked {} ago{}", 3, "1.2 秒", "");
        assert_eq!(zh, "代次 3，1.2 秒前已确认");
        let zh_stale = tr_fmt!(
            Lang::Zh,
            "Generation {}, acked {} ago{}",
            3,
            "120 毫秒",
            tr(Lang::Zh, " (stale)")
        );
        assert_eq!(zh_stale, "代次 3，120 毫秒前已确认（已过期）");
        let en = tr_fmt!(Lang::En, "Generation {}, acked {} ago{}", 3, "1.2 s", "");
        assert_eq!(en, "Generation 3, acked 1.2 s ago");
        // Templates only take positional `{}` placeholders; specs like
        // `{:?}` are applied by the call site before the argument arrives.
        let zh_two = tr_fmt!(
            Lang::Zh,
            "{}: modifier {} must come before the key",
            format!("{:?}", "ctrl+a+ctrl"),
            format!("{:?}", "ctrl")
        );
        assert_eq!(zh_two, "\"ctrl+a+ctrl\"：修饰键 \"ctrl\" 必须放在按键之前");
        // An arity bug keeps the hole visible instead of silently dropping it.
        assert_eq!(fill("no placeholder here", &[]), "no placeholder here");
        assert_eq!(
            fill("value: {} and {}", &[&1 as &dyn std::fmt::Display]),
            "value: 1 and {}"
        );
    }

    #[test]
    fn explicit_languages_resolve_directly() {
        assert_eq!(Language::En.resolve(), Lang::En);
        assert_eq!(Language::Zh.resolve(), Lang::Zh);
    }

    #[test]
    fn settings_language_serializes_and_deserializes() {
        assert_eq!(
            serde_json::from_str::<Language>(r#""system""#).unwrap(),
            Language::System
        );
        assert_eq!(
            serde_json::from_str::<Language>(r#""en""#).unwrap(),
            Language::En
        );
        assert_eq!(
            serde_json::from_str::<Language>(r#""zh""#).unwrap(),
            Language::Zh
        );
        assert_eq!(serde_json::to_string(&Language::Zh).unwrap(), r#""zh""#);
    }

    #[test]
    fn windows_primary_language_0x04_is_chinese() {
        assert_eq!(lang_from_windows_primary(0x0004), Lang::Zh);
        assert_eq!(
            lang_from_windows_primary(0x0804),
            Lang::Zh,
            "zh-CN packs 0x0804: primary still 0x04"
        );
        assert_eq!(lang_from_windows_primary(0x0009), Lang::En);
        assert_eq!(
            lang_from_windows_primary(0x0409),
            Lang::En,
            "en-US packs 0x0409"
        );
        assert_eq!(
            lang_from_windows_primary(0x0007),
            Lang::En,
            "German is not Chinese"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn env_locale_detection_prefers_lc_all_and_detects_zh() {
        assert_eq!(lang_from_env(Some("zh_CN.UTF-8"), None), Lang::Zh);
        assert_eq!(lang_from_env(None, Some("zh")), Lang::Zh);
        assert_eq!(lang_from_env(None, Some("zh_TW.UTF-8")), Lang::Zh);
        assert_eq!(lang_from_env(None, Some("en_US.UTF-8")), Lang::En);
        assert_eq!(
            lang_from_env(Some("C"), Some("zh_CN.UTF-8")),
            Lang::En,
            "LC_ALL wins over LANG"
        );
        assert_eq!(
            lang_from_env(Some(""), Some("zh_CN.UTF-8")),
            Lang::Zh,
            "empty LC_ALL falls through"
        );
        assert_eq!(lang_from_env(None, None), Lang::En);
        // The call path matches what system_lang() passes on this host.
        let lc_all = std::env::var("LC_ALL").ok();
        let lang = std::env::var("LANG").ok();
        assert_eq!(
            lang_from_env(lc_all.as_deref(), lang.as_deref()),
            system_lang()
        );
    }

    /// Collect the first string literal of every `tr(`/`tr_fmt!(` call in
    /// this crate's sources. A small lexer skips comments, string and char
    /// literals and `#[cfg(test)]` modules (so the scanner does not match
    /// itself and test-only calls do not count), decodes escapes and
    /// collapses whitespace runs, matching what [`tr`] looks up at runtime.
    fn scanned_keys() -> Vec<String> {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut keys = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    keys.extend(scan_source_keys(&text));
                }
            }
        }
        keys
    }

    /// One pass over a source file: walks code outside comments, string and
    /// char literals and `#[cfg(test)]` modules, capturing the first string
    /// literal of each `tr(`/`tr_fmt!(` call as a key.
    fn scan_source_keys(text: &str) -> Vec<String> {
        let v: Vec<char> = text.chars().collect();
        let n = v.len();
        let mut keys = Vec::new();
        let mut skip_test = false;
        let mut skip_depth = 0usize;
        let mut i = 0usize;
        while i < n {
            let c = v[i];
            if skip_test {
                match c {
                    '"' => i = skip_string(&v, i),
                    '\'' => i = skip_char_or_advance(&v, i),
                    '/' if i + 1 < n && v[i + 1] == '/' => i = skip_line_comment(&v, i),
                    '/' if i + 1 < n && v[i + 1] == '*' => i = skip_block_comment(&v, i),
                    '{' => {
                        skip_depth += 1;
                        i += 1;
                    }
                    '}' => {
                        skip_depth -= 1;
                        i += 1;
                        if skip_depth == 0 {
                            skip_test = false;
                        }
                    }
                    _ => i += 1,
                }
                continue;
            }
            match c {
                '"' => i = skip_string(&v, i),
                '\'' => i = skip_char_or_advance(&v, i),
                '/' if i + 1 < n && v[i + 1] == '/' => i = skip_line_comment(&v, i),
                '/' if i + 1 < n && v[i + 1] == '*' => i = skip_block_comment(&v, i),
                '#' if v[i..]
                    .starts_with(&['#', '[', 'c', 'f', 'g', '(', 't', 'e', 's', 't', ')', ']']) =>
                {
                    if let Some(body_start) = maybe_enter_test_module(&v, i) {
                        skip_test = true;
                        skip_depth = 1;
                        i = body_start;
                    } else {
                        i += 12;
                    }
                }
                't' if !(i > 0 && (v[i - 1].is_alphanumeric() || v[i - 1] == '_')) => {
                    if v[i..].starts_with(&['t', 'r', '_', 'f', 'm', 't', '!', '(']) {
                        i = capture_key(&v, i + 8, &mut keys);
                    } else if v[i..].starts_with(&['t', 'r', '(']) {
                        i = capture_key(&v, i + 3, &mut keys);
                    } else {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
        keys
    }

    /// At a `#[cfg(test)]` attribute: when a `mod <name> {` follows, return
    /// the index of the body's first char (the caller enters skip mode);
    /// otherwise return None and the caller just advances.
    fn maybe_enter_test_module(v: &[char], mut j: usize) -> Option<usize> {
        j += 12; // "#[cfg(test)]"
        while j < v.len() && v[j].is_whitespace() {
            j += 1;
        }
        let ident_end = j + 3;
        if !v[j..].starts_with(&['m', 'o', 'd'])
            || (ident_end < v.len() && (v[ident_end].is_alphanumeric() || v[ident_end] == '_'))
        {
            return None;
        }
        j = ident_end;
        while j < v.len() && v[j].is_whitespace() {
            j += 1;
        }
        while j < v.len() && (v[j].is_alphanumeric() || v[j] == '_') {
            j += 1;
        }
        while j < v.len() && v[j].is_whitespace() {
            j += 1;
        }
        if j < v.len() && v[j] == '{' {
            Some(j + 1)
        } else {
            None
        }
    }

    /// Past a string literal; escapes only advance the scan (keys are
    /// decoded in [`capture_key`]).
    fn skip_string(v: &[char], mut j: usize) -> usize {
        j += 1; // opening quote
        while j < v.len() {
            match v[j] {
                '\\' => j += 2,
                '"' => return j + 1,
                _ => j += 1,
            }
        }
        j
    }

    /// Past a char literal; a lone `'` (lifetime or label) advances by one.
    fn skip_char_or_advance(v: &[char], j: usize) -> usize {
        let is_char =
            j + 1 < v.len() && (v[j + 1] == '\\' || (j + 2 < v.len() && v[j + 2] == '\''));
        if !is_char {
            return j + 1;
        }
        let mut k = j + 1;
        while k < v.len() {
            match v[k] {
                '\\' => k += 2,
                '\'' => return k + 1,
                _ => k += 1,
            }
        }
        k
    }

    fn skip_line_comment(v: &[char], mut j: usize) -> usize {
        while j < v.len() && v[j] != '\n' {
            j += 1;
        }
        j + 1
    }

    fn skip_block_comment(v: &[char], mut j: usize) -> usize {
        j += 2;
        let mut depth = 1usize;
        while j < v.len() && depth > 0 {
            if v[j] == '/' && j + 1 < v.len() && v[j + 1] == '*' {
                depth += 1;
                j += 2;
            } else if v[j] == '*' && j + 1 < v.len() && v[j + 1] == '/' {
                depth -= 1;
                j += 2;
            } else {
                j += 1;
            }
        }
        j
    }

    /// At the `(` of a `tr(`/`tr_fmt!(` call: capture the first string
    /// literal argument (the key), whitespace-collapsed like [`tr`]'s
    /// lookup, and return the scan position after it.
    fn capture_key(v: &[char], mut j: usize, keys: &mut Vec<String>) -> usize {
        while j < v.len() && v[j].is_whitespace() {
            j += 1;
        }
        if j >= v.len() || v[j] != '"' {
            return j;
        }
        j += 1;
        let mut out = String::new();
        while j < v.len() {
            match v[j] {
                '"' => {
                    j += 1;
                    break;
                }
                '\\' => {
                    j += 1;
                    if j < v.len() {
                        match v[j] {
                            '"' => out.push('"'),
                            '\\' => out.push('\\'),
                            'n' => out.push('\n'),
                            't' => out.push('\t'),
                            '\n' => {
                                // Line continuation: strip the newline and
                                // the next line's leading whitespace.
                                while j + 1 < v.len() && v[j + 1].is_whitespace() {
                                    j += 1;
                                }
                            }
                            other => out.push(other),
                        }
                        j += 1;
                    }
                }
                other => {
                    out.push(other);
                    j += 1;
                }
            }
        }
        keys.push(collapse_ws(&out));
        j
    }
}
