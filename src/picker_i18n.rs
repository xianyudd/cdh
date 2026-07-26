use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Language {
    ZhCn,
    En,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextKey {
    PermissionDenied,
    DirectoryMissing,
    SearchPlaceholder,
    MissingStatus,
    FooterPrimary,
    FooterCompact,
    FooterShort,
    SettingsTitle,
    SettingLanguage,
    SettingPreviewStartup,
    SettingColor,
    SettingMouseCapture,
    LanguageAuto,
    LanguageSimplifiedChinese,
    LanguageEnglish,
    SettingOn,
    SettingOff,
    EnvironmentControlled,
    SettingsFooter,
    RecordMissing,
    HistoryDeleted,
    PreviewUnavailable,
    HistoryWriteUnavailable,
    DeleteFailedPrefix,
    NoDeletableHistory,
    DiscoveredNotDeletable,
    MissingDeleteHint,
    NoJumpTarget,
    TerminalTooSmall,
    EmptyHistory,
    NoMatches,
    ClearSearch,
    NoSelection,
    Loading,
    CannotReadPrefix,
    LastVisitPrefix,
    EmptyDirectory,
    MoreEntries,
    NoVisitRecord,
    JustNow,
    PreviewSpaceInsufficient,
    HelpTitle,
    Movement,
    PreviousItem,
    NextItem,
    Paging,
    PreviousPage,
    NextPage,
    FirstItem,
    LastItem,
    Search,
    MoveCursor,
    DeleteBeforeCursor,
    DeleteAtCursor,
    ClearSearchDescription,
    Actions,
    JumpToDirectory,
    TogglePreview,
    DeleteHistoryEntry,
    OpenHelp,
    OpenSettings,
    SettingsControls,
    EscapeDescription,
    UnknownDirectory,
    ConfirmDeleteTitle,
    ConfirmDeleteAgain,
    ConfirmDeletePrefix,
    ConfirmDeleteSuffix,
    ConfirmDeleteShort,
    GitModified,
    GitClean,
    SettingsLocked,
    SettingsSaved,
    SettingsSaveFailedPrefix,
    SettingsLoadFailedPrefix,
    MouseEnabled,
    MouseDisabled,
    MouseTerminalFailedPrefix,
    MousePersistenceFailedPrefix,
    MouseRollbackFailedPrefix,
    SettingTheme,
    ThemeGraphite,
    ThemeNord,
    ThemeDaylight,
    ThemeMono,
    ThemeDracula,
    ThemeAmber,
    ThemeForest,
}

impl Language {
    pub(super) fn text(self, key: TextKey) -> &'static str {
        use TextKey::*;
        match key {
            PermissionDenied => self.pick("权限不足", "Permission denied"),
            DirectoryMissing => self.pick("目录已不存在", "Directory no longer exists"),
            SearchPlaceholder => self.pick("输入路径片段…", "Search paths…"),
            MissingStatus => self.pick("失效", "missing"),
            FooterPrimary => self.pick(
                "↑↓ 选择 · Ctrl+↑↓ 翻页 · Enter 跳转 · Tab 预览 · F1 帮助 · F2 设置 · Esc 退出",
                "↑↓ Select · Ctrl+↑↓ Page · Enter Jump · Tab Preview · F1 Help · F2 Settings · Esc Exit",
            ),
            FooterCompact => self.pick(
                "↑↓ 选择 · Enter 跳转 · Tab 预览 · F1 帮助 · F2 设置 · Esc 退出",
                "↑↓ Select · Enter Jump · Tab Preview · F1 Help · F2 Settings · Esc Exit",
            ),
            FooterShort => self.pick(
                "Enter 跳转 · F1 帮助 · F2 设置 · Esc 退出",
                "Enter Jump · F1 Help · F2 Settings · Esc Exit",
            ),
            SettingsTitle => self.pick("设置", "Settings"),
            SettingLanguage => self.pick("语言", "Language"),
            SettingPreviewStartup => self.pick("启动时预览", "Preview on startup"),
            SettingColor => self.pick("颜色", "Color"),
            SettingMouseCapture => self.pick("鼠标捕获", "Mouse capture"),
            LanguageAuto => self.pick("自动", "Auto"),
            LanguageSimplifiedChinese => self.pick("简体中文", "Simplified Chinese"),
            LanguageEnglish => self.pick("英语", "English"),
            SettingOn => self.pick("开", "On"),
            SettingOff => self.pick("关", "Off"),
            EnvironmentControlled => {
                self.pick("环境控制/只读", "Environment controlled/read-only")
            }
            SettingsFooter => self.pick(
                "上下选择 · 左右更改 · 回车/空格更改 · Esc 完成",
                "Up/Down select · Left/Right change · Enter/Space change · Esc done",
            ),
            RecordMissing => self.pick("记录已不存在", "History entry no longer exists"),
            HistoryDeleted => self.pick("已删除历史记录", "History entry deleted"),
            PreviewUnavailable => self.pick("预览功能不可用", "Preview unavailable"),
            HistoryWriteUnavailable => self.pick(
                "当前环境无法修改历史记录",
                "History cannot be modified in this environment",
            ),
            DeleteFailedPrefix => self.pick("删除失败: ", "Delete failed: "),
            NoDeletableHistory => self.pick("没有可删除的历史记录", "No history entry to delete"),
            DiscoveredNotDeletable => self.pick(
                "该目录来自目录树，没有历史记录可删",
                "This directory came from the tree scan; no history entry to delete",
            ),
            MissingDeleteHint => self.pick(
                "目录已失效，按 Ctrl+D 删除历史记录",
                "Directory is missing; press Ctrl+D to delete its history entry",
            ),
            NoJumpTarget => self.pick("没有可跳转的目录", "No directory to jump to"),
            TerminalTooSmall => self.pick(
                "终端太小（至少需要 8 行）",
                "Terminal too small (at least 8 rows required)",
            ),
            EmptyHistory => self.pick("暂无目录历史", "No directory history"),
            NoMatches => self.pick("未找到匹配目录", "No matching directories"),
            ClearSearch => self.pick(" 清空搜索", " Clear search"),
            NoSelection => self.pick("没有选中项", "No selection"),
            Loading => self.pick("加载中…", "Loading…"),
            CannotReadPrefix => self.pick("无法读取: ", "Cannot read: "),
            LastVisitPrefix => self.pick("最后访问: ", "Last visit: "),
            EmptyDirectory => self.pick("空目录", "Empty directory"),
            MoreEntries => self.pick("… 还有更多项", "… more entries"),
            NoVisitRecord => self.pick("暂无记录", "No record"),
            JustNow => self.pick("刚刚", "Just now"),
            PreviewSpaceInsufficient => {
                self.pick("预览: 终端空间不足", "Preview: not enough terminal space")
            }
            HelpTitle => self.pick("快捷键", "Keyboard shortcuts"),
            Movement => self.pick("移动", "Movement"),
            PreviousItem => self.pick("上一项", "Previous item"),
            NextItem => self.pick("下一项", "Next item"),
            Paging => self.pick("分页", "Paging"),
            PreviousPage => self.pick("上一页", "Previous page"),
            NextPage => self.pick("下一页", "Next page"),
            FirstItem => self.pick("第一项", "First item"),
            LastItem => self.pick("最后一项", "Last item"),
            Search => self.pick("搜索", "Search"),
            MoveCursor => self.pick("移动输入光标", "Move input cursor"),
            DeleteBeforeCursor => self.pick("删除光标左侧字符", "Delete before cursor"),
            DeleteAtCursor => self.pick("删除光标所在字符", "Delete at cursor"),
            ClearSearchDescription => self.pick("清空搜索", "Clear search"),
            Actions => self.pick("操作", "Actions"),
            JumpToDirectory => self.pick("跳转到目录", "Jump to directory"),
            TogglePreview => self.pick("切换预览", "Toggle preview"),
            DeleteHistoryEntry => self.pick("删除当前历史记录", "Delete history entry"),
            OpenHelp => self.pick("打开帮助", "Open help"),
            OpenSettings => self.pick("打开设置", "Open settings"),
            SettingsControls => self.pick(
                "选择 · 更改 · 更改 · 完成",
                "Select · Change · Change · Done",
            ),
            EscapeDescription => self.pick(
                "关闭面板、清空搜索或退出",
                "Close panel, clear search, or exit",
            ),
            UnknownDirectory => self.pick("未知目录", "Unknown directory"),
            ConfirmDeleteTitle => self.pick("确认删除", "Confirm deletion"),
            ConfirmDeleteAgain => self.pick(
                "再次按 Ctrl+D 确认，Esc 取消",
                "Press Ctrl+D again to confirm, Esc to cancel",
            ),
            ConfirmDeletePrefix => self.pick("删除历史记录 “", "Delete history entry “"),
            ConfirmDeleteSuffix => self.pick("”？", "”?"),
            ConfirmDeleteShort => self.pick("确认删除", "Confirm delete"),
            GitModified => self.pick("已修改", "modified"),
            GitClean => self.pick("干净", "clean"),
            SettingsLocked => self.pick("此设置由环境变量锁定", "Setting locked by environment"),
            SettingsSaved => self.pick("设置已保存", "Settings saved"),
            SettingsSaveFailedPrefix => self.pick("保存设置失败: ", "Failed to save settings: "),
            SettingsLoadFailedPrefix => {
                self.pick("TUI 设置加载失败: ", "Failed to load TUI settings: ")
            }
            MouseEnabled => self.pick("鼠标已启用", "Mouse enabled"),
            MouseDisabled => self.pick("鼠标已禁用", "Mouse disabled"),
            MouseTerminalFailedPrefix => self.pick(
                "切换终端鼠标捕获失败: ",
                "Failed to change terminal mouse capture: ",
            ),
            MousePersistenceFailedPrefix => {
                self.pick("保存鼠标设置失败: ", "Failed to save mouse setting: ")
            }
            MouseRollbackFailedPrefix => self.pick(
                "；恢复终端鼠标状态失败: ",
                "; failed to restore terminal mouse state: ",
            ),
            SettingTheme => self.pick("主题", "Theme"),
            ThemeGraphite => self.pick("石墨", "Graphite"),
            ThemeNord => self.pick("夜航", "Nord"),
            ThemeDaylight => self.pick("晨光", "Daylight"),
            ThemeMono => self.pick("素墨", "Mono"),
            ThemeDracula => self.pick("紫夜", "Dracula"),
            ThemeAmber => self.pick("琥珀", "Amber"),
            ThemeForest => self.pick("林间", "Forest"),
        }
    }

    pub(super) fn page_summary(
        self,
        start: usize,
        end: usize,
        total: usize,
        page: usize,
        page_count: usize,
    ) -> String {
        if total == 0 {
            return match self {
                Self::ZhCn => "0 / 0 · 第 0/0 页".to_string(),
                Self::En => "0 / 0 · Page 0/0".to_string(),
            };
        }
        match self {
            Self::ZhCn => format!("{}–{end} / {total} · 第 {page}/{page_count} 页", start + 1),
            Self::En => format!("{}–{end} / {total} · Page {page}/{page_count}", start + 1),
        }
    }

    pub(super) fn relative_time(self, timestamp: Option<i64>, now: i64) -> String {
        let Some(timestamp) = timestamp else {
            return self.text(TextKey::NoVisitRecord).to_string();
        };
        let elapsed = now.saturating_sub(timestamp);
        if elapsed < 60 {
            return self.text(TextKey::JustNow).to_string();
        }
        if elapsed < 60 * 60 {
            let minutes = elapsed / 60;
            return match self {
                Self::ZhCn => format!("{minutes} 分钟前"),
                Self::En => plural_ago(minutes, "minute"),
            };
        }
        if elapsed < 24 * 60 * 60 {
            let hours = elapsed / (60 * 60);
            return match self {
                Self::ZhCn => format!("{hours} 小时前"),
                Self::En => plural_ago(hours, "hour"),
            };
        }
        let days = elapsed / (24 * 60 * 60);
        match self {
            Self::ZhCn => format!("{days} 天前"),
            Self::En => plural_ago(days, "day"),
        }
    }

    const fn pick(self, zh_cn: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::ZhCn => zh_cn,
            Self::En => en,
        }
    }
}

