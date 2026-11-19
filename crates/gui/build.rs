//! Встраивает иконку и версию в trackcut.exe через windres из MinGW.
//!
//! Если windres не найден, сборка не падает: просто остаётся стандартная иконка.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icon = manifest.join("assets").join("icon.ico");
    if !icon.is_file() {
        println!("cargo:warning=assets/icon.ico не найден — exe останется без иконки");
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let resource = out.join("trackcut_icon.rc");
    let object = out.join("trackcut_icon.o");

    let rc = format!(
        "1 ICON \"{}\"\n1 VERSIONINFO\nFILEVERSION 0,1,0,0\nPRODUCTVERSION 0,1,0,0\n\
         FILEFLAGSMASK 0x3fL\nFILEFLAGS 0x0L\nFILEOS 0x40004L\nFILETYPE 0x1L\nFILESUBTYPE 0x0L\n\
         BEGIN\n  BLOCK \"StringFileInfo\"\n  BEGIN\n    BLOCK \"040904b0\"\n    BEGIN\n\
         VALUE \"FileDescription\", \"TrackCut - remove tracks without re-encoding\"\n\
         VALUE \"ProductName\", \"TrackCut\"\n\
         VALUE \"OriginalFilename\", \"trackcut.exe\"\n    END\n  END\n\
         BLOCK \"VarFileInfo\"\n  BEGIN\n    VALUE \"Translation\", 0x409, 1200\n  END\nEND\n",
        icon.display().to_string().replace('\\', "/")
    );
    if std::fs::write(&resource, rc).is_err() {
        println!("cargo:warning=не удалось записать .rc — exe останется без иконки");
        return;
    }

    let Some(windres) = find_windres() else {
        println!("cargo:warning=windres не найден — exe останется без иконки");
        return;
    };

    let status = Command::new(&windres)
        .arg("-i")
        .arg(&resource)
        .arg("-o")
        .arg(&object)
        .arg("--input-format=rc")
        .arg("--output-format=coff")
        .status();

    match status {
        Ok(status) if status.success() && object.is_file() => {
            println!("cargo:rustc-link-arg={}", object.display());
        }
        Ok(status) => println!("cargo:warning=windres завершился с кодом {status}"),
        Err(err) => println!("cargo:warning=не удалось запустить windres: {err}"),
    }
}

fn find_windres() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("TRACKCUT_WINDRES") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }

    // windres из PATH (build.ps1 добавляет туда MinGW).
    if Command::new("windres")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("windres"));
    }

    // Куда build.ps1 / README кладут MinGW на этой машине.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("trackcut-tools")
                .join("mingw64")
                .join("bin")
                .join("windres.exe"),
        );
    }
    for root in ["C:\\msys64\\mingw64\\bin", "C:\\mingw64\\bin"] {
        candidates.push(Path::new(root).join("windres.exe"));
    }
    candidates.into_iter().find(|path| path.is_file())
}
