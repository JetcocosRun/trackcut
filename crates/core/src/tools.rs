use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct ToolPaths {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub mkvmerge: Option<PathBuf>,
}

impl ToolPaths {
    /// Ищет инструменты в переменных окружения, рядом с приложением, в ./tools и в PATH.
    pub fn detect() -> Self {
        Self {
            ffmpeg: locate("ffmpeg", &["ffmpeg.exe", "ffmpeg"]),
            ffprobe: locate("ffprobe", &["ffprobe.exe", "ffprobe"]),
            mkvmerge: locate("mkvmerge", &["mkvmerge.exe", "mkvmerge"]),
        }
    }

    pub fn with_overrides(
        ffmpeg: Option<PathBuf>,
        ffprobe: Option<PathBuf>,
        mkvmerge: Option<PathBuf>,
    ) -> Self {
        let detected = Self::detect();
        Self {
            ffmpeg: ffmpeg.filter(|p| p.exists()).or(detected.ffmpeg),
            ffprobe: ffprobe.filter(|p| p.exists()).or(detected.ffprobe),
            mkvmerge: mkvmerge.filter(|p| p.exists()).or(detected.mkvmerge),
        }
    }

    pub fn ffmpeg(&self) -> Result<PathBuf> {
        self.ffmpeg.clone().ok_or(Error::ToolMissing("ffmpeg"))
    }

    pub fn ffprobe(&self) -> Result<PathBuf> {
        self.ffprobe.clone().ok_or(Error::ToolMissing("ffprobe"))
    }

    pub fn mkvmerge(&self) -> Result<PathBuf> {
        self.mkvmerge.clone().ok_or(Error::ToolMissing("mkvmerge"))
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.ffmpeg.is_none() {
            missing.push("ffmpeg");
        }
        if self.ffprobe.is_none() {
            missing.push("ffprobe");
        }
        if self.mkvmerge.is_none() {
            missing.push("mkvmerge");
        }
        missing
    }

    pub fn versions(&self) -> Vec<(String, Option<String>)> {
        let entries = [
            ("ffmpeg", self.ffmpeg.as_ref()),
            ("ffprobe", self.ffprobe.as_ref()),
            ("mkvmerge", self.mkvmerge.as_ref()),
        ];
        entries
            .into_iter()
            .map(|(name, path)| {
                let version = path.and_then(|p| tool_version(p));
                (name.to_string(), version)
            })
            .collect()
    }
}

fn locate(env_name: &str, exe_names: &[&str]) -> Option<PathBuf> {
    let env_key = format!("TRACKCUT_{}", env_name.to_ascii_uppercase());
    if let Ok(value) = std::env::var(&env_key) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("tools"));
        roots.push(cwd.clone());
        if let Some(parent) = cwd.parent() {
            roots.push(parent.join("tools"));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut current = exe.parent().map(Path::to_path_buf);
        let mut depth = 0;
        while let Some(dir) = current {
            roots.push(dir.join("tools"));
            roots.push(dir.clone());
            if depth >= 3 {
                break;
            }
            current = dir.parent().map(Path::to_path_buf);
            depth += 1;
        }
    }
    if let Some(home) = dirs_home() {
        roots.push(home.join("tools"));
    }

    for root in &roots {
        if !root.is_dir() {
            continue;
        }
        // `tools` может содержать архивные раскладки вида
        // tools/mkvtoolnix/mkvtoolnix/mkvmerge.exe и tools/ffmpeg-*/bin/ffmpeg.exe.
        let depth = if root
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("tools"))
        {
            2
        } else {
            1
        };
        if let Some(found) = search_dir(root, exe_names, depth) {
            return Some(found);
        }
    }

    for name in exe_names {
        if let Ok(found) = which::which(name) {
            return Some(found);
        }
    }
    None
}

fn search_dir(dir: &Path, exe_names: &[&str], depth: u8) -> Option<PathBuf> {
    for name in exe_names {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        let bin = dir.join("bin").join(name);
        if bin.is_file() {
            return Some(bin);
        }
    }
    if depth == 0 {
        return None;
    }

    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    subdirs.sort();

    for subdir in subdirs {
        let name = subdir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "target" | "downloads" | "node_modules" | ".git" | "crates" | "demo"
        ) {
            continue;
        }
        if let Some(found) = search_dir(&subdir, exe_names, depth - 1) {
            return Some(found);
        }
    }
    None
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

pub fn tool_version(path: &Path) -> Option<String> {
    let output = crate::sys::hidden_command(path)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        text.into_owned()
    };
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}
