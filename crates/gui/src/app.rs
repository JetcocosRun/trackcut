use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align, Color32, Layout, RichText};
use trackcut_core::extract::{default_track_path, ExtractContainer, ExtractPlan};
use trackcut_core::model::{format_duration, format_size, OutputContainer, StreamInfo, StreamKind};
use trackcut_core::plan::{parse_ranges, paths_equal, validate, SplitSpec};
use trackcut_core::{
    estimate_delay, probe, run_extract, run_mux, Error, ExecEvent, ExtraInput, ExtraTrack,
    KeepTrack, MediaInfo, MuxPlan, OffsetEstimate, ToolPaths,
};

use crate::i18n::{save_lang, tr, Lang};
use crate::preview::{
    request_frame, start_pair_preview, start_preview, start_track_preview, start_video_segment,
    FrameMsg, PreviewMsg, PreviewState, VideoFrame,
};

const ACCENT: Color32 = Color32::from_rgb(96, 165, 250);
const GREEN: Color32 = Color32::from_rgb(74, 222, 128);
const ORANGE: Color32 = Color32::from_rgb(251, 191, 36);
const RED: Color32 = Color32::from_rgb(248, 113, 113);
const MUTED: Color32 = Color32::from_rgb(148, 163, 184);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContainerChoice {
    Auto,
    Mkv,
    Webm,
    Mp4,
    Mov,
}

impl ContainerChoice {
    fn resolve(self, input: &Path) -> OutputContainer {
        match self {
            ContainerChoice::Auto => input
                .extension()
                .and_then(|ext| OutputContainer::from_extension(&ext.to_string_lossy()))
                .unwrap_or(OutputContainer::Mkv),
            ContainerChoice::Mkv => OutputContainer::Mkv,
            ContainerChoice::Webm => OutputContainer::Webm,
            ContainerChoice::Mp4 => OutputContainer::Mp4,
            ContainerChoice::Mov => OutputContainer::Mov,
        }
    }
}

#[derive(Clone, Copy)]
enum Preset {
    BestAudio,
    VideoAudio,
    Everything,
    RussianOnly,
    Nothing,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SplitChoice {
    /// Не резать.
    #[default]
    None,
    /// Каждые N минут.
    Every,
    /// Диапазоны вида «00:10:00-00:20:00, 00:30:00-».
    Ranges,
}

impl SplitChoice {
    fn label(self, lang: Lang) -> &'static str {
        match self {
            SplitChoice::None => tr(lang, "Не резать"),
            SplitChoice::Every => tr(lang, "Каждые N минут"),
            SplitChoice::Ranges => tr(lang, "Диапазоны"),
        }
    }
}

#[derive(Clone)]
struct ExtraTrackUi {
    uid: usize,
    delay_ms: i64,
    default: bool,
    /// Идёт подбор сдвига по звуку.
    syncing: bool,
    /// Результат подбора: «совпало 3/3 окон» или текст ошибки.
    sync_note: Option<String>,
}

#[derive(Clone)]
struct ExtraSource {
    info: MediaInfo,
    tracks: Vec<ExtraTrackUi>,
}

impl ExtraSource {
    fn name(&self) -> String {
        self.info
            .path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.info.path.display().to_string())
    }
}

struct ExtraPick {
    path: PathBuf,
    info: Option<MediaInfo>,
    error: Option<String>,
    selected: Vec<bool>,
}

struct FileEntry {
    path: PathBuf,
    info: Option<MediaInfo>,
    error: Option<String>,
    probing: bool,
    include: bool,
    keep: Vec<bool>,
    default_flag: Vec<bool>,
    forced_flag: Vec<bool>,
    keep_delay: Vec<i64>,
    extras: Vec<ExtraSource>,
    chapters_file: Option<PathBuf>,
    chapters_label: Option<String>,
    append: Vec<PathBuf>,
    file_size: u64,
    result: Option<String>,
}

impl FileEntry {
    fn new(path: PathBuf) -> Self {
        let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self {
            path,
            info: None,
            error: None,
            probing: true,
            include: true,
            keep: Vec::new(),
            default_flag: Vec::new(),
            forced_flag: Vec::new(),
            keep_delay: Vec::new(),
            extras: Vec::new(),
            chapters_file: None,
            chapters_label: None,
            append: Vec::new(),
            file_size,
            result: None,
        }
    }

    fn reset_flags(&mut self) {
        let Some(info) = &self.info else { return };
        self.keep = info
            .streams
            .iter()
            .map(|stream| stream.kind.is_selectable())
            .collect();
        self.default_flag = info.streams.iter().map(|stream| stream.default).collect();
        self.forced_flag = info.streams.iter().map(|stream| stream.forced).collect();
        self.keep_delay = vec![0; info.streams.len()];
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    fn kept_count(&self) -> usize {
        self.keep.iter().filter(|value| **value).count()
    }
}

enum ProbeMsg {
    Done(usize, Result<MediaInfo, String>),
    ExtraPick(Result<MediaInfo, String>),
    SyncDone {
        source: usize,
        uid: usize,
        internal: bool,
        result: Result<OffsetEstimate, String>,
    },
}

enum JobMsg {
    FileStarted {
        index: usize,
        total: usize,
        name: String,
    },
    Progress(f32),
    Warning(String),
    Message(String),
    FileDone {
        file_index: usize,
        ok: bool,
        message: String,
        output: Option<String>,
        mark_result: bool,
    },
    AllDone {
        ok: usize,
        failed: usize,
    },
}

enum JobState {
    Idle,
    Running {
        index: usize,
        total: usize,
        name: String,
        percent: f32,
    },
    Done {
        ok: usize,
        failed: usize,
    },
}

impl Clone for JobState {
    fn clone(&self) -> Self {
        match self {
            JobState::Idle => JobState::Idle,
            JobState::Running {
                index,
                total,
                name,
                percent,
            } => JobState::Running {
                index: *index,
                total: *total,
                name: name.clone(),
                percent: *percent,
            },
            JobState::Done { ok, failed } => JobState::Done {
                ok: *ok,
                failed: *failed,
            },
        }
    }
}

enum JobOp {
    Mux { info: MediaInfo, plan: MuxPlan },
    Extract { info: MediaInfo, plan: ExtractPlan },
}

struct Job {
    file_index: usize,
    label: String,
    op: JobOp,
}

/// Окно синхрона: кадр из видео, обе дорожки и подгонка сдвига.
struct SyncWindow {
    source: usize,
    uid: usize,
    internal: bool,
    time_ms: u64,
    delay_ms: i64,
    texture: Option<egui::TextureHandle>,
    image_size: [f32; 2],
    frame_pending: bool,
    frame_token: u64,
    frame_for: Option<u64>,
    playing: bool,
    stop: Option<Arc<AtomicBool>>,
    video_playing: bool,
    video_stop: Option<Arc<AtomicBool>>,
    video_rx: Option<Receiver<VideoFrame>>,
    video_token: u64,
    syncing: bool,
    sync_note: Option<String>,
    status: String,
    open: bool,
}

/// Токены фрагментов «обе дорожки вместе» не должны пересекаться с обычными.
const SYNC_TOKEN_BASE: u64 = 1 << 40;

pub struct App {
    tools: ToolPaths,
    tool_versions: Vec<(String, Option<String>)>,

    files: Vec<FileEntry>,
    selected: usize,
    probe_tx: Sender<ProbeMsg>,
    probe_rx: Receiver<ProbeMsg>,
    probe_pending: usize,

    output_container: ContainerChoice,
    output_dir: Option<PathBuf>,
    suffix: String,
    keep_chapters: bool,
    keep_tags: bool,
    keep_attachments: bool,
    mov_text: bool,
    overwrite: bool,
    split: SplitChoice,
    split_minutes: f32,
    split_ranges: String,

    lang: Lang,

    filter: String,
    tags_open: Option<usize>,
    extra_pick: Option<ExtraPick>,

    job: JobState,
    job_rx: Option<Receiver<JobMsg>>,
    cancel: Option<Arc<AtomicBool>>,

    preview: PreviewState,
    preview_tx: Sender<PreviewMsg>,
    preview_rx: Receiver<PreviewMsg>,

    sync_window: Option<SyncWindow>,
    frame_tx: Sender<FrameMsg>,
    frame_rx: Receiver<FrameMsg>,

    log: Vec<String>,
    status: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        setup_style(&cc.egui_ctx);
        trackcut_core::chapters::cleanup_temp();

        let tools = ToolPaths::detect();
        let tool_versions = tools.versions();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (preview_tx, preview_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel();

        let lang = crate::i18n::load_lang();
        let mut app = Self {
            tools,
            tool_versions,
            files: Vec::new(),
            selected: 0,
            probe_tx,
            probe_rx,
            probe_pending: 0,
            output_container: ContainerChoice::Auto,
            output_dir: None,
            suffix: "_clean".to_string(),
            keep_chapters: true,
            keep_tags: true,
            keep_attachments: true,
            mov_text: false,
            overwrite: false,
            split: SplitChoice::None,
            split_minutes: 10.0,
            split_ranges: String::new(),
            lang,
            filter: String::new(),
            tags_open: None,
            extra_pick: None,
            job: JobState::Idle,
            job_rx: None,
            cancel: None,
            preview: PreviewState::default(),
            preview_tx,
            preview_rx,
            sync_window: None,
            frame_tx,
            frame_rx,
            log: Vec::new(),
            status: tr(lang, "Добавьте файл, чтобы увидеть дорожки").to_string(),
        };

        if let Some(arg) = std::env::args().nth(1) {
            let path = PathBuf::from(arg);
            if path.is_file() {
                app.add_paths(vec![path]);
            }
        }

        app
    }

    fn is_busy(&self) -> bool {
        matches!(self.job, JobState::Running { .. })
    }

    fn can_mux(&self) -> bool {
        !self.is_busy()
            && self.tools.ffmpeg.is_some()
            && self.tools.ffprobe.is_some()
            && self.files.iter().any(|file| {
                file.include && file.info.is_some() && file.keep.iter().any(|keep| *keep)
            })
    }

    fn refresh_tools(&mut self) {
        self.tools = ToolPaths::detect();
        self.tool_versions = self.tools.versions();
        let missing = self.tools.missing();
        self.status = if missing.is_empty() {
            tr(self.lang, "Все инструменты найдены").to_string()
        } else {
            format!("{} {}", tr(self.lang, "Не найдены:"), missing.join(", "))
        };
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        if self.is_busy() {
            return;
        }
        let mut added = 0;
        for path in paths {
            if !path.is_file() {
                continue;
            }
            if self.files.iter().any(|file| paths_equal(&file.path, &path)) {
                continue;
            }
            let id = self.files.len();
            self.files.push(FileEntry::new(path.clone()));
            self.probe_pending += 1;
            added += 1;

            let tx = self.probe_tx.clone();
            let tools = self.tools.clone();
            std::thread::spawn(move || {
                let result = probe::probe(&path, &tools).map_err(|err| err.to_string());
                let _ = tx.send(ProbeMsg::Done(id, result));
            });
        }
        if added > 0 {
            if let Some(first) = self.files.iter().position(|file| file.probing) {
                self.selected = first;
            }
            self.status = format!(
                "{} {added}. {}",
                tr(self.lang, "Добавлено файлов:"),
                tr(self.lang, "Читаю метаданные…")
            );
        }
    }

