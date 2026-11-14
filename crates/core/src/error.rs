use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("не найден инструмент: {0}")]
    ToolMissing(&'static str),

    #[error("не удалось запустить {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{program} завершился с кодом {code}: {stderr}")]
    CommandFailed {
        program: String,
        code: i32,
        stderr: String,
    },

    #[error("ошибка разбора JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),

    #[error("неподдерживаемый файл: {0}")]
    Unsupported(String),

    #[error("некорректный план: {0}")]
    InvalidPlan(String),

    #[error("операция отменена")]
    Cancelled,

    #[error("проверка результата не пройдена: {0}")]
    Verify(String),

    #[error("файл уже существует: {0}")]
    OutputExists(PathBuf),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
