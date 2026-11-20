//! Настройки интерфейса: язык, контейнер, папка результата, тема, окно.
//!
//! Файл `ui.ini` лежит в `%APPDATA%\TrackCut\`. Если рядом с программой есть
//! `portable.ini` или `portable.txt`, настройки пишутся рядом с программой —
//! это портативный режим (например, с флешки).

use std::path::PathBuf;

use crate::i18n::Lang;

const FILE_NAME: &str = "ui.ini";

/// Всё, что переживает перезапуск.
#[derive(Clone, PartialEq)]
pub struct Settings {
    pub lang: Lang,
    /// `auto` | `mkv` | `webm` | `mp4` | `mov`
    pub container: String,
    pub output_dir: Option<PathBuf>,
    pub suffix: String,
    pub keep_chapters: bool,
    pub keep_tags: bool,
    pub keep_attachments: bool,
    pub mov_text: bool,
    pub overwrite: bool,
    /// `none` | `every` | `ranges`
    pub split: String,
    pub split_minutes: f32,
    pub split_ranges: String,
    /// `dark` | `light` | `system`
    pub theme: String,
    pub notify: bool,
    /// Позиция и размер окна: x, y, ширина, высота.
    pub window: Option<(f32, f32, f32, f32)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lang: Lang::Ru,
            container: "auto".to_string(),
            output_dir: None,
            suffix: "_clean".to_string(),
            keep_chapters: true,
            keep_tags: true,
            keep_attachments: true,
            mov_text: false,
            overwrite: false,
            split: "none".to_string(),
            split_minutes: 10.0,
            split_ranges: String::new(),
            theme: "dark".to_string(),
            notify: true,
            window: None,
        }
    }
}

impl Settings {
    /// Читает настройки; при любой проблеме — значения по умолчанию.
    pub fn load() -> Self {
        let mut settings = Settings::default();
        let Some(path) = config_file() else {
            return settings;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return settings;
        };
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "lang" => {
                    if let Some(lang) = Lang::from_code(value) {
                        settings.lang = lang;
                    }
                }
                "container" => settings.container = value.to_string(),
                "output_dir" => {
                    settings.output_dir = if value.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    };
                }
                "suffix" => settings.suffix = value.to_string(),
                "keep_chapters" => settings.keep_chapters = flag(value),
                "keep_tags" => settings.keep_tags = flag(value),
                "keep_attachments" => settings.keep_attachments = flag(value),
                "mov_text" => settings.mov_text = flag(value),
                "overwrite" => settings.overwrite = flag(value),
                "split" => settings.split = value.to_string(),
                "split_minutes" => {
                    if let Ok(minutes) = value.parse::<f32>() {
                        settings.split_minutes = minutes.clamp(1.0, 240.0);
                    }
                }
                "split_ranges" => settings.split_ranges = value.to_string(),
                "theme" => settings.theme = value.to_string(),
                "notify" => settings.notify = flag(value),
                "window" => settings.window = parse_window(value),
                _ => {}
            }
        }
        settings
    }

    /// Сохраняет настройки. Ошибки записи не критичны — останутся прошлые.
    pub fn save(&self) {
        let Some(path) = config_file() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut text = String::new();
        text.push_str(&format!("lang={}\n", self.lang.code()));
        text.push_str(&format!("container={}\n", self.container));
        text.push_str(&format!(
            "output_dir={}\n",
            self.output_dir
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default()
        ));
        text.push_str(&format!("suffix={}\n", self.suffix));
        text.push_str(&format!("keep_chapters={}\n", number(self.keep_chapters)));
        text.push_str(&format!("keep_tags={}\n", number(self.keep_tags)));
        text.push_str(&format!(
            "keep_attachments={}\n",
            number(self.keep_attachments)
        ));
        text.push_str(&format!("mov_text={}\n", number(self.mov_text)));
        text.push_str(&format!("overwrite={}\n", number(self.overwrite)));
        text.push_str(&format!("split={}\n", self.split));
        text.push_str(&format!("split_minutes={:.1}\n", self.split_minutes));
        text.push_str(&format!("split_ranges={}\n", self.split_ranges));
        text.push_str(&format!("theme={}\n", self.theme));
        text.push_str(&format!("notify={}\n", number(self.notify)));
        if let Some((x, y, width, height)) = self.window {
            text.push_str(&format!("window={x:.0},{y:.0},{width:.0},{height:.0}\n"));
        }

        let temp = path.with_extension("tmp");
        if std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
    }
}

/// Путь к файлу настроек (или None, если некуда писать).
pub fn config_file() -> Option<PathBuf> {
    if portable() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return Some(dir.join(FILE_NAME));
            }
        }
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    Some(base.join("TrackCut").join(FILE_NAME))
}

/// Портативный режим: рядом с программой лежит portable.ini или portable.txt.
pub fn portable() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    dir.join("portable.ini").is_file() || dir.join("portable.txt").is_file()
}

fn flag(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
}

fn number(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

fn parse_window(value: &str) -> Option<(f32, f32, f32, f32)> {
    let parts: Vec<f32> = value
        .split(',')
        .filter_map(|part| part.trim().parse::<f32>().ok())
        .collect();
    if parts.len() != 4 {
        return None;
    }
    if parts[2] < 400.0 || parts[3] < 300.0 {
        return None;
    }
    Some((parts[0], parts[1], parts[2], parts[3]))
}