    fn pick_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                tr(self.lang, "Видео"),
                &[
                    "mkv", "mp4", "m4v", "mov", "webm", "avi", "ts", "m2ts", "mpg", "mpeg",
                ],
            )
            .pick_files()
        {
            self.add_paths(paths);
        }
    }

    fn pick_output_dir(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.output_dir = Some(dir);
        }
    }

    fn poll_channels(&mut self, ctx: &egui::Context) {
        let probe_messages: Vec<ProbeMsg> = self.probe_rx.try_iter().collect();
        for message in probe_messages {
            let (id, result) = match message {
                ProbeMsg::Done(id, result) => (id, result),
                ProbeMsg::ExtraPick(result) => {
                    if let Some(pick) = &mut self.extra_pick {
                        match result {
                            Ok(info) => {
                                pick.selected = default_extra_selection(&info);
                                pick.info = Some(info);
                                pick.error = None;
                            }
                            Err(err) => {
                                pick.error = Some(err);
                                pick.info = None;
                            }
                        }
                    }
                    continue;
                }
                ProbeMsg::SyncDone {
                    source,
                    uid,
                    internal,
                    result,
                } => {
                    self.apply_sync_result(source, uid, internal, result);
                    continue;
                }
            };
            self.probe_pending = self.probe_pending.saturating_sub(1);
            let Some(entry) = self.files.get_mut(id) else {
                continue;
            };
            entry.probing = false;
            match result {
                Ok(info) => {
                    entry.error = None;
                    entry.info = Some(info);
                    entry.reset_flags();
                    if entry.file_size == 0 {
                        entry.file_size =
                            std::fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(0);
                    }
                }
                Err(err) => {
                    entry.error = Some(err);
                }
            }
            if self.probe_pending == 0 {
                self.status = tr(self.lang, "Метаданные загружены").to_string();
            }
        }

        let preview_messages: Vec<PreviewMsg> = self.preview_rx.try_iter().collect();
        for message in preview_messages {
            match message {
                PreviewMsg::Finished(token) => {
                    if token >= SYNC_TOKEN_BASE {
                        if let Some(win) = self.sync_window.as_mut() {
                            if SYNC_TOKEN_BASE + win.uid as u64 == token {
                                win.playing = false;
                                win.stop = None;
                            }
                        }
                    } else if self.preview.token == Some(token) {
                        self.preview.token = None;
                        self.preview.stop = None;
                        self.preview.status.clear();
                    }
                }
                PreviewMsg::Error(token, err) => {
                    if token >= SYNC_TOKEN_BASE {
                        if let Some(win) = self.sync_window.as_mut() {
                            if SYNC_TOKEN_BASE + win.uid as u64 == token {
                                win.playing = false;
                                win.stop = None;
                                win.status = format!("{}: {err}", tr(self.lang, "звук"));
                            }
                        }
                    } else {
                        if self.preview.token == Some(token) {
                            self.preview.token = None;
                            self.preview.stop = None;
                        }
                        self.log
                            .push(format!("{}: {err}", tr(self.lang, "Прослушивание")));
                        self.status =
                            tr(self.lang, "Не удалось воспроизвести фрагмент").to_string();
                    }
                }
            }
        }

        let frames: Vec<FrameMsg> = self.frame_rx.try_iter().collect();
        for frame in frames {
            if let Some(win) = self.sync_window.as_mut() {
                if frame.token != win.frame_token {
                    continue;
                }
                win.frame_pending = false;
                match frame.result {
                    Ok((rgba, width, height)) => {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        win.texture = Some(ctx.load_texture(
                            "sync_frame",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                        win.image_size = [width as f32, height as f32];
                        win.status.clear();
                    }
                    Err(err) => win.status = format!("{}: {err}", tr(self.lang, "кадр")),
                }
            }
        }

        // Кадры видеофрагмента: заменяют статичный кадр, пока играет звук.
        if let Some(window) = self.sync_window.as_mut() {
            if let Some(rx) = window.video_rx.take() {
                let mut disconnected = false;
                loop {
                    match rx.try_recv() {
                        Ok(frame) => {
                            if frame.token != window.video_token {
                                continue;
                            }
                            let image = egui::ColorImage::from_rgba_unmultiplied(
                                [frame.width as usize, frame.height as usize],
                                &frame.rgba,
                            );
                            let size = [frame.width as usize, frame.height as usize];
                            match &mut window.texture {
                                Some(texture) if texture.size() == size => {
                                    texture.set(image, egui::TextureOptions::LINEAR);
                                }
                                _ => {
                                    window.texture = Some(ctx.load_texture(
                                        "sync_video",
                                        image,
                                        egui::TextureOptions::LINEAR,
                                    ));
                                }
                            }
                            window.image_size = [frame.width as f32, frame.height as f32];
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if disconnected {
                    window.video_playing = false;
                } else {
                    window.video_rx = Some(rx);
                }
            }
        }

        let mut all_done: Option<(usize, usize)> = None;
        let messages: Vec<JobMsg> = match &self.job_rx {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for message in messages {
            match message {
                JobMsg::FileStarted { index, total, name } => {
                    self.job = JobState::Running {
                        index,
                        total,
                        name: name.clone(),
                        percent: 0.0,
                    };
                    self.log.push(format!("▶ {index}/{total}: {name}"));
                }
                JobMsg::Progress(percent) => {
                    if let JobState::Running {
                        percent: current, ..
                    } = &mut self.job
                    {
                        *current = percent;
                    }
                }
                JobMsg::Warning(warning) => {
                    self.log.push(format!("  ! {warning}"));
                }
                JobMsg::Message(message) => {
                    if self.log.len() < 4000 {
                        self.log.push(format!("  {message}"));
                    }
                }
                JobMsg::FileDone {
                    file_index,
                    ok,
                    message,
                    output,
                    mark_result,
                } => {
                    self.log.push(if ok {
                        format!("✔ {message}")
                    } else {
                        format!("✘ {message}")
                    });
                    if ok && mark_result {
                        if let Some(entry) = self.files.get_mut(file_index) {
                            entry.result = output;
                        }
                    }
                }
                JobMsg::AllDone { ok, failed } => {
                    all_done = Some((ok, failed));
                }
            }
        }

        if let Some((ok, failed)) = all_done {
            self.job_rx = None;
            self.cancel = None;
            self.job = JobState::Done { ok, failed };
            self.status = if failed == 0 {
                format!("{}: {ok}", tr(self.lang, "Файлов собрано"))
            } else {
                format!(
                    "{} {ok}, {} {failed}",
                    tr(self.lang, "Успешно:"),
                    tr(self.lang, "с ошибками:")
                )
            };
            self.log.push(self.status.clone());
            if let Some(rx) = &self.job_rx {
                let _ = rx;
            }
            ctx.request_repaint();
        }

        if self.probe_pending > 0
            || self.is_busy()
            || self.preview.token.is_some()
            || self
                .sync_window
                .as_ref()
                .map(|window| window.playing || window.video_playing || window.frame_pending)
                .unwrap_or(false)
        {
            let video_running = self
                .sync_window
                .as_ref()
                .map(|window| window.video_playing)
                .unwrap_or(false);
            ctx.request_repaint_after(Duration::from_millis(if video_running { 15 } else { 100 }));
        }
    }

    fn output_path_for(&self, input: &Path, container: OutputContainer) -> PathBuf {
        let dir = self
            .output_dir
            .clone()
            .unwrap_or_else(|| input.parent().unwrap_or(Path::new(".")).to_path_buf());
        let stem = input
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "output".to_string());
        let suffix = self.suffix.trim();
        let base = format!("{stem}{suffix}.{}", container.extension());
        let mut candidate = dir.join(&base);
        if !self.overwrite {
            let mut index = 1;
            while candidate.exists() {
                candidate = dir.join(format!(
                    "{stem}{suffix} ({index}).{}",
                    container.extension()
                ));
                index += 1;
            }
        }
        candidate
    }

    /// Готовит настройку нарезки из полей панели слева.
    fn split_spec(&self) -> Result<Option<SplitSpec>, String> {
        match self.split {
            SplitChoice::None => Ok(None),
            SplitChoice::Every => {
                let minutes = self.split_minutes.max(0.5);
                Ok(Some(SplitSpec::by_duration(
                    (minutes * 60_000.0).round() as u64
                )))
            }
            SplitChoice::Ranges => {
                let text = self.split_ranges.trim();
                if text.is_empty() {
                    return Err(tr(
                        self.lang,
                        "нарезка по диапазонам: впишите, например, 00:10:00-00:20:00",
                    )
                    .into());
                }
                let ranges = parse_ranges(text).filter(|ranges| !ranges.is_empty()).ok_or_else(
                    || {
                        tr(
                            self.lang,
                            "нарезка по диапазонам: не понял формат. Пример: 00:10:00-00:20:00, 00:30:00-",
                        )
                        .to_string()
                    },
                )?;
                Ok(Some(SplitSpec::by_ranges(ranges)))
            }
        }
    }

    fn build_jobs(&self) -> Result<Vec<Job>, String> {
        let mut jobs = Vec::new();
        for (file_index, entry) in self.files.iter().enumerate() {
            if !entry.include {
                continue;
            }
            let Some(info) = entry.info.clone() else {
                continue;
            };
            let kept: Vec<usize> = info
                .streams
                .iter()
                .enumerate()
                .filter(|(uid, _)| entry.keep.get(*uid).copied().unwrap_or(false))
                .map(|(uid, _)| uid)
                .collect();
            if kept.is_empty() {
                continue;
            }

            let container = self.output_container.resolve(&entry.path);
            let output = self.output_path_for(&entry.path, container);
            let keep = kept
                .iter()
                .map(|uid| KeepTrack {
                    uid: *uid,
                    default: Some(entry.default_flag.get(*uid).copied().unwrap_or(false)),
                    forced: Some(entry.forced_flag.get(*uid).copied().unwrap_or(false)),
                    delay_ms: entry.keep_delay.get(*uid).copied().unwrap_or(0),
                })
                .collect();

            let extras: Vec<ExtraInput> = entry
                .extras
                .iter()
                .map(|extra| ExtraInput {
                    info: extra.info.clone(),
                    tracks: extra
                        .tracks
                        .iter()
                        .map(|track| ExtraTrack {
                            uid: track.uid,
                            delay_ms: track.delay_ms,
                            default: Some(track.default),
                            forced: None,
                        })
                        .collect(),
                })
                .filter(|extra| !extra.tracks.is_empty())
                .collect();

            let split = self.split_spec()?;

            let plan = MuxPlan {
                input: entry.path.clone(),
                output,
                container,
                keep,
                keep_chapters: self.keep_chapters,
                keep_tags: self.keep_tags,
                keep_attachments: self.keep_attachments,
                convert_subtitles_to_mov_text: self.mov_text,
                overwrite: self.overwrite,
                backend: None,
                extras,
                chapters_file: entry.chapters_file.clone(),
                split,
                append: entry.append.clone(),
            };

            validate(&info, &plan).map_err(|err| format!("{}: {err}", entry.name()))?;
            jobs.push(Job {
                file_index,
                label: entry.name(),
                op: JobOp::Mux { info, plan },
            });
        }

        if jobs.is_empty() {
            return Err(tr(self.lang, "нет файлов с выбранными дорожками").to_string());
        }
        Ok(jobs)
    }

    fn start_mux(&mut self) {
        if self.is_busy() {
            return;
        }
        let jobs = match self.build_jobs() {
            Ok(jobs) => jobs,
            Err(err) => {
                self.status = err;
                return;
            }
        };
        let total = jobs.len();
        self.start_jobs(jobs, format!("{}: {total}", tr(self.lang, "Сборка")));
    }

    fn start_extract(&mut self, job: Job) {
        if self.is_busy() {
            return;
        }
        let status = format!("{}: {}", tr(self.lang, "Извлечение"), job.label);
        self.start_jobs(vec![job], status);
    }

    fn start_jobs(&mut self, jobs: Vec<Job>, status: String) {
        let total = jobs.len();
        let first_name = jobs[0].label.clone();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.job_rx = Some(rx);
        self.job = JobState::Running {
            index: 1,
            total,
            name: first_name,
            percent: 0.0,
        };
        self.log.clear();
        self.status = status;

        let tools = self.tools.clone();
        let lang = self.lang;
        std::thread::spawn(move || {
            let mut ok = 0usize;
            let mut failed = 0usize;
            for (position, job) in jobs.into_iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let _ = tx.send(JobMsg::FileStarted {
                    index: position + 1,
                    total,
                    name: job.label.clone(),
                });

                let events = tx.clone();
                let forward = |event: ExecEvent| match event {
                    ExecEvent::Progress(percent) => {
                        let _ = events.send(JobMsg::Progress(percent));
                    }
                    ExecEvent::Warning(warning) => {
                        let _ = events.send(JobMsg::Warning(warning));
                    }
                    ExecEvent::Message(message) => {
                        let _ = events.send(JobMsg::Message(message));
                    }
                };

                let is_extract = matches!(job.op, JobOp::Extract { .. });
                let result = match &job.op {
                    JobOp::Mux { info, plan } => {
                        run_mux(info, plan, &tools, cancel.clone(), forward)
                    }
                    JobOp::Extract { info, plan } => {
                        run_extract(info, plan, &tools, cancel.clone(), forward)
                    }
                };

                match result {
                    Ok(outcome) => {
                        ok += 1;
                        let name = outcome
                            .output
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let message = if is_extract {
                            format!("{}: {} {name}", job.label, tr(lang, "сохранено в"))
                        } else {
                            format!("{} → {name}", job.label)
                        };
                        let _ = tx.send(JobMsg::FileDone {
                            file_index: job.file_index,
                            ok: true,
                            message,
                            output: Some(outcome.output.display().to_string()),
                            mark_result: !is_extract,
                        });
                    }
                    Err(Error::Cancelled) => break,
                    Err(err) => {
                        failed += 1;
                        let _ = tx.send(JobMsg::FileDone {
                            file_index: job.file_index,
                            ok: false,
                            message: format!("{}: {err}", job.label),
                            output: None,
                            mark_result: false,
                        });
                    }
                }
            }
            let _ = tx.send(JobMsg::AllDone { ok, failed });
        });
    }

    fn request_cancel(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.status = tr(self.lang, "Отмена…").to_string();
        }
    }

    fn preview_stream(&mut self, uid: usize) {
        let token = main_token(uid);
        if self.preview.token == Some(token) {
            self.preview.stop();
            return;
        }
        self.preview.stop();

        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = entry.info.clone() else {
            return;
        };
        self.play_preview(info, uid, token);
    }

    /// Прослушивание дорожки, взятой из другого файла.
    fn preview_extra(&mut self, source: usize, uid: usize) {
        let token = extra_token(source, uid);
        if self.preview.token == Some(token) {
            self.preview.stop();
            return;
        }
        self.preview.stop();

        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(extra) = entry.extras.get(source) else {
            return;
        };
        let info = extra.info.clone();
        self.play_preview(info, uid, token);
    }

    /// Открывает окно синхрона: `internal` — дорожка самого файла, иначе внешняя.
    fn open_sync_window(&mut self, source: usize, uid: usize, internal: bool) {
        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = &entry.info else {
            return;
        };
        let (delay_ms, kind) = if internal {
            let Some(stream) = info.streams.get(uid) else {
                return;
            };
            (entry.keep_delay.get(uid).copied().unwrap_or(0), stream.kind)
        } else {
            let Some(extra) = entry.extras.get(source) else {
                return;
            };
            let Some(stream) = extra.info.streams.get(uid) else {
                return;
            };
            let delay = extra
                .tracks
                .iter()
                .find(|track| track.uid == uid)
                .map(|track| track.delay_ms)
                .unwrap_or(0);
            (delay, stream.kind)
        };
        if kind != StreamKind::Audio && kind != StreamKind::Subtitle {
            self.status = tr(self.lang, "Сдвиг задаётся для аудио и субтитров").to_string();
            return;
        }
        let time_ms = trackcut_core::preview::preview_start_ms(info.duration_ms);

        self.stop_sync_playback();
        self.sync_window = Some(SyncWindow {
            source,
            uid,
            internal,
            time_ms,
            delay_ms,
            texture: None,
            image_size: [720.0, 405.0],
            frame_pending: false,
            frame_token: 0,
            frame_for: None,
            playing: false,
            stop: None,
            video_playing: false,
            video_stop: None,
            video_rx: None,
            video_token: 0,
            syncing: false,
            sync_note: None,
            status: String::new(),
            open: true,
        });
        self.request_sync_frame();
    }

    fn stop_sync_playback(&mut self) {
        if let Some(window) = self.sync_window.as_mut() {
            if let Some(stop) = window.stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(stop) = window.video_stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            window.playing = false;
            window.video_playing = false;
            window.video_rx = None;
        }
    }

    fn request_sync_frame(&mut self) {
        let Some(window) = self.sync_window.as_mut() else {
            return;
        };
        if window.frame_pending {
            return;
        }
        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = &entry.info else {
            return;
        };
        let video = info
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Video && stream.ff_index.is_some());
        let Some(ff_index) = video.and_then(|stream| stream.ff_index) else {
            window.status =
                tr(self.lang, "В файле нет видеодорожки — видно только звук").to_string();
            return;
        };
        if self.tools.ffmpeg.is_none() {
            window.status = tr(self.lang, "Для кадра нужен ffmpeg").to_string();
            return;
        }
        window.frame_token += 1;
        window.frame_pending = true;
        window.frame_for = Some(window.time_ms);
        request_frame(
            self.tools.clone(),
            info.path.clone(),
            ff_index,
            window.time_ms,
            720,
            window.frame_token,
            self.frame_tx.clone(),
        );
    }

    fn play_sync_pair(&mut self) {
        self.stop_sync_playback();
        let Some((source, uid, at_ms, delay_ms, internal)) =
            self.sync_window.as_ref().map(|window| {
                (
                    window.source,
                    window.uid,
                    window.time_ms,
                    window.delay_ms,
                    window.internal,
                )
            })
        else {
            return;
        };
        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = entry.info.clone() else {
            return;
        };
        if self.tools.ffmpeg.is_none() {
            self.status = tr(self.lang, "Для прослушивания нужен ffmpeg").to_string();
            return;
        }

        let token = SYNC_TOKEN_BASE + uid as u64;
        let main_path = info.path.clone();
        let stop = if internal {
            let Some(ff_index) = info.streams.get(uid).and_then(|stream| stream.ff_index) else {
                self.status =
                    tr(self.lang, "У этой дорожки нет индекса для декодирования").to_string();
                return;
            };
            start_track_preview(
                self.tools.clone(),
                info.clone(),
                ff_index,
                at_ms,
                delay_ms,
                token,
                self.preview_tx.clone(),
            )
        } else {
            let Some(extra) = entry.extras.get(source).cloned() else {
                return;
            };
            let Some(main_uid) = main_audio_uid(&info, &entry.keep) else {
                self.status = tr(self.lang, "В основном файле нет аудиодорожки").to_string();
                return;
            };
            let Some(main_ff) = info
                .streams
                .get(main_uid)
                .and_then(|stream| stream.ff_index)
            else {
                self.status = tr(
                    self.lang,
                    "У основной дорожки нет индекса для декодирования",
                )
                .to_string();
                return;
            };
            let Some(extra_ff) = extra
                .info
                .streams
                .get(uid)
                .and_then(|stream| stream.ff_index)
            else {
                self.status =
                    tr(self.lang, "У внешней дорожки нет индекса для декодирования").to_string();
                return;
            };
            start_pair_preview(
                self.tools.clone(),
                info.clone(),
                main_ff,
                extra.info.clone(),
                extra_ff,
                at_ms,
                delay_ms,
                token,
                self.preview_tx.clone(),
            )
        };

        let video = info
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Video && stream.ff_index.is_some())
            .map(|stream| {
                let width = 640u32;
                let height = stream
                    .resolution
                    .map(|(w, h)| {
                        let scaled = (width as f64 * h as f64 / w as f64).round() as u32;
                        (scaled.max(2) / 2) * 2
                    })
                    .unwrap_or(360);
                (stream.ff_index, width, height)
            });

        let mut video_stop = None;
        let mut video_rx = None;
        let mut video_token = 0;
        if let Some((Some(ff_index), width, height)) = video {
            let (tx, rx) = mpsc::channel();
            video_token = token.wrapping_add(1);
            video_stop = Some(start_video_segment(
                self.tools.clone(),
                main_path,
                ff_index,
                at_ms,
                width,
                height,
                12.0,
                video_token,
                tx,
            ));
            video_rx = Some(rx);
        }

        if let Some(window) = self.sync_window.as_mut() {
            window.playing = true;
            window.stop = Some(stop);
            window.video_playing = video_rx.is_some();
            window.video_stop = video_stop;
            window.video_rx = video_rx;
            window.video_token = video_token;
            window.status = tr(self.lang, "Играют обе дорожки и видео…").to_string();
        }
    }

    fn show_sync_window(&mut self, ctx: &egui::Context) {
        let Some(mut window) = self.sync_window.take() else {
            return;
        };
        let main_info = self
            .files
            .get(self.selected)
            .and_then(|entry| entry.info.clone());
        let extra_name = if window.internal {
            self.files
                .get(self.selected)
                .map(|entry| entry.name())
                .unwrap_or_default()
        } else {
            self.files
                .get(self.selected)
                .and_then(|entry| entry.extras.get(window.source))
                .map(|extra| extra.name())
                .unwrap_or_default()
        };
        let track_label = if window.internal {
            main_info
                .as_ref()
                .and_then(|info| info.streams.get(window.uid))
                .map(|stream| stream_name(self.lang, stream))
        } else {
            self.files
                .get(self.selected)
                .and_then(|entry| entry.extras.get(window.source))
                .and_then(|extra| extra.info.streams.get(window.uid))
                .map(|stream| stream_name(self.lang, stream))
        }
        .unwrap_or_default();
        let duration_ms = main_info
            .as_ref()
            .and_then(|info| info.duration_ms)
            .unwrap_or(0);
        let has_ffmpeg = self.tools.ffmpeg.is_some();
        let busy = self.is_busy();
        let (source, uid) = (window.source, window.uid);

        let mut open = window.open;
        let mut close = false;
        let mut play = false;
        let mut stop_play = false;
        let mut auto = false;
        let mut request = false;

        egui::Window::new(tr(self.lang, "Синхрон дорожки"))
            .open(&mut open)
            .default_width(790.0)
            .resizable(true)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("📄 {extra_name}")).color(ACCENT));
                    if !track_label.is_empty() {
                        ui.label(RichText::new(&track_label).small().color(MUTED));
                    }
                });
                let hint = if window.internal {
                    tr(
                        self.lang,
                        "Дорожка из этого файла со сдвигом относительно остальных. Видно кадр и \
                         слышно, как звук ложится на видео.",
                    )
                } else {
                    tr(
                        self.lang,
                        "Кадр из основного файла, обе дорожки звучат вместе. Двигайте сдвиг, пока \
                         речь не совпадёт. Положительный сдвиг — внешняя позже.",
                    )
                };
                ui.label(RichText::new(hint).small().color(MUTED));

                ui.add_space(4.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    if let Some(texture) = &window.texture {
                        let max_width = ui.available_width().min(760.0);
                        let scale = (max_width / window.image_size[0])
                            .min(400.0 / window.image_size[1])
                            .max(0.1);
                        let size =
                            egui::vec2(window.image_size[0] * scale, window.image_size[1] * scale);
                        ui.add(egui::Image::new((texture.id(), size)));
                    } else if window.frame_pending {
                        ui.allocate_space(egui::vec2(ui.available_width(), 240.0));
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Spinner::new());
                        });
                    } else {
                        ui.allocate_space(egui::vec2(ui.available_width(), 60.0));
                        ui.label(
                            RichText::new(tr(self.lang, "Кадр не загружен"))
                                .small()
                                .color(MUTED),
                        );
                    }
                });
                if !window.status.is_empty() {
                    ui.label(RichText::new(&window.status).small().color(MUTED));
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(tr(self.lang, "−10 с")).clicked() {
                        window.time_ms = window.time_ms.saturating_sub(10_000);
                        request = true;
                    }
                    if ui.button(tr(self.lang, "−1 с")).clicked() {
                        window.time_ms = window.time_ms.saturating_sub(1_000);
                        request = true;
                    }
                    let mut seconds = window.time_ms as f32 / 1000.0;
                    let max = (duration_ms as f32 / 1000.0).max(0.1);
                    let slider = ui.add_sized(
                        [260.0, 18.0],
                        egui::Slider::new(&mut seconds, 0.0..=max).show_value(false),
                    );
                    if slider.changed() {
                        window.time_ms = (seconds * 1000.0).round() as u64;
                        request = true;
                    }
                    ui.label(
                        RichText::new(format!(
                            "{}\u{2009}/\u{2009}{}",
                            format_duration(window.time_ms),
                            format_duration(duration_ms)
                        ))
                        .small()
                        .color(MUTED),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button(tr(self.lang, "+1 с")).clicked() {
                        window.time_ms = (window.time_ms + 1_000).min(duration_ms.max(1_000));
                        request = true;
                    }
                    if ui.button(tr(self.lang, "+10 с")).clicked() {
                        window.time_ms = (window.time_ms + 10_000).min(duration_ms.max(1_000));
                        request = true;
                    }
                    if ui.button(tr(self.lang, "Начало")).clicked() {
                        window.time_ms = 0;
                        request = true;
                    }
                    if ui.button(tr(self.lang, "Середина")).clicked() {
                        window.time_ms = duration_ms / 2;
                        request = true;
                    }
                    ui.label(
                        RichText::new(tr(self.lang, "Кадр берётся по ближайшему ключевому кадру"))
                            .small()
                            .color(MUTED),
                    );
                });

                ui.add_space(6.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr(self.lang, "Сдвиг")).strong());
                    let response = ui.add(
                        egui::Slider::new(&mut window.delay_ms, -10_000..=10_000)
                            .suffix(format!(" {}", tr(self.lang, "мс")))
                            .step_by(1.0),
                    );
                    if response.changed() {
                        stop_play = true;
                    }
                    for step in [-500_i64, -100, -10, 10, 100, 500] {
                        if ui
                            .add(egui::Button::new(format!("{step:+}")).small())
                            .clicked()
                        {
                            window.delay_ms = (window.delay_ms + step).clamp(-600_000, 600_000);
                            stop_play = true;
                        }
                    }
                    if ui.button("0").clicked() {
                        window.delay_ms = 0;
                        stop_play = true;
                    }
                });
                ui.horizontal(|ui| {
                    let label = if window.playing || window.video_playing {
                        tr(self.lang, "■ Стоп")
                    } else {
                        tr(self.lang, "▶ Смотреть и слушать (12 с)")
                    };
                    if ui
                        .add_enabled(has_ffmpeg, egui::Button::new(label))
                        .on_hover_text(if window.internal {
                            tr(
                                self.lang,
                                "Видео и эта дорожка со сдвигом играют вместе 12 секунд — \
                                 видно и слышно, как звук ложится на картинку",
                            )
                        } else {
                            tr(
                                self.lang,
                                "Видео и обе дорожки играют вместе 12 секунд — видно и слышно, \
                                 как ложится сдвиг",
                            )
                        })
                        .clicked()
                    {
                        if window.playing || window.video_playing {
                            stop_play = true;
                        } else {
                            play = true;
                        }
                    }
                    if window.syncing {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(
                            RichText::new(tr(self.lang, "сравниваю…"))
                                .small()
                                .color(MUTED),
                        );
                    } else if ui
                        .add_enabled(!busy, egui::Button::new(tr(self.lang, "⏱ Авто")))
                        .on_hover_text(tr(
                            self.lang,
                            "Подобрать сдвиг по звуку: сравнить огибающие громкости дорожек",
                        ))
                        .clicked()
                    {
                        auto = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(tr(self.lang, "Готово")).clicked() {
                            close = true;
                        }
                    });
                });
                if let Some(note) = &window.sync_note {
                    ui.label(RichText::new(note).small().color(MUTED));
                }
            });

        if close || !open {
            self.stop_sync_playback();
            self.sync_window = None;
            self.status = tr(self.lang, "Окно синхрона закрыто").to_string();
            return;
        }

        // Сдвиг из окна сразу попадает в дорожку — в таблице видно то же число.
        let internal = window.internal;
        if let Some(entry) = self.files.get_mut(self.selected) {
            if internal {
                if let Some(slot) = entry.keep_delay.get_mut(uid) {
                    *slot = window.delay_ms;
                }
            } else if let Some(extra) = entry.extras.get_mut(source) {
                if let Some(track) = extra.tracks.iter_mut().find(|track| track.uid == uid) {
                    track.delay_ms = window.delay_ms;
                }
            }
        }

        window.open = open;
        self.sync_window = Some(window);

        if stop_play {
            self.stop_sync_playback();
        }
        if play {
            self.play_sync_pair();
        }
        if auto {
            if internal {
                self.estimate_internal_delay(uid);
            } else {
                self.estimate_extra_delay(source, uid);
                if let Some(window) = self.sync_window.as_mut() {
                    window.syncing = self
                        .files
                        .get(self.selected)
                        .and_then(|entry| entry.extras.get(source))
                        .and_then(|extra| extra.tracks.iter().find(|track| track.uid == uid))
                        .map(|track| track.syncing)
                        .unwrap_or(false);
                }
            }
        }

        // Если время сдвинулось, пока грузился кадр, догружаем актуальный.
        let needs_frame = self
            .sync_window
            .as_ref()
            .map(|window| !window.frame_pending && window.frame_for != Some(window.time_ms))
            .unwrap_or(false);
        if request || needs_frame {
            self.request_sync_frame();
        }
    }

    /// Подбирает сдвиг внешней дорожки по звуку: сравнивает огибающие громкости
    /// с аудиодорожкой основного файла.
    fn estimate_extra_delay(&mut self, source: usize, uid: usize) {
        if self.is_busy() {
            return;
        }
        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = entry.info.clone() else {
            return;
        };
        let Some(extra) = entry.extras.get(source) else {
            return;
        };
        let extra_info = extra.info.clone();

        let Some(main_uid) = main_audio_uid(&info, &entry.keep) else {
            self.status = tr(
                self.lang,
                "В основном файле нет аудиодорожки — не с чем сравнивать",
            )
            .to_string();
            return;
        };
        if extra_info.streams.get(uid).map(|stream| stream.kind) != Some(StreamKind::Audio) {
            self.status = tr(self.lang, "Сдвиг подбирается только для аудио").to_string();
            return;
        }
        if self.tools.ffmpeg.is_none() {
            self.status = tr(self.lang, "Для подбора сдвига нужен ffmpeg").to_string();
            return;
        }

        if let Some(extra) = self.files[self.selected].extras.get_mut(source) {
            if let Some(track) = extra.tracks.iter_mut().find(|track| track.uid == uid) {
                track.syncing = true;
                track.sync_note = None;
            }
        }

        let tools = self.tools.clone();
        let tx = self.probe_tx.clone();
        self.status = tr(self.lang, "Сравниваю звук дорожек…").to_string();
        std::thread::spawn(move || {
            let result = estimate_delay(&tools, &info, main_uid, &extra_info, uid)
                .map_err(|err| err.to_string());
            let _ = tx.send(ProbeMsg::SyncDone {
                source,
                uid,
                internal: false,
                result,
            });
        });
    }

    /// Подбирает сдвиг дорожки внутри файла: сравнивает её с другой аудиодорожкой.
    fn estimate_internal_delay(&mut self, uid: usize) {
        if self.is_busy() {
            return;
        }
        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = entry.info.clone() else {
            return;
        };
        if info.streams.get(uid).map(|stream| stream.kind) != Some(StreamKind::Audio) {
            self.status = tr(self.lang, "Сдвиг подбирается только для аудио").to_string();
            return;
        }
        if self.tools.ffmpeg.is_none() {
            self.status = tr(self.lang, "Для подбора сдвига нужен ffmpeg").to_string();
            return;
        }

        let reference = main_audio_uid(&info, &entry.keep)
            .filter(|main| *main != uid)
            .or_else(|| {
                info.streams
                    .iter()
                    .enumerate()
                    .find(|(index, stream)| *index != uid && stream.kind == StreamKind::Audio)
                    .map(|(index, _)| index)
            });
        let Some(reference) = reference else {
            self.status = tr(
                self.lang,
                "В файле только одна аудиодорожка — сравнивать не с чем",
            )
            .to_string();
            return;
        };

        if let Some(window) = self.sync_window.as_mut() {
            window.syncing = true;
            window.sync_note = None;
        }
        let tools = self.tools.clone();
        let tx = self.probe_tx.clone();
        self.status = tr(self.lang, "Сравниваю звук дорожек…").to_string();
        std::thread::spawn(move || {
            let result =
                estimate_delay(&tools, &info, reference, &info, uid).map_err(|err| err.to_string());
            let _ = tx.send(ProbeMsg::SyncDone {
                source: 0,
                uid,
                internal: true,
                result,
            });
        });
    }

    fn apply_sync_result(
        &mut self,
        source: usize,
        uid: usize,
        internal: bool,
        result: Result<OffsetEstimate, String>,
    ) {
        let selected = self.selected;
        let note_text;
        let mut delay_value = 0;
        let estimated_ok = result.is_ok();

        if internal {
            note_text = match &result {
                Ok(estimate) => {
                    delay_value = estimate.delay_ms;
                    self.status = match estimate.warnings.first() {
                        Some(warning) => format!(
                            "{} {} {}. {} {warning}",
                            tr(self.lang, "Сдвиг"),
                            estimate.delay_ms,
                            tr(self.lang, "мс"),
                            tr(self.lang, "но")
                        ),
                        None => format!(
                            "{}: {} {}",
                            tr(self.lang, "Сдвиг подобран:"),
                            estimate.delay_ms,
                            tr(self.lang, "мс")
                        ),
                    };
                    for warning in &estimate.warnings {
                        self.log
                            .push(format!("{}: {warning}", tr(self.lang, "Сдвиг")));
                    }
                    Some(format!(
                        "{}{} {} · {} {}/{} · {} {:.0}%",
                        if estimate.delay_ms > 0 { "+" } else { "" },
                        estimate.delay_ms,
                        tr(self.lang, "мс"),
                        tr(self.lang, "окна"),
                        estimate.windows_agree,
                        estimate.windows_total,
                        tr(self.lang, "совпадение"),
                        estimate.score.max(0.0) * 100.0
                    ))
                }
                Err(err) => {
                    self.status = tr(self.lang, "Не удалось подобрать сдвиг").to_string();
                    Some(format!("{}: {err}", tr(self.lang, "не удалось")))
                }
            };
            if estimated_ok {
                if let Some(entry) = self.files.get_mut(selected) {
                    if let Some(slot) = entry.keep_delay.get_mut(uid) {
                        *slot = delay_value;
                    }
                }
            }
        } else {
            let Some(entry) = self.files.get_mut(selected) else {
                return;
            };
            let Some(extra) = entry.extras.get_mut(source) else {
                return;
            };
            let Some(track) = extra.tracks.iter_mut().find(|track| track.uid == uid) else {
                return;
            };
            track.syncing = false;
            match result {
                Ok(estimate) => {
                    track.delay_ms = estimate.delay_ms;
                    delay_value = estimate.delay_ms;
                    let sign = if estimate.delay_ms > 0 { "+" } else { "" };
                    track.sync_note = Some(format!(
                        "{sign}{} {} · {} {}/{} · {} {:.0}%",
                        estimate.delay_ms,
                        tr(self.lang, "мс"),
                        tr(self.lang, "окна"),
                        estimate.windows_agree,
                        estimate.windows_total,
                        tr(self.lang, "совпадение"),
                        estimate.score.max(0.0) * 100.0
                    ));
                    self.status = match estimate.warnings.first() {
                        Some(warning) => format!(
                            "{} {} {}. {} {warning}",
                            tr(self.lang, "Сдвиг"),
                            estimate.delay_ms,
                            tr(self.lang, "мс"),
                            tr(self.lang, "но")
                        ),
                        None => format!(
                            "{}: {} {}",
                            tr(self.lang, "Сдвиг подобран:"),
                            estimate.delay_ms,
                            tr(self.lang, "мс")
                        ),
                    };
                    for warning in &estimate.warnings {
                        self.log
                            .push(format!("{}: {warning}", tr(self.lang, "Сдвиг")));
                    }
                }
                Err(err) => {
                    track.sync_note = Some(format!("{}: {err}", tr(self.lang, "не удалось")));
                    self.status = tr(self.lang, "Не удалось подобрать сдвиг").to_string();
                }
            }
            note_text = track.sync_note.clone();
        }

        if let Some(window) = self.sync_window.as_mut() {
            if window.internal == internal
                && window.uid == uid
                && (internal || window.source == source)
            {
                window.syncing = false;
                window.sync_note = note_text;
                if estimated_ok {
                    window.delay_ms = delay_value;
                }
            }
        }
    }

    fn play_preview(&mut self, info: MediaInfo, uid: usize, token: u64) {
        let Some(stream) = info.streams.get(uid) else {
            return;
        };
        let Some(ff_index) = stream.ff_index else {
            self.status = tr(self.lang, "У этой дорожки нет индекса для декодирования").to_string();
            return;
        };
        if self.tools.ffmpeg.is_none() {
            self.status = tr(self.lang, "Для прослушивания нужен ffmpeg").to_string();
            return;
        }

        let stop = start_preview(
            self.tools.clone(),
            info,
            token,
            ff_index,
            self.preview_tx.clone(),
        );
        self.preview.token = Some(token);
        self.preview.stop = Some(stop);
        self.preview.status = tr(self.lang, "Играет фрагмент…").to_string();
    }

    /// Сохраняет одну дорожку текущего файла в mka/mks/mkv.
    fn extract_track(&mut self, uid: usize) {
        if self.is_busy() {
            return;
        }
        let Some(entry) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = entry.info.clone() else {
            return;
        };
        let Some(stream) = info.streams.get(uid).cloned() else {
            return;
        };
        let label = entry.name();

        let default = default_track_path(&info.path, &[&stream]);
        let Some(path) = rfd::FileDialog::new()
            .set_file_name(
                default
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "track.mka".to_string()),
            )
            .add_filter(tr(self.lang, "Дорожка"), &["mka", "mks", "mkv"])
            .add_filter(tr(self.lang, "Аудио (MKA)"), &["mka"])
            .add_filter(tr(self.lang, "Субтитры (MKS)"), &["mks"])
            .save_file()
        else {
            return;
        };

        let container = path
            .extension()
            .and_then(|ext| ExtractContainer::from_extension(&ext.to_string_lossy()))
            .unwrap_or_else(|| ExtractContainer::for_streams(&[&stream]));
        let output = if path.extension().is_none() {
            path.with_extension(container.extension())
        } else {
            path
        };

        let plan = ExtractPlan {
            input: info.path.clone(),
            output,
            tracks: vec![uid],
            container,
            overwrite: self.overwrite,
            backend: None,
        };
        self.start_extract(Job {
            file_index: self.selected,
            label: format!("{label} · {} {}", tr(self.lang, "дорожка"), uid + 1),
            op: JobOp::Extract { info, plan },
        });
    }

    /// Открывает диалог выбора файла-источника внешней дорожки.
    fn pick_extra_file(&mut self) {
        if self.is_busy() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                tr(self.lang, "Видео и дорожки"),
                &[
                    "mkv", "mka", "mks", "mp4", "m4v", "mov", "webm", "avi", "ts", "m2ts", "mpg",
                    "mpeg", "ac3", "eac3", "dts", "flac", "aac", "m4a", "ogg", "opus", "srt",
                    "ass",
                ],
            )
            .pick_file()
        else {
            return;
        };

        self.extra_pick = Some(ExtraPick {
            path: path.clone(),
            info: None,
            error: None,
            selected: Vec::new(),
        });
        let tx = self.probe_tx.clone();
        let tools = self.tools.clone();
        std::thread::spawn(move || {
            let result = probe::probe(&path, &tools).map_err(|err| err.to_string());
            let _ = tx.send(ProbeMsg::ExtraPick(result));
        });
    }

    /// Выбирает файл-источник глав: MKV или файл глав (XML/OGM/Simple).
    fn pick_chapters_file(&mut self) {
        if self.is_busy() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                tr(self.lang, "Главы или MKV"),
                &["mkv", "xml", "txt", "ogm", "cue"],
            )
            .pick_file()
        else {
            return;
        };
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let xml = match trackcut_core::resolve_chapters(&path, &self.tools) {
            Ok(xml) => xml,
            Err(err) => {
                self.status = format!("{}: {err}", tr(self.lang, "Главы"));
                return;
            }
        };
        if let Some(entry) = self.files.get_mut(self.selected) {
            entry.chapters_file = Some(xml);
            entry.chapters_label = Some(label);
        }
        self.status = format!("{}: {}", tr(self.lang, "Главы из"), path.display());
    }

    /// Добавляет файлы в склейку (append) текущего файла.
    fn pick_append_files(&mut self) {
        if self.is_busy() {
            return;
        }
        let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                tr(self.lang, "Видео"),
                &["mkv", "webm", "mp4", "mov", "m4v"],
            )
            .pick_files()
        else {
            return;
        };
        let added = {
            let Some(entry) = self.files.get_mut(self.selected) else {
                return;
            };
            let mut added = 0;
            for path in paths {
                if paths_equal(&path, &entry.path)
                    || entry
                        .append
                        .iter()
                        .any(|existing| paths_equal(existing, &path))
                {
                    continue;
                }
                entry.append.push(path);
                added += 1;
            }
            added
        };
        self.status = format!("{} {added}", tr(self.lang, "В склейку добавлено файлов:"));
    }

    fn apply_preset(&mut self, preset: Preset) {
        let Some(entry) = self.files.get_mut(self.selected) else {
            return;
        };
        let Some(info) = &entry.info else { return };

        let best_audio = info
            .streams
            .iter()
            .enumerate()
            .filter(|(_, stream)| stream.kind == StreamKind::Audio)
            .max_by_key(|(_, stream)| audio_score(stream))
            .map(|(uid, _)| uid);

        let is_russian = |stream: &StreamInfo| {
            stream
                .language
                .as_deref()
                .map(|code| {
                    let code = code.to_ascii_lowercase();
                    code == "ru" || code == "rus"
                })
                .unwrap_or(false)
        };

        for (uid, stream) in info.streams.iter().enumerate() {
            let keep = match preset {
                Preset::BestAudio => stream.kind == StreamKind::Video || Some(uid) == best_audio,
                Preset::VideoAudio => {
                    matches!(stream.kind, StreamKind::Video | StreamKind::Audio)
                }
                Preset::Everything => stream.kind.is_selectable(),
                Preset::RussianOnly => {
                    stream.kind == StreamKind::Video
                        || (matches!(stream.kind, StreamKind::Audio | StreamKind::Subtitle)
                            && is_russian(stream))
                }
                Preset::Nothing => false,
            };
            entry.keep[uid] = keep && stream.kind.is_selectable();
        }
    }

    fn apply_to_all(&mut self) {
        let Some(source) = self.files.get(self.selected) else {
            return;
        };
        let Some(info) = &source.info else { return };
        let pattern: Vec<(StreamKind, bool)> = info
            .streams
            .iter()
            .zip(source.keep.iter())
            .map(|(stream, keep)| (stream.kind, *keep))
            .collect();
        let source_path = source.path.clone();

        for entry in &mut self.files {
            if paths_equal(&entry.path, &source_path) {
                continue;
            }
            let Some(info) = &entry.info else { continue };
            let mut counters: HashMap<StreamKind, usize> = HashMap::new();
            for (uid, stream) in info.streams.iter().enumerate() {
                let position = counters.entry(stream.kind).or_insert(0);
                let value = pattern
                    .iter()
                    .filter(|(kind, _)| *kind == stream.kind)
                    .nth(*position)
                    .map(|(_, keep)| *keep)
                    .unwrap_or(false);
                *position += 1;
                if uid < entry.keep.len() {
                    entry.keep[uid] = value && stream.kind.is_selectable();
                }
            }
        }
        self.status = tr(self.lang, "Правило применено ко всем файлам").to_string();
    }

    fn open_tags(&mut self, uid: usize) {
        self.tags_open = Some(uid);
    }
}

