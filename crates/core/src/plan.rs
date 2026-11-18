use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{Backend, MediaInfo, OutputContainer, StreamInfo, StreamKind};
use crate::tools::ToolPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepTrack {
    /// Индекс дорожки в `MediaInfo.streams`.
    pub uid: usize,
    /// Переопределить флаг Default в выходном файле.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    /// Переопределить флаг Forced в выходном файле.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced: Option<bool>,
}

impl KeepTrack {
    pub fn new(uid: usize) -> Self {
        Self {
            uid,
            default: None,
            forced: None,
        }
    }
}

/// Дорожка, которую берут из другого файла.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraTrack {
    /// Индекс дорожки в `ExtraInput::info.streams`.
    pub uid: usize,
    /// Сдвиг дорожки в миллисекундах: положительный — позже, отрицательный — раньше.
    #[serde(default)]
    pub delay_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraInput {
    /// Метаданные файла-источника (mka/mks/mkv или целый фильм).
    pub info: MediaInfo,
    pub tracks: Vec<ExtraTrack>,
}

impl ExtraInput {
    pub fn streams(&self) -> impl Iterator<Item = &StreamInfo> {
        self.tracks
            .iter()
            .filter_map(|track| self.info.streams.get(track.uid))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    pub container: OutputContainer,
    pub keep: Vec<KeepTrack>,
    pub keep_chapters: bool,
    pub keep_tags: bool,
    pub keep_attachments: bool,
    #[serde(default)]
    pub convert_subtitles_to_mov_text: bool,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Файлы-источники дополнительных дорожек.
    #[serde(default)]
    pub extras: Vec<ExtraInput>,
}

impl MuxPlan {
    pub fn keep_all(info: &MediaInfo, output: PathBuf, container: OutputContainer) -> Self {
        Self {
            input: info.path.clone(),
            output,
            container,
            keep: info
                .streams
                .iter()
                .enumerate()
                .filter(|(_, stream)| stream.kind.is_selectable())
                .map(|(uid, _)| KeepTrack::new(uid))
                .collect(),
            keep_chapters: true,
            keep_tags: true,
            keep_attachments: true,
            convert_subtitles_to_mov_text: false,
            overwrite: false,
            backend: None,
            extras: Vec::new(),
        }
    }

    pub fn kept_streams<'a>(&'a self, info: &'a MediaInfo) -> Vec<&'a StreamInfo> {
        self.keep
            .iter()
            .filter_map(|entry| info.streams.get(entry.uid))
            .collect()
    }

    /// Все дорожки из внешних файлов.
    pub fn extra_streams(&self) -> Vec<&StreamInfo> {
        self.extras.iter().flat_map(ExtraInput::streams).collect()
    }

    pub fn has_extras(&self) -> bool {
        self.extras.iter().any(|extra| !extra.tracks.is_empty())
    }

