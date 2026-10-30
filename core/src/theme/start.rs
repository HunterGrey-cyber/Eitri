//! The initial theme shared by consumers before a live nvim payload arrives.

use std::path::Path;

use super::{payload::NvimThemePayload, store, ThemeTokens};

#[derive(Debug, PartialEq)]
pub enum FallbackWhy {
    Clean,
    NoStateDir,
    Missing,
    Unusable(String),
}

#[derive(Debug, PartialEq)]
pub enum StartSource {
    Remembered,
    Fallback(FallbackWhy),
}

pub struct StartTheme {
    pub payload: Option<NvimThemePayload>,
    pub tokens: ThemeTokens,
    pub source: StartSource,
}

fn fallback(why: FallbackWhy) -> StartTheme {
    StartTheme {
        payload: None,
        tokens: ThemeTokens::fallback(),
        source: StartSource::Fallback(why),
    }
}

pub enum StartReport {
    Out(String),
    Err(String),
}

impl StartTheme {
    pub fn read(dir: Option<&Path>, project_root: &Path, clean: bool) -> StartTheme {
        if clean {
            return fallback(FallbackWhy::Clean);
        }
        match dir {
            Some(dir) => StartTheme::from_loaded(store::load(dir, project_root)),
            None => fallback(FallbackWhy::NoStateDir),
        }
    }

    pub fn from_loaded(loaded: store::Loaded) -> StartTheme {
        match loaded {
            store::Loaded::Remembered(payload) => StartTheme {
                tokens: ThemeTokens::derive(&payload),
                payload: Some(payload),
                source: StartSource::Remembered,
            },
            store::Loaded::Missing => fallback(FallbackWhy::Missing),
            store::Loaded::Unusable(why) => fallback(FallbackWhy::Unusable(why)),
        }
    }

    pub fn editor_background(&self) -> Option<(u8, u8, u8)> {
        self.payload
            .as_ref()
            .map(|_| (self.tokens.bg.r, self.tokens.bg.g, self.tokens.bg.b))
    }

    pub fn prefers_dark(&self) -> Option<bool> {
        self.payload.as_ref().map(|_| self.tokens.color_scheme == "dark")
    }

    pub fn guifont(&self) -> &str {
        self.payload.as_ref().map_or("", |p| p.options.guifont.as_str())
    }

    pub fn report(&self) -> Option<StartReport> {
        match &self.source {
            StartSource::Remembered => self.payload.as_ref().map(|p| {
                StartReport::Out(format!(
                    "[theme] starting from the remembered colorscheme {:?} (background={})",
                    p.options.colors_name, p.options.background,
                ))
            }),
            StartSource::Fallback(FallbackWhy::Unusable(why)) => Some(StartReport::Err(format!(
                "[theme] the remembered colours are unusable: {why} -- starting from the built-in fallback"
            ))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::payload::HlAttrs;

    fn payload() -> NvimThemePayload {
        let mut p = NvimThemePayload::empty();
        p.options.background = "light".into();
        p.options.colors_name = "rose-pine-dawn".into();
        p.options.guifont = "Mono:h14".into();
        p.groups.insert(
            "Normal".into(),
            HlAttrs {
                fg: Some(0x575279),
                bg: Some(0xfaf4ed),
                reverse: false,
            },
        );
        p
    }
    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("eitri-theme-start-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }
    #[test]
    fn with_no_file_the_start_theme_is_the_fallback() {
        let dir = scratch();
        let theme = StartTheme::read(Some(&dir), Path::new("/project"), false);
        assert_eq!(theme.source, StartSource::Fallback(FallbackWhy::Missing));
        assert_eq!(theme.tokens, ThemeTokens::fallback());
        assert!(theme.payload.is_none());
        assert_eq!(theme.editor_background(), None);
        assert_eq!(theme.prefers_dark(), None);
        assert_eq!(theme.guifont(), "");
        assert!(theme.report().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_remembered_light_payload_starts_every_token_from_it() {
        let p = payload();
        let theme = StartTheme::from_loaded(store::Loaded::Remembered(p.clone()));
        assert_eq!(theme.tokens, ThemeTokens::derive(&p));
        assert_eq!(theme.tokens.color_scheme, "light");
        assert_eq!(theme.source, StartSource::Remembered);
        assert_eq!(theme.payload, Some(p.clone()));
        assert_eq!(theme.editor_background(), Some((0xfa, 0xf4, 0xed)));
        assert_eq!(theme.prefers_dark(), Some(false));
        assert_eq!(theme.guifont(), p.options.guifont);
        let Some(StartReport::Out(line)) = theme.report() else {
            panic!("missing start report")
        };
        assert!(line.contains("rose-pine-dawn"));
        assert!(line.contains("background=light"));
    }
    #[test]
    fn clean_never_reads_the_file() {
        let dir = scratch();
        let root = Path::new("/project");
        std::fs::write(store::path(&dir, root), store::encode(root, &payload()).unwrap()).unwrap();
        let theme = StartTheme::read(Some(&dir), root, true);
        assert_eq!(theme.source, StartSource::Fallback(FallbackWhy::Clean));
        assert_eq!(theme.tokens, ThemeTokens::fallback());
        assert!(theme.report().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn no_state_dir_is_quiet() {
        let theme = StartTheme::read(None, Path::new("/project"), false);
        assert_eq!(theme.source, StartSource::Fallback(FallbackWhy::NoStateDir));
        assert!(theme.report().is_none());
    }
    #[test]
    fn an_unusable_file_falls_back_and_says_why() {
        let theme = StartTheme::from_loaded(store::Loaded::Unusable("broken file".into()));
        assert_eq!(
            theme.source,
            StartSource::Fallback(FallbackWhy::Unusable("broken file".into()))
        );
        assert_eq!(theme.tokens, ThemeTokens::fallback());
        assert!(theme.payload.is_none());
        let Some(StartReport::Err(line)) = theme.report() else {
            panic!("missing error report")
        };
        assert!(line.contains("broken file"));
        assert!(line.contains("built-in fallback"));
    }
}
