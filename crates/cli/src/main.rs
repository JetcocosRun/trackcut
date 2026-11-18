use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use trackcut_core::extract::{default_track_path, prepare_extract, ExtractContainer, ExtractPlan};
use trackcut_core::model::{format_duration, format_size, OutputContainer, StreamKind};
use trackcut_core::plan::{default_output_path, paths_equal, prepare};
use trackcut_core::{
    probe, run_extract, run_mux, ExecEvent, ExtraInput, ExtraTrack, MediaInfo, MuxPlan, ToolPaths,
};

#[derive(Parser)]
#[command(
    name = "trackcut-cli",
    version,
    about = "Удаление ненужных дорожек из MKV/MP4/MOV/WebM без перекодирования"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Показать дорожки файла.
    Probe {
        input: PathBuf,
        /// Вывести полный JSON.
        #[arg(long)]
        json: bool,
        /// Показать все теги дорожек.
        #[arg(long, short)]
        verbose: bool,
    },
    /// Собрать новый файл без выбранных дорожек.
    Mux {
        input: PathBuf,
        /// Выходной файл.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Папка для результата (имя будет <исходник>_clean.<ext>).
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Контейнер: mkv, webm, mp4, mov.
        #[arg(long)]
        container: Option<String>,
        /// Оставить только эти дорожки (номера из probe, через запятую).
        #[arg(long, value_delimiter = ',')]
        keep: Vec<usize>,
        /// Удалить эти дорожки (номера из probe, через запятую).
        #[arg(long, value_delimiter = ',')]
        drop: Vec<usize>,
        #[arg(long)]
        no_chapters: bool,
        #[arg(long)]
        no_tags: bool,
        #[arg(long)]
        no_attachments: bool,
        /// Конвертировать текстовые субтитры в MOV text (только MP4/MOV).
        #[arg(long)]
        mov_text: bool,
        /// Взять дорожки из другого файла (можно повторять).
        #[arg(long = "add", value_name = "FILE")]
        add: Vec<PathBuf>,
        /// Сдвиг добавленных дорожек, мс (положительный — позже).
        #[arg(long = "add-delay", default_value_t = 0)]
        add_delay: i64,
        /// Перезаписать существующий выходной файл.
        #[arg(long, short = 'y')]
        overwrite: bool,
        /// Только показать план и команду, ничего не запускать.
        #[arg(long)]
        dry_run: bool,
    },
    /// Сохранить выбранные дорожки в отдельный файл (mka/mks/mkv).
    Extract {
        input: PathBuf,
        /// Выходной файл (.mka, .mks или .mkv). По умолчанию — рядом с исходником.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Папка для результата.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Какие дорожки сохранить (номера из probe, через запятую). По умолчанию все.
        #[arg(long, short = 't', value_delimiter = ',')]
        track: Vec<usize>,
        /// Контейнер: mka, mks, mkv.
        #[arg(long)]
        container: Option<String>,
        /// Перезаписать существующий файл.
        #[arg(long, short = 'y')]
        overwrite: bool,
        /// Только показать план и команду, ничего не запускать.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let tools = ToolPaths::detect();

    let result = match cli.command {
        Command::Probe {
            input,
            json,
            verbose,
        } => cmd_probe(&tools, &input, json, verbose),
        Command::Extract {
            input,
            output,
            out_dir,
            track,
            container,
            overwrite,
            dry_run,
        } => cmd_extract(
            &tools, input, output, out_dir, track, container, overwrite, dry_run,
        ),
        Command::Mux {
            input,
            output,
            out_dir,
            container,
            keep,
            drop,
            no_chapters,
            no_tags,
            no_attachments,
            mov_text,
            add,
            add_delay,
            overwrite,
            dry_run,
        } => cmd_mux(
            &tools,
            input,
            output,
            out_dir,
            container,
            keep,
            drop,
            no_chapters,
            no_tags,
            no_attachments,
            mov_text,
            &add,
            add_delay,
            overwrite,
            dry_run,
        ),
    };

    if let Err(err) = result {
        eprintln!("Ошибка: {err}");
        std::process::exit(1);
    }
}

