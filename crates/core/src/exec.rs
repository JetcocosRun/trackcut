use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::model::{Backend, MediaInfo};
use crate::plan::{prepare, MuxPlan};
use crate::sys::hidden_command;
use crate::tools::ToolPaths;

#[derive(Debug, Clone)]
pub enum ExecEvent {
    Progress(f32),
    Message(String),
    Warning(String),
}

#[derive(Debug, Clone)]
pub struct MuxOutcome {
    pub output: PathBuf,
    pub backend: Backend,
    pub warnings: Vec<String>,
    pub log: Vec<String>,
}

struct Line {
    is_stderr: bool,
    text: String,
}

pub fn run_mux(
    info: &MediaInfo,
    plan: &MuxPlan,
    tools: &ToolPaths,
    cancel: Arc<AtomicBool>,
    mut on_event: impl FnMut(ExecEvent),
) -> Result<MuxOutcome> {
    let prepared = prepare(info, plan, tools)?;
    let spec = prepared.spec;
    for warning in &prepared.warnings {
        on_event(ExecEvent::Warning(warning.clone()));
    }

    if spec.output_part.exists() {
        let _ = std::fs::remove_file(&spec.output_part);
    }

    let mut command = hidden_command(&spec.program);
    command
        .args(spec.args.iter().map(|arg| arg.as_os_str()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|source| Error::Spawn {
        program: spec.program.display().to_string(),
        source,
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel::<Line>();

    if let Some(stdout) = stdout {
        spawn_reader(stdout, false, tx.clone());
    }
    if let Some(stderr) = stderr {
        spawn_reader(stderr, true, tx.clone());
    }
    drop(tx);

    let duration_ms = info.duration_ms;

    let mut log: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = prepared.warnings.clone();
    let mut last_percent: f32 = 0.0;

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&spec.output_part);
            return Err(Error::Cancelled);
        }

        match rx.recv_timeout(Duration::from_millis(120)) {
            Ok(line) => {
                process_line(
                    &line,
                    &spec.backend,
                    duration_ms,
                    &mut log,
                    &mut warnings,
                    &mut last_percent,
                    &mut on_event,
                );
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    break status;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                break child.wait()?;
            }
        }
    };

    // Забираем хвост вывода, который успел появиться до завершения процесса.
    while let Ok(line) = rx.try_recv() {
        process_line(
            &line,
            &spec.backend,
            duration_ms,
            &mut log,
            &mut warnings,
            &mut last_percent,
            &mut on_event,
        );
    }

    let code = status.code().unwrap_or(-1);
    let success = match spec.backend {
        Backend::Mkvmerge => code == 0 || code == 1,
        Backend::Ffmpeg => code == 0,
    };

    if !success {
        let stderr_tail = log
            .iter()
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::remove_file(&spec.output_part);
        return Err(Error::CommandFailed {
            program: spec.program.display().to_string(),
            code,
            stderr: stderr_tail,
        });
    }

    if code == 1 {
        warnings.push(
            "mkvmerge завершился с предупреждениями — проверьте лог (результат обычно корректен)"
                .to_string(),
        );
    }

    let metadata = std::fs::metadata(&spec.output_part).map_err(|err| {
        Error::Other(format!(
            "инструмент отработал, но выходной файл не создан: {err}"
        ))
    })?;
    if metadata.len() == 0 {
        let _ = std::fs::remove_file(&spec.output_part);
        return Err(Error::Other(
            "создан пустой выходной файл — операция прервана".to_string(),
        ));
    }

    match crate::verify::verify_output(&spec.output_part, info, plan, tools) {
        Ok(report) => {
            warnings.extend(report.warnings);
        }
        Err(err) => {
            let _ = std::fs::remove_file(&spec.output_part);
            return Err(err);
        }
    }

    if spec.output_final.exists() {
        std::fs::remove_file(&spec.output_final)?;
    }
    std::fs::rename(&spec.output_part, &spec.output_final)?;

    on_event(ExecEvent::Progress(1.0));
    Ok(MuxOutcome {
        output: spec.output_final,
        backend: spec.backend,
        warnings,
        log,
    })
}

