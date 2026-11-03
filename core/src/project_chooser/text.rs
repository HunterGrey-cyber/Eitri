use crate::onboarding::Lang;

pub(super) struct Text {
    pub title: &'static str,
    pub placeholder: &'static str,
    pub placeholder_list: &'static str,
    pub footer: &'static str,
    pub footer_list: &'static str,
    pub folder: &'static str,
    pub home: &'static str,
    pub empty: &'static str,
    pub open: &'static str,
    pub gone: &'static str,
    pub browse: &'static str,
    pub hidden: &'static str,
    pub here: &'static str,
    pub git: &'static str,
    pub link: &'static str,
    pub mount: &'static str,
    pub file: &'static str,
    pub reading: &'static str,
    pub stuck: &'static str,
    pub too_many: &'static str,
    pub denied: &'static str,
    pub looped: &'static str,
    pub not_folder: &'static str,
    pub no_folders: &'static str,
    pub folders_cap: &'static str,
    pub scanned_cap: &'static str,
}

pub(super) fn text(lang: Lang) -> Text {
    match lang {
        Lang::En => Text {
            title: "Open a project",
            placeholder: "filter, or a path: ~/src/app",
            placeholder_list: "press / to filter or type a path",
            footer: "Enter open · ↑↓ move · Tab complete · Ctrl+o folder · Esc list",
            footer_list: "Enter open · j k move · h l browse · / filter · Ctrl+o dialog · q close",
            folder: "Browse folders…",
            home: "Home folder",
            empty: "No recent projects",
            open: "open",
            gone: "gone",
            browse: "Browse folders",
            hidden: " · hidden shown",
            here: "this folder",
            git: "git",
            link: "link",
            mount: "mount",
            file: "file",
            reading: "reading",
            stuck: "not answering · h goes back",
            too_many: "too many folders not answering · Ctrl+o for the system dialog",
            denied: "cannot read: permission denied · h goes back",
            looped: "cannot read: too many links",
            not_folder: "not a folder",
            no_folders: "no folders here · Enter opens this folder",
            folders_cap: "first 2000 folders shown · / searches the whole folder",
            scanned_cap: "first 50 000 entries read",
        },
        Lang::ZhCn => Text {
            title: "打开项目",
            placeholder: "筛选，或输入路径：~/src/app",
            placeholder_list: "按 / 筛选或输入路径",
            footer: "Enter 打开 · ↑↓ 移动 · Tab 补全 · Ctrl+o 文件夹 · Esc 返回列表",
            footer_list: "Enter 打开 · j k 移动 · h l 浏览 · / 筛选 · Ctrl+o 对话框 · q 关闭",
            folder: "浏览文件夹…",
            home: "主目录",
            empty: "没有最近的项目",
            open: "已打开",
            gone: "不在了",
            browse: "浏览文件夹",
            hidden: " · 显示隐藏",
            here: "此文件夹",
            git: "git",
            link: "链接",
            mount: "挂载点",
            file: "文件",
            reading: "正在读取",
            stuck: "没有响应 · h 返回",
            too_many: "太多文件夹没有响应 · Ctrl+o 用系统对话框",
            denied: "读不了：没有权限 · h 返回",
            looped: "读不了：链接太多",
            not_folder: "不是文件夹",
            no_folders: "这里没有文件夹 · Enter 打开此文件夹",
            folders_cap: "只显示前 2000 个文件夹 · / 搜索整个文件夹",
            scanned_cap: "只读了前 50 000 项",
        },
    }
}

pub(super) fn missing(lang: Lang, path: &str) -> String {
    match lang {
        Lang::En => format!("no such folder: {path}"),
        Lang::ZhCn => format!("没有这个文件夹：{path}"),
    }
}

pub(super) fn cannot_read(lang: Lang, path: &str) -> String {
    match lang {
        Lang::En => format!("cannot read {path} now"),
        Lang::ZhCn => format!("现在读不了 {path}"),
    }
}

pub(super) fn no_match(lang: Lang, filter: &str) -> String {
    match lang {
        Lang::En => format!("no folder matches \"{filter}\""),
        Lang::ZhCn => format!("没有匹配“{filter}”的文件夹"),
    }
}

/// Keep the action words visible even when the selected name is longer than the panel.
fn short_name(name: &str) -> String {
    let chars: Vec<_> = name.chars().collect();
    if chars.len() <= 24 {
        return name.into();
    }
    chars[..12]
        .iter()
        .chain(std::iter::once(&'…'))
        .chain(chars[chars.len() - 11..].iter())
        .collect()
}

/// What `Enter` does on the selected row, which also decides whether `l` goes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Enter {
    /// Opens the folder shown (the `.` row): `l` goes in too.
    Open,
    /// Opens a file row: `l` does nothing on it.
    OpenFile,
    /// Goes into a folder.
    Into,
    /// Answers "not a folder": the footer names no `Enter` action.
    Inert,
}

/// The name sits in a first-strong isolate, so a right-to-left name cannot reorder the action words around it.
pub(super) fn footer(lang: Lang, name: &str, enter: Enter, in_line: bool) -> String {
    let name = format!("\u{2068}{}\u{2069}", short_name(name));
    let action = match (lang, enter) {
        (Lang::En, Enter::Open | Enter::OpenFile) => Some(format!("Enter open {name}")),
        (Lang::En, Enter::Into) => Some(format!("Enter into {name}/")),
        (Lang::ZhCn, Enter::Open | Enter::OpenFile) => Some(format!("Enter 打开 {name}")),
        (Lang::ZhCn, Enter::Into) => Some(format!("Enter 进入 {name}/")),
        (_, Enter::Inert) => None,
    };
    let keys = match (lang, in_line, enter) {
        (Lang::En, true, _) => "Tab complete · ↑↓ move · Ctrl+w up · Esc clear",
        (Lang::ZhCn, true, _) => "Tab 补全 · ↑↓ 移动 · Ctrl+w 上一级 · Esc 清除",
        (Lang::En, false, Enter::Open) => "l in · h up · / filter · . hidden · Esc back · q close",
        (Lang::En, false, _) => "h up · / filter · . hidden · Esc back · q close",
        (Lang::ZhCn, false, Enter::Open) => "l 进入 · h 上一级 · / 筛选 · . 隐藏 · Esc 返回 · q 关闭",
        (Lang::ZhCn, false, _) => "h 上一级 · / 筛选 · . 隐藏 · Esc 返回 · q 关闭",
    };
    match action {
        Some(action) => format!("{action} · {keys}"),
        None => keys.into(),
    }
}

pub(super) fn gone(lang: Lang, path: &str) -> String {
    match lang {
        Lang::En => format!("{path} is not there"),
        Lang::ZhCn => format!("{path} 不在了"),
    }
}

pub(super) fn opening(lang: Lang, name: &str) -> String {
    match lang {
        Lang::En => format!("Opening {name}…"),
        Lang::ZhCn => format!("正在打开 {name}…"),
    }
}
