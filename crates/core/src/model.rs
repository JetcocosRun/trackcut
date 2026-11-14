use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Button,
    Attachment,
    #[default]
    Other,
}

impl StreamKind {
    pub fn label_ru(self) -> &'static str {
        match self {
            StreamKind::Video => "Видео",
            StreamKind::Audio => "Аудио",
            StreamKind::Subtitle => "Субтитры",
            StreamKind::Button => "Кнопки",
            StreamKind::Attachment => "Вложение",
            StreamKind::Other => "Прочее",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            StreamKind::Video => "🎬",
            StreamKind::Audio => "🔊",
            StreamKind::Subtitle => "💬",
            StreamKind::Button => "🔘",
            StreamKind::Attachment => "📎",
            StreamKind::Other => "•",
        }
    }

    /// Дорожки, которые имеет смысл оставлять/удалять.
    pub fn is_selectable(self) -> bool {
        matches!(
            self,
            StreamKind::Video | StreamKind::Audio | StreamKind::Subtitle
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputContainer {
    Mkv,
    Webm,
    Mp4,
    Mov,
}

impl OutputContainer {
    pub fn extension(self) -> &'static str {
        match self {
            OutputContainer::Mkv => "mkv",
            OutputContainer::Webm => "webm",
            OutputContainer::Mp4 => "mp4",
            OutputContainer::Mov => "mov",
        }
    }

    pub fn label_ru(self) -> &'static str {
        match self {
            OutputContainer::Mkv => "MKV",
            OutputContainer::Webm => "WebM",
            OutputContainer::Mp4 => "MP4",
            OutputContainer::Mov => "MOV",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "mkv" => Some(OutputContainer::Mkv),
            "webm" => Some(OutputContainer::Webm),
            "mp4" | "m4v" => Some(OutputContainer::Mp4),
            "mov" => Some(OutputContainer::Mov),
            _ => None,
        }
    }

    pub fn is_mp4_family(self) -> bool {
        matches!(self, OutputContainer::Mp4 | OutputContainer::Mov)
    }

    pub fn supports_mkvmerge(self) -> bool {
        matches!(self, OutputContainer::Mkv | OutputContainer::Webm)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Mkvmerge,
    Ffmpeg,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Mkvmerge => "mkvmerge",
            Backend::Ffmpeg => "ffmpeg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AttachmentInfo {
    pub id: u64,
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub container: String,
    pub duration_ms: Option<u64>,
    pub streams: Vec<StreamInfo>,
    pub chapters: usize,
    pub attachments: Vec<AttachmentInfo>,
    pub tags: BTreeMap<String, String>,
}

impl MediaInfo {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ..Default::default()
        }
    }

    pub fn duration_secs(&self) -> Option<f64> {
        self.duration_ms.map(|ms| ms as f64 / 1000.0)
    }

    pub fn kind_count(&self, kind: StreamKind) -> usize {
        self.streams.iter().filter(|s| s.kind == kind).count()
    }

    pub fn stream(&self, uid: usize) -> Option<&StreamInfo> {
        self.streams.get(uid)
    }

    pub fn has_buttons(&self) -> bool {
        self.kind_count(StreamKind::Button) > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    pub kind: StreamKind,
    /// ID дорожки в терминах mkvmerge (для -a/-s/-d).
    pub mkv_id: Option<u32>,
    /// Глобальный индекс потока в терминах ffprobe (для -map 0:N).
    pub ff_index: Option<usize>,
    /// Человекочитаемое имя кодека.
    pub codec: String,
    /// Сырой идентификатор: `A_AAC`, `V_MPEGH/ISO/HEVC`, `aac`, `hevc`…
    pub codec_raw: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub channels: Option<u8>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
    pub resolution: Option<(u32, u32)>,
    pub fps: Option<f64>,
    pub bitrate: Option<u64>,
    pub delay_ms: Option<i64>,
    /// Длительность самой дорожки, если контейнер её сообщает.
    pub duration_ms: Option<u64>,
    pub default: bool,
    pub forced: bool,
    pub size_bytes: Option<u64>,
}

