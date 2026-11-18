use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{MediaInfo, StreamKind};
use crate::sys::hidden_command;
use crate::tools::ToolPaths;

/// Частота, на которой сравниваем дорожки: речь и музыка в ней уже различимы,
/// а декодирование 120 секунд занимает доли секунды.
const SAMPLE_RATE: u32 = 8_000;
/// Шаг огибающей: 5 мс даёт точность сдвига ±5 мс — для синхронизации это с запасом.
const HOP_MS: i64 = 5;
/// Сколько секунд сравнивать в одном окне.
const WINDOW_MS: u64 = 60_000;
/// Насколько далеко искать рассинхрон.
const MAX_LAG_MS: i64 = 8_000;
/// Минимальная доля перекрытия, чтобы корреляция считалась осмысленной.
const MIN_OVERLAP: f32 = 0.5;

#[derive(Debug, Clone, Default)]
pub struct OffsetEstimate {
    /// Сдвиг, который надо применить к внешней дорожке: положительный — позже.
    pub delay_ms: i64,
    /// Нормированная корреляция лучшего совпадения (0…1).
    pub score: f32,
    /// Сколько окон совпало по сдвигу.
    pub windows_agree: usize,
    pub windows_total: usize,
    pub warnings: Vec<String>,
}

/// Сравнивает две дорожки по огибающей громкости и предлагает сдвиг.
pub fn estimate_delay(
    tools: &ToolPaths,
    main: &MediaInfo,
    main_uid: usize,
    extra: &MediaInfo,
    extra_uid: usize,
) -> Result<OffsetEstimate> {
    let ffmpeg = tools
        .ffmpeg()
        .map_err(|_| Error::ToolMissing("ffmpeg"))?
        .to_path_buf();

    let main_stream = main
        .streams
        .get(main_uid)
        .ok_or_else(|| Error::InvalidPlan("не найдена дорожка основного файла".to_string()))?;
    let extra_stream = extra
        .streams
        .get(extra_uid)
        .ok_or_else(|| Error::InvalidPlan("не найдена внешняя дорожка".to_string()))?;
    if main_stream.kind != StreamKind::Audio || extra_stream.kind != StreamKind::Audio {
        return Err(Error::InvalidPlan(
            "сравнивать можно только аудио".to_string(),
        ));
    }
    let main_index = main_stream.ff_index.ok_or_else(|| {
        Error::InvalidPlan("у дорожки основного файла нет индекса ffmpeg".to_string())
    })?;
    let extra_index = extra_stream
        .ff_index
        .ok_or_else(|| Error::InvalidPlan("у внешней дорожки нет индекса ffmpeg".to_string()))?;

    let duration = match (main.duration_ms, extra.duration_ms) {
        (Some(a), Some(b)) => a.min(b),
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => 0,
    };
    if duration < 10_000 {
        return Err(Error::Other(
            "файл слишком короткий для подбора сдвига".to_string(),
        ));
    }

    let window = WINDOW_MS.min(duration.saturating_sub(2_000)).max(10_000);
    let starts = window_starts(duration, window);

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut warnings = Vec::new();

    for start in &starts {
        let main_env = match decode_envelope(&ffmpeg, &main.path, main_index, *start, window) {
            Ok(env) => env,
            Err(err) => {
                warnings.push(format!("окно {}: {err}", start / 1000));
                continue;
            }
        };
        let extra_env = match decode_envelope(&ffmpeg, &extra.path, extra_index, *start, window) {
            Ok(env) => env,
            Err(err) => {
                warnings.push(format!("окно {}: {err}", start / 1000));
                continue;
            }
        };

        if main_env.len() < 100 || extra_env.len() < 100 {
            warnings.push("окно слишком короткое — пропущено".to_string());
            continue;
        }
        if variance(&main_env) < 1e-6 || variance(&extra_env) < 1e-6 {
            warnings.push("в окне тишина — пропущено".to_string());
            continue;
        }

        let max_lag_frames = (MAX_LAG_MS / HOP_MS) as isize;
        let (lag, score) = best_lag(&extra_env, &main_env, max_lag_frames);
        if score.is_finite() {
            candidates.push(Candidate {
                delay_ms: lag as i64 * HOP_MS,
                score,
                start_ms: *start,
            });
        }
    }

    if candidates.is_empty() {
        return Err(Error::Other(
            "не удалось сравнить дорожки: нет пригодных фрагментов".to_string(),
        ));
    }

    candidates.sort_by_key(|candidate| candidate.delay_ms);
    // Группируем окна, которые дали близкий сдвиг: так отсекаются случайные совпадения.
    let mut best_cluster: Vec<&Candidate> = Vec::new();
    for candidate in &candidates {
        let mut cluster: Vec<&Candidate> = candidates
            .iter()
            .filter(|other| (other.delay_ms - candidate.delay_ms).abs() <= 60)
            .collect();
        cluster.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let weight: f32 = cluster.iter().map(|item| item.score).sum();
        let best_weight: f32 = best_cluster.iter().map(|item| item.score).sum();
        if cluster.len() > best_cluster.len()
            || (cluster.len() == best_cluster.len() && weight > best_weight)
        {
            best_cluster = cluster;
        }
    }

    let weight: f32 = best_cluster.iter().map(|item| item.score).sum();
    let delay_ms = if weight > 0.0 {
        (best_cluster
            .iter()
            .map(|item| item.delay_ms as f32 * item.score)
            .sum::<f32>()
            / weight)
            .round() as i64
    } else {
        best_cluster[0].delay_ms
    };
    let score = best_cluster
        .iter()
        .map(|item| item.score)
        .fold(0.0f32, f32::max);

    if best_cluster.len() < starts.len() {
        warnings.push(format!(
            "совпало {}/{} окон — проверьте результат на слух",
            best_cluster.len(),
            starts.len()
        ));
    }
    if score < 0.20 {
        warnings.push("совпадение слабое: возможно, это другой монтаж".to_string());
    }

    Ok(OffsetEstimate {
        delay_ms,
        score,
        windows_agree: best_cluster.len(),
        windows_total: starts.len(),
        warnings,
    })
}

