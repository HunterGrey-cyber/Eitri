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
    pub stuck: &'static str,
}

pub(super) fn text(lang: Lang) -> Text {
    match lang {
        Lang::En => Text {
            title: "Open a project",
            placeholder: "filter, or a path: ~/src/app",
            placeholder_list: "press / to filter or type a path",
            footer: "Enter open · ↑↓ move · Tab complete · Ctrl+o folder · Esc list",
            footer_list: "Enter open · j k move · gg G ends · / filter · Ctrl+o folder · q Esc close",
            folder: "Choose a folder…",
            home: "Home folder",
            empty: "No recent projects",
            open: "open",
            gone: "gone",
            stuck: "cannot read that folder now",
        },
        Lang::ZhCn => Text {
            title: "打开项目",
            placeholder: "筛选，或输入路径：~/src/app",
            placeholder_list: "按 / 筛选或输入路径",
            footer: "Enter 打开 · ↑↓ 移动 · Tab 补全 · Ctrl+o 文件夹 · Esc 返回列表",
            footer_list: "Enter 打开 · j k 移动 · gg G 首尾 · / 筛选 · Ctrl+o 文件夹 · q Esc 关闭",
            folder: "选择文件夹…",
            home: "主目录",
            empty: "没有最近的项目",
            open: "已打开",
            gone: "不在了",
            stuck: "现在读不了这个文件夹",
        },
    }
}

pub(super) fn missing(lang: Lang, path: &str) -> String {
    match lang {
        Lang::En => format!("no such folder: {path}"),
        Lang::ZhCn => format!("没有这个文件夹：{path}"),
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