fn total_kept_bytes(entry: &FileEntry) -> u64 {
    let Some(info) = &entry.info else { return 0 };
    info.streams
        .iter()
        .enumerate()
        .filter(|(uid, _)| entry.keep.get(*uid).copied().unwrap_or(false))
        .filter_map(|(_, stream)| stream.effective_size(info.duration_ms))
        .sum()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_channels(ctx);
        self.handle_input(ctx);
        self.show_top(ctx);
        self.show_left(ctx);
        self.show_bottom(ctx);
        self.show_center(ctx);
        self.show_sync_window(ctx);
        self.show_tags(ctx);
        self.show_extra_picker(ctx);
        self.show_drop_overlay(ctx);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.preview.stop();
        if let Some(window) = &mut self.sync_window {
            if let Some(stop) = window.stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl App {
    fn handle_input(&mut self, ctx: &egui::Context) {
        let open = ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::O));
        if open && !self.is_busy() {
            self.pick_files();
        }

        let start =
            ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Enter));
        if start && self.can_mux() {
            self.start_mux();
        }

        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            if self.is_busy() {
                self.request_cancel();
            } else {
                self.preview.stop();
            }
        }

        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !dropped.is_empty() && !self.is_busy() {
            self.add_paths(dropped);
        }
    }

    fn show_top(&mut self, ctx: &egui::Context) {
        let lang = self.lang;
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("TrackCut")
                        .size(24.0)
                        .strong()
                        .color(Color32::WHITE),
                );
                ui.label(
                    RichText::new(tr(lang, "удаление дорожек без перекодирования"))
                        .size(13.0)
                        .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.is_busy(),
                            egui::Button::new(
                                RichText::new(format!("＋ {}", tr(lang, "Добавить файлы")))
                                    .strong(),
                            ),
                        )
                        .clicked()
                    {
                        self.pick_files();
                    }
                    if ui
                        .add_enabled(!self.is_busy(), egui::Button::new(tr(lang, "Очистить")))
                        .clicked()
                    {
                        self.files.clear();
                        self.selected = 0;
                        self.status = tr(lang, "Список очищен").to_string();
                    }
                });
            });
            ui.add_space(4.0);

            ui.horizontal(|ui| {
                for (name, version) in &self.tool_versions {
                    let path = match name.as_str() {
                        "ffmpeg" => &self.tools.ffmpeg,
                        "ffprobe" => &self.tools.ffprobe,
                        _ => &self.tools.mkvmerge,
                    };
                    let found = path.is_some();
                    let color = if found { GREEN } else { RED };
                    let label = format!("{} {}", name, if found { "●" } else { "○" });
                    let response = ui.label(RichText::new(label).color(color).small());
                    if let Some(path) = path {
                        let mut tooltip = path.display().to_string();
                        if let Some(version) = version {
                            tooltip.push('\n');
                            tooltip.push_str(version);
                        }
                        response.on_hover_text(tooltip);
                    }
                }

                let missing = self.tools.missing();
                if !missing.is_empty() {
                    ui.separator();
                    let color = if missing.iter().any(|name| *name != "mkvmerge") {
                        RED
                    } else {
                        ORANGE
                    };
                    ui.label(
                        RichText::new(format!(
                            "{}: {}",
                            tr(lang, "не найдены"),
                            missing.join(", ")
                        ))
                        .color(color)
                        .small(),
                    );
                    if ui.small_button(tr(lang, "Повторить поиск")).clicked() {
                        self.refresh_tools();
                    }
                }
            });

            if self.tools.ffmpeg.is_none() || self.tools.ffprobe.is_none() {
                ui.label(
                    RichText::new(tr(
                        self.lang,
                        "Положите ffmpeg/ffprobe/mkvmerge в папку tools рядом с программой \
                         или задайте TRACKCUT_FFMPEG, TRACKCUT_FFPROBE, TRACKCUT_MKVMERGE.",
                    ))
                    .color(ORANGE)
                    .small(),
                );
            }
        });
    }

    fn show_left(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("left")
            .resizable(true)
            .default_width(300.0)
            .width_range(250.0..=420.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(tr(self.lang, "Язык"))
                            .small()
                            .color(MUTED),
                    );
                    let current = self.lang;
                    egui::ComboBox::from_id_salt("lang")
                        .selected_text(current.label())
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for lang in Lang::all() {
                                if ui
                                    .selectable_label(current == lang, lang.label())
                                    .clicked()
                                    && current != lang
                                {
                                    self.lang = lang;
                                    save_lang(lang);
                                }
                            }
                        });
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr(self.lang, "Файлы")).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{}", self.files.len()))
                                .color(MUTED)
                                .small(),
                        );
                    });
                });

                let mut remove: Option<usize> = None;
                let mut select: Option<usize> = None;
                let busy = self.is_busy();
                let selected_index = self.selected;
                egui::ScrollArea::vertical()
                    .id_salt("files")
                    .max_height(230.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (index, entry) in self.files.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add_enabled(
                                    entry.info.is_some() && !busy,
                                    egui::Checkbox::without_text(&mut entry.include),
                                );

                                let name = entry.name();
                                let short = truncate_chars(&name, 28);
                                let is_selected = index == selected_index;
                                let label = if entry.probing {
                                    format!("{short}  …")
                                } else if entry.error.is_some() {
                                    format!("{short}  ⚠")
                                } else if entry.result.is_some() {
                                    format!("{short}  ✔")
                                } else {
                                    short
                                };
                                let response = ui.selectable_label(is_selected, label);
                                if response.clicked() {
                                    select = Some(index);
                                }
                                response.on_hover_text(entry.path.display().to_string());

                                if ui
                                    .add_enabled(
                                        !busy,
                                        egui::Button::new(
                                            egui::RichText::new("✕").color(RED).small(),
                                        ),
                                    )
                                    .on_hover_text(tr(self.lang, "Убрать файл из списка"))
                                    .clicked()
                                {
                                    remove = Some(index);
                                }
                            });
                        }
                        if self.files.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(tr(
                                    self.lang,
                                    "Перетащите видео сюда или нажмите «Добавить файлы»",
                                ))
                                .color(MUTED)
                                .small(),
                            );
                        }
                    });

                if let Some(index) = select {
                    self.selected = index;
                }
                if let Some(index) = remove {
                    let was_selected = index == self.selected;
                    self.files.remove(index);
                    if was_selected {
                        self.selected = self
                            .files
                            .len()
                            .checked_sub(1)
                            .map(|last| last.min(index))
                            .unwrap_or(0);
                    } else if index < self.selected {
                        self.selected -= 1;
                    }
                }

                ui.separator();
                ui.label(RichText::new(tr(self.lang, "Выходной файл")).strong());
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.label(tr(self.lang, "Контейнер"));
                    egui::ComboBox::from_id_salt("container")
                        .selected_text(match self.output_container {
                            ContainerChoice::Auto => tr(self.lang, "как у входа"),
                            ContainerChoice::Mkv => "MKV",
                            ContainerChoice::Webm => "WebM",
                            ContainerChoice::Mp4 => "MP4",
                            ContainerChoice::Mov => "MOV",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.output_container,
                                ContainerChoice::Auto,
                                tr(self.lang, "как у входа"),
                            );
                            ui.selectable_value(
                                &mut self.output_container,
                                ContainerChoice::Mkv,
                                "MKV",
                            );
                            ui.selectable_value(
                                &mut self.output_container,
                                ContainerChoice::Webm,
                                "WebM",
                            );
                            ui.selectable_value(
                                &mut self.output_container,
                                ContainerChoice::Mp4,
                                "MP4",
                            );
                            ui.selectable_value(
                                &mut self.output_container,
                                ContainerChoice::Mov,
                                "MOV",
                            );
                        });
                });

                ui.horizontal(|ui| {
                    let text = self
                        .output_dir
                        .as_ref()
                        .map(|dir| truncate_chars(&dir.display().to_string(), 22))
                        .unwrap_or_else(|| tr(self.lang, "рядом с исходником").to_string());
                    ui.label(RichText::new(text).small().color(MUTED))
                        .on_hover_text(
                            self.output_dir
                                .as_ref()
                                .map(|dir| dir.display().to_string())
                                .unwrap_or_else(|| tr(self.lang, "рядом с исходником").to_string()),
                        );
                    if ui.small_button(tr(self.lang, "Папка…")).clicked() {
                        self.pick_output_dir();
                    }
                    if self.output_dir.is_some() && ui.small_button("×").clicked() {
                        self.output_dir = None;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(tr(self.lang, "Суффикс"));
                    ui.add(egui::TextEdit::singleline(&mut self.suffix).desired_width(110.0));
                });

                ui.add_space(2.0);
                ui.checkbox(
                    &mut self.keep_chapters,
                    tr(self.lang, "Сохранять главы"),
                );
                ui.checkbox(&mut self.keep_tags, tr(self.lang, "Сохранять теги"));
                ui.checkbox(
                    &mut self.keep_attachments,
                    tr(self.lang, "Сохранять вложения (шрифты ASS)"),
                );
                ui.checkbox(
                    &mut self.mov_text,
                    tr(self.lang, "Конвертировать субтитры в MOV text (MP4)"),
                )
                .on_hover_text(tr(
                    self.lang,
                    "Единственное исключение из правила «без перекодирования»: SRT/ASS → MOV text",
                ));
                ui.checkbox(
                    &mut self.overwrite,
                    tr(self.lang, "Перезаписывать готовый файл"),
                )
                    .on_hover_text(tr(
                        self.lang,
                        "Исходник не изменяется никогда: результат всегда пишется в отдельный файл. Галочка лишь разрешает заменить уже существующий результат с тем же именем — иначе рядом появится файл с припиской (2).",
                    ));

                ui.add_space(6.0);
                ui.label(RichText::new(tr(self.lang, "Нарезка")).strong().small());
                egui::ComboBox::from_id_salt("split_kind")
                    .selected_text(self.split.label(self.lang))
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for choice in [SplitChoice::None, SplitChoice::Every, SplitChoice::Ranges] {
                            ui.selectable_value(&mut self.split, choice, choice.label(self.lang));
                        }
                    });
                match self.split {
                    SplitChoice::None => {}
                    SplitChoice::Every => {
                        ui.horizontal(|ui| {
                            ui.label(tr(self.lang, "Каждые"));
                            ui.add(
                                egui::DragValue::new(&mut self.split_minutes)
                                    .range(1.0..=240.0)
                                    .speed(1.0)
                                    .suffix(format!(" {}", tr(self.lang, "мин"))),
                            );
                        });
                    }
                    SplitChoice::Ranges => {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.split_ranges)
                                .hint_text("00:10:00-00:20:00, 00:30:00-")
                                .desired_width(190.0),
                        );
                        ui.label(
                            RichText::new(tr(
                                self.lang,
                                "Начало-конец через запятую; пустой конец — до конца файла",
                            ))
                            .small()
                            .color(MUTED),
                        );
                    }
                }
                if self.split != SplitChoice::None {
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Только MKV/WebM · резка по ключевым кадрам",
                        ))
                        .small()
                        .color(MUTED),
                    );
                }

                ui.add_space(6.0);
                if ui
                    .add_enabled(
                        self.files
                            .get(self.selected)
                            .is_some_and(|f| f.info.is_some()),
                        egui::Button::new(tr(self.lang, "Применить правило ко всем")),
                    )
                    .clicked()
                {
                    self.apply_to_all();
                }
            });
    }

    fn show_center(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.files.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    ui.label(
                        RichText::new(tr(self.lang, "Перетащите MKV, MP4, MOV, WebM или AVI"))
                            .size(20.0),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Покажу все дорожки: язык, название перевода, кодек, каналы и размер. \
                             Отметьте ненужные — остальное скопируется без перекодирования.",
                        ))
                        .color(MUTED),
                    );
                    ui.add_space(14.0);
                    if ui
                        .button(RichText::new(tr(self.lang, "Открыть файл…")).size(16.0))
                        .clicked()
                    {
                        self.pick_files();
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Ctrl+O — открыть · Ctrl+Enter — собрать · Esc — отмена",
                        ))
                        .small()
                        .color(MUTED),
                    );
                });
                return;
            }

            if self.selected >= self.files.len() {
                self.selected = self.files.len() - 1;
            }

            let preview_token = self.preview.token;
            let busy = self.is_busy();
            let mut preset_action: Option<Preset> = None;
            let mut preview_request: Option<usize> = None;
            let mut tags_request: Option<usize> = None;
            let mut extract_request: Option<usize> = None;
            let mut extra_preview: Option<(usize, usize)> = None;
            let mut extra_sync: Option<(usize, usize)> = None;
            let mut sync_open: Option<(usize, usize)> = None;
            let mut sync_internal: Option<usize> = None;
            let mut extra_remove: Option<(usize, usize)> = None;
            let mut extra_pick = false;
            let mut apply_all = false;
            let mut chapters_pick = false;
            let mut chapters_clear = false;
            let mut append_pick = false;
            let mut append_remove: Option<usize> = None;

            let file_size;
            {
                let entry = &mut self.files[self.selected];
                file_size = entry.file_size;
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(entry.name()).size(18.0).strong());
                    if entry.probing {
                        ui.spinner();
                        ui.label(RichText::new(tr(self.lang, "читаю метаданные…")).color(MUTED));
                    }
                });
                ui.label(
                    RichText::new(entry.path.display().to_string())
                        .small()
                        .color(MUTED),
                );

                if let Some(error) = &entry.error {
                    ui.add_space(8.0);
                    ui.colored_label(
                        RED,
                        format!(
                            "{}: {error}",
                            tr(self.lang, "Не удалось прочитать файл")
                        ),
                    );
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Проверьте, что установлены ffprobe и mkvmerge (папка tools или PATH).",
                        ))
                        .small()
                        .color(MUTED),
                    );
                    return;
                }

                let Some(info) = &entry.info else {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(tr(self.lang, "Читаю дорожки…")).color(MUTED));
                    });
                    return;
                };

                ui.add_space(6.0);
                let meta = format!(
                    "{}  ·  {}  ·  {}  ·  {} {}",
                    if info.container.is_empty() {
                        tr(self.lang, "контейнер неизвестен")
                    } else {
                        &info.container
                    },
                    info.duration_ms
                        .map(format_duration)
                        .unwrap_or_else(|| tr(self.lang, "длительность неизвестна").to_string()),
                    format_size(entry.file_size),
                    tr(self.lang, "дорожек:"),
                    info.streams.len()
                );
                ui.label(RichText::new(meta).color(MUTED).small());

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text(tr(self.lang, "Фильтр: язык, название, кодек…"))
                            .desired_width(230.0),
                    );
                    ui.separator();
                    if ui.button(tr(self.lang, "Видео + лучшая аудио")).clicked() {
                        preset_action = Some(Preset::BestAudio);
                    }
                    if ui.button(tr(self.lang, "Видео + аудио")).clicked() {
                        preset_action = Some(Preset::VideoAudio);
                    }
                    if ui.button(tr(self.lang, "Всё")).clicked() {
                        preset_action = Some(Preset::Everything);
                    }
                    if ui.button(tr(self.lang, "Только русский")).clicked() {
                        preset_action = Some(Preset::RussianOnly);
                    }
                    if ui.button(tr(self.lang, "Снять все")).clicked() {
                        preset_action = Some(Preset::Nothing);
                    }
                    if ui.button(tr(self.lang, "Правило → все файлы")).clicked() {
                        apply_all = true;
                    }
                });

                let filter = self.filter.trim().to_ascii_lowercase();
                let kept = entry.kept_count();
                let total = info.streams.iter().filter(|s| s.kind.is_selectable()).count();
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} {kept} {} {total} {}",
                            tr(self.lang, "Оставлено"),
                            tr(self.lang, "из"),
                            tr(self.lang, "дорожек")
                        ))
                        .color(if kept == 0 { RED } else { MUTED })
                        .small(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "· ~{} {}",
                            format_size(total_kept_bytes(entry)),
                            tr(self.lang, "в выходе")
                        ))
                        .color(MUTED)
                        .small(),
                    );
                });

                ui.add_space(4.0);
                egui::TopBottomPanel::bottom("center_lower")
                    .frame(egui::Frame::NONE)
                    .show_inside(ui, |ui| {
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr(self.lang, "Главы")).strong());
                    if info.chapters_count > 0 {
                        ui.label(
                            RichText::new(format!("{}", info.chapters_count))
                                .color(MUTED)
                                .small(),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(file) = &entry.chapters_file {
                            if ui
                                .small_button("×")
                                .on_hover_text(tr(self.lang, "Не импортировать главы"))
                                .clicked()
                            {
                                chapters_clear = true;
                            }
                            let name = entry
                                .chapters_label
                                .clone()
                                .unwrap_or_else(|| file.display().to_string());
                            ui.label(
                                RichText::new(truncate_chars(&name, 28))
                                    .color(ACCENT)
                                    .small(),
                            )
                            .on_hover_text(file.display().to_string());
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new(tr(self.lang, "Импорт глав…")))
                            .on_hover_text(tr(
                                self.lang,
                                "Взять главы из другого MKV или из файла глав (XML/OGM)",
                            ))
                            .clicked()
                        {
                            chapters_pick = true;
                        }
                    });
                });
                if info.chapters.is_empty() {
                    ui.label(
                        RichText::new(if info.chapters_count > 0 {
                            tr(
                                self.lang,
                                "Список глав не прочитан — mkvmerge скопирует их как есть.",
                            )
                        } else {
                            tr(
                                self.lang,
                                "Глав нет. Можно импортировать из другого файла.",
                            )
                        })
                        .small()
                        .color(MUTED),
                    );
                } else {
                    egui::CollapsingHeader::new(format!(
                        "{} ({})",
                        tr(self.lang, "Список глав"),
                        info.chapters.len()
                    ))
                        .id_salt("chapters")
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("chapters_scroll")
                                .max_height(110.0)
                                .show(ui, |ui| {
                                    for (index, chapter) in info.chapters.iter().enumerate() {
                                        ui.label(
                                            RichText::new(format!(
                                                "{:>2}. {}",
                                                index + 1,
                                                chapter.label()
                                            ))
                                            .small(),
                                        );
                                    }
                                });
                        });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr(self.lang, "Дорожки из других файлов")).strong());
                    let extra_count: usize =
                        entry.extras.iter().map(|extra| extra.tracks.len()).sum();
                    if extra_count > 0 {
                        ui.label(
                            RichText::new(format!("+{extra_count}"))
                                .color(GREEN)
                                .small(),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!busy, egui::Button::new(tr(self.lang, "＋ Взять из файла…")))
                            .on_hover_text(tr(self.lang, "Пришить аудио или субтитры из другого релиза"))
                            .clicked()
                        {
                            extra_pick = true;
                        }
                    });
                });

                if entry.extras.is_empty() {
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Например, озвучка из другого релиза: дорожка скопируется в результат без перекодирования. Сдвиг в мс поправит рассинхрон.",
                        ))
                        .small()
                        .color(MUTED),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("extras_scroll")
                        .max_height(152.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                    for (source_index, extra) in entry.extras.iter_mut().enumerate() {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("📄 {}", truncate_chars(&extra.name(), 44)))
                                    .color(ACCENT)
                                    .small(),
                            )
                            .on_hover_text(extra.info.path.display().to_string());
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    tr(self.lang, "дорожек:"),
                                    extra.tracks.len()
                                ))
                                .small()
                                .color(MUTED),
                            );
                        });

                        for (track_index, track) in extra.tracks.iter_mut().enumerate() {
                            let stream = extra.info.streams.get(track.uid).cloned();
                            ui.horizontal_wrapped(|ui| {
                                let Some(stream) = &stream else {
                                    ui.label(
                                        RichText::new(tr(self.lang, "дорожка не найдена в файле"))
                                            .color(RED)
                                            .small(),
                                    );
                                    return;
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{} {}",
                                        stream.kind.icon(),
                                        kind_label(self.lang, stream.kind)
                                    ))
                                    .color(kind_color(stream.kind))
                                    .small(),
                                );
                                ui.label(RichText::new(stream.codec_display()).small());
                                    ui.label(
                                        RichText::new(truncate_chars(
                                            &trackcut_core::extract::track_caption(stream),
                                            30,
                                        ))
                                        .small(),
                                    );
                                ui.label(RichText::new(tr(self.lang, "сдвиг")).small().color(MUTED));
                                ui.add(
                                    egui::DragValue::new(&mut track.delay_ms)
                                        .speed(10.0)
                                        .range(-600_000..=600_000)
                                        .suffix(format!(" {}", tr(self.lang, "мс"))),
                                )
                                .on_hover_text(tr(
                                    self.lang,
                                    "Положительное значение — дорожка зазвучит позже",
                                ));
                                ui.checkbox(&mut track.default, tr(self.lang, "По умолч.")).on_hover_text(
                                    tr(self.lang, "Пометить дорожку как выбранную по умолчанию"),
                                );
                                if stream.kind == StreamKind::Audio && stream.ff_index.is_some() {
                                    if track.syncing {
                                        ui.add(egui::Spinner::new().size(12.0));
                                        ui.label(
                                            RichText::new(tr(self.lang, "сравниваю…"))
                                                .small()
                                                .color(MUTED),
                                        );
                                    } else if ui
                                        .add_enabled(!busy, egui::Button::new("⏱").small())
                                        .on_hover_text(tr(
                                            self.lang,
                                            "Подобрать сдвиг по звуку: сравнить с аудиодорожкой основного файла",
                                        ))
                                        .clicked()
                                    {
                                        extra_sync = Some((source_index, track.uid));
                                    }
                                    let playing = preview_token
                                        == Some(extra_token(source_index, track.uid));
                                    let label = if playing { "■" } else { "▶" };
                                    if ui
                                        .add(egui::Button::new(label).small())
                                        .on_hover_text(tr(self.lang, "Прослушать 15 секунд"))
                                        .clicked()
                                    {
                                        extra_preview = Some((source_index, track_index));
                                    }
                                    if ui
                                        .add_enabled(!busy, egui::Button::new(tr(self.lang, "кадр")).small())
                                        .on_hover_text(tr(
                                            self.lang,
                                            "Окно синхрона: кадр из видео, обе дорожки вместе и подгонка сдвига",
                                        ))
                                        .clicked()
                                    {
                                        sync_open = Some((source_index, track.uid));
                                    }
                                }
                                if ui
                                    .add_enabled(
                                        !busy,
                                        egui::Button::new(
                                            egui::RichText::new("✕").color(RED).small(),
                                        ),
                                    )
                                    .on_hover_text(tr(self.lang, "Убрать дорожку из результата"))
                                    .clicked()
                                {
                                    extra_remove = Some((source_index, track_index));
                                }
                            });
                            if let Some(note) = &track.sync_note {
                                ui.horizontal(|ui| {
                                    ui.add_space(18.0);
                                    let color = if note.starts_with(tr(self.lang, "не удалось")) { RED } else { MUTED };
                                    ui.label(RichText::new(note).small().color(color));
                                });
                            }
                        }
                    }
                        });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr(self.lang, "Склейка (append)")).strong());
                    if !entry.append.is_empty() {
                        ui.label(
                            RichText::new(format!("+{}", entry.append.len()))
                                .color(GREEN)
                                .small(),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!busy, egui::Button::new(tr(self.lang, "＋ Приклеить файл…")))
                            .on_hover_text(tr(
                                self.lang,
                                "Дописать другой файл в конец результата. Кодеки и параметры \
                                 дорожек должны совпадать",
                            ))
                            .clicked()
                        {
                            append_pick = true;
                        }
                    });
                });
                if entry.append.is_empty() {
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Например, вторая часть фильма или продолжение серии — склеится в один файл без перекодирования.",
                        ))
                        .small()
                        .color(MUTED),
                    );
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("append_scroll")
                        .max_height(96.0)
                        .show(ui, |ui| {
                    for (index, path) in entry.append.iter().enumerate() {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{}. {}",
                                    index + 1,
                                    truncate_chars(&name, 44)
                                ))
                                .small(),
                            )
                            .on_hover_text(path.display().to_string());
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("✕").color(RED).small(),
                                    )
                                    .small(),
                                )
                                .on_hover_text(tr(self.lang, "Убрать файл из склейки"))
                                .clicked()
                            {
                                append_remove = Some(index);
                            }
                        });
                    }
                        });
                }
                    });

                egui::Frame::group(ui.style()).show(ui, |ui| {
                    let table_height = ui.available_height().max(120.0);
                    egui::ScrollArea::both()
                        .id_salt("tracks")
                        .auto_shrink([false, false])
                        .max_height(table_height)
                        .show(ui, |ui| {
                            egui::Grid::new("track_grid")
                                .striped(true)
                                .num_columns(13)
                                .spacing([10.0, 6.0])
                                .show(ui, |ui| {
                                    for header in [
                                        "", tr(self.lang, "Тип"), "№", tr(self.lang, "Кодек"), tr(self.lang, "Язык дорожки"), tr(self.lang, "Название"),
                                        tr(self.lang, "Параметры"), tr(self.lang, "Размер"), tr(self.lang, "По умолч."), tr(self.lang, "Принуд."), tr(self.lang, "Файл"), "", "",
                                    ] {
                                        ui.label(RichText::new(header).strong().small());
                                    }
                                    ui.end_row();

                                    for (uid, stream) in info.streams.iter().enumerate() {
                                        let haystack = format!(
                                            "{} {} {} {}",
                                            stream_name(self.lang, stream),
                                            stream_language(self.lang, stream),
                                            stream.codec_display(),
                                            kind_label(self.lang, stream.kind)
                                        )
                                        .to_ascii_lowercase();
                                        if !filter.is_empty() && !haystack.contains(&filter) {
                                            continue;
                                        }

                                        let selectable = stream.kind.is_selectable();
                                        let is_kept =
                                            entry.keep.get(uid).copied().unwrap_or(false);

                                        ui.add_enabled(
                                            selectable,
                                            egui::Checkbox::without_text(&mut entry.keep[uid]),
                                        );

                                        ui.label(
                                            RichText::new(format!(
                                                "{} {}",
                                                stream.kind.icon(),
                                                kind_label(self.lang, stream.kind)
                                            ))
                                            .color(kind_color(stream.kind))
                                            .small(),
                                        );

                                        ui.label(
                                            RichText::new(format!("{}", uid + 1))
                                                .small()
                                                .color(MUTED),
                                        );

                                        ui.label(stream.codec_display());

                                        ui.label(stream_language(self.lang, stream));

                                        let (name_text, color) = if stream.has_name() {
                                            (stream.display_name().to_string(), Color32::WHITE)
                                        } else {
                                            (tr(self.lang, "Без названия").to_string(), MUTED)
                                        };
                                        ui.label(
                                            RichText::new(truncate_chars(&name_text, 34))
                                                .color(color),
                                        )
                                        .on_hover_text(name_text);

                                        ui.label(
                                            RichText::new(params_for(self.lang, stream))
                                                .small()
                                                .color(MUTED),
                                        );

                                        let size = stream
                                            .effective_size(info.duration_ms)
                                            .map(|bytes| {
                                                let percent = if file_size > 0 {
                                                    bytes as f64 / file_size as f64 * 100.0
                                                } else {
                                                    0.0
                                                };
                                                let approx = if stream.size_bytes.is_none()
                                                    && stream.bitrate.is_some()
                                                {
                                                    "≈"
                                                } else {
                                                    ""
                                                };
                                                format!(
                                                    "{approx}{} ({percent:.1}%)",
                                                    format_size(bytes)
                                                )
                                            })
                                            .unwrap_or_default();
                                        ui.label(RichText::new(size).small());

                                        ui.add_enabled(
                                            selectable && is_kept,
                                            egui::Checkbox::without_text(
                                                &mut entry.default_flag[uid],
                                            ),
                                        );
                                        ui.add_enabled(
                                            selectable && is_kept,
                                            egui::Checkbox::without_text(
                                                &mut entry.forced_flag[uid],
                                            ),
                                        );

                                        if stream.kind == StreamKind::Audio
                                            || stream.kind == StreamKind::Subtitle
                                        {
                                            let ext = if stream.kind == StreamKind::Audio {
                                                "mka"
                                            } else {
                                                "mks"
                                            };
                                            let response = ui
                                                .add_enabled(
                                                    !busy,
                                                    egui::Button::new(
                                                        egui::RichText::new(ext)
                                                            .color(ACCENT)
                                                            .small(),
                                                    ),
                                                )
                                                .on_hover_text(format!(
                                                    "{} .{ext} — {}",
                                                    tr(self.lang, "Сохранить дорожку отдельным файлом"),
                                                    tr(self.lang, "чтобы потом пришить к другому видео")
                                                ));
                                            if response.clicked() {
                                                extract_request = Some(uid);
                                            }
                                        } else {
                                            ui.label("");
                                        }

                                        if stream.kind == StreamKind::Audio
                                            && stream.ff_index.is_some()
                                        {
                                            let playing = preview_token == Some(main_token(uid));
                                            let label = if playing { "■" } else { "▶" };
                                            let response = ui
                                                .add(egui::Button::new(label).small())
                                                .on_hover_text(tr(
                                                    self.lang,
                                                    "Прослушать 15 секунд этой дорожки",
                                                ));
                                            if response.clicked() {
                                                preview_request = Some(uid);
                                            }
                                        } else {
                                            ui.label("");
                                        }

                                        ui.horizontal(|ui| {
                                            let delay =
                                                entry.keep_delay.get(uid).copied().unwrap_or(0);
                                            let shiftable = stream.kind == StreamKind::Audio
                                                || stream.kind == StreamKind::Subtitle;
                                            if shiftable {
                                                let shifted = delay != 0;
                                                let label = if shifted {
                                                    format!("{} {delay:+}", tr(self.lang, "сдвиг"))
                                                } else {
                                                    tr(self.lang, "сдвиг").to_string()
                                                };
                                                let button = egui::Button::new(label).small();
                                                let button = if shifted {
                                                    button.fill(ACCENT.gamma_multiply(0.35))
                                                } else {
                                                    button
                                                };
                                                if ui
                                                    .add(button)
                                                    .on_hover_text(if shifted {
                                                        format!(
                                                            "{} {delay:+} {}. {}",
                                                            tr(self.lang, "Дорожка сдвинута на"),
                                                            tr(self.lang, "мс"),
                                                            tr(self.lang, "Открыть окно синхрона")
                                                        )
                                                    } else {
                                                        tr(
                                                            self.lang,
                                                            "Окно синхрона: кадр из видео и сдвиг этой дорожки относительно остальных",
                                                        )
                                                        .to_string()
                                                    })
                                                    .clicked()
                                                {
                                                    sync_internal = Some(uid);
                                                }
                                            }
                                            if ui
                                                .add(egui::Button::new("🏷").small())
                                                .on_hover_text(tr(self.lang, "Все теги дорожки"))
                                                .clicked()
                                            {
                                                tags_request = Some(uid);
                                            }
                                        });

                                        ui.end_row();
                                    }
                                });
                        });
                });

                let container = self.output_container.resolve(&entry.path);
                if container.is_mp4_family()
                    && info.streams.iter().enumerate().any(|(uid, s)| {
                        entry.keep.get(uid).copied().unwrap_or(false)
                            && s.kind == StreamKind::Subtitle
                            && s.is_text_subtitle()
                    })
                    && !self.mov_text
                {
                    ui.add_space(4.0);
                    ui.colored_label(
                        ORANGE,
                        tr(
                            self.lang,
                            "Для MP4 текст субтитров нужно конвертировать в MOV text — включите опцию слева.",
                        ),
                    );
                }
            }

            if let Some(preset) = preset_action {
                self.apply_preset(preset);
            }
            if apply_all {
                self.apply_to_all();
            }
            if let Some(uid) = preview_request {
                self.preview_stream(uid);
            }
            if let Some(uid) = tags_request {
                self.open_tags(uid);
            }
            if let Some(uid) = extract_request {
                self.extract_track(uid);
            }
            if extra_pick {
                self.pick_extra_file();
            }
            if let Some((source, track)) = extra_remove {
                if let Some(entry) = self.files.get_mut(self.selected) {
                    if let Some(extra) = entry.extras.get_mut(source) {
                        if track < extra.tracks.len() {
                            extra.tracks.remove(track);
                        }
                    }
                    entry.extras.retain(|extra| !extra.tracks.is_empty());
                }
                self.status = tr(self.lang, "Внешняя дорожка убрана").to_string();
            }
            if let Some((source, uid)) = extra_preview {
                self.preview_extra(source, uid);
            }
            if let Some((source, uid)) = extra_sync {
                self.estimate_extra_delay(source, uid);
            }
            if let Some((source, uid)) = sync_open {
                self.open_sync_window(source, uid, false);
            }
            if let Some(uid) = sync_internal {
                self.open_sync_window(0, uid, true);
            }
            if chapters_clear {
                if let Some(entry) = self.files.get_mut(self.selected) {
                    entry.chapters_file = None;
                    entry.chapters_label = None;
                }
                self.status = tr(self.lang, "Импорт глав отменён").into();
            }
            if chapters_pick {
                self.pick_chapters_file();
            }
            if append_pick {
                self.pick_append_files();
            }
            if let Some(index) = append_remove {
                if let Some(entry) = self.files.get_mut(self.selected) {
                    if index < entry.append.len() {
                        entry.append.remove(index);
                    }
                }
                self.status = tr(self.lang, "Файл убран из склейки").into();
            }
        });
    }

    fn show_bottom(&mut self, ctx: &egui::Context) {
        let job_state = self.job.clone();
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.add_space(4.0);
            match &job_state {
                JobState::Idle => {
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Готов к сборке: отметьте дорожки и нажмите «Собрать»",
                        ))
                        .color(MUTED)
                        .small(),
                    );
                }
                JobState::Running {
                    index,
                    total,
                    name,
                    percent,
                } => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{index}/{total} · {name}"))
                                .small()
                                .color(MUTED),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button(tr(self.lang, "Отмена")).clicked() {
                                self.request_cancel();
                            }
                        });
                    });
                    ui.add(
                        egui::ProgressBar::new(*percent)
                            .show_percentage()
                            .animate(true),
                    );
                }
                JobState::Done { ok, failed } => {
                    let (text, color) = if *failed == 0 {
                        (
                            format!(
                                "{}: {ok} {}",
                                tr(self.lang, "Готово"),
                                tr(self.lang, "файл(ов) собрано")
                            ),
                            GREEN,
                        )
                    } else {
                        (
                            format!(
                                "{}: {} {ok}, {} {failed}",
                                tr(self.lang, "Завершено"),
                                tr(self.lang, "успешно"),
                                tr(self.lang, "с ошибками")
                            ),
                            ORANGE,
                        )
                    };
                    ui.label(RichText::new(text).color(color).small());
                }
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let enabled = self.can_mux();
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(
                            RichText::new(format!("▶  {}", tr(self.lang, "Собрать"))).strong(),
                        )
                        .min_size(egui::vec2(130.0, 30.0)),
                    )
                    .clicked()
                {
                    self.start_mux();
                }
                if self.is_busy() && ui.button(tr(self.lang, "Остановить")).clicked() {
                    self.request_cancel();
                }

                ui.label(RichText::new(self.status.clone()).color(MUTED).small());

                if self.preview.token.is_some() {
                    ui.separator();
                    ui.label(
                        RichText::new(tr(self.lang, "прослушивание включено"))
                            .color(ACCENT)
                            .small(),
                    );
                    if ui.small_button(tr(self.lang, "стоп")).clicked() {
                        self.preview.stop();
                    }
                }
            });

            let title = format!("{} ({})", tr(self.lang, "Лог"), self.log.len());
            egui::CollapsingHeader::new(RichText::new(title).small())
                .id_salt("log")
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("log_scroll")
                        .max_height(130.0)
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for line in self.log.iter().rev().take(400).rev() {
                                ui.label(RichText::new(line).monospace().small());
                            }
                            if self.log.is_empty() {
                                ui.label(
                                    RichText::new(tr(self.lang, "пусто")).small().color(MUTED),
                                );
                            }
                        });
                });
            ui.add_space(2.0);
        });
    }

    fn show_tags(&mut self, ctx: &egui::Context) {
        let Some(uid) = self.tags_open else { return };
        let Some(stream) = self
            .files
            .get(self.selected)
            .and_then(|file| file.info.as_ref())
            .and_then(|info| info.streams.get(uid))
            .cloned()
        else {
            self.tags_open = None;
            return;
        };

        let mut open = true;
        egui::Window::new(format!(
            "{} {} · {}",
            tr(self.lang, "Дорожка"),
            uid + 1,
            stream_name(self.lang, &stream)
        ))
        .open(&mut open)
        .default_width(560.0)
        .default_height(420.0)
        .resizable(true)
        .show(ctx, |ui| {
            egui::Grid::new("tags_grid")
                .num_columns(2)
                .spacing([16.0, 4.0])
                .show(ui, |ui| {
                    for (key, value) in [
                        (
                            tr(self.lang, "Тип"),
                            kind_label(self.lang, stream.kind).to_string(),
                        ),
                        (tr(self.lang, "Кодек"), stream.codec_display()),
                        (tr(self.lang, "Сырой кодек"), stream.codec_raw.clone()),
                        (tr(self.lang, "Язык"), stream_language(self.lang, &stream)),
                        (tr(self.lang, "Название"), stream_name(self.lang, &stream)),
                        (tr(self.lang, "Каналы"), stream_channels(self.lang, &stream)),
                        ("Layout", stream.channel_layout.clone().unwrap_or_default()),
                        (tr(self.lang, "Частота"), stream.sample_rate_label()),
                        (tr(self.lang, "Разрешение"), stream.resolution_label()),
                        ("FPS", stream.fps_label()),
                        (tr(self.lang, "Битрейт"), stream.bitrate_label()),
                        (
                            tr(self.lang, "Длительность"),
                            stream.duration_ms.map(format_duration).unwrap_or_default(),
                        ),
                        (
                            tr(self.lang, "Размер"),
                            stream
                                .size_bytes
                                .map(format_size)
                                .unwrap_or_else(|| "—".to_string()),
                        ),
                        (
                            tr(self.lang, "По умолч."),
                            yes_no(self.lang, stream.default),
                        ),
                        (tr(self.lang, "Принуд."), yes_no(self.lang, stream.forced)),
                    ] {
                        if value.is_empty() {
                            continue;
                        }
                        ui.label(RichText::new(key).color(MUTED).small());
                        ui.label(RichText::new(value).small());
                        ui.end_row();
                    }
                });

            ui.separator();
            ui.label(RichText::new(tr(self.lang, "Теги из контейнера")).strong());
            if stream.tags.is_empty() {
                ui.label(
                    RichText::new(tr(self.lang, "Тегов нет"))
                        .color(MUTED)
                        .small(),
                );
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("tags_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("tags_values")
                            .num_columns(2)
                            .striped(true)
                            .spacing([14.0, 4.0])
                            .show(ui, |ui| {
                                let mut tags: Vec<_> = stream.tags.iter().collect();
                                tags.sort_by(|a, b| a.0.cmp(b.0));
                                for (key, value) in tags {
                                    ui.label(RichText::new(key).small().color(ACCENT));
                                    ui.label(RichText::new(value).small());
                                    ui.end_row();
                                }
                            });
                    });
            }
        });

        if !open {
            self.tags_open = None;
        }
    }

    fn show_extra_picker(&mut self, ctx: &egui::Context) {
        let Some(mut pick) = self.extra_pick.take() else {
            return;
        };

        let mut open = true;
        let mut add = false;
        let mut close = false;

        egui::Window::new(tr(self.lang, "Взять дорожку из другого файла"))
            .open(&mut open)
            .default_width(780.0)
            .default_height(460.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(pick.path.display().to_string())
                        .small()
                        .color(MUTED),
                );
                ui.add_space(4.0);

                if let Some(err) = &pick.error {
                    ui.colored_label(
                        RED,
                        format!("{}: {err}", tr(self.lang, "Не удалось прочитать файл")),
                    );
                    ui.label(
                        RichText::new(tr(
                            self.lang,
                            "Нужны ffprobe или mkvmerge: папка tools рядом с программой или PATH.",
                        ))
                        .small()
                        .color(MUTED),
                    );
                    return;
                }

                let Some(info) = pick.info.clone() else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(tr(self.lang, "Читаю дорожки…")).color(MUTED));
                    });
                    return;
                };

                if pick.selected.len() != info.streams.len() {
                    pick.selected = default_extra_selection(&info);
                }

                egui::ScrollArea::vertical()
                    .id_salt("extra_pick_scroll")
                    .max_height(280.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("extra_pick_grid")
                            .num_columns(6)
                            .striped(true)
                            .spacing([10.0, 6.0])
                            .show(ui, |ui| {
                                for header in [
                                    "",
                                    tr(self.lang, "Тип"),
                                    tr(self.lang, "Кодек"),
                                    tr(self.lang, "Язык дорожки"),
                                    tr(self.lang, "Название"),
                                    tr(self.lang, "Параметры"),
                                ] {
                                    ui.label(RichText::new(header).strong().small());
                                }
                                ui.end_row();

                                for (uid, stream) in info.streams.iter().enumerate() {
                                    let selectable = stream.kind.is_selectable();
                                    ui.add_enabled(
                                        selectable,
                                        egui::Checkbox::without_text(&mut pick.selected[uid]),
                                    );
                                    ui.label(
                                        RichText::new(format!(
                                            "{} {}",
                                            stream.kind.icon(),
                                            kind_label(self.lang, stream.kind)
                                        ))
                                        .color(kind_color(stream.kind))
                                        .small(),
                                    );
                                    ui.label(stream.codec_display());
                                    ui.label(stream_language(self.lang, stream));
                                    ui.label(truncate_chars(&stream_name(self.lang, stream), 34));
                                    ui.label(
                                        RichText::new(params_for(self.lang, stream))
                                            .small()
                                            .color(MUTED),
                                    );
                                    ui.end_row();
                                }
                            });
                    });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.small_button(tr(self.lang, "Только аудио")).clicked() {
                        for (uid, stream) in info.streams.iter().enumerate() {
                            pick.selected[uid] = stream.kind == StreamKind::Audio;
                        }
                    }
                    if ui.small_button(tr(self.lang, "Снять все")).clicked() {
                        pick.selected.iter_mut().for_each(|value| *value = false);
                    }
                    let chosen = pick.selected.iter().filter(|value| **value).count();
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} {chosen}", tr(self.lang, "выбрано:")))
                            .small()
                            .color(MUTED),
                    );
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let chosen = pick.selected.iter().filter(|value| **value).count();
                    if ui
                        .add_enabled(
                            chosen > 0,
                            egui::Button::new(RichText::new(tr(self.lang, "Добавить")).strong()),
                        )
                        .clicked()
                    {
                        add = true;
                    }
                    if ui.button(tr(self.lang, "Отмена")).clicked() {
                        close = true;
                    }
                });
            });

        if add {
            let Some(info) = pick.info.clone() else {
                self.extra_pick = Some(pick);
                return;
            };
            let tracks: Vec<ExtraTrackUi> = info
                .streams
                .iter()
                .enumerate()
                .filter(|(uid, stream)| {
                    pick.selected.get(*uid).copied().unwrap_or(false) && stream.kind.is_selectable()
                })
                .map(|(uid, _)| ExtraTrackUi {
                    uid,
                    delay_ms: 0,
                    default: false,
                    syncing: false,
                    sync_note: None,
                })
                .collect();

            if tracks.is_empty() {
                self.status = tr(self.lang, "Ничего не выбрано").to_string();
                self.extra_pick = Some(pick);
                return;
            }

            if let Some(entry) = self.files.get_mut(self.selected) {
                if let Some(existing) = entry
                    .extras
                    .iter_mut()
                    .find(|extra| paths_equal(&extra.info.path, &info.path))
                {
                    for track in tracks {
                        if !existing.tracks.iter().any(|item| item.uid == track.uid) {
                            existing.tracks.push(track);
                        }
                    }
                } else {
                    entry.extras.push(ExtraSource {
                        info: info.clone(),
                        tracks,
                    });
                }
                let added: usize = entry.extras.iter().map(|extra| extra.tracks.len()).sum();
                self.status = format!("{} {added}", tr(self.lang, "Внешних дорожек:"));
            }
            return;
        }

        if close || !open {
            return;
        }
        self.extra_pick = Some(pick);
    }

    fn show_drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|input| !input.raw.hovered_files.is_empty());
        if !hovering || self.is_busy() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        let screen = ctx.screen_rect();
        painter.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
        painter.text(
            screen.center(),
            egui::Align2::CENTER_CENTER,
            tr(self.lang, "Отпустите файлы, чтобы добавить"),
            egui::FontId::proportional(26.0),
            Color32::WHITE,
        );
    }
}