fn spawn_reader<R: Read + Send + 'static>(reader: R, is_stderr: bool, tx: mpsc::Sender<Line>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.split(b'\n') {
            match line {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).trim_end().to_string();
                    if !text.is_empty() {
                        let _ = tx.send(Line { is_stderr, text });
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn process_line(
    line: &Line,
    backend: &Backend,
    duration_ms: Option<u64>,
    log: &mut Vec<String>,
    warnings: &mut Vec<String>,
    last_percent: &mut f32,
    on_event: &mut impl FnMut(ExecEvent),
) {
    let text = line.text.trim();
    if text.is_empty() {
        return;
    }

    log.push(text.to_string());
    if log.len() > 5000 {
        log.drain(0..1000);
    }

    if let Some(percent) = parse_progress(text, backend, duration_ms) {
        if (percent - *last_percent).abs() >= 0.005 || percent >= 1.0 {
            *last_percent = percent;
            on_event(ExecEvent::Progress(percent.clamp(0.0, 1.0)));
        }
        return;
    }

    if let Some(rest) = text.strip_prefix("#GUI#warning") {
        let rest = rest.trim();
        if !rest.is_empty() {
            warnings.push(rest.to_string());
            on_event(ExecEvent::Warning(rest.to_string()));
        }
        return;
    }
    if text.starts_with("#GUI#error") {
        return;
    }
    if let Some(rest) = text.strip_prefix("#GUI#message") {
        let rest = rest.trim();
        if !rest.is_empty() {
            on_event(ExecEvent::Message(rest.to_string()));
        }
        return;
    }

    let lower = text.to_ascii_lowercase();
    let is_warning = line.is_stderr
        || lower.contains("warning")
        || lower.contains("deprecated")
        || lower.contains("error");

    if !text.starts_with("#GUI#") && (is_warning || !line.is_stderr) {
        on_event(ExecEvent::Message(text.to_string()));
    }
}

fn parse_progress(text: &str, backend: &Backend, duration_ms: Option<u64>) -> Option<f32> {
    match backend {
        Backend::Mkvmerge => {
            let rest = text
                .strip_prefix("#GUI#progress")
                .or_else(|| text.strip_prefix("Progress:"))?
                .trim();
            let value = rest.trim_end_matches('%').trim();
            let percent: f32 = value.parse().ok()?;
            Some(percent / 100.0)
        }
        Backend::Ffmpeg => {
            if text.starts_with("progress=end") {
                return Some(1.0);
            }
            let value = text
                .strip_prefix("out_time_us=")
                .or_else(|| text.strip_prefix("out_time_ms="))?;
            let micros: u64 = value.trim().parse().ok()?;
            let elapsed_ms = micros / 1000;
            let total = duration_ms.filter(|d| *d > 0)?;
            Some((elapsed_ms as f32 / total as f32).min(1.0))
        }
    }
}

/// Проверяет, что инструмент отвечает, и возвращает первую строку версии.
pub fn version_of(program: &OsStr) -> Option<String> {
    let output = hidden_command(std::path::Path::new(program))
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mkvmerge_gui_progress() {
        let value = parse_progress("#GUI#progress 42%", &Backend::Mkvmerge, None).unwrap();
        assert!((value - 0.42).abs() < 1e-6);
    }

    #[test]
    fn parses_legacy_mkvmerge_progress() {
        let value = parse_progress("Progress: 100%", &Backend::Mkvmerge, None).unwrap();
        assert!((value - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parses_ffmpeg_time_into_percent() {
        let value = parse_progress("out_time_us=5000000", &Backend::Ffmpeg, Some(10_000)).unwrap();
        assert!((value - 0.5).abs() < 1e-6);
    }

    #[test]
    fn ffmpeg_end_means_finished() {
        let value = parse_progress("progress=end", &Backend::Ffmpeg, Some(10_000)).unwrap();
        assert_eq!(value, 1.0);
    }

    #[test]
    fn ffmpeg_progress_without_duration_is_ignored() {
        assert!(parse_progress("out_time_us=5000000", &Backend::Ffmpeg, None).is_none());
    }
}
