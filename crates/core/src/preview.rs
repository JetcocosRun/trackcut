use std::path::Path;

use crate::error::{Error, Result};
use crate::sys::hidden_command;
use crate::tools::ToolPaths;

/// Декодирует 15 секунд выбранной аудиодорожки в WAV (для прослушивания).
pub fn extract_preview(
    tools: &ToolPaths,
    input: &Path,
    ff_index: usize,
    at_ms: u64,
    destination: &Path,
) -> Result<()> {
    let ffmpeg = tools.ffmpeg()?;
    let seconds = (at_ms as f64 / 1000.0).max(0.0);
    let output = hidden_command(&ffmpeg)
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-y")
        .arg("-ss")
        .arg(format!("{seconds:.3}"))
        .arg("-t")
        .arg("15")
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg(format!("0:{ff_index}"))
        .arg("-vn")
        .arg("-ac")
        .arg("2")
        .arg("-ar")
        .arg("48000")
        .arg("-f")
        .arg("wav")
        .arg("-loglevel")
        .arg("error")
        .arg(destination)
        .output()
        .map_err(|source| Error::Spawn {
            program: ffmpeg.display().to_string(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::CommandFailed {
            program: ffmpeg.display().to_string(),
            code: output.status.code().unwrap_or(-1),
            stderr: stderr.trim().to_string(),
        });
    }
    Ok(())
}

/// Куда ставить точку начала фрагмента: 10% длительности, но не позже конца.
pub fn preview_start_ms(duration_ms: Option<u64>) -> u64 {
    match duration_ms {
        Some(duration) if duration > 30_000 => duration / 10,
        _ => 0,
    }
}