/// Ключ прослушивания дорожки основного файла.
fn main_token(uid: usize) -> u64 {
    uid as u64
}

/// Ключ прослушивания внешней дорожки (файл + номер дорожки).
fn extra_token(source: usize, uid: usize) -> u64 {
    (1u64 << 40) + source as u64 * 1000 + uid as u64
}

/// Что отметить по умолчанию при выборе внешнего файла.
fn default_extra_selection(info: &MediaInfo) -> Vec<bool> {
    let has_audio = info
        .streams
        .iter()
        .any(|stream| stream.kind == StreamKind::Audio);
    info.streams
        .iter()
        .map(|stream| match stream.kind {
            StreamKind::Audio => has_audio,
            StreamKind::Subtitle => !has_audio,
            _ => false,
        })
        .collect()
}

/// Лучшая дорожка основного файла, с которой сравнивать внешнюю: сначала та,
/// что останется в результате, иначе — любая аудио.
fn main_audio_uid(info: &MediaInfo, keep: &[bool]) -> Option<usize> {
    let usable = |uid: usize, stream: &&StreamInfo| {
        stream.kind == StreamKind::Audio
            && stream.ff_index.is_some()
            && keep.get(uid).copied().unwrap_or(false)
    };
    info.streams
        .iter()
        .enumerate()
        .find(|(uid, stream)| usable(*uid, stream))
        .or_else(|| {
            info.streams
                .iter()
                .enumerate()
                .find(|(_, stream)| stream.kind == StreamKind::Audio && stream.ff_index.is_some())
        })
        .map(|(uid, _)| uid)
}