struct Candidate {
    delay_ms: i64,
    score: f32,
    #[allow(dead_code)]
    start_ms: u64,
}

/// Три окна по ходу фильма — так видны и разные сцены, и общий сдвиг.
fn window_starts(duration_ms: u64, window_ms: u64) -> Vec<u64> {
    let latest = duration_ms.saturating_sub(window_ms);
    // Места для разноса окон нет — сравниваем один фрагмент с начала.
    if latest < window_ms / 2 {
        return vec![0];
    }
    let candidates = [latest / 4, latest / 2, latest.saturating_mul(3) / 4];
    let mut starts: Vec<u64> = Vec::new();
    for start in candidates {
        if !starts
            .iter()
            .any(|existing| start.abs_diff(*existing) < window_ms / 2)
        {
            starts.push(start);
        }
    }
    starts
}

fn variance(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    values
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f32>()
        / values.len() as f32
}

/// Декодирует фрагмент в моно 8 кГц и превращает его в огибающую громкости.
fn decode_envelope(
    ffmpeg: &Path,
    file: &Path,
    stream_index: usize,
    start_ms: u64,
    duration_ms: u64,
) -> Result<Vec<f32>> {
    let start = format!("{:.3}", start_ms as f64 / 1000.0);
    let duration = format!("{:.3}", duration_ms as f64 / 1000.0);
    let map = format!("0:{stream_index}");
    let rate = SAMPLE_RATE.to_string();

    let output = hidden_command(ffmpeg)
        .args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-ss",
            &start,
            "-t",
            &duration,
            "-i",
        ])
        .arg(file)
        .args([
            "-map", &map, "-vn", "-sn", "-dn", "-ac", "1", "-ar", &rate, "-f", "s16le", "pipe:1",
        ])
        .output()
        .map_err(|source| Error::Spawn {
            program: ffmpeg.display().to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(Error::Other(format!(
            "ffmpeg не смог декодировать дорожку: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if output.stdout.len() < 2 {
        return Err(Error::Other("пустой фрагмент аудио".to_string()));
    }

    Ok(envelope(&output.stdout))
}

fn envelope(bytes: &[u8]) -> Vec<f32> {
    let frame = (SAMPLE_RATE as usize * HOP_MS as usize) / 1000;
    let mut result = Vec::with_capacity(bytes.len() / 2 / frame + 1);
    for chunk in bytes.chunks_exact(frame * 2) {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        let (samples, _) = chunk.as_chunks::<2>();
        for sample in samples {
            let value = i16::from_le_bytes(*sample) as f64 / 32768.0;
            sum += value * value;
            count += 1;
        }
        result.push((sum / count.max(1) as f64).sqrt() as f32);
    }
    // Убираем постоянную составляющую, чтобы корреляция не ловила «общий фон».
    let mean = if result.is_empty() {
        0.0
    } else {
        result.iter().sum::<f32>() / result.len() as f32
    };
    for value in &mut result {
        *value -= mean;
    }
    result
}

/// Ищет сдвиг `d`, при котором `extra[i] ≈ main[i + d]`.
fn best_lag(extra: &[f32], main: &[f32], max_lag: isize) -> (isize, f32) {
    let mut best = (0isize, f32::NEG_INFINITY);
    let min_overlap = extra.len() as f32 * MIN_OVERLAP;

    for lag in -max_lag..=max_lag {
        let (extra_start, main_start) = if lag >= 0 {
            (0usize, lag as usize)
        } else {
            ((-lag) as usize, 0usize)
        };
        if extra_start >= extra.len() || main_start >= main.len() {
            continue;
        }
        let len = (extra.len() - extra_start).min(main.len() - main_start);
        if (len as f32) < min_overlap {
            continue;
        }

        let mut dot = 0.0f64;
        let mut extra_energy = 0.0f64;
        let mut main_energy = 0.0f64;
        for index in 0..len {
            let a = extra[extra_start + index] as f64;
            let b = main[main_start + index] as f64;
            dot += a * b;
            extra_energy += a * a;
            main_energy += b * b;
        }
        let score = (dot / (extra_energy.sqrt() * main_energy.sqrt()).max(1e-9)) as f32;
        if score > best.1 {
            best = (lag, score);
        }
    }

    best
}

/// Записывает WAV-фрагмент — нужен для отладки и тестов.
#[allow(dead_code)]
pub fn write_envelope_csv(values: &[f32], path: &Path) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    for (index, value) in values.iter().enumerate() {
        writeln!(file, "{};{value}", index as i64 * HOP_MS)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Псевдослучайная огибающая без периодичности — иначе корреляция даёт ложные пики.
    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 33) as f32 / u32::MAX as f32) - 0.5
            })
            .collect()
    }

    #[test]
    fn finds_positive_lag() {
        let main = noise(4_000, 7);
        // extra запаздывает на 20 кадров: extra[i] = main[i - 20]
        let mut extra = vec![0.0f32; 20];
        extra.extend_from_slice(&main[..main.len() - 20]);
        let (lag, score) = best_lag(&extra, &main, 100);
        assert_eq!(lag, -20, "score={score}");
        assert!(score > 0.9, "score={score}");
    }

    #[test]
    fn finds_negative_lag() {
        let main = noise(4_000, 11);
        // extra спешит на 15 кадров: extra[i] = main[i + 15]
        let mut extra = main[15..].to_vec();
        extra.resize(main.len(), 0.0);
        let (lag, _) = best_lag(&extra, &main, 100);
        assert_eq!(lag, 15);
    }

    #[test]
    fn handles_quiet_signal() {
        let silence = vec![0.0f32; 500];
        let (_, score) = best_lag(&silence, &silence, 10);
        assert!(score.is_nan() || score <= 0.0);
    }

    #[test]
    fn windows_are_spread_over_duration() {
        let starts = window_starts(3_600_000, 60_000);
        assert_eq!(starts.len(), 3);
        assert!(starts[0] < starts[1] && starts[1] < starts[2]);
        assert!(starts[2] + 60_000 <= 3_600_000);
    }

    #[test]
    fn short_file_has_single_window() {
        assert_eq!(window_starts(30_000, 28_000), vec![0]);
    }

    #[test]
    fn envelope_ignores_dc() {
        let mut bytes = Vec::new();
        for _ in 0..8_000 {
            bytes.extend_from_slice(&1000i16.to_le_bytes());
        }
        let values = envelope(&bytes);
        assert!(values.iter().all(|value| value.abs() < 1e-6));
    }
}