impl Default for StreamInfo {
    fn default() -> Self {
        Self {
            kind: StreamKind::Other,
            mkv_id: None,
            ff_index: None,
            codec: String::new(),
            codec_raw: String::new(),
            language: None,
            title: None,
            tags: BTreeMap::new(),
            channels: None,
            channel_layout: None,
            sample_rate: None,
            resolution: None,
            fps: None,
            bitrate: None,
            delay_ms: None,
            duration_ms: None,
            default: false,
            forced: false,
            size_bytes: None,
        }
    }
}

impl StreamInfo {
    pub fn display_name(&self) -> &str {
        match self.title.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => t,
            _ => "Без названия",
        }
    }

    pub fn has_name(&self) -> bool {
        self.title
            .as_deref()
            .map(str::trim)
            .is_some_and(|t| !t.is_empty())
    }

    pub fn language_label(&self) -> String {
        match self.language.as_deref().map(str::trim) {
            Some(code) if !code.is_empty() && code != "und" => match language_name(code) {
                Some(name) => format!("{code} — {name}"),
                None => code.to_uppercase(),
            },
            _ => "не указан".to_string(),
        }
    }

    pub fn codec_display(&self) -> String {
        let raw = self.codec_raw.to_ascii_lowercase();
        let mapped = match raw.as_str() {
            "v_mpeg4/iso/avc" | "h264" | "avc" => Some("H.264"),
            "v_mpegh/iso/hevc" | "hevc" | "h265" => Some("HEVC"),
            "v_av1" | "av1" => Some("AV1"),
            "v_vp9" | "vp9" => Some("VP9"),
            "v_vp8" | "vp8" => Some("VP8"),
            "v_mpeg2" | "mpeg2video" => Some("MPEG-2"),
            "v_mpeg4/iso/asp" | "mpeg4" => Some("MPEG-4"),
            "v_theora" | "theora" => Some("Theora"),
            "a_aac" | "aac" => Some("AAC"),
            "a_ac3" | "ac3" => Some("AC-3"),
            "a_eac3" | "eac3" => Some("E-AC-3"),
            "a_dts" | "dts" => Some("DTS"),
            "a_truehd" | "truehd" => Some("TrueHD"),
            "a_flac" | "flac" => Some("FLAC"),
            "a_opus" | "opus" => Some("Opus"),
            "a_vorbis" | "vorbis" => Some("Vorbis"),
            "a_mp3" | "mp3" => Some("MP3"),
            "a_pcm/int16le" | "a_pcm/int24le" | "a_pcm/float32le" | "pcm_s16le" | "pcm_s24le"
            | "pcm_f32le" => Some("PCM"),
            "s_text/utf8" | "subrip" | "srt" => Some("SRT"),
            "s_text/ass" | "ass" | "ssa" => Some("ASS"),
            "s_text/webvtt" | "webvtt" => Some("WebVTT"),
            "s_hdmv/pgs" | "hdmv_pgs_subtitle" | "pgs" => Some("PGS"),
            "s_vobsub" | "dvd_subtitle" => Some("VobSub"),
            "s_text/mov_text" | "mov_text" => Some("MOV text"),
            _ => None,
        };
        match mapped {
            Some(v) => v.to_string(),
            None if !self.codec.trim().is_empty() => self.codec.clone(),
            None if !raw.is_empty() => raw,
            None => "—".to_string(),
        }
    }

    pub fn channels_label(&self) -> String {
        match self.channels {
            None => String::new(),
            Some(1) => "1.0 (моно)".to_string(),
            Some(2) => "2.0 (стерео)".to_string(),
            Some(6) => "5.1".to_string(),
            Some(8) => "7.1".to_string(),
            Some(n) => format!("{n} кан."),
        }
    }

    pub fn resolution_label(&self) -> String {
        match self.resolution {
            Some((w, h)) if h > 0 => {
                let quality = match h {
                    0..=479 => "SD",
                    480..=719 => "480p",
                    720..=1079 => "720p",
                    1080..=1439 => "1080p",
                    1440..=2159 => "1440p",
                    _ => "4K",
                };
                format!("{w}×{h} ({quality})")
            }
            _ => String::new(),
        }
    }

    pub fn bitrate_label(&self) -> String {
        self.bitrate
            .map(|b| {
                if b >= 1_000_000 {
                    format!("{:.1} Mbps", b as f64 / 1_000_000.0)
                } else {
                    format!("{} kbps", b / 1000)
                }
            })
            .unwrap_or_default()
    }

    pub fn sample_rate_label(&self) -> String {
        self.sample_rate
            .map(|r| format!("{:.1} kHz", r as f64 / 1000.0))
            .unwrap_or_default()
    }

    pub fn fps_label(&self) -> String {
        self.fps.map(|f| format!("{f:.3} fps")).unwrap_or_default()
    }

    /// Размер из контейнера, иначе оценка по битрейту.
    pub fn effective_size(&self, duration_ms: Option<u64>) -> Option<u64> {
        if let Some(size) = self.size_bytes.filter(|s| *s > 0) {
            return Some(size);
        }
        let bitrate = self.bitrate?;
        let duration = self.duration_ms.or(duration_ms)?;
        Some(bitrate.saturating_mul(duration) / 8 / 1000)
    }

    pub fn is_text_subtitle(&self) -> bool {
        let raw = self.codec_raw.to_ascii_lowercase();
        raw.contains("s_text")
            || raw.contains("subrip")
            || raw.contains("ass")
            || raw.contains("ssa")
            || raw.contains("webvtt")
            || raw.contains("mov_text")
            || raw == "srt"
    }

    pub fn is_image_subtitle(&self) -> bool {
        let raw = self.codec_raw.to_ascii_lowercase();
        raw.contains("pgs")
            || raw.contains("vobsub")
            || raw.contains("dvd_subtitle")
            || raw.contains("hdmv")
            || raw.contains("dvb_subtitle")
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Расшифровка кодов ISO 639-1/639-2.
pub fn language_name(code: &str) -> Option<&'static str> {
    let code = code.trim().to_ascii_lowercase();
    Some(match code.as_str() {
        "ru" | "rus" => "Русский",
        "uk" | "ukr" => "Украинский",
        "be" | "bel" => "Белорусский",
        "en" | "eng" => "Английский",
        "de" | "ger" | "deu" => "Немецкий",
        "fr" | "fre" | "fra" => "Французский",
        "es" | "spa" => "Испанский",
        "it" | "ita" => "Итальянский",
        "pt" | "por" => "Португальский",
        "pl" | "pol" => "Польский",
        "cs" | "cze" | "ces" => "Чешский",
        "sk" | "slo" | "slk" => "Словацкий",
        "hu" | "hun" => "Венгерский",
        "ro" | "rum" | "ron" => "Румынский",
        "bg" | "bul" => "Болгарский",
        "sr" | "srp" => "Сербский",
        "hr" | "hrv" => "Хорватский",
        "sl" | "slv" => "Словенский",
        "nl" | "dut" | "nld" => "Нидерландский",
        "sv" | "swe" => "Шведский",
        "no" | "nor" | "nb" | "nob" => "Норвежский",
        "da" | "dan" => "Датский",
        "fi" | "fin" => "Финский",
        "et" | "est" => "Эстонский",
        "lv" | "lav" => "Латышский",
        "lt" | "lit" => "Литовский",
        "el" | "gre" | "ell" => "Греческий",
        "tr" | "tur" => "Турецкий",
        "ar" | "ara" => "Арабский",
        "he" | "heb" => "Иврит",
        "fa" | "per" | "fas" => "Персидский",
        "hi" | "hin" => "Хинди",
        "ja" | "jpn" => "Японский",
        "ko" | "kor" => "Корейский",
        "zh" | "chi" | "zho" => "Китайский",
        "th" | "tha" => "Тайский",
        "vi" | "vie" => "Вьетнамский",
        "id" | "ind" => "Индонезийский",
        "ms" | "may" | "msa" => "Малайский",
        "kk" | "kaz" => "Казахский",
        "uz" | "uzb" => "Узбекский",
        "ka" | "geo" | "kat" => "Грузинский",
        "hy" | "arm" | "hye" => "Армянский",
        "az" | "aze" => "Азербайджанский",
        "la" | "lat" => "Латинский",
        _ => return None,
    })
}

pub fn human_language(code: &str) -> String {
    match language_name(code) {
        Some(name) => name.to_string(),
        None => code.to_uppercase(),
    }
}
