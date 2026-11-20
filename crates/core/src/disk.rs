//! Свободное и полное место на диске.
//!
//! Нужно, чтобы до начала извлечения или склейки предупредить пользователя,
//! что на целевом диске может не хватить места под результат.
//!
//! На Windows объём запрашивается напрямую у `GetDiskFreeSpaceExW` через FFI:
//! без новых зависимостей и без вызовов внешних утилит. На остальных
//! платформах функция честно возвращает `None` — там своих средств нет.

use std::path::Path;

/// Свободное место на диске, где лежит путь (или ближайший существующий родитель).
/// None — если определить не удалось.
pub fn free_bytes(path: &Path) -> Option<u64> {
    volume_info(path).map(|info| info.free)
}

/// Полный объём диска, аналогично.
pub fn total_bytes(path: &Path) -> Option<u64> {
    volume_info(path).map(|info| info.total)
}

/// Данные о томе, на котором лежит путь.
#[derive(Debug, Clone, Copy)]
struct VolumeInfo {
    /// Доступно вызывающему процессу байт.
    free: u64,
    /// Полный размер тома в байтах.
    total: u64,
}

/// Свободное и полное место для пути; `None`, если том определить не удалось.
fn volume_info(path: &Path) -> Option<VolumeInfo> {
    #[cfg(windows)]
    {
        let dir = existing_dir(path)?;
        windows::volume_info(&dir)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// Ближайший существующий каталог: сам путь, затем его родители.
///
/// Путь может указывать на ещё не созданный файл или каталог — тогда место
/// считается по тому диску, где он появится.
#[cfg(windows)]
fn existing_dir(path: &Path) -> Option<std::path::PathBuf> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    loop {
        if current.is_dir() {
            return Some(current);
        }
        // Файл существует — место нужно на его диске, то есть у родителя.
        current = current.parent()?.to_path_buf();
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use super::VolumeInfo;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_available: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }

    /// Запрашивает у системы свободное и полное место для каталога.
    pub(super) fn volume_info(dir: &Path) -> Option<VolumeInfo> {
        let wide = wide_path(dir);
        let mut free_available = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        // SAFETY: `wide` — корректный UTF-16 путь с завершающим нулём, все
        // указатели на выходные значения валидны и живут до конца вызова.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_available,
                &mut total,
                &mut total_free,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(VolumeInfo {
            free: free_available,
            total,
        })
    }

    /// Путь в UTF-16 с завершающим нулём и `\\?\`-префиксом (для длинных путей).
    ///
    /// Разделители `/` заменяются на `\`: с префиксом `\\?\` система больше не
    /// нормализует путь сама. UNC-пути (`\\server\share\...`) переводятся
    /// в форму `\\?\UNC\server\share\...`.
    fn wide_path(path: &Path) -> Vec<u16> {
        let text: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .map(|unit| {
                if unit == u16::from(b'/') {
                    u16::from(b'\\')
                } else {
                    unit
                }
            })
            .collect();
        let mut wide = Vec::with_capacity(text.len() + 8);
        let slash = u16::from(b'\\');
        let question = u16::from(b'?');
        let dot = u16::from(b'.');
        if text.starts_with(&[slash, question, slash])
            || text.starts_with(&[slash, slash, dot, slash])
        {
            // Уже префиксованный или device-путь — не трогаем.
            wide.extend_from_slice(&text);
        } else if text.starts_with(&[slash, slash]) {
            wide.extend(r"\\?\UNC\".encode_utf16());
            wide.extend_from_slice(&text[2..]);
        } else {
            wide.extend(r"\\?\".encode_utf16());
            wide.extend_from_slice(&text);
        }
        wide.push(0);
        wide
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn temp_dir_has_free_space() {
        let free = free_bytes(&std::env::temp_dir());
        assert!(
            matches!(free, Some(bytes) if bytes > 0),
            "нет места: {free:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn total_is_at_least_free() {
        let dir = std::env::temp_dir();
        let free = free_bytes(&dir).expect("свободное место");
        let total = total_bytes(&dir).expect("полный объём");
        assert!(total >= free, "total={total} free={free}");
        assert!(total > 0);
    }

    #[cfg(windows)]
    #[test]
    fn missing_path_uses_parent() {
        let dir = std::env::temp_dir().join(format!(
            "trackcut-no-such-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(!dir.exists());
        assert!(free_bytes(&dir).is_some());
        assert!(total_bytes(&dir).is_some());
    }

    #[cfg(not(windows))]
    #[test]
    fn other_platforms_have_nothing() {
        let dir = std::env::temp_dir();
        assert_eq!(free_bytes(&dir), None);
        assert_eq!(total_bytes(&dir), None);
    }
}