fn cmd_probe(tools: &ToolPaths, input: &Path, json: bool, verbose: bool) -> Result<(), String> {
    let info = probe::probe(input, tools).map_err(|err| err.to_string())?;

    if json {
        let rendered = serde_json::to_string_pretty(&info).map_err(|err| format!("JSON: {err}"))?;
        println!("{rendered}");
        return Ok(());
    }

    print_info(&info, verbose);
    Ok(())
}

fn print_info(info: &MediaInfo, verbose: bool) {
    println!("Файл:      {}", info.path.display());
    println!(
        "Контейнер: {}{}",
        if info.container.is_empty() {
            "неизвестен"
        } else {
            &info.container
        },
        info.duration_ms
            .map(|ms| format!(" · {}", format_duration(ms)))
            .unwrap_or_default()
    );
    println!(
        "Главы:     {} · Вложения: {}",
        info.chapters,
        info.attachments.len()
    );
    println!();

    let header = format!(
        "{:>3}  {:<10} {:<12} {:<16} {:<30} {:<28} {:>10}  {:>3} {:>3}",
        "#", "Тип", "Кодек", "Язык", "Название", "Параметры", "Размер", "D", "F"
    );
    println!("{header}");
    println!("{}", "-".repeat(header.chars().count()));

    for (index, stream) in info.streams.iter().enumerate() {
        let params = match stream.kind {
            StreamKind::Video => format!("{} {}", stream.resolution_label(), stream.fps_label())
                .trim()
                .to_string(),
            StreamKind::Audio => format!(
                "{} {} {}",
                stream.channels_label(),
                stream.sample_rate_label(),
                stream.bitrate_label()
            )
            .trim()
            .to_string(),
            StreamKind::Subtitle => stream.codec_display(),
            _ => String::new(),
        };
        let size = stream
            .effective_size(info.duration_ms)
            .map(|bytes| {
                if stream.size_bytes.is_none() && stream.bitrate.is_some() {
                    format!("≈{}", format_size(bytes))
                } else {
                    format_size(bytes)
                }
            })
            .unwrap_or_default();

        println!(
            "{:>3}  {:<10} {:<12} {:<16} {:<30} {:<28} {:>10}  {:>3} {:>3}",
            index + 1,
            stream.kind.icon().to_string() + " " + stream.kind.label_ru(),
            truncate(&stream.codec_display(), 12),
            truncate(&stream.language_label(), 16),
            truncate(stream.display_name(), 30),
            truncate(&params, 28),
            size,
            if stream.default { "да" } else { "—" },
            if stream.forced { "да" } else { "—" },
        );

        if verbose && !stream.tags.is_empty() {
            let mut tags: Vec<_> = stream.tags.iter().collect();
            tags.sort_by(|a, b| a.0.cmp(b.0));
            for (key, value) in tags {
                println!("      {key} = {value}");
            }
        }
    }

    if !info.attachments.is_empty() {
        println!();
        println!("Вложения:");
        for attachment in &info.attachments {
            println!(
                "  {} ({}, {})",
                attachment.file_name,
                attachment.mime_type,
                format_size(attachment.size)
            );
        }
    }
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut result: String = value.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[allow(clippy::too_many_arguments)]
fn cmd_mux(
    tools: &ToolPaths,
    input: PathBuf,
    output: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    container: Option<String>,
    keep: Vec<usize>,
    drop: Vec<usize>,
    no_chapters: bool,
    no_tags: bool,
    no_attachments: bool,
    mov_text: bool,
    add: &[PathBuf],
    add_delay: i64,
    overwrite: bool,
    dry_run: bool,
) -> Result<(), String> {
    let info = probe::probe(&input, tools).map_err(|err| err.to_string())?;

    let container = match (container, output.as_ref().and_then(|p| p.extension())) {
        (Some(value), _) => parse_container(&value)?,
        (None, Some(ext)) => {
            OutputContainer::from_extension(&ext.to_string_lossy()).ok_or_else(|| {
                format!(
                    "неизвестное расширение выходного файла: {}",
                    ext.to_string_lossy()
                )
            })?
        }
        (None, None) => OutputContainer::Mkv,
    };

    let output = match output {
        Some(path) => {
            if path.extension().is_none() {
                path.with_extension(container.extension())
            } else {
                path
            }
        }
        None => {
            let dir = out_dir.unwrap_or_else(|| {
                input
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            });
            default_output_path(&input, container, "_clean")
                .components()
                .next_back()
                .map(|name| dir.join(name.as_os_str()))
                .unwrap_or_else(|| dir.join("output"))
        }
    };

    if paths_equal(&input, &output) {
        return Err("выходной файл совпадает с исходным".to_string());
    }

    let selected = select_streams(&info, &keep, &drop)?;
    let extras = build_extras(tools, add, add_delay)?;
    let plan = MuxPlan {
        input: input.clone(),
        output: output.clone(),
        container,
        keep: selected,
        keep_chapters: !no_chapters,
        keep_tags: !no_tags,
        keep_attachments: !no_attachments,
        convert_subtitles_to_mov_text: mov_text,
        overwrite,
        backend: None,
        extras,
    };

    let prepared = prepare(&info, &plan, tools).map_err(|err| err.to_string())?;
    for warning in &prepared.warnings {
        eprintln!("Предупреждение: {warning}");
    }
    eprintln!("Бэкенд: {}", prepared.spec.backend.label());
    eprintln!(
        "Команда: {} {}",
        prepared.spec.program.display(),
        prepared
            .spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    eprintln!("Выход: {}", plan.output.display());

    if dry_run {
        return Ok(());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut last_line: Option<String> = None;

    let outcome = run_mux(&info, &plan, tools, cancel, |event| match event {
        ExecEvent::Progress(percent) => {
            eprint!("\r{:>5.1}%", percent * 100.0);
            let _ = std::io::stderr().flush();
        }
        ExecEvent::Warning(warning) => {
            eprintln!("\nПредупреждение: {warning}");
        }
        ExecEvent::Message(message) => {
            last_line = Some(message);
        }
    })
    .map_err(|err| err.to_string())?;

    eprintln!("\r100.0%");
    println!("Готово: {}", outcome.output.display());
    for warning in &outcome.warnings {
        println!("  ! {warning}");
    }
    if let Some(line) = last_line {
        println!("  последняя строка лога: {line}");
    }
    Ok(())
}

fn parse_container(value: &str) -> Result<OutputContainer, String> {
    match value.to_ascii_lowercase().as_str() {
        "mkv" | "matroska" => Ok(OutputContainer::Mkv),
        "webm" => Ok(OutputContainer::Webm),
        "mp4" | "m4v" => Ok(OutputContainer::Mp4),
        "mov" => Ok(OutputContainer::Mov),
        other => Err(format!(
            "неизвестный контейнер «{other}» (доступны mkv, webm, mp4, mov)"
        )),
    }
}

fn parse_extract_container(value: &str) -> Result<ExtractContainer, String> {
    match value.to_ascii_lowercase().as_str() {
        "mka" | "audio" => Ok(ExtractContainer::Mka),
        "mks" | "subs" | "subtitles" => Ok(ExtractContainer::Mks),
        "mkv" | "matroska" => Ok(ExtractContainer::Mkv),
        other => Err(format!(
            "неизвестный контейнер «{other}» (доступны mka, mks, mkv)"
        )),
    }
}

/// Читает файлы из `--add` и превращает их в источники дорожек.
fn build_extras(
    tools: &ToolPaths,
    add: &[PathBuf],
    delay_ms: i64,
) -> Result<Vec<ExtraInput>, String> {
    let mut extras = Vec::new();
    for path in add {
        let extra_info = probe::probe(path, tools)
            .map_err(|err| format!("не удалось прочитать «{}»: {err}", path.display()))?;
        let tracks: Vec<ExtraTrack> = extra_info
            .streams
            .iter()
            .enumerate()
            .filter(|(_, stream)| stream.kind.is_selectable())
            .map(|(uid, _)| ExtraTrack {
                uid,
                delay_ms,
                default: None,
                forced: None,
            })
            .collect();
        if tracks.is_empty() {
            return Err(format!(
                "в «{}» нет дорожек, которые можно добавить",
                path.display()
            ));
        }
        extras.push(ExtraInput {
            info: extra_info,
            tracks,
        });
    }
    Ok(extras)
}

#[allow(clippy::too_many_arguments)]
fn cmd_extract(
    tools: &ToolPaths,
    input: PathBuf,
    output: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    tracks: Vec<usize>,
    container: Option<String>,
    overwrite: bool,
    dry_run: bool,
) -> Result<(), String> {
    let info = probe::probe(&input, tools).map_err(|err| err.to_string())?;
    let uids = select_uids(&info, &tracks, &[])?;
    let streams: Vec<&trackcut_core::StreamInfo> = uids
        .iter()
        .filter_map(|uid| info.streams.get(*uid))
        .collect();

    let container = match container {
        Some(value) => parse_extract_container(&value)?,
        None => ExtractContainer::for_streams(&streams),
    };

    let output = match output {
        Some(path) => {
            if path.extension().is_none() {
                path.with_extension(container.extension())
            } else {
                path
            }
        }
        None => {
            let default = default_track_path(&input, &streams);
            let dir = out_dir.unwrap_or_else(|| {
                input
                    .parent()
                    .map(|path| path.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            });
            let name = default
                .file_name()
                .map(|name| name.to_os_string())
                .unwrap_or_else(|| std::ffi::OsString::from("track"));
            unique_path(&dir.join(name), !overwrite)
        }
    };

    let plan = ExtractPlan {
        input: input.clone(),
        output: output.clone(),
        tracks: uids,
        container,
        overwrite,
        backend: None,
    };

    let prepared = prepare_extract(&info, &plan, tools).map_err(|err| err.to_string())?;
    for warning in &prepared.warnings {
        eprintln!("Предупреждение: {warning}");
    }
    eprintln!("Бэкенд: {}", prepared.spec.backend.label());
    eprintln!(
        "Команда: {} {}",
        prepared.spec.program.display(),
        prepared
            .spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    eprintln!("Выход: {}", plan.output.display());

    if dry_run {
        return Ok(());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut last_line: Option<String> = None;
    let outcome = run_extract(&info, &plan, tools, cancel, |event| match event {
        ExecEvent::Progress(percent) => {
            eprint!("\r{:>5.1}%", percent * 100.0);
            let _ = std::io::stderr().flush();
        }
        ExecEvent::Warning(warning) => eprintln!("\nПредупреждение: {warning}"),
        ExecEvent::Message(message) => last_line = Some(message),
    })
    .map_err(|err| err.to_string())?;

    eprintln!("\r100.0%");
    println!("Сохранено: {}", outcome.output.display());
    for warning in &outcome.warnings {
        println!("  ! {warning}");
    }
    if let Some(line) = last_line {
        println!("  последняя строка лога: {line}");
    }
    Ok(())
}

/// Подбирает свободное имя, если файл уже существует.
fn unique_path(path: &Path, dedupe: bool) -> PathBuf {
    if !dedupe || !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "track".to_string());
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut index = 1;
    loop {
        let candidate = parent.join(format!("{stem} ({index}).{extension}"));
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

fn select_uids(info: &MediaInfo, keep: &[usize], drop: &[usize]) -> Result<Vec<usize>, String> {
    let to_uid = |display: usize| -> Result<usize, String> {
        let uid = display
            .checked_sub(1)
            .ok_or_else(|| format!("номера дорожек начинаются с 1, получено {display}"))?;
        let stream = info
            .streams
            .get(uid)
            .ok_or_else(|| format!("дорожки №{display} нет в файле"))?;
        if !stream.kind.is_selectable() {
            return Err(format!(
                "дорожка №{display} имеет тип «{}» и не может быть выбрана",
                stream.kind.label_ru()
            ));
        }
        Ok(uid)
    };

    let mut uids: Vec<usize> = if keep.is_empty() {
        info.streams
            .iter()
            .enumerate()
            .filter(|(_, stream)| stream.kind.is_selectable())
            .map(|(uid, _)| uid)
            .collect()
    } else {
        keep.iter()
            .map(|value| to_uid(*value))
            .collect::<Result<Vec<_>, _>>()?
    };

    for value in drop {
        let uid = to_uid(*value)?;
        uids.retain(|existing| *existing != uid);
    }

    uids.sort_unstable();
    uids.dedup();

    if uids.is_empty() {
        return Err("после применения --keep/--drop не осталось ни одной дорожки".to_string());
    }

    Ok(uids)
}

fn select_streams(
    info: &MediaInfo,
    keep: &[usize],
    drop: &[usize],
) -> Result<Vec<trackcut_core::KeepTrack>, String> {
    Ok(select_uids(info, keep, drop)?
        .into_iter()
        .map(trackcut_core::KeepTrack::new)
        .collect())
}
