use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use trackcut_core::preview::{extract_preview, preview_start_ms};
use trackcut_core::{MediaInfo, ToolPaths};

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