    pub fn is_kept(&self, uid: usize) -> bool {
        self.keep.iter().any(|entry| entry.uid == uid)
    }
}

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub backend: Backend,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub output_part: PathBuf,
    pub output_final: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PreparedCommand {
    pub spec: CommandSpec,
    pub warnings: Vec<String>,
}

pub fn prepare(info: &MediaInfo, plan: &MuxPlan, tools: &ToolPaths) -> Result<PreparedCommand> {
    let mut warnings = validate(info, plan)?;
    let backend = resolve_backend(info, plan, tools, &mut warnings)?;

    let output_final = plan.output.clone();
    let output_part = part_path(&output_final, plan.container.extension());

    if let Some(parent) = output_final.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let spec = match backend {
        Backend::Mkvmerge => {
            let program = tools.mkvmerge()?;
            let args = mkvmerge_args(info, plan, &output_part);
            CommandSpec {
                backend,
                program,
                args,
                output_part,
                output_final,
            }
        }
        Backend::Ffmpeg => {
            let program = tools.ffmpeg()?;
            let args = ffmpeg_args(info, plan, &output_part);
            CommandSpec {
                backend,
                program,
                args,
                output_part,
                output_final,
            }
        }
    };

    Ok(PreparedCommand { spec, warnings })
}

fn resolve_backend(
    info: &MediaInfo,
    plan: &MuxPlan,
    tools: &ToolPaths,
    warnings: &mut Vec<String>,
) -> Result<Backend> {
    let all_have_mkv_ids = plan
        .keep
        .iter()
        .filter_map(|entry| info.streams.get(entry.uid))
        .chain(plan.extra_streams())
        .all(|stream| stream.mkv_id.is_some());

    let requested = plan.backend.unwrap_or_else(|| {
        if plan.container.supports_mkvmerge()
            && tools.mkvmerge.is_some()
            && all_have_mkv_ids
            && !info.has_buttons()
        {
            Backend::Mkvmerge
        } else {
            Backend::Ffmpeg
        }
    });

    match requested {
        Backend::Mkvmerge => {
            if tools.mkvmerge.is_none() {
                if tools.ffmpeg.is_some() {
                    warnings.push(
                        "mkvmerge не найден — использую ffmpeg (вложения и часть тегов могут не сохраниться)"
                            .to_string(),
                    );
                    return Ok(Backend::Ffmpeg);
                }
                return Err(Error::ToolMissing("mkvmerge"));
            }
            if !all_have_mkv_ids {
                if tools.ffmpeg.is_some() {
                    warnings.push(
                        "для части дорожек нет ID mkvmerge — переключаюсь на ffmpeg".to_string(),
                    );
                    return Ok(Backend::Ffmpeg);
                }
                return Err(Error::InvalidPlan(
                    "не удалось определить ID дорожек для mkvmerge".to_string(),
                ));
            }
            if info.has_buttons() {
                warnings.push("кнопки (menu buttons) не переносятся".to_string());
            }
            Ok(Backend::Mkvmerge)
        }
        Backend::Ffmpeg => {
            if tools.ffmpeg.is_none() {
                return Err(Error::ToolMissing("ffmpeg"));
            }
            if plan.keep_attachments && !info.attachments.is_empty() {
                warnings.push(
                    "ffmpeg не переносит вложения (шрифты для ASS): выберите MKV + mkvmerge, чтобы их сохранить"
                        .to_string(),
                );
            }
            Ok(Backend::Ffmpeg)
        }
    }
}

/// Возвращает предупреждения или ошибку, если план невыполним.
pub fn validate(info: &MediaInfo, plan: &MuxPlan) -> Result<Vec<String>> {
    if plan.keep.is_empty() {
        return Err(Error::InvalidPlan(
            "не выбрано ни одной дорожки — удалять нечего".to_string(),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    for entry in &plan.keep {
        if !seen.insert(entry.uid) {
            return Err(Error::InvalidPlan(format!(
                "дорожка {} указана дважды",
                entry.uid + 1
            )));
        }
        let stream = info.streams.get(entry.uid).ok_or_else(|| {
            Error::InvalidPlan(format!("дорожка с индексом {} не найдена", entry.uid))
        })?;
        if !stream.kind.is_selectable() {
            return Err(Error::InvalidPlan(format!(
                "дорожку «{}» нельзя оставить: тип {}",
                stream.display_name(),
                stream.kind.label_ru()
            )));
        }
    }

    let same_path = paths_equal(&info.path, &plan.output) || paths_equal(&plan.input, &plan.output);
    if same_path {
        return Err(Error::InvalidPlan(
            "выходной файл совпадает с исходным — исходник перезаписывать нельзя".to_string(),
        ));
    }

    if plan.output.exists() && !plan.overwrite {
        return Err(Error::OutputExists(plan.output.clone()));
    }

    let mut warnings = Vec::new();
    validate_extras(plan, &mut warnings)?;

    let mut all_streams: Vec<&StreamInfo> = plan.kept_streams(info);
    all_streams.extend(plan.extra_streams());

    if !all_streams.iter().any(|s| s.kind == StreamKind::Video) {
        warnings.push("в выходе не останется видео".to_string());
    }
    if !all_streams.iter().any(|s| s.kind == StreamKind::Audio) {
        warnings.push("в выходе не останется аудио".to_string());
    }

    check_container_compatibility(info, plan, &all_streams, &mut warnings)?;
    Ok(warnings)
}

/// Проверяет файлы-источники дополнительных дорожек.
fn validate_extras(plan: &MuxPlan, warnings: &mut Vec<String>) -> Result<()> {
    if plan.extras.is_empty() {
        return Ok(());
    }

    let mut seen_paths: Vec<&Path> = Vec::new();
    for extra in &plan.extras {
        if extra.tracks.is_empty() {
            continue;
        }
        if paths_equal(&extra.info.path, &plan.output) {
            return Err(Error::InvalidPlan(
                "выходной файл совпадает с файлом-источником внешней дорожки".to_string(),
            ));
        }
        if seen_paths
            .iter()
            .any(|path| paths_equal(path, &extra.info.path))
        {
            return Err(Error::InvalidPlan(format!(
                "файл «{}» добавлен дважды",
                extra.info.path.display()
            )));
        }
        seen_paths.push(&extra.info.path);

        let mut seen_uids = std::collections::HashSet::new();
        for track in &extra.tracks {
            if !seen_uids.insert(track.uid) {
                return Err(Error::InvalidPlan(format!(
                    "дорожка {} из «{}» указана дважды",
                    track.uid + 1,
                    extra.info.path.display()
                )));
            }
            let stream = extra.info.streams.get(track.uid).ok_or_else(|| {
                Error::InvalidPlan(format!(
                    "в «{}» нет дорожки с индексом {}",
                    extra.info.path.display(),
                    track.uid
                ))
            })?;
            if !stream.kind.is_selectable() {
                return Err(Error::InvalidPlan(format!(
                    "из «{}» нельзя взять дорожку «{}»: тип {}",
                    extra.info.path.display(),
                    stream.display_name(),
                    stream.kind.label_ru()
                )));
            }
        }

        let delays: std::collections::BTreeSet<i64> =
            extra.tracks.iter().map(|track| track.delay_ms).collect();
        if delays.len() > 1 {
            warnings.push(format!(
                "в «{}» разные сдвиги у дорожек — для ffmpeg применится первый ({:+} мс)",
                extra.info.path.display(),
                extra.tracks[0].delay_ms
            ));
        }

        if !extra.info.attachments.is_empty() && plan.keep_attachments {
            warnings.push(format!(
                "вложения из «{}» не переносятся — берутся только дорожки",
                extra.info.path.display()
            ));
        }
    }

    Ok(())
}

fn check_container_compatibility(
    info: &MediaInfo,
    plan: &MuxPlan,
    kept: &[&StreamInfo],
    warnings: &mut Vec<String>,
) -> Result<()> {
    if plan.container == OutputContainer::Webm {
        let bad: Vec<String> = kept
            .iter()
            .filter(|stream| !webm_compatible(stream))
            .map(|stream| {
                format!(
                    "{} «{}» ({})",
                    stream.kind.label_ru(),
                    stream.display_name(),
                    stream.codec_display()
                )
            })
            .collect();
        if !bad.is_empty() {
            return Err(Error::InvalidPlan(format!(
                "WebM не поддерживает: {}. Выберите MKV — он принимает любые кодеки без перекодирования.",
                bad.join(", ")
            )));
        }
    }

    if plan.container.is_mp4_family() {
        let image_subs: Vec<&StreamInfo> = kept
            .iter()
            .copied()
            .filter(|stream| stream.kind == StreamKind::Subtitle && stream.is_image_subtitle())
            .collect();
        if !image_subs.is_empty() {
            return Err(Error::InvalidPlan(
                "MP4/MOV не поддерживает графические субтитры (PGS/VobSub). Удалите их или выберите MKV."
                    .to_string(),
            ));
        }

        let text_subs: Vec<&StreamInfo> = kept
            .iter()
            .copied()
            .filter(|stream| stream.kind == StreamKind::Subtitle && stream.is_text_subtitle())
            .collect();
        if !text_subs.is_empty() {
            if plan.convert_subtitles_to_mov_text {
                warnings.push(
                    "субтитры будут сконвертированы в MOV text — это единственное исключение из правила «без перекодирования»"
                        .to_string(),
                );
            } else {
                return Err(Error::InvalidPlan(
                    "MP4/MOV не хранит SRT/ASS напрямую: включите «конвертировать субтитры в MOV text» или удалите субтитры"
                        .to_string(),
                ));
            }
        }
    }

    if info.duration_ms.is_none() {
        warnings.push("не удалось определить длительность файла".to_string());
    }

    Ok(())
}

fn webm_compatible(stream: &StreamInfo) -> bool {
    let raw = stream.codec_raw.to_ascii_lowercase();
    match stream.kind {
        StreamKind::Video => raw.contains("vp8") || raw.contains("vp9") || raw.contains("av1"),
        StreamKind::Audio => raw.contains("opus") || raw.contains("vorbis"),
        StreamKind::Subtitle => raw.contains("webvtt"),
        _ => false,
    }
}

fn mkvmerge_args(info: &MediaInfo, plan: &MuxPlan, output_part: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    args.push("--gui-mode".into());
    args.push("-o".into());
    args.push(output_part.into());

    let kept = plan.kept_streams(info);
    let ids_for = |kind: StreamKind| -> Vec<String> {
        kept.iter()
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

    if !plan.keep_chapters {
        args.push("--no-chapters".into());
    }
    if !plan.keep_tags {
        args.push("--no-global-tags".into());
        args.push("--no-track-tags".into());
    }
    if !plan.keep_attachments {
        args.push("--no-attachments".into());
    }

    for entry in &plan.keep {
        let Some(stream) = info.streams.get(entry.uid) else {
            continue;
        };
        let Some(id) = stream.mkv_id else {
            continue;
        };
        if let Some(default) = entry.default {
            args.push("--default-track".into());
            args.push(format!("{id}:{}", yes_no(default)).into());
        }
        if let Some(forced) = entry.forced {
            args.push("--forced-track".into());
            args.push(format!("{id}:{}", yes_no(forced)).into());
        }
    }

    args.push(info.path.clone().into_os_string());

    for extra in &plan.extras {
        if extra.tracks.is_empty() {
            continue;
        }
        let selected: Vec<&StreamInfo> = extra.streams().collect();
        let ids_for = |kind: StreamKind| -> Vec<String> {
            selected
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
        args.push("--no-track-tags".into());
        args.push("--no-attachments".into());

        for track in &extra.tracks {
            let Some(stream) = extra.info.streams.get(track.uid) else {
                continue;
            };
            let Some(id) = stream.mkv_id else {
                continue;
            };
            if track.delay_ms != 0 {
                args.push("--sync".into());
                args.push(format!("{id}:{}", track.delay_ms).into());
            }
            if let Some(default) = track.default {
                args.push("--default-track".into());
                args.push(format!("{id}:{}", yes_no(default)).into());
            }
            if let Some(forced) = track.forced {
                args.push("--forced-track".into());
                args.push(format!("{id}:{}", yes_no(forced)).into());
            }
        }

        args.push(extra.info.path.clone().into_os_string());
    }

    args
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn ffmpeg_args(info: &MediaInfo, plan: &MuxPlan, output_part: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    for arg in ["-hide_banner", "-nostdin", "-y", "-i"] {
        args.push(arg.into());
    }
    args.push(info.path.clone().into_os_string());

    let extras: Vec<&ExtraInput> = plan
        .extras
        .iter()
        .filter(|extra| !extra.tracks.is_empty())
        .collect();

    for extra in &extras {
        // ffmpeg задаёт сдвиг на весь вход, поэтому берём первый ненулевой.
        if let Some(delay) = extra
            .tracks
            .iter()
            .map(|track| track.delay_ms)
            .find(|d| *d != 0)
        {
            args.push("-itsoffset".into());
            args.push(format!("{:.3}", delay as f64 / 1000.0).into());
        }
        args.push("-i".into());
        args.push(extra.info.path.clone().into_os_string());
    }

    let mut has_text_subtitles = false;
    for entry in &plan.keep {
        let Some(stream) = info.streams.get(entry.uid) else {
            continue;
        };
        if stream.kind == StreamKind::Subtitle && stream.is_text_subtitle() {
            has_text_subtitles = true;
        }
        let Some(index) = stream.ff_index else {
            continue;
        };
        args.push("-map".into());
        args.push(format!("0:{index}").into());
    }

    for (position, extra) in extras.iter().enumerate() {
        let input_index = position + 1;
        for track in &extra.tracks {
            let Some(stream) = extra.info.streams.get(track.uid) else {
                continue;
            };
            if stream.kind == StreamKind::Subtitle && stream.is_text_subtitle() {
                has_text_subtitles = true;
            }
            let Some(index) = stream.ff_index else {
                continue;
            };
            args.push("-map".into());
            args.push(format!("{input_index}:{index}").into());
        }
    }

    args.push("-c".into());
    args.push("copy".into());

    if plan.convert_subtitles_to_mov_text && has_text_subtitles && plan.container.is_mp4_family() {
        args.push("-c:s".into());
        args.push("mov_text".into());
    }

    args.push("-map_metadata".into());
    args.push(if plan.keep_tags { "0" } else { "-1" }.into());

    args.push("-map_chapters".into());
    let copy_chapters = plan.keep_chapters && info.chapters > 0;
    args.push(if copy_chapters { "0" } else { "-1" }.into());

    if plan.container.is_mp4_family() {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }

    args.push("-progress".into());
    args.push("pipe:1".into());
    args.push("-nostats".into());
    args.push("-loglevel".into());
    args.push("error".into());

    args.push(output_part.into());
    args
}

pub(crate) fn part_path(output: &Path, extension: &str) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "output".to_string());
    parent.join(format!("{stem}.part.{extension}"))
}

pub fn paths_equal(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let cleaned = path_clean(&absolute);
        #[cfg(windows)]
        {
            cleaned.to_string_lossy().to_lowercase()
        }
        #[cfg(not(windows))]
        {
            cleaned.to_string_lossy().into_owned()
        }
    };
    normalize(a) == normalize(b)
}

fn path_clean(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Имя выходного файла по умолчанию: `<stem>_clean.<ext>`.
pub fn default_output_path(input: &Path, container: OutputContainer, suffix: &str) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    let mut candidate = parent.join(format!("{stem}{suffix}.{}", container.extension()));
    let mut index = 1;
    while candidate.exists() {
        candidate = parent.join(format!(
            "{stem}{suffix} ({index}).{}",
            container.extension()
        ));
        index += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StreamKind;
    use std::collections::BTreeMap;

    fn tools() -> ToolPaths {
        ToolPaths {
            ffmpeg: Some(PathBuf::from("ffmpeg")),
            ffprobe: Some(PathBuf::from("ffprobe")),
            mkvmerge: Some(PathBuf::from("mkvmerge")),
        }
    }

    fn stream(
        kind: StreamKind,
        mkv_id: u32,
        ff_index: usize,
        codec_raw: &str,
        language: &str,
    ) -> StreamInfo {
        StreamInfo {
            kind,
            mkv_id: Some(mkv_id),
            ff_index: Some(ff_index),
            codec: codec_raw.to_string(),
            codec_raw: codec_raw.to_string(),
            language: Some(language.to_string()),
            title: Some(format!("{language} track")),
            tags: BTreeMap::new(),
            channels: (kind == StreamKind::Audio).then_some(2),
            channel_layout: None,
            sample_rate: (kind == StreamKind::Audio).then_some(48_000),
            resolution: (kind == StreamKind::Video).then_some((1920, 1080)),
            fps: (kind == StreamKind::Video).then_some(23.976),
            bitrate: Some(4_000_000),
            delay_ms: None,
            duration_ms: None,
            default: mkv_id == 0,
            forced: false,
            size_bytes: None,
        }
    }

    fn sample_info() -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("input.mkv"),
            container: "Matroska".to_string(),
            duration_ms: Some(3_600_000),
            streams: vec![
                stream(StreamKind::Video, 0, 0, "V_MPEGH/ISO/HEVC", "und"),
                stream(StreamKind::Audio, 1, 1, "A_AAC", "rus"),
                stream(StreamKind::Audio, 2, 2, "A_AC3", "eng"),
                stream(StreamKind::Subtitle, 3, 3, "S_TEXT/UTF8", "rus"),
            ],
            chapters: 3,
            attachments: Vec::new(),
            tags: BTreeMap::new(),
        }
    }

    fn plan(container: OutputContainer, keep: Vec<usize>) -> MuxPlan {
        MuxPlan {
            input: PathBuf::from("input.mkv"),
            output: PathBuf::from("output.mkv"),
            container,
            keep: keep.into_iter().map(KeepTrack::new).collect(),
            keep_chapters: true,
            keep_tags: true,
            keep_attachments: true,
            convert_subtitles_to_mov_text: false,
            overwrite: true,
            backend: None,
            extras: Vec::new(),
        }
    }

    fn command_line(prepared: &PreparedCommand) -> String {
        prepared
            .spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn mkvmerge_keeps_only_selected_tracks() {
        let info = sample_info();
        let plan = plan(OutputContainer::Mkv, vec![0, 1]);
        let prepared = prepare(&info, &plan, &tools()).unwrap();
        assert_eq!(prepared.spec.backend, Backend::Mkvmerge);
        let line = command_line(&prepared);
        assert!(line.contains("-d 0"), "{line}");
        assert!(line.contains("-a 1"), "{line}");
        assert!(line.contains("-S"), "{line}");
        assert!(line.contains("-B"), "{line}");
        assert!(line.contains("input.mkv"), "{line}");
        assert!(prepared.spec.output_part.ends_with("output.part.mkv"));
    }

    #[test]
    fn mkvmerge_applies_default_and_forced_overrides() {
        let info = sample_info();
        let mut plan = plan(OutputContainer::Mkv, vec![0, 1]);
        plan.keep[1].default = Some(false);
        plan.keep[1].forced = Some(true);
        let line = command_line(&prepare(&info, &plan, &tools()).unwrap());
        assert!(line.contains("--default-track 1:no"), "{line}");
        assert!(line.contains("--forced-track 1:yes"), "{line}");
    }

    #[test]
    fn ffmpeg_maps_global_stream_indices() {
        let info = sample_info();
        let mut plan = plan(OutputContainer::Mp4, vec![0, 2]);
        plan.backend = Some(Backend::Ffmpeg);
        let prepared = prepare(&info, &plan, &tools()).unwrap();
        let line = command_line(&prepared);
        assert_eq!(prepared.spec.backend, Backend::Ffmpeg);
        assert!(line.contains("-map 0:0"), "{line}");
        assert!(line.contains("-map 0:2"), "{line}");
        assert!(line.contains("-c copy"), "{line}");
        assert!(line.contains("-movflags +faststart"), "{line}");
        assert!(!line.contains("-map 0:1"), "{line}");
    }

    #[test]
    fn mp4_text_subtitles_require_mov_text_or_blocking() {
        let info = sample_info();
        let blocked_plan = plan(OutputContainer::Mp4, vec![0, 3]);
        let err = prepare(&info, &blocked_plan, &tools()).unwrap_err();
        assert!(matches!(err, Error::InvalidPlan(_)), "{err:?}");

        let mut converted = plan(OutputContainer::Mp4, vec![0, 3]);
        converted.convert_subtitles_to_mov_text = true;
        converted.backend = Some(Backend::Ffmpeg);
        let line = command_line(&prepare(&info, &converted, &tools()).unwrap());
        assert!(line.contains("-c:s mov_text"), "{line}");
    }

    #[test]
    fn webm_rejects_incompatible_codecs() {
        let info = sample_info();
        let plan = plan(OutputContainer::Webm, vec![0, 1]);
        let err = prepare(&info, &plan, &tools()).unwrap_err();
        match err {
            Error::InvalidPlan(message) => assert!(message.contains("WebM"), "{message}"),
            other => panic!("ожидалась InvalidPlan, получено {other:?}"),
        }
    }

    #[test]
    fn empty_selection_is_rejected() {
        let info = sample_info();
        let plan = plan(OutputContainer::Mkv, vec![]);
        assert!(prepare(&info, &plan, &tools()).is_err());
    }

    #[test]
    fn output_path_must_differ_from_input() {
        let info = sample_info();
        let mut plan = plan(OutputContainer::Mkv, vec![0]);
        plan.input = PathBuf::from("video.mkv");
        plan.output = PathBuf::from("video.mkv");
        let err = prepare(&info, &plan, &tools()).unwrap_err();
        assert!(matches!(err, Error::InvalidPlan(_)), "{err:?}");
    }

    fn dub_extra(delay_ms: i64) -> ExtraInput {
        ExtraInput {
            info: MediaInfo {
                path: PathBuf::from("dub.mka"),
                duration_ms: Some(3_600_000),
                streams: vec![stream(StreamKind::Audio, 0, 0, "A_AC3", "rus")],
                ..Default::default()
            },
            tracks: vec![ExtraTrack {
                uid: 0,
                delay_ms,
                default: Some(true),
                forced: None,
            }],
        }
    }

    #[test]
    fn mkvmerge_attaches_tracks_from_another_file() {
        let info = sample_info();
        let mut plan = plan(OutputContainer::Mkv, vec![0]);
        plan.extras.push(dub_extra(250));
        let prepared = prepare(&info, &plan, &tools()).unwrap();
        let line = command_line(&prepared);
        assert_eq!(prepared.spec.backend, Backend::Mkvmerge);
        assert!(line.contains("-a 0"), "{line}");
        assert!(line.contains("--sync 0:250"), "{line}");
        assert!(line.contains("--default-track 0:yes"), "{line}");
        assert!(line.contains("--no-chapters"), "{line}");
        assert!(line.contains("dub.mka"), "{line}");
    }

    #[test]
    fn ffmpeg_maps_extra_inputs() {
        let info = sample_info();
        let mut plan = plan(OutputContainer::Mp4, vec![0]);
        plan.backend = Some(Backend::Ffmpeg);
        plan.extras.push(dub_extra(-120));
        let prepared = prepare(&info, &plan, &tools()).unwrap();
        let line = command_line(&prepared);
        assert!(line.contains("-itsoffset -0.120"), "{line}");
        assert!(line.contains("-i dub.mka"), "{line}");
        assert!(line.contains("-map 1:0"), "{line}");
    }

    #[test]
    fn output_must_differ_from_extra_source() {
        let info = sample_info();
        let mut plan = plan(OutputContainer::Mkv, vec![0]);
        plan.extras.push(dub_extra(0));
        plan.output = PathBuf::from("dub.mka");
        let err = prepare(&info, &plan, &tools()).unwrap_err();
        assert!(matches!(err, Error::InvalidPlan(_)), "{err:?}");
    }
}
