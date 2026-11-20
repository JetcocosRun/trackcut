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

/// Одна дорожка со сдвигом: то же, что pair, но без второй дорожки.
#[allow(clippy::too_many_arguments)]
pub fn extract_track_preview(
    tools: &ToolPaths,
    input: &Path,
    ff_index: usize,
    at_ms: u64,
    delay_ms: i64,
    seconds: f32,
    destination: &Path,
) -> Result<()> {
    let ffmpeg = tools.ffmpeg()?;
    let start_ms = at_ms as i64 - delay_ms;
    let (start, delay_filter) = if start_ms >= 0 {
        (start_ms as u64, String::new())
    } else {
        (0, format!(",adelay={}:all=1", -start_ms))
    };

    let output = hidden_command(&ffmpeg)
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-y")
        .arg("-ss")
        .arg(format!("{:.3}", start as f64 / 1000.0))
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg(format!("0:{ff_index}"))
        .arg("-af")
        .arg(format!("aresample=48000{delay_filter}"))
        .arg("-vn")
        .arg("-t")
        .arg(format!("{seconds:.2}"))
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

/// Достаёт один кадр видео как RGBA заданной ширины (высота — по пропорциям).
///
/// Кадр берётся по ближайшему ключевому кадру до указанной позиции: так он
/// появляется за доли секунды даже в 15-гигабайтном файле.
pub fn extract_frame(
    tools: &ToolPaths,
    input: &Path,
    ff_index: usize,
    at_ms: u64,
    width: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    let ffmpeg = tools.ffmpeg()?;
    let seconds = at_ms as f64 / 1000.0;
    let width = width.max(64);
    let output = hidden_command(&ffmpeg)
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(format!("{seconds:.3}"))
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg(format!("0:{ff_index}"))
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(format!("scale={width}:-2"))
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-")
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

    let stride = width as usize * 4;
    let rgba = output.stdout;
    if rgba.is_empty() {
        return Err(Error::Other("не удалось получить кадр".to_string()));
    }
    if rgba.len() % stride != 0 {
        return Err(Error::Other(
            "ffmpeg вернул кадр неожиданного размера".to_string(),
        ));
    }
    let height = (rgba.len() / stride) as u32;
    Ok((rgba, width, height))
}

/// Фрагмент, в котором обе дорожки звучат одновременно с учётом сдвига.
///
/// Основная дорожка играет с позиции `at_ms`, внешняя — со своей позиции
/// `at_ms - delay_ms`: положительный сдвиг означает, что она зазвучит позже.
/// Если внешняя по сдвигу уходит в начало файла, перед ней добавляется тишина.
#[allow(clippy::too_many_arguments)]
pub fn extract_pair_preview(
    tools: &ToolPaths,
    main: &Path,
    main_ff: usize,
    extra: &Path,
    extra_ff: usize,
    at_ms: u64,
    delay_ms: i64,
    seconds: f32,
    destination: &Path,
) -> Result<()> {
    let ffmpeg = tools.ffmpeg()?;
    let at = at_ms as i64;
    let extra_start = at - delay_ms;
    let (extra_at, silence) = if extra_start >= 0 {
        (extra_start as u64, 0)
    } else {
        (0, -extra_start)
    };

    let mut filter = format!(
        "[0:{main_ff}]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a0];\
         [1:{extra_ff}]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a1]"
    );
    if silence > 0 {
        filter.push_str(&format!(";[a1]adelay={silence}:all=1[a1d]"));
        filter.push_str(";[a0][a1d]amix=inputs=2:duration=shortest:normalize=0[mix]");
    } else {
        filter.push_str(";[a0][a1]amix=inputs=2:duration=shortest:normalize=0[mix]");
    }
    filter.push_str(";[mix]volume=0.8[out]");

    let output = hidden_command(&ffmpeg)
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-y")
        .arg("-ss")
        .arg(format!("{:.3}", at.max(0) as f64 / 1000.0))
        .arg("-i")
        .arg(main)
        .arg("-ss")
        .arg(format!("{:.3}", extra_at as f64 / 1000.0))
        .arg("-i")
        .arg(extra)
        .arg("-filter_complex")
        .arg(filter)
        .arg("-map")
        .arg("[out]")
        .arg("-t")
        .arg(format!("{seconds:.2}"))
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
