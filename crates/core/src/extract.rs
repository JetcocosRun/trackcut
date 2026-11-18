use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{human_language, Backend, MediaInfo, StreamInfo, StreamKind};
use crate::plan::{part_path, paths_equal, CommandSpec, PreparedCommand};
use crate::tools::ToolPaths;

/// Во что сохраняем отдельную дорожку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractContainer {
    /// Только аудио.
    Mka,
    /// Только субтитры.
    Mks,
    /// Смешанный набор дорожек.
    Mkv,
}

impl ExtractContainer {
    pub fn extension(self) -> &'static str {
        match self {
            ExtractContainer::Mka => "mka",
            ExtractContainer::Mks => "mks",
            ExtractContainer::Mkv => "mkv",
        }
    }

    pub fn label_ru(self) -> &'static str {
        match self {
            ExtractContainer::Mka => "MKA (аудио)",
            ExtractContainer::Mks => "MKS (субтитры)",
            ExtractContainer::Mkv => "MKV (видео и всё остальное)",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "mka" => Some(ExtractContainer::Mka),
            "mks" => Some(ExtractContainer::Mks),
            "mkv" => Some(ExtractContainer::Mkv),
            _ => None,
        }
    }

    /// Подбирает контейнер по типам выбранных дорожек.
    pub fn for_streams(streams: &[&StreamInfo]) -> Self {
        if !streams.is_empty()
            && streams
                .iter()
                .all(|stream| stream.kind == StreamKind::Audio)
        {
            ExtractContainer::Mka
        } else if !streams.is_empty()
            && streams
                .iter()
                .all(|stream| stream.kind == StreamKind::Subtitle)
        {
            ExtractContainer::Mks
        } else {
            ExtractContainer::Mkv
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Индексы дорожек в `MediaInfo.streams`.
    pub tracks: Vec<usize>,
    pub container: ExtractContainer,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
}

impl ExtractPlan {
    pub fn streams<'a>(&'a self, info: &'a MediaInfo) -> Vec<&'a StreamInfo> {
        self.tracks
            .iter()
            .filter_map(|uid| info.streams.get(*uid))
            .collect()
    }
}

pub fn prepare_extract(
    info: &MediaInfo,
    plan: &ExtractPlan,
    tools: &ToolPaths,
) -> Result<PreparedCommand> {
    let mut warnings = validate_extract(info, plan)?;
    let backend = resolve_backend(info, plan, tools, &mut warnings)?;

    let output_final = plan.output.clone();
    let output_part = part_path(&output_final, plan.container.extension());

    if let Some(parent) = output_final.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let (program, args) = match backend {
        Backend::Mkvmerge => (tools.mkvmerge()?, mkvmerge_args(info, plan, &output_part)),
        Backend::Ffmpeg => (tools.ffmpeg()?, ffmpeg_args(info, plan, &output_part)),
    };

    Ok(PreparedCommand {
        spec: CommandSpec {
            backend,
            program,
            args,
            output_part,
            output_final,
        },
        warnings,
    })
}

fn resolve_backend(
    info: &MediaInfo,
    plan: &ExtractPlan,
    tools: &ToolPaths,
    warnings: &mut Vec<String>,
) -> Result<Backend> {
    let all_have_mkv_ids = plan
        .streams(info)
        .iter()
        .all(|stream| stream.mkv_id.is_some());

    let requested = plan.backend.unwrap_or_else(|| {
        if tools.mkvmerge.is_some() && all_have_mkv_ids {
            Backend::Mkvmerge
        } else {
            Backend::Ffmpeg
        }
    });

    match requested {
        Backend::Mkvmerge => {
            if tools.mkvmerge.is_none() {
                if tools.ffmpeg.is_some() {
                    warnings.push("mkvmerge не найден — извлекаю через ffmpeg".to_string());
                    return Ok(Backend::Ffmpeg);
                }
                return Err(Error::ToolMissing("mkvmerge"));
            }
            if !all_have_mkv_ids {
                if tools.ffmpeg.is_some() {
                    warnings.push(
                        "для части дорожек нет ID mkvmerge — извлекаю через ffmpeg".to_string(),
                    );
                    return Ok(Backend::Ffmpeg);
                }
                return Err(Error::InvalidPlan(
                    "не удалось определить ID дорожек для mkvmerge".to_string(),
                ));
            }
            Ok(Backend::Mkvmerge)
        }
        Backend::Ffmpeg => {
            if tools.ffmpeg.is_none() {
                return Err(Error::ToolMissing("ffmpeg"));
            }
            Ok(Backend::Ffmpeg)
        }
    }
}

fn validate_extract(info: &MediaInfo, plan: &ExtractPlan) -> Result<Vec<String>> {
    if plan.tracks.is_empty() {
        return Err(Error::InvalidPlan(
            "не выбрана ни одна дорожка для извлечения".to_string(),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    for uid in &plan.tracks {
        if !seen.insert(*uid) {
            return Err(Error::InvalidPlan(format!(
                "дорожка {} указана дважды",
                uid + 1
            )));
        }
        let stream = info
            .streams
            .get(*uid)
            .ok_or_else(|| Error::InvalidPlan(format!("дорожка с индексом {} не найдена", uid)))?;
        if !stream.kind.is_selectable() {
            return Err(Error::InvalidPlan(format!(
                "дорожку «{}» нельзя извлечь: тип {}",
                stream.display_name(),
                stream.kind.label_ru()
            )));
        }
    }

    if paths_equal(&plan.input, &plan.output) {
        return Err(Error::InvalidPlan(
            "нельзя сохранить дорожку поверх исходного файла".to_string(),
        ));
    }
    if plan.output.exists() && !plan.overwrite {
        return Err(Error::OutputExists(plan.output.clone()));
    }

    let streams = plan.streams(info);
    let has_video = streams.iter().any(|s| s.kind == StreamKind::Video);
    let has_audio = streams.iter().any(|s| s.kind == StreamKind::Audio);
    let has_subs = streams.iter().any(|s| s.kind == StreamKind::Subtitle);

    match plan.container {
        ExtractContainer::Mka if !has_audio || has_video || has_subs => {
            return Err(Error::InvalidPlan(
                "в MKA можно сохранить только аудио. Выберите MKV для смешанного набора дорожек."
                    .to_string(),
            ));
        }
        ExtractContainer::Mks if !has_subs || has_video || has_audio => {
            return Err(Error::InvalidPlan(
                "в MKS можно сохранить только субтитры. Выберите MKV для смешанного набора."
                    .to_string(),
            ));
        }
        _ => {}
    }

    let mut warnings = Vec::new();
    if streams.iter().any(|stream| stream.bitrate.is_none()) {
        warnings
            .push("часть дорожек без битрейта — размер в оценке может быть неточным".to_string());
    }
    Ok(warnings)
}

fn mkvmerge_args(info: &MediaInfo, plan: &ExtractPlan, output_part: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    args.push("--gui-mode".into());
    args.push("-o".into());
    args.push(output_part.into());

    let streams = plan.streams(info);
    let ids_for = |kind: StreamKind| -> Vec<String> {
        streams
            .iter()
            .filter(|stream| stream.kind == kind)
            .filter_map(|stream| stream.mkv_id)
            .map(|id| id.to_string())
            .collect()
    };

    let video = ids_for(StreamKind::Video);
    let audio = ids_for(StreamKind::Audio);
    let subtitles = ids_for(StreamKind::Subtitle);

    if video.is_empty() {
        args.push("-D".into());
    } else {
        args.push("-d".into());
        args.push(video.join(",").into());
    }
    if audio.is_empty() {
        args.push("-A".into());
    } else {
        args.push("-a".into());
        args.push(audio.join(",").into());
    }
    if subtitles.is_empty() {
        args.push("-S".into());
    } else {
        args.push("-s".into());
        args.push(subtitles.join(",").into());
    }
    args.push("-B".into());
    args.push("--no-chapters".into());
    args.push("--no-global-tags".into());
    args.push("--no-attachments".into());

    args.push(info.path.clone().into_os_string());
    args
}

fn ffmpeg_args(info: &MediaInfo, plan: &ExtractPlan, output_part: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    for arg in ["-hide_banner", "-nostdin", "-y", "-i"] {
        args.push(arg.into());
    }
    args.push(info.path.clone().into_os_string());

    for uid in &plan.tracks {
        let Some(stream) = info.streams.get(*uid) else {
            continue;
        };
        let Some(index) = stream.ff_index else {
            continue;
        };
        args.push("-map".into());
        args.push(format!("0:{index}").into());
    }

    args.push("-c".into());
    args.push("copy".into());
    args.push("-map_metadata".into());
    args.push("0".into());
    args.push("-map_chapters".into());
    args.push("-1".into());

    // MKA/MKS — это Matroska с другим расширением; ffmpeg не угадывает его по .mks.
    if plan.container != ExtractContainer::Mkv {
        args.push("-f".into());
        args.push("matroska".into());
    }

    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push("-nostats".into());
    args.push("-loglevel".into());
    args.push("error".into());

    args.push(output_part.into());
    args
}

/// Имя файла по умолчанию: `<исходник>.<язык>.<название>.<ext>`.
pub fn default_track_path(input: &Path, streams: &[&StreamInfo]) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "track".to_string());
    let container = ExtractContainer::for_streams(streams);

    let mut parts: Vec<String> = vec![stem];
    if let Some(first) = streams.first() {
        if let Some(language) = first
            .language
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty() && *code != "und")
        {
            parts.push(sanitize(language));
        }
        if first.has_name() {
            parts.push(sanitize(first.display_name()));
        }
        if streams.len() > 1 {
            parts.push(format!("{} дорожки", streams.len()));
        } else {
            parts.push("дорожка".to_string());
        }
    } else {
        parts.push("дорожка".to_string());
    }

    let mut name = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(".");
    name = truncate_chars(&name, 110);

    let mut candidate = parent.join(format!("{name}.{}", container.extension()));
    let mut index = 1;
    while candidate.exists() {
        candidate = parent.join(format!("{name} ({index}).{}", container.extension()));
        index += 1;
    }
    candidate
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches([' ', '.']).to_string();
    truncate_chars(&trimmed, 60)
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut result: String = value.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

/// Подпись дорожки для интерфейса: «rus — Русский · Дубляж».
pub fn track_caption(stream: &StreamInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(code) = stream
        .language
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty() && *code != "und")
    {
        parts.push(format!("{code} — {}", human_language(code)));
    }
    if stream.has_name() {
        parts.push(stream.display_name().to_string());
    }
    parts.push(stream.codec_display());
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn stream(kind: StreamKind, mkv_id: u32, ff_index: usize, codec: &str) -> StreamInfo {
        StreamInfo {
            kind,
            mkv_id: Some(mkv_id),
            ff_index: Some(ff_index),
            codec: codec.to_string(),
            codec_raw: codec.to_string(),
            tags: BTreeMap::new(),
            ..Default::default()
        }
    }

    fn info() -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("movie.mkv"),
            duration_ms: Some(60_000),
            streams: vec![
                stream(StreamKind::Video, 0, 0, "V_MPEGH/ISO/HEVC"),
                stream(StreamKind::Audio, 1, 1, "A_AC3"),
                stream(StreamKind::Subtitle, 2, 2, "S_TEXT/UTF8"),
            ],
            ..Default::default()
        }
    }

    fn tools() -> ToolPaths {
        ToolPaths {
            ffmpeg: Some(PathBuf::from("ffmpeg")),
            ffprobe: Some(PathBuf::from("ffprobe")),
            mkvmerge: Some(PathBuf::from("mkvmerge")),
        }
    }

    fn line(prepared: &PreparedCommand) -> String {
        prepared
            .spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn plan(container: ExtractContainer, tracks: Vec<usize>) -> ExtractPlan {
        ExtractPlan {
            input: PathBuf::from("movie.mkv"),
            output: PathBuf::from(format!("track.{}", container.extension())),
            tracks,
            container,
            overwrite: true,
            backend: None,
        }
    }

    #[test]
    fn mka_keeps_only_audio() {
        let prepared = prepare_extract(&info(), &plan(ExtractContainer::Mka, vec![1]), &tools())
            .expect("план извлечения");
        assert_eq!(prepared.spec.backend, Backend::Mkvmerge);
        let line = line(&prepared);
        assert!(line.contains("-D"), "{line}");
        assert!(line.contains("-a 1"), "{line}");
        assert!(line.contains("-S"), "{line}");
        assert!(line.contains("-B"), "{line}");
        assert!(line.contains("--no-chapters"), "{line}");
    }

    #[test]
    fn mka_rejects_video() {
        let err = prepare_extract(&info(), &plan(ExtractContainer::Mka, vec![0]), &tools())
            .expect_err("видео в MKA не помещается");
        assert!(matches!(err, Error::InvalidPlan(_)), "{err:?}");
    }

    #[test]
    fn ffmpeg_extract_forces_matroska_format() {
        let mut plan = plan(ExtractContainer::Mks, vec![2]);
        plan.backend = Some(Backend::Ffmpeg);
        let prepared = prepare_extract(&info(), &plan, &tools()).expect("ffmpeg-план");
        let line = line(&prepared);
        assert!(line.contains("-map 0:2"), "{line}");
        assert!(line.contains("-c copy"), "{line}");
        assert!(line.contains("-f matroska"), "{line}");
    }

    #[test]
    fn container_is_inferred_from_kinds() {
        let info = info();
        assert_eq!(
            ExtractContainer::for_streams(&[&info.streams[1]]),
            ExtractContainer::Mka
        );
        assert_eq!(
            ExtractContainer::for_streams(&[&info.streams[2]]),
            ExtractContainer::Mks
        );
        assert_eq!(
            ExtractContainer::for_streams(&[&info.streams[0], &info.streams[1]]),
            ExtractContainer::Mkv
        );
    }

    #[test]
    fn default_path_uses_language_and_title() {
        let info = MediaInfo {
            path: PathBuf::from("C:/films/Movie.2026.mkv"),
            streams: vec![StreamInfo {
                kind: StreamKind::Audio,
                language: Some("rus".to_string()),
                title: Some("Дубляж".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let path = default_track_path(&info.path, &[&info.streams[0]]);
        assert!(
            path.file_name().unwrap().to_string_lossy().contains("rus"),
            "{}",
            path.display()
        );
        assert!(path.extension().unwrap().eq_ignore_ascii_case("mka"));
    }
}
