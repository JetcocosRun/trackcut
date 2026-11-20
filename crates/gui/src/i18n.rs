//! Строки интерфейса и выбор языка.
//!
//! Русский текст — исходный: он же ключ. Если для языка нет перевода,
//! возвращается русская строка, поэтому недопереведённые места не ломают окно.
//!
//! Каждый язык — отдельный файл в этой папке: одна таблица «русский → перевод».
//! Чтобы добавить новый язык, достаточно нового файла и трёх мест в `Lang`.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Ru,
    En,
    Zh,
    Ja,
    Fr,
    De,
    It,
    Es,
    Kk,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
            Lang::Zh => "zh",
            Lang::Ja => "ja",
            Lang::Fr => "fr",
            Lang::De => "de",
            Lang::It => "it",
            Lang::Es => "es",
            Lang::Kk => "kk",
        }
    }

    pub fn from_code(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "ru" | "rus" | "russian" | "русский" => Some(Lang::Ru),
            "en" | "eng" | "english" => Some(Lang::En),
            "zh" | "chi" | "zho" | "zh-cn" | "zh-hans" | "chinese" => Some(Lang::Zh),
            "ja" | "jpn" | "japanese" => Some(Lang::Ja),
            "fr" | "fre" | "fra" | "french" => Some(Lang::Fr),
            "de" | "ger" | "deu" | "german" => Some(Lang::De),
            "it" | "ita" | "italian" => Some(Lang::It),
            "es" | "spa" | "spanish" => Some(Lang::Es),
            "kk" | "kaz" | "kazakh" => Some(Lang::Kk),
            _ => None,
        }
    }

    /// Название языка — всегда на самом языке, чтобы его узнали в списке.
    pub fn label(self) -> &'static str {
        match self {
            Lang::Ru => "Русский",
            Lang::En => "English",
            Lang::Zh => "中文",
            Lang::Ja => "日本語",
            Lang::Fr => "Français",
            Lang::De => "Deutsch",
            Lang::It => "Italiano",
            Lang::Es => "Español",
            Lang::Kk => "Қазақша",
        }
    }

    pub fn all() -> [Lang; 9] {
        [
            Lang::Ru,
            Lang::En,
            Lang::Zh,
            Lang::Ja,
            Lang::Fr,
            Lang::De,
            Lang::It,
            Lang::Es,
            Lang::Kk,
        ]
    }
}

/// Таблица одного языка: `"русский текст" => "перевод"`.
macro_rules! lang_table {
    ($($ru:literal => $t:literal),* $(,)?) => {
        /// Перевод строки; `None` — перевода нет, останется русская.
        pub fn lookup(text: &str) -> Option<&'static str> {
            match text {
                $($ru => Some($t),)*
                _ => None,
            }
        }
    };
}

mod de;
mod en;
mod es;
mod fr;
mod it;
mod ja;
mod kk;
mod zh;

/// Перевод строки интерфейса. Незнакомый текст остаётся как есть.
pub fn tr(lang: Lang, text: &'static str) -> &'static str {
    match lang {
        Lang::Ru => text,
        Lang::En => en::lookup(text).unwrap_or(text),
        Lang::Zh => zh::lookup(text).unwrap_or(text),
        Lang::Ja => ja::lookup(text).unwrap_or(text),
        Lang::Fr => fr::lookup(text).unwrap_or(text),
        Lang::De => de::lookup(text).unwrap_or(text),
        Lang::It => it::lookup(text).unwrap_or(text),
        Lang::Es => es::lookup(text).unwrap_or(text),
        Lang::Kk => kk::lookup(text).unwrap_or(text),
    }
}

/// Перевод строки, полученной из данных (названия языков, единицы измерения).
pub fn tr_dyn(lang: Lang, text: &'static str) -> &'static str {
    tr(lang, text)
}

const FILE_NAME: &str = "ui.ini";

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    Some(base.join("TrackCut").join(FILE_NAME))
}

/// Читает сохранённый язык; при любой проблеме — русский.
pub fn load_lang() -> Lang {
    let Some(path) = config_path() else {
        return Lang::Ru;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Lang::Ru;
    };
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("lang=") {
            if let Some(lang) = Lang::from_code(value) {
                return lang;
            }
        }
    }
    Lang::Ru
}

/// Сохраняет выбор языка. Ошибки записи не критичны — просто останется русский.
pub fn save_lang(lang: Lang) {
    let Some(path) = config_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("lang={}\n", lang.code()));
}