fn plural_ago(value: i64, unit: &str) -> String {
    let suffix = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{suffix} ago")
}

pub(super) fn detect_locale_language() -> Language {
    let lc_all = env::var("LC_ALL").ok();
    let lc_messages = env::var("LC_MESSAGES").ok();
    let lang = env::var("LANG").ok();
    resolve_locale_language(&[lc_all.as_deref(), lc_messages.as_deref(), lang.as_deref()])
}

pub(super) fn resolve_locale_language(locales: &[Option<&str>]) -> Language {
    resolve_language(None, locales)
}

pub(super) fn resolve_language(explicit: Option<&str>, locales: &[Option<&str>]) -> Language {
    if let Some(language) = explicit.and_then(parse_language_tag) {
        return language;
    }
    for locale in locales.iter().flatten() {
        if let Some(language) = parse_language_tag(locale) {
            return language;
        }
    }
    let has_locale = explicit.is_some() || locales.iter().any(Option::is_some);
    if has_locale {
        Language::En
    } else {
        Language::ZhCn
    }
}

fn parse_language_tag(value: &str) -> Option<Language> {
    let value = value.trim().to_ascii_lowercase().replace('_', "-");
    let tag = value.split(['.', '@']).next().unwrap_or_default();
    if tag == "c" || tag == "posix" || tag == "en" || tag.starts_with("en-") {
        Some(Language::En)
    } else if tag == "zh" || tag.starts_with("zh-") {
        Some(Language::ZhCn)
    } else {
        None
    }
}
