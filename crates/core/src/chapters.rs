//! Импорт глав из другого файла.
//!
//! mkvmerge принимает главы только в текстовых форматах (XML, OGM, simple).
//! Если пользователь указал MKV/WebM, главы сначала извлекаются `mkvextract`
//! во временный XML.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{Error, Result};
use crate::tools::ToolPaths;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Возвращает путь к файлу глав, пригодному для `mkvmerge --chapters`.
pub fn resolve(source: &Path, tools: &ToolPaths) -> Result<PathBuf> {
    let ext = source
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "mkv" | "mka" | "mks" | "webm") {
        return Ok(source.to_path_buf());
    }

    let extract = mkvextract(tools)?;
    let temp = std::env::temp_dir().join(format!(
        "trackcut-chapters-{}-{}.xml",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let output = Command::new(&extract)
        .arg(source)
        .arg("chapters")
        .arg(&temp)
        .output()
        .map_err(|err| Error::Other(format!("не удалось запустить mkvextract: {err}")))?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&temp);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Other(format!(
            "не удалось извлечь главы: {}",
            stderr.trim()
        )));
    }

    let empty = std::fs::metadata(&temp)
        .map(|meta| meta.len() < 64)
        .unwrap_or(true);
    if empty {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::Other(format!(
            "в файле нет глав: {}",
            source.display()
        )));
    }

    Ok(temp)
}

/// Удаляет временные XML, оставшиеся от прошлых запусков (старше часа).
pub fn cleanup_temp() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("trackcut-chapters-") || !name.ends_with(".xml") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age.as_secs() > 3600);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn mkvextract(tools: &ToolPaths) -> Result<PathBuf> {
    let name = if cfg!(windows) {
        "mkvextract.exe"
    } else {
        "mkvextract"
    };

    if let Ok(mkvmerge) = tools.mkvmerge() {
        if let Some(dir) = mkvmerge.parent() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    // Рядом с mkvmerge не оказалось — пробуем PATH.
    let probe = Command::new(name).arg("--version").output();
    match probe {
        Ok(output) if output.status.success() => Ok(PathBuf::from(name)),
        _ => Err(Error::ToolMissing("mkvextract")),
    }
}
