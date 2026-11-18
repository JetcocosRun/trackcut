use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Align, Color32, Layout, RichText};
use trackcut_core::extract::{default_track_path, ExtractContainer, ExtractPlan};
use trackcut_core::model::{format_duration, format_size, OutputContainer, StreamInfo, StreamKind};
use trackcut_core::plan::{paths_equal, validate};
use trackcut_core::{
    probe, run_extract, run_mux, Error, ExecEvent, ExtraInput, ExtraTrack, KeepTrack, MediaInfo,
    MuxPlan, ToolPaths,
};

use crate::preview::{start_preview, PreviewMsg, PreviewState};

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

#[derive(Clone)]
struct ExtraTrackUi {
    uid: usize,
    delay_ms: i64,
    default: bool,
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
    extras: Vec<ExtraSource>,
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
            extras: Vec::new(),
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

    filter: String,
    tags_open: Option<usize>,
    extra_pick: Option<ExtraPick>,

    job: JobState,
    job_rx: Option<Receiver<JobMsg>>,
    cancel: Option<Arc<AtomicBool>>,

    preview: PreviewState,
    preview_tx: Sender<PreviewMsg>,
    preview_rx: Receiver<PreviewMsg>,

    log: Vec<String>,
    status: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        setup_style(&cc.egui_ctx);