fn params_for(lang: Lang, stream: &StreamInfo) -> String {
    let parts: Vec<String> = match stream.kind {
        StreamKind::Video => vec![stream.resolution_label(), stream.fps_label()],
        StreamKind::Audio => vec![
            stream_channels(lang, stream),
            stream.sample_rate_label(),
            stream.bitrate_label(),
        ],
        StreamKind::Subtitle => vec![if stream.forced {
            "forced".to_string()
        } else {
            String::new()
        }],
        _ => vec![],
    };
    parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn kind_color(kind: StreamKind) -> Color32 {
    match kind {
        StreamKind::Video => Color32::from_rgb(125, 180, 255),
        StreamKind::Audio => Color32::from_rgb(110, 231, 183),
        StreamKind::Subtitle => Color32::from_rgb(253, 224, 71),
        StreamKind::Button => Color32::from_rgb(196, 181, 253),
        StreamKind::Attachment | StreamKind::Other => MUTED,
    }
}

/// Название типа дорожки на выбранном языке.
fn kind_label(lang: Lang, kind: StreamKind) -> &'static str {
    match kind {
        StreamKind::Video => tr(lang, "Видео"),
        StreamKind::Audio => tr(lang, "Аудио"),
        StreamKind::Subtitle => tr(lang, "Субтитры"),
        StreamKind::Button => tr(lang, "Кнопки"),
        StreamKind::Attachment => tr(lang, "Вложение"),
        StreamKind::Other => tr(lang, "Прочее"),
    }
}

