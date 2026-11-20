use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use trackcut_core::preview::{extract_preview, preview_start_ms};
use trackcut_core::{MediaInfo, ToolPaths};

/// Запускает консольную утилиту без мелькающего окна терминала.
#[cfg(windows)]
fn hidden_command(program: PathBuf) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(not(windows))]
fn hidden_command(program: PathBuf) -> std::process::Command {
    std::process::Command::new(program)
}

pub enum PreviewMsg {
    Finished(u64),
    Error(u64, String),
}

#[derive(Default)]
pub struct PreviewState {
    /// Уникальный ключ играющей дорожки (основной файл или внешний).
    pub token: Option<u64>,
    pub stop: Option<Arc<AtomicBool>>,
    pub status: String,
}

impl PreviewState {
    pub fn stop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
        self.stop = None;
        self.token = None;
        self.status.clear();
    }
}

/// Достаёт фрагмент и играет его в фоне. Останавливается по флагу или по концу файла.
pub fn start_preview(
    tools: ToolPaths,
    info: MediaInfo,
    token: u64,
    ff_index: usize,
    tx: Sender<PreviewMsg>,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();

    std::thread::spawn(move || {
        let temp: PathBuf = std::env::temp_dir().join(format!(
            "trackcut_preview_{}_{}.wav",
            std::process::id(),
            token
        ));

        let start = preview_start_ms(info.duration_ms);
        if let Err(err) = extract_preview(&tools, &info.path, ff_index, start, &temp) {
            let _ = tx.send(PreviewMsg::Error(token, err.to_string()));
            return;
        }

        match play_wav(&temp, &flag) {
            Ok(()) => {
                let _ = std::fs::remove_file(&temp);
                let _ = tx.send(PreviewMsg::Finished(token));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&temp);
                let _ = tx.send(PreviewMsg::Error(token, err));
            }
        }
    });

    stop
}

fn play_wav(path: &PathBuf, stop: &AtomicBool) -> Result<(), String> {
    use rodio::{Decoder, OutputStream, Sink};

    let (_stream, handle) =
        OutputStream::try_default().map_err(|err| format!("аудиоустройство: {err}"))?;
    let sink = Sink::try_new(&handle).map_err(|err| format!("аудиоустройство: {err}"))?;

    let file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let source = Decoder::new(std::io::BufReader::new(file))
        .map_err(|err| format!("декодирование фрагмента: {err}"))?;

    sink.set_volume(0.8);
    sink.append(source);

    while !sink.empty() && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(80));
    }
    sink.stop();
    Ok(())
}

/// Кадр для окна синхрона: грузится в фоне, чтобы не подвешивать интерфейс.
pub struct FrameMsg {
    pub token: u64,
    pub result: Result<(Vec<u8>, u32, u32), String>,
}

pub fn request_frame(
    tools: ToolPaths,
    input: PathBuf,
    ff_index: usize,
    at_ms: u64,
    width: u32,
    token: u64,
    tx: Sender<FrameMsg>,
) {
    std::thread::spawn(move || {
        let result = trackcut_core::preview::extract_frame(&tools, &input, ff_index, at_ms, width)
            .map_err(|err| err.to_string());
        let _ = tx.send(FrameMsg { token, result });
    });
}

/// Кадр видеофрагмента для окна синхрона.
pub struct VideoFrame {
    pub token: u64,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Декодирует 12 секунд видео в rawvideo и отдаёт кадры по одному.
/// `-re` держит реальный темп, чтобы картинка не улетала вперёд звука.
#[allow(clippy::too_many_arguments)]
pub fn start_video_segment(
    tools: ToolPaths,
    input: PathBuf,
    ff_index: usize,
    at_ms: u64,
    width: u32,
    height: u32,
    seconds: f32,
    token: u64,
    tx: Sender<VideoFrame>,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();

    std::thread::spawn(move || {
        let Ok(ffmpeg) = tools.ffmpeg() else {
            return;
        };
        // Звук сначала собирается в wav (доли секунды) — чуть отстаём, чтобы стартовать вместе.
        std::thread::sleep(Duration::from_millis(350));
        if flag.load(Ordering::Relaxed) {
            return;
        }

        let Ok(mut child) = hidden_command(ffmpeg)
            .args(["-hide_banner", "-nostdin", "-loglevel", "error"])
            .arg("-re")
            .arg("-ss")
            .arg(format!("{:.3}", at_ms as f64 / 1000.0))
            .arg("-i")
            .arg(&input)
            .arg("-map")
            .arg(format!("0:{ff_index}"))
            .args(["-an", "-sn", "-dn"])
            .arg("-t")
            .arg(format!("{seconds:.2}"))
            .arg("-vf")
            .arg(format!("scale={width}:{height},fps=24"))
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };

        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            return;
        };

        let frame_len = (width * height * 4) as usize;
        let mut buffer = vec![0u8; frame_len];
        while !flag.load(Ordering::Relaxed) {
            if std::io::Read::read_exact(&mut stdout, &mut buffer).is_err() {
                break;
            }
            let frame = std::mem::replace(&mut buffer, vec![0u8; frame_len]);
            if tx
                .send(VideoFrame {
                    token,
                    rgba: frame,
                    width,
                    height,
                })
                .is_err()
            {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    });

    stop
}

/// Играет обе дорожки одновременно, чтобы на слух подогнать сдвиг.
#[allow(clippy::too_many_arguments)]
pub fn start_pair_preview(
    tools: ToolPaths,
    main: MediaInfo,
    main_ff: usize,
    extra: MediaInfo,
    extra_ff: usize,
    at_ms: u64,
    delay_ms: i64,
    token: u64,
    tx: Sender<PreviewMsg>,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();

    std::thread::spawn(move || {
        let temp: PathBuf = std::env::temp_dir().join(format!(
            "trackcut_pair_{}_{}.wav",
            std::process::id(),
            token
        ));

        if let Err(err) = trackcut_core::preview::extract_pair_preview(
            &tools,
            &main.path,
            main_ff,
            &extra.path,
            extra_ff,
            at_ms,
            delay_ms,
            12.0,
            &temp,
        ) {
            let _ = std::fs::remove_file(&temp);
            let _ = tx.send(PreviewMsg::Error(token, err.to_string()));
            return;
        }

        match play_wav(&temp, &flag) {
            Ok(()) => {
                let _ = std::fs::remove_file(&temp);
                let _ = tx.send(PreviewMsg::Finished(token));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&temp);
                let _ = tx.send(PreviewMsg::Error(token, err));
            }
        }
    });

    stop
}
