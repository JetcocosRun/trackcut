use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};
use crate::extract::ExtractPlan;
use crate::model::{MediaInfo, StreamKind};
use crate::plan::MuxPlan;
use crate::probe;
use crate::tools::ToolPaths;

#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub streams_expected: usize,
    pub streams_found: usize,
    pub duration_delta_ms: Option<i64>,
    pub warnings: Vec<String>,
}

/// Повторно читает выходной файл и сверяет состав дорожек и длительность с планом.
pub fn verify_output(
    output: &Path,
    input: &MediaInfo,
    plan: &MuxPlan,
    tools: &ToolPaths,
) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();

    let mut expected: HashMap<StreamKind, usize> = HashMap::new();
    for entry in &plan.keep {
        if let Some(stream) = input.streams.get(entry.uid) {
            *expected.entry(stream.kind).or_default() += 1;
        }
    }
    for extra in &plan.extras {
        for track in &extra.tracks {
            if let Some(stream) = extra.info.streams.get(track.uid) {
                *expected.entry(stream.kind).or_default() += 1;
            }
        }
    }
    report.streams_expected = expected.values().sum();

    let output_info = match probe::probe(output, tools) {
        Ok(info) => info,
        Err(err) => {
            // Не валим готовый файл из-за капризов probe, но честно сообщаем.
            report.warnings.push(format!(
                "не удалось проверить результат ({err}); файл создан, откройте его вручную"
            ));
            return Ok(report);
        }
    };

    let mut actual: HashMap<StreamKind, usize> = HashMap::new();
    for stream in &output_info.streams {
        if stream.kind.is_selectable() {
            *actual.entry(stream.kind).or_default() += 1;
        }
    }
    report.streams_found = actual.values().sum();

    for (kind, count) in &expected {
        let found = actual.get(kind).copied().unwrap_or(0);
        if found != *count {
            return Err(Error::Verify(format!(
                "{}: ожидалось дорожек {}, получено {}",
                kind.label_ru(),
                count,
                found
            )));
        }
    }

    if let (Some(before), Some(after)) = (input.duration_ms, output_info.duration_ms) {
        let delta = after as i64 - before as i64;
        report.duration_delta_ms = Some(delta);
        if delta.abs() > 2000 {
            report.warnings.push(format!(
                "длительность отличается на {:.1} с — проверьте файл",
                delta as f64 / 1000.0
            ));
        }
    }

    if plan.convert_subtitles_to_mov_text {
        if let Some(stream) = output_info
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Subtitle)
        {
            if !stream.codec_raw.to_ascii_lowercase().contains("mov_text") {
                report.warnings.push(format!(
                    "субтитры в выходе имеют кодек {}, а не MOV text",
                    stream.codec_display()
                ));
            }
        }
    }

    Ok(report)
}

/// Проверяет, что в сохранённом файле ровно те дорожки, что просили.
pub fn verify_extract(
    output: &Path,
    input: &MediaInfo,
    plan: &ExtractPlan,
    tools: &ToolPaths,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let info = match probe::probe(output, tools) {
        Ok(info) => info,
        Err(err) => {
            warnings.push(format!(
                "не удалось проверить извлечённую дорожку ({err}); файл создан, проверьте вручную"
            ));
            return Ok(warnings);
        }
    };

    let mut expected: HashMap<StreamKind, usize> = HashMap::new();
    for uid in &plan.tracks {
        if let Some(stream) = input.streams.get(*uid) {
            *expected.entry(stream.kind).or_default() += 1;
        }
    }

    let mut actual: HashMap<StreamKind, usize> = HashMap::new();
    for stream in &info.streams {
        if stream.kind.is_selectable() {
            *actual.entry(stream.kind).or_default() += 1;
        }
    }

    for (kind, count) in &expected {
        let found = actual.get(kind).copied().unwrap_or(0);
        if found != *count {
            return Err(Error::Verify(format!(
                "{}: ожидалось дорожек {}, получено {}",
                kind.label_ru(),
                count,
                found
            )));
        }
    }

    if expected.len() > 1 {
        warnings.push("в файле несколько типов дорожек — проверьте, что взяли нужные".to_string());
    }

    Ok(warnings)
}