/// Название дорожки: заголовок из контейнера или «Без названия».
fn stream_name(lang: Lang, stream: &StreamInfo) -> String {
    if stream.has_name() {
        stream
            .title
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        tr(lang, "Без названия").to_string()
    }
}

/// Язык дорожки с расшифровкой кода на выбранном языке интерфейса.
fn stream_language(lang: Lang, stream: &StreamInfo) -> String {
    match stream.language.as_deref().map(str::trim) {
        Some(code) if !code.is_empty() && code != "und" => {
            match trackcut_core::model::language_name(code) {
                Some(name) => format!("{code} — {}", crate::i18n::tr_dyn(lang, name)),
                None => code.to_uppercase(),
            }
        }
        _ => tr(lang, "не указан").to_string(),
    }
}

/// Каналы дорожки: «1.0 (моно)», «2.0 (стерео)», «5.1», «6 кан.»…
fn stream_channels(lang: Lang, stream: &StreamInfo) -> String {
    match stream.channels {
        None => String::new(),
        Some(1) => tr(lang, "1.0 (моно)").to_string(),
        Some(2) => tr(lang, "2.0 (стерео)").to_string(),
        Some(6) => "5.1".to_string(),
        Some(8) => "7.1".to_string(),
        Some(n) => format!("{n} {}", tr(lang, "кан.")),
    }
}