        let tools = ToolPaths::detect();
        let tool_versions = tools.versions();
        let (probe_tx, probe_rx) = mpsc::channel();
        let (preview_tx, preview_rx) = mpsc::channel();

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
            filter: String::new(),
            tags_open: None,
            extra_pick: None,
            job: JobState::Idle,
            job_rx: None,
            cancel: None,
            preview: PreviewState::default(),
            preview_tx,
            preview_rx,
            log: Vec::new(),
            status: "Добавьте файл, чтобы увидеть дорожки".to_string(),
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
            "Все инструменты найдены".to_string()
        } else {
            format!("Не найдены: {}", missing.join(", "))
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
            self.status = format!("Добавлено файлов: {added}. Читаю метаданные…");
        }
    }

    fn pick_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                "Видео",
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
                self.status = "Метаданные загружены".to_string();
            }
        }

        let preview_messages: Vec<PreviewMsg> = self.preview_rx.try_iter().collect();
        for message in preview_messages {
            match message {
                PreviewMsg::Finished(token) => {
                    if self.preview.token == Some(token) {
                        self.preview.token = None;
                        self.preview.stop = None;
                        self.preview.status.clear();
                    }
                }
                PreviewMsg::Error(token, err) => {
                    if self.preview.token == Some(token) {
                        self.preview.token = None;
                        self.preview.stop = None;
                    }
                    self.log.push(format!("Прослушивание: {err}"));
                    self.status = "Не удалось воспроизвести фрагмент".to_string();
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
                format!("Готово: {ok} файл(ов) собрано")
            } else {
                format!("Завершено: успешно {ok}, с ошибками {failed}")
            };
            self.log.push(self.status.clone());
            if let Some(rx) = &self.job_rx {
                let _ = rx;
            }
            ctx.request_repaint();
        }

        if self.probe_pending > 0 || self.is_busy() || self.preview.token.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
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
            };

            validate(&info, &plan).map_err(|err| format!("{}: {err}", entry.name()))?;
            jobs.push(Job {
                file_index,
                label: entry.name(),
                op: JobOp::Mux { info, plan },
            });
        }

        if jobs.is_empty() {
            return Err("нет файлов с выбранными дорожками".to_string());
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
        self.start_jobs(jobs, format!("Сборка: {total} файл(ов)"));
    }

    fn start_extract(&mut self, job: Job) {
        if self.is_busy() {
            return;
        }
        let status = format!("Извлечение: {}", job.label);
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
                            format!("{}: сохранено в {name}", job.label)
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
            self.status = "Отмена…".to_string();
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

    fn play_preview(&mut self, info: MediaInfo, uid: usize, token: u64) {
        let Some(stream) = info.streams.get(uid) else {
            return;
        };
        let Some(ff_index) = stream.ff_index else {
            self.status = "У этой дорожки нет индекса для декодирования".to_string();
            return;
        };
        if self.tools.ffmpeg.is_none() {
            self.status = "Для прослушивания нужен ffmpeg".to_string();
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
        self.preview.status = "Играет фрагмент…".to_string();
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
            .add_filter("Дорожка", &["mka", "mks", "mkv"])
            .add_filter("Аудио (MKA)", &["mka"])
            .add_filter("Субтитры (MKS)", &["mks"])
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
            label: format!("{label} · дорожка {}", uid + 1),
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
                "Видео и дорожки",
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
        self.status = "Правило применено ко всем файлам".to_string();
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
                    RichText::new("удаление дорожек без перекодирования")
                        .size(13.0)
                        .color(MUTED),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !self.is_busy(),
                            egui::Button::new(RichText::new("＋ Добавить файлы").strong()),
                        )
                        .clicked()
                    {
                        self.pick_files();
                    }
                    if ui
                        .add_enabled(!self.is_busy(), egui::Button::new("Очистить"))
                        .clicked()
                    {
                        self.files.clear();
                        self.selected = 0;
                        self.status = "Список очищен".to_string();
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
                        RichText::new(format!("не найдены: {}", missing.join(", ")))
                            .color(color)
                            .small(),
                    );
                    if ui.small_button("Повторить поиск").clicked() {
                        self.refresh_tools();
                    }
                }
            });

            if self.tools.ffmpeg.is_none() || self.tools.ffprobe.is_none() {
                ui.label(
                    RichText::new(
                        "Положите ffmpeg/ffprobe/mkvmerge в папку tools рядом с программой \
                             или задайте TRACKCUT_FFMPEG, TRACKCUT_FFPROBE, TRACKCUT_MKVEMBLE.",
                    )
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
                    ui.label(RichText::new("Файлы").strong());
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
                                    .add_enabled(!busy, egui::Button::new("✕").small())
                                    .clicked()
                                {
                                    remove = Some(index);
                                }
                            });
                        }
                        if self.files.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(
                                    "Перетащите видео сюда\nили нажмите «Добавить файлы»",
                                )
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
                ui.label(RichText::new("Выходной файл").strong());
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.label("Контейнер");
                    egui::ComboBox::from_id_salt("container")
                        .selected_text(match self.output_container {
                            ContainerChoice::Auto => "как у входа",
                            ContainerChoice::Mkv => "MKV",
                            ContainerChoice::Webm => "WebM",
                            ContainerChoice::Mp4 => "MP4",
                            ContainerChoice::Mov => "MOV",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.output_container,
                                ContainerChoice::Auto,
                                "как у входа",
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
                        .unwrap_or_else(|| "рядом с исходником".to_string());
                    ui.label(RichText::new(text).small().color(MUTED))
                        .on_hover_text(
                            self.output_dir
                                .as_ref()
                                .map(|dir| dir.display().to_string())
                                .unwrap_or_else(|| "рядом с исходником".to_string()),
                        );
                    if ui.small_button("Папка…").clicked() {
                        self.pick_output_dir();
                    }
                    if self.output_dir.is_some() && ui.small_button("×").clicked() {
                        self.output_dir = None;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Суффикс");
                    ui.add(egui::TextEdit::singleline(&mut self.suffix).desired_width(110.0));
                });

                ui.add_space(2.0);
                ui.checkbox(&mut self.keep_chapters, "Сохранять главы");
                ui.checkbox(&mut self.keep_tags, "Сохранять теги");
                ui.checkbox(
                    &mut self.keep_attachments,
                    "Сохранять вложения (шрифты ASS)",
                );
                ui.checkbox(
                    &mut self.mov_text,
                    "Конвертировать субтитры в MOV text (MP4)",
                )
                .on_hover_text(
                    "Единственное исключение из правила «без перекодирования»: SRT/ASS → MOV text",
                );
                ui.checkbox(&mut self.overwrite, "Перезаписывать существующие файлы");

                ui.add_space(6.0);
                if ui
                    .add_enabled(
                        self.files
                            .get(self.selected)
                            .is_some_and(|f| f.info.is_some()),
                        egui::Button::new("Применить правило ко всем"),
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
                    ui.label(RichText::new("Перетащите MKV, MP4, MOV, WebM или AVI").size(20.0));
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Покажу все дорожки: язык, название перевода, кодек, каналы и размер.\n\
                             Отметьте ненужные — остальное скопируется без перекодирования.",
                        )
                        .color(MUTED),
                    );
                    ui.add_space(14.0);
                    if ui.button(RichText::new("Открыть файл…").size(16.0)).clicked() {
                        self.pick_files();
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Ctrl+O — открыть · Ctrl+Enter — собрать · Esc — отмена")
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
            let mut extra_remove: Option<(usize, usize)> = None;
            let mut extra_pick = false;
            let mut apply_all = false;

            let file_size;
            {
                let entry = &mut self.files[self.selected];
                file_size = entry.file_size;
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(entry.name()).size(18.0).strong());
                    if entry.probing {
                        ui.spinner();
                        ui.label(RichText::new("читаю метаданные…").color(MUTED));
                    }
                });
                ui.label(
                    RichText::new(entry.path.display().to_string())
                        .small()
                        .color(MUTED),
                );

                if let Some(error) = &entry.error {
                    ui.add_space(8.0);
                    ui.colored_label(RED, format!("Не удалось прочитать файл: {error}"));
                    ui.label(
                        RichText::new(
                            "Проверьте, что установлены ffprobe и mkvmerge (папка tools или PATH).",
                        )
                        .small()
                        .color(MUTED),
                    );
                    return;
                }

                let Some(info) = &entry.info else {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Читаю дорожки…").color(MUTED));
                    });
                    return;
                };

                ui.add_space(6.0);
                let meta = format!(
                    "{}  ·  {}  ·  {}  ·  дорожек: {}",
                    if info.container.is_empty() {
                        "контейнер неизвестен"
                    } else {
                        &info.container
                    },
                    info.duration_ms
                        .map(format_duration)
                        .unwrap_or_else(|| "длительность неизвестна".to_string()),
                    format_size(entry.file_size),
                    info.streams.len()
                );
                ui.label(RichText::new(meta).color(MUTED).small());

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Фильтр: язык, название, кодек…")
                            .desired_width(230.0),
                    );
                    ui.separator();
                    if ui.button("Видео + лучшая аудио").clicked() {
                        preset_action = Some(Preset::BestAudio);
                    }
                    if ui.button("Видео + аудио").clicked() {
                        preset_action = Some(Preset::VideoAudio);
                    }
                    if ui.button("Всё").clicked() {
                        preset_action = Some(Preset::Everything);
                    }
                    if ui.button("Только русский").clicked() {
                        preset_action = Some(Preset::RussianOnly);
                    }
                    if ui.button("Снять все").clicked() {
                        preset_action = Some(Preset::Nothing);
                    }
                    if ui.button("Правило → все файлы").clicked() {
                        apply_all = true;
                    }
                });

                let filter = self.filter.trim().to_ascii_lowercase();
                let kept = entry.kept_count();
                let total = info.streams.iter().filter(|s| s.kind.is_selectable()).count();
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Оставлено {kept} из {total} дорожек"))
                            .color(if kept == 0 { RED } else { MUTED })
                            .small(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "· ~{} в выходе",
                            format_size(total_kept_bytes(entry))
                        ))
                        .color(MUTED)
                        .small(),
                    );
                });

                ui.add_space(4.0);
                let table_height = (ui.available_height() - 190.0).max(150.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
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
                                        "", "Тип", "№", "Кодек", "Язык", "Название",
                                        "Параметры", "Размер", "Def", "Forced", "⤓", "", "",
                                    ] {
                                        ui.label(RichText::new(header).strong().small());
                                    }
                                    ui.end_row();

                                    for (uid, stream) in info.streams.iter().enumerate() {
                                        let haystack = format!(
                                            "{} {} {} {}",
                                            stream.display_name(),
                                            stream.language_label(),
                                            stream.codec_display(),
                                            stream.kind.label_ru()
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
                                                stream.kind.label_ru()
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

                                        ui.label(stream.language_label());

                                        let (name_text, color) = if stream.has_name() {
                                            (stream.display_name().to_string(), Color32::WHITE)
                                        } else {
                                            ("Без названия".to_string(), MUTED)
                                        };
                                        ui.label(
                                            RichText::new(truncate_chars(&name_text, 34))
                                                .color(color),
                                        )
                                        .on_hover_text(name_text);

                                        ui.label(
                                            RichText::new(params_for(stream))
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
                                            let response = ui
                                                .add_enabled(!busy, egui::Button::new("⤓").small())
                                                .on_hover_text(
                                                    "Сохранить эту дорожку отдельно (MKA/MKS), \
                                                     чтобы потом пришить к другому файлу",
                                                );
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
                                                .on_hover_text(
                                                    "Прослушать 15 секунд этой дорожки",
                                                );
                                            if response.clicked() {
                                                preview_request = Some(uid);
                                            }
                                        } else {
                                            ui.label("");
                                        }

                                        if ui
                                            .add(egui::Button::new("🏷").small())
                                            .on_hover_text("Все теги дорожки")
                                            .clicked()
                                        {
                                            tags_request = Some(uid);
                                        }

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
                        "Для MP4 текст субтитров нужно конвертировать в MOV text — включите опцию слева.",
                    );
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Дорожки из других файлов").strong());
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
                            .add_enabled(!busy, egui::Button::new("＋ Взять из файла…"))
                            .on_hover_text("Пришить аудио или субтитры из другого релиза")
                            .clicked()
                        {
                            extra_pick = true;
                        }
                    });
                });

                if entry.extras.is_empty() {
                    ui.label(
                        RichText::new(
                            "Например, озвучка из другого релиза: дорожка скопируется \
                             в результат без перекодирования. Сдвиг в мс поправит рассинхрон.",
                        )
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
                                RichText::new(format!("дорожек: {}", extra.tracks.len()))
                                    .small()
                                    .color(MUTED),
                            );
                        });

                        for (track_index, track) in extra.tracks.iter_mut().enumerate() {
                            let stream = extra.info.streams.get(track.uid).cloned();
                            ui.horizontal(|ui| {
                                let Some(stream) = &stream else {
                                    ui.label(
                                        RichText::new("дорожка не найдена в файле")
                                            .color(RED)
                                            .small(),
                                    );
                                    return;
                                };
                                ui.label(
                                    RichText::new(format!(
                                        "{} {}",
                                        stream.kind.icon(),
                                        stream.kind.label_ru()
                                    ))
                                    .color(kind_color(stream.kind))
                                    .small(),
                                );
                                ui.label(RichText::new(stream.codec_display()).small());
                                ui.label(
                                    RichText::new(truncate_chars(
                                        &trackcut_core::extract::track_caption(stream),
                                        46,
                                    ))
                                    .small(),
                                );
                                ui.label(RichText::new("сдвиг").small().color(MUTED));
                                ui.add(
                                    egui::DragValue::new(&mut track.delay_ms)
                                        .speed(10.0)
                                        .range(-600_000..=600_000)
                                        .suffix(" мс"),
                                )
                                .on_hover_text(
                                    "Положительное значение — дорожка зазвучит позже",
                                );
                                ui.checkbox(&mut track.default, "Def").on_hover_text(
                                    "Пометить дорожку как выбранную по умолчанию",
                                );
                                if stream.kind == StreamKind::Audio && stream.ff_index.is_some() {
                                    let playing = preview_token
                                        == Some(extra_token(source_index, track.uid));
                                    let label = if playing { "■" } else { "▶" };
                                    if ui
                                        .add(egui::Button::new(label).small())
                                        .on_hover_text("Прослушать 15 секунд")
                                        .clicked()
                                    {
                                        extra_preview = Some((source_index, track_index));
                                    }
                                }
                                if ui
                                    .add_enabled(!busy, egui::Button::new("✕").small())
                                    .on_hover_text("Убрать дорожку из результата")
                                    .clicked()
                                {
                                    extra_remove = Some((source_index, track_index));
                                }
                            });
                        }
                    }
                        });
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
                self.status = "Внешняя дорожка убрана".to_string();
            }
            if let Some((source, uid)) = extra_preview {
                self.preview_extra(source, uid);
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
                        RichText::new("Готов к сборке: отметьте дорожки и нажмите «Собрать»")
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
                            if ui.button("Отмена").clicked() {
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
                        (format!("Готово: {ok} файл(ов) собрано"), GREEN)
                    } else {
                        (
                            format!("Завершено: успешно {ok}, с ошибками {failed}"),
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
                        egui::Button::new(RichText::new("▶  Собрать").strong())
                            .min_size(egui::vec2(130.0, 30.0)),
                    )
                    .clicked()
                {
                    self.start_mux();
                }
                if self.is_busy() && ui.button("Остановить").clicked() {
                    self.request_cancel();
                }

                ui.label(RichText::new(self.status.clone()).color(MUTED).small());

                if self.preview.token.is_some() {
                    ui.separator();
                    ui.label(
                        RichText::new("прослушивание включено")
                            .color(ACCENT)
                            .small(),
                    );
                    if ui.small_button("стоп").clicked() {
                        self.preview.stop();
                    }
                }
            });

            let title = format!("Лог ({})", self.log.len());
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
                                ui.label(RichText::new("пусто").small().color(MUTED));
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
        egui::Window::new(format!("Дорожка №{} · {}", uid + 1, stream.display_name()))
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
                            ("Тип", stream.kind.label_ru().to_string()),
                            ("Кодек", stream.codec_display()),
                            ("Сырой кодек", stream.codec_raw.clone()),
                            ("Язык", stream.language_label()),
                            ("Название", stream.display_name().to_string()),
                            ("Каналы", stream.channels_label()),
                            ("Layout", stream.channel_layout.clone().unwrap_or_default()),
                            ("Частота", stream.sample_rate_label()),
                            ("Разрешение", stream.resolution_label()),
                            ("FPS", stream.fps_label()),
                            ("Битрейт", stream.bitrate_label()),
                            (
                                "Длительность",
                                stream.duration_ms.map(format_duration).unwrap_or_default(),
                            ),
                            (
                                "Размер",
                                stream
                                    .size_bytes
                                    .map(format_size)
                                    .unwrap_or_else(|| "—".to_string()),
                            ),
                            ("Default", yes_no(stream.default)),
                            ("Forced", yes_no(stream.forced)),
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
                ui.label(RichText::new("Теги из контейнера").strong());
                if stream.tags.is_empty() {
                    ui.label(RichText::new("Тегов нет").color(MUTED).small());
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

        egui::Window::new("Взять дорожку из другого файла")
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
                    ui.colored_label(RED, format!("Не удалось прочитать файл: {err}"));
                    ui.label(
                        RichText::new(
                            "Нужны ffprobe или mkvmerge: папка tools рядом с программой или PATH.",
                        )
                        .small()
                        .color(MUTED),
                    );
                    return;
                }

                let Some(info) = pick.info.clone() else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Читаю дорожки…").color(MUTED));
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
                                for header in ["", "Тип", "Кодек", "Язык", "Название", "Параметры"]
                                {
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
                                            stream.kind.label_ru()
                                        ))
                                        .color(kind_color(stream.kind))
                                        .small(),
                                    );
                                    ui.label(stream.codec_display());
                                    ui.label(stream.language_label());
                                    ui.label(truncate_chars(stream.display_name(), 34));
                                    ui.label(
                                        RichText::new(params_for(stream)).small().color(MUTED),
                                    );
                                    ui.end_row();
                                }
                            });
                    });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.small_button("Только аудио").clicked() {
                        for (uid, stream) in info.streams.iter().enumerate() {
                            pick.selected[uid] = stream.kind == StreamKind::Audio;
                        }
                    }
                    if ui.small_button("Снять все").clicked() {
                        pick.selected.iter_mut().for_each(|value| *value = false);
                    }
                    let chosen = pick.selected.iter().filter(|value| **value).count();
                    ui.separator();
                    ui.label(
                        RichText::new(format!("выбрано: {chosen}"))
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
                            egui::Button::new(RichText::new("Добавить").strong()),
                        )
                        .clicked()
                    {
                        add = true;
                    }
                    if ui.button("Отмена").clicked() {
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
                })
                .collect();

            if tracks.is_empty() {
                self.status = "Ничего не выбрано".to_string();
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
                self.status = format!("Внешних дорожек: {added}");
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
            "Отпустите файлы, чтобы добавить",
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

fn params_for(stream: &StreamInfo) -> String {
    let parts: Vec<String> = match stream.kind {
        StreamKind::Video => vec![stream.resolution_label(), stream.fps_label()],
        StreamKind::Audio => vec![
            stream.channels_label(),
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

fn yes_no(value: bool) -> String {
    if value {
        "да".to_string()
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