fn audio_score(stream: &StreamInfo) -> u64 {
    let language_bonus = stream
        .language
        .as_deref()
        .map(|code| {
            let code = code.to_ascii_lowercase();
            if code == "ru" || code == "rus" {
                1_000_000_000_000
            } else {
                0
            }
        })
        .unwrap_or(0);
    let channels = stream.channels.unwrap_or(2) as u64 * 1_000_000_000;
    let bitrate = stream.bitrate.unwrap_or(0);
    language_bonus + channels + bitrate
}

fn yes_no(lang: Lang, value: bool) -> String {
    if value {
        tr(lang, "Да").to_string()
    } else {
        "—".to_string()
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut result: String = value.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());

    if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\segoeui.ttf")) {
        fonts.font_data.insert(
            "segoe".to_string(),
            egui::FontData::from_owned(bytes).into(),
        );
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.insert(0, "segoe".to_string());
        }
    }
    if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\consola.ttf")) {
        fonts.font_data.insert(
            "consolas".to_string(),
            egui::FontData::from_owned(bytes).into(),
        );
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            family.insert(0, "consolas".to_string());
        }
    }

    ctx.set_fonts(fonts);
}

fn setup_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(22, 25, 32);
    visuals.window_fill = Color32::from_rgb(28, 32, 41);
    visuals.extreme_bg_color = Color32::from_rgb(16, 18, 24);
    visuals.faint_bg_color = Color32::from_rgb(32, 37, 47);
    visuals.selection.bg_fill = Color32::from_rgb(45, 78, 130);
    visuals.selection.stroke.color = Color32::WHITE;
    visuals.hyperlink_color = ACCENT;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
    ctx.set_style(style);
}
