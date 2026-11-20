use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use trackcut_core::extract::{default_track_path, prepare_extract, ExtractContainer, ExtractPlan};
use trackcut_core::model::{format_duration, format_size, OutputContainer, StreamKind};
use trackcut_core::plan::{default_output_path, parse_ranges, parse_time_ms, paths_equal, prepare};
use trackcut_core::{
    estimate_delay, probe, run_extract, run_mux, ExecEvent, ExtraInput, ExtraTrack, MediaInfo,
    MuxPlan, SplitSpec, ToolPaths,
};

mod text;

use text::Lang;

#[derive(Parser)]
#[command(
    name = "trackcut-cli",
    version,
    about = "Удаление ненужных дорожек из MKV/MP4/MOV/WebM без перекодирования"
)]
struct Cli {
    /// Вывести один JSON-объект в stdout (сообщения — в stderr).
    #[arg(long, global = true)]
    json: bool,
    /// Язык сообщений: ru, en, de, fr, es, it, ja, zh, kk.
    #[arg(
        long,
        global = true,
        default_value = "ru",
        value_parser = ["ru", "en", "de", "fr", "es", "it", "ja", "zh", "kk"]
    )]
    lang: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Показать дорожки файла.
    Probe {
        input: PathBuf,
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
        /// Сдвиг своей дорожки: UID:МС, можно повторять (например 2:-250).
        #[arg(long = "delay", value_name = "UID:MS")]
        delay: Vec<String>,
        /// Импортировать главы из файла (XML, OGM или Matroska).
        #[arg(long, value_name = "FILE")]
        chapters: Option<PathBuf>,
        /// Нарезать результат по размеру части (например, 1000M или 700M).
        #[arg(long, value_name = "SIZE")]
        split_size: Option<String>,
        /// Нарезать по длительности каждой части (00:10:00 или 600).
        #[arg(long, value_name = "TIME")]
        split_duration: Option<String>,
        /// Вырезать диапазоны: 00:01:00-00:05:00,00:10:00- (конец — до конца файла).
        #[arg(long, value_name = "RANGES")]
        split_parts: Option<String>,
        /// Приклеить файл к исходному (тот же набор дорожек, MKV/WebM).
        #[arg(long = "append", value_name = "FILE")]
        append: Vec<PathBuf>,
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
    /// Подобрать сдвиг внешней дорожки по звуку.
    Sync {
        /// Основной файл (с эталонной аудиодорожкой).
        input: PathBuf,
        /// Файл с внешней дорожкой.
        extra: PathBuf,
        /// Дорожка основного файла (номер из probe). По умолчанию первая аудио.
        #[arg(long)]
        main_track: Option<usize>,
        /// Дорожка внешнего файла (номер из probe). По умолчанию первая аудио.
        #[arg(long)]
        extra_track: Option<usize>,
    },
}

/// Общие настройки вывода: JSON и язык человекочитаемых сообщений.
struct Ui {
    json: bool,
    lang: Lang,
}

impl Ui {
    fn new(json: bool, lang: Lang) -> Self {
        // В JSON-режиме человекочитаемые строки — диагностика в stderr;
        // сам JSON от языка не зависит, поэтому ошибки всегда русские.
        Self {
            json,
            lang: if json { Lang::Ru } else { lang },
        }
    }

    fn t(&self, key: &'static str) -> &'static str {
        text::tr(self.lang, key)
    }

    fn f(&self, key: &'static str, args: &[String]) -> String {
        text::fill(self.lang, key, args)
    }

    fn s<'a>(&self, value: &'a str) -> &'a str {
        text::tr_str(self.lang, value)
    }

    /// Человекочитаемая строка: stdout в обычном режиме, stderr в JSON-режиме.
    fn say(&self, message: &str) {
        if self.json {
            eprintln!("{message}");
        } else {
            println!("{message}");
        }
    }

    fn warn(&self, warning: &str) {
        eprintln!(
            "{}",
            self.f("Предупреждение: {}", &[self.s(warning).to_string()])
        );
    }

    fn emit(&self, value: &serde_json::Value) -> Result<(), String> {
        let mut stdout = std::io::stdout();
        serde_json::to_writer_pretty(&mut stdout, value)
            .map_err(|err| self.f("не удалось записать JSON: {}", &[err.to_string()]))?;
        writeln!(stdout)
            .map_err(|err| self.f("не удалось записать JSON: {}", &[err.to_string()]))?;
        Ok(())
    }
}

fn main() {
    let cli = Cli::parse();
    let lang = Lang::from_code(&cli.lang).unwrap_or(Lang::Ru);
    let ui = Ui::new(cli.json, lang);
    let tools = ToolPaths::detect();
    trackcut_core::chapters::cleanup_temp();

    let result = match cli.command {
        Command::Probe { input, verbose } => cmd_probe(&ui, &tools, &input, verbose),
        Command::Extract {
            input,
            output,
            out_dir,
            track,
            container,
            overwrite,
            dry_run,
        } => cmd_extract(
            &ui, &tools, input, output, out_dir, track, container, overwrite, dry_run,
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
            delay,
            chapters,
            split_size,
            split_duration,
            split_parts,
            append,
            overwrite,
            dry_run,
        } => cmd_mux(
            &ui,
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
            &delay,
            chapters,
            split_size,
            split_duration,
            split_parts,
            &append,
            overwrite,
            dry_run,
        ),
        Command::Sync {
            input,
            extra,
            main_track,
            extra_track,
        } => cmd_sync(&ui, &tools, &input, &extra, main_track, extra_track),
    };

    if let Err(err) = result {
        if ui.json {
            let value = serde_json::json!({ "error": err });
            let _ = serde_json::to_writer_pretty(std::io::stdout(), &value);
            println!();
        } else {
            eprintln!("{}", ui.f("Ошибка: {}", &[err]));
        }
        std::process::exit(1);
    }
}

fn cmd_sync(
    ui: &Ui,
    tools: &ToolPaths,
    input: &Path,
    extra: &Path,
    main_track: Option<usize>,
    extra_track: Option<usize>,
) -> Result<(), String> {
    let main_info = probe::probe(input, tools).map_err(|err| err.to_string())?;
    let extra_info = probe::probe(extra, tools).map_err(|err| err.to_string())?;

    let main_uid = pick_audio_uid(ui, &main_info, main_track, "в основном файле")?;
    let extra_uid = pick_audio_uid(ui, &extra_info, extra_track, "во внешнем файле")?;

    eprintln!(
        "{}",
        ui.f(
            "Сравниваю «{}» (дорожка {}) и «{}» (дорожка {})…",
            &[
                input.display().to_string(),
                (main_uid + 1).to_string(),
                extra.display().to_string(),
                (extra_uid + 1).to_string(),
            ]
        )
    );
    let estimate = estimate_delay(tools, &main_info, main_uid, &extra_info, extra_uid)
        .map_err(|err| err.to_string())?;

    let sign = if estimate.delay_ms > 0 { "+" } else { "" };
    ui.say(&ui.f(
        "Сдвиг: {}{} мс",
        &[sign.to_string(), estimate.delay_ms.to_string()],
    ));
    ui.say(&ui.f(
        "Совпало окон: {}/{} · совпадение {}%",
        &[
            estimate.windows_agree.to_string(),
            estimate.windows_total.to_string(),
            format!("{:.0}", estimate.score.max(0.0) * 100.0),
        ],
    ));
    ui.say(&ui.f(
        "Подсказка: --add \"{}\" --add-delay {}",
        &[extra.display().to_string(), estimate.delay_ms.to_string()],
    ));
    for warning in &estimate.warnings {
        ui.warn(warning);
    }

    if ui.json {
        let confidence = if estimate.score.is_finite() {
            Some(estimate.score.max(0.0) as f64)
        } else {
            None
        };
        let value = serde_json::json!({
            "offset_ms": estimate.delay_ms,
            "confidence": confidence,
            "windows_agree": estimate.windows_agree,
            "windows_total": estimate.windows_total,
            "warnings": estimate.warnings,
        });
        ui.emit(&value)?;
    }
    Ok(())
}

/// Номер дорожки из probe (с 1) или первая аудио, если номер не задан.
fn pick_audio_uid(
    ui: &Ui,
    info: &MediaInfo,
    display: Option<usize>,
    where_: &'static str,
) -> Result<usize, String> {
    match display {
        Some(display) => {
            let uid = display.checked_sub(1).ok_or_else(|| {
                ui.f(
                    "номера дорожек начинаются с 1, получено {}",
                    &[display.to_string()],
                )
            })?;
            let stream = info.streams.get(uid).ok_or_else(|| {
                ui.f(
                    "в {} нет дорожки №{}",
                    &[ui.t(where_).to_string(), display.to_string()],
                )
            })?;
            if stream.kind != StreamKind::Audio {
                return Err(ui.f(
                    "дорожка №{} в {} — не аудио",
                    &[display.to_string(), ui.t(where_).to_string()],
                ));
            }
            Ok(uid)
        }
        None => info
            .streams
            .iter()
            .position(|stream| stream.kind == StreamKind::Audio && stream.ff_index.is_some())
            .ok_or_else(|| {
                ui.f(
                    "в {} нет аудиодорожки для сравнения",
                    &[ui.t(where_).to_string()],
                )
            }),
    }
}

fn cmd_probe(ui: &Ui, tools: &ToolPaths, input: &Path, verbose: bool) -> Result<(), String> {
    let info = probe::probe(input, tools).map_err(|err| err.to_string())?;

    if ui.json {
        let file = std::path::absolute(input).unwrap_or_else(|_| input.to_path_buf());
        let value = serde_json::json!({ "file": file, "info": info });
        ui.emit(&value)?;
        return Ok(());
    }

    print_info(ui, &info, verbose);
    Ok(())
}

fn print_info(ui: &Ui, info: &MediaInfo, verbose: bool) {
    println!("{:<11}{}", ui.t("Файл:"), info.path.display());
    println!(
        "{:<11}{}{}",
        ui.t("Контейнер:"),
        if info.container.is_empty() {
            ui.t("неизвестен")
        } else {
            &info.container
        },
        info.duration_ms
            .map(|ms| format!(" · {}", format_duration(ms)))
            .unwrap_or_default()
    );
    println!(
        "{:<11}{} · {} {}",
        ui.t("Главы:"),
        info.chapters_count,
        ui.t("Вложения:"),
        info.attachments.len()
    );
    println!();

    let header = format!(
        "{:>3}  {:<10} {:<12} {:<16} {:<30} {:<28} {:>10}  {:>3} {:>3}",
        "#",
        ui.t("Тип"),
        ui.t("Кодек"),
        ui.t("Язык"),
        ui.t("Название"),
        ui.t("Параметры"),
        ui.t("Размер"),
        "D",
        "F"
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
                localize_channels(ui, &stream.channels_label()),
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
                    format!("≈{}", localize_size(ui, format_size(bytes)))
                } else {
                    localize_size(ui, format_size(bytes))
                }
            })
            .unwrap_or_default();

        println!(
            "{:>3}  {:<10} {:<12} {:<16} {:<30} {:<28} {:>10}  {:>3} {:>3}",
            index + 1,
            stream.kind.icon().to_string() + " " + ui.t(stream.kind.label_ru()),
            truncate(&stream.codec_display(), 12),
            truncate(&localize_language(ui, &stream.language_label()), 16),
            truncate(ui.s(stream.display_name()), 30),
            truncate(&params, 28),
            size,
            if stream.default { ui.t("да") } else { "—" },
            if stream.forced { ui.t("да") } else { "—" },
        );

        if verbose && !stream.tags.is_empty() {
            let mut tags: Vec<_> = stream.tags.iter().collect();
            tags.sort_by(|a, b| a.0.cmp(b.0));
            for (key, value) in tags {
                println!("      {key} = {value}");
            }
        }
    }

    if !info.chapters.is_empty() {
        println!();
        println!("{}", ui.t("Главы:"));
        for (index, chapter) in info.chapters.iter().enumerate() {
            println!("  {:>2}. {}", index + 1, chapter.label());
        }
    } else if info.chapters_count > 0 {
        println!();
        println!(
            "{}",
            ui.f(
                "Главы: {} (содержимое прочитать не удалось)",
                &[info.chapters_count.to_string()]
            )
        );
    }

    if !info.attachments.is_empty() {
        println!();
        println!("{}", ui.t("Вложения:"));
        for attachment in &info.attachments {
            println!(
                "  {} ({}, {})",
                attachment.file_name,
                attachment.mime_type,
                localize_size(ui, format_size(attachment.size))
            );
        }
    }
}

/// Переводит подпись языка вида "ru — Русский", сохраняя код как есть.
fn localize_language(ui: &Ui, label: &str) -> String {
    match label.split_once(" — ") {
        Some((code, name)) => format!("{code} — {}", ui.s(name)),
        None => ui.s(label).to_string(),
    }
}

/// Переводит единицу размера, которую вернул core (`format_size`).
fn localize_size(ui: &Ui, value: String) -> String {
    if ui.lang == Lang::Ru {
        return value;
    }
    let Some((number, unit)) = value.rsplit_once(' ') else {
        return value;
    };
    match text::size_unit(ui.lang, unit) {
        Some(unit) => format!("{number} {unit}"),
        None => value,
    }
}

/// Переводит подпись каналов из core ("2.0 (стерео)", "6 кан.").
fn localize_channels(ui: &Ui, label: &str) -> String {
    if let Some(number) = label.strip_suffix(" кан.") {
        return match ui.lang {
            Lang::Ru => label.to_string(),
            Lang::Zh => format!("{number} 声道"),
            Lang::Kk => format!("{number} арна"),
            _ => format!("{number} ch"),
        };
    }
    ui.s(label).to_string()
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
    ui: &Ui,
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
    delay: &[String],
    chapters: Option<PathBuf>,
    split_size: Option<String>,
    split_duration: Option<String>,
    split_parts: Option<String>,
    append: &[PathBuf],
    overwrite: bool,
    dry_run: bool,
) -> Result<(), String> {
    let info = probe::probe(&input, tools).map_err(|err| err.to_string())?;

    let container = match (container, output.as_ref().and_then(|p| p.extension())) {
        (Some(value), _) => parse_container(ui, &value)?,
        (None, Some(ext)) => {
            OutputContainer::from_extension(&ext.to_string_lossy()).ok_or_else(|| {
                ui.f(
                    "неизвестное расширение выходного файла: {}",
                    &[ext.to_string_lossy().to_string()],
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
        return Err(ui.f("выходной файл совпадает с исходным", &[]));
    }

    let mut selected = select_streams(ui, &info, &keep, &drop)?;
    apply_delays(ui, &mut selected, delay)?;
    let extras = build_extras(ui, tools, add, add_delay)?;
    let chapters = match chapters {
        Some(path) => {
            Some(trackcut_core::resolve_chapters(&path, tools).map_err(|err| err.to_string())?)
        }
        None => None,
    };
    let split = build_split(
        ui,
        split_size.as_deref(),
        split_duration.as_deref(),
        split_parts.as_deref(),
    )?;
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
        chapters_file: chapters,
        split,
        append: append.to_vec(),
    };

    if let Some(split) = &plan.split {
        eprintln!(
            "{}",
            ui.f("Нарезка: {}", &[ui.s(&split.label()).to_string()])
        );
    }
    if let Some(chapters) = &plan.chapters_file {
        eprintln!(
            "{}",
            ui.f("Главы из: {}", &[chapters.display().to_string()])
        );
    }
    if !plan.append.is_empty() {
        eprintln!(
            "{}",
            ui.f("Склейка: {} файл(ов)", &[plan.append.len().to_string()])
        );
    }

    let prepared = prepare(&info, &plan, tools).map_err(|err| err.to_string())?;
    for warning in &prepared.warnings {
        ui.warn(warning);
    }
    eprintln!(
        "{}",
        ui.f(
            "Бэкенд: {}",
            &[ui.s(prepared.spec.backend.label()).to_string()]
        )
    );
    eprintln!(
        "{}",
        ui.f(
            "Команда: {} {}",
            &[
                prepared.spec.program.display().to_string(),
                prepared
                    .spec
                    .args
                    .iter()
                    .map(|arg| arg.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            ]
        )
    );
    eprintln!(
        "{}",
        ui.f("Выход: {}", &[plan.output.display().to_string()])
    );

    if dry_run {
        if ui.json {
            let value = serde_json::json!({ "output": &plan.output, "plan": &plan });
            ui.emit(&value)?;
        }
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
            eprintln!();
            ui.warn(&warning);
        }
        ExecEvent::Message(message) => {
            last_line = Some(message);
        }
    })
    .map_err(|err| err.to_string())?;

    eprintln!("\r100.0%");
    ui.say(&ui.f("Готово: {}", &[outcome.output.display().to_string()]));
    for warning in &outcome.warnings {
        ui.say(&format!("  ! {}", ui.s(warning)));
    }
    if let Some(line) = last_line {
        ui.say(&ui.f("последняя строка лога: {}", &[line]));
    }

    if ui.json {
        let value =
            serde_json::json!({ "output": &outcome.output, "plan": &plan, "verified": true });
        ui.emit(&value)?;
    }
    Ok(())
}

fn build_split(
    ui: &Ui,
    size: Option<&str>,
    duration: Option<&str>,
    parts: Option<&str>,
) -> Result<Option<SplitSpec>, String> {
    let modes = [size.is_some(), duration.is_some(), parts.is_some()]
        .iter()
        .filter(|set| **set)
        .count();
    if modes > 1 {
        return Err(ui.f(
            "выберите один способ нарезки: --split-size, --split-duration или --split-parts",
            &[],
        ));
    }
    if let Some(size) = size {
        return Ok(Some(SplitSpec::by_size(size.trim())));
    }
    if let Some(duration) = duration {
        let ms = parse_time_ms(duration)
            .ok_or_else(|| ui.f("не понимаю длительность части: {}", &[duration.to_string()]))?;
        if ms == 0 {
            return Err(ui.f("длительность части должна быть больше нуля", &[]));
        }
        return Ok(Some(SplitSpec::by_duration(ms)));
    }
    if let Some(parts) = parts {
        let ranges = parse_ranges(parts).ok_or_else(|| {
            ui.f(
                "не понимаю диапазоны: {} (нужно 00:01:00-00:05:00)",
                &[parts.to_string()],
            )
        })?;
        return Ok(Some(SplitSpec::by_ranges(ranges)));
    }
    Ok(None)
}

fn parse_container(ui: &Ui, value: &str) -> Result<OutputContainer, String> {
    match value.to_ascii_lowercase().as_str() {
        "mkv" | "matroska" => Ok(OutputContainer::Mkv),
        "webm" => Ok(OutputContainer::Webm),
        "mp4" | "m4v" => Ok(OutputContainer::Mp4),
        "mov" => Ok(OutputContainer::Mov),
        other => Err(ui.f(
            "неизвестный контейнер «{}» (доступны mkv, webm, mp4, mov)",
            &[other.to_string()],
        )),
    }
}

fn parse_extract_container(ui: &Ui, value: &str) -> Result<ExtractContainer, String> {
    match value.to_ascii_lowercase().as_str() {
        "mka" | "audio" => Ok(ExtractContainer::Mka),
        "mks" | "subs" | "subtitles" => Ok(ExtractContainer::Mks),
        "mkv" | "matroska" => Ok(ExtractContainer::Mkv),
        other => Err(ui.f(
            "неизвестный контейнер «{}» (доступны mka, mks, mkv)",
            &[other.to_string()],
        )),
    }
}

/// Читает файлы из `--add` и превращает их в источники дорожек.
fn build_extras(
    ui: &Ui,
    tools: &ToolPaths,
    add: &[PathBuf],
    delay_ms: i64,
) -> Result<Vec<ExtraInput>, String> {
    let mut extras = Vec::new();
    for path in add {
        let extra_info = probe::probe(path, tools).map_err(|err| {
            ui.f(
                "не удалось прочитать «{}»: {}",
                &[path.display().to_string(), err.to_string()],
            )
        })?;
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
            return Err(ui.f(
                "в «{}» нет дорожек, которые можно добавить",
                &[path.display().to_string()],
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
    ui: &Ui,
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
    let uids = select_uids(ui, &info, &tracks, &[])?;
    let streams: Vec<&trackcut_core::StreamInfo> = uids
        .iter()
        .filter_map(|uid| info.streams.get(*uid))
        .collect();

    let container = match container {
        Some(value) => parse_extract_container(ui, &value)?,
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
        ui.warn(warning);
    }
    eprintln!(
        "{}",
        ui.f(
            "Бэкенд: {}",
            &[ui.s(prepared.spec.backend.label()).to_string()]
        )
    );
    eprintln!(
        "{}",
        ui.f(
            "Команда: {} {}",
            &[
                prepared.spec.program.display().to_string(),
                prepared
                    .spec
                    .args
                    .iter()
                    .map(|arg| arg.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            ]
        )
    );
    eprintln!(
        "{}",
        ui.f("Выход: {}", &[plan.output.display().to_string()])
    );

    if dry_run {
        if ui.json {
            let value = extract_json(&info, &plan, &plan.output);
            ui.emit(&value)?;
        }
        return Ok(());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let mut last_line: Option<String> = None;
    let outcome = run_extract(&info, &plan, tools, cancel, |event| match event {
        ExecEvent::Progress(percent) => {
            eprint!("\r{:>5.1}%", percent * 100.0);
            let _ = std::io::stderr().flush();
        }
        ExecEvent::Warning(warning) => {
            eprintln!();
            ui.warn(&warning);
        }
        ExecEvent::Message(message) => last_line = Some(message),
    })
    .map_err(|err| err.to_string())?;

    eprintln!("\r100.0%");
    ui.say(&ui.f("Сохранено: {}", &[outcome.output.display().to_string()]));
    for warning in &outcome.warnings {
        ui.say(&format!("  ! {}", ui.s(warning)));
    }
    if let Some(line) = last_line {
        ui.say(&ui.f("последняя строка лога: {}", &[line]));
    }

    if ui.json {
        let value = extract_json(&info, &plan, &outcome.output);
        ui.emit(&value)?;
    }
    Ok(())
}

/// JSON-объект extract: output, track, kind.
fn extract_json(info: &MediaInfo, plan: &ExtractPlan, output: &Path) -> serde_json::Value {
    let track = plan.tracks.first().map(|uid| uid + 1).unwrap_or(0);
    let all_audio = plan
        .streams(info)
        .iter()
        .all(|stream| stream.kind == StreamKind::Audio);
    let kind = if plan.container == ExtractContainer::Mka || all_audio {
        "audio"
    } else {
        "subtitle"
    };
    serde_json::json!({ "output": output, "track": track, "kind": kind })
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

fn select_uids(
    ui: &Ui,
    info: &MediaInfo,
    keep: &[usize],
    drop: &[usize],
) -> Result<Vec<usize>, String> {
    let to_uid = |display: usize| -> Result<usize, String> {
        let uid = display.checked_sub(1).ok_or_else(|| {
            ui.f(
                "номера дорожек начинаются с 1, получено {}",
                &[display.to_string()],
            )
        })?;
        let stream = info
            .streams
            .get(uid)
            .ok_or_else(|| ui.f("дорожки №{} нет в файле", &[display.to_string()]))?;
        if !stream.kind.is_selectable() {
            return Err(ui.f(
                "дорожка №{} имеет тип «{}» и не может быть выбрана",
                &[
                    display.to_string(),
                    ui.t(stream.kind.label_ru()).to_string(),
                ],
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
        return Err(ui.f(
            "после применения --keep/--drop не осталось ни одной дорожки",
            &[],
        ));
    }

    Ok(uids)
}

fn apply_delays(
    ui: &Ui,
    tracks: &mut [trackcut_core::KeepTrack],
    delays: &[String],
) -> Result<(), String> {
    for value in delays {
        let (uid, ms) = value
            .split_once(':')
            .ok_or_else(|| ui.f("сдвиг задаётся как UID:МС, получено «{}»", std::slice::from_ref(value)))?;
        let uid: usize = uid
            .trim()
            .parse()
            .map_err(|_| ui.f("не номер дорожки в «{}»", std::slice::from_ref(value)))?;
        let uid = uid
            .checked_sub(1)
            .ok_or_else(|| ui.f("номера дорожек начинаются с 1 в «{}»", std::slice::from_ref(value)))?;
        let ms: i64 = ms
            .trim()
            .parse()
            .map_err(|_| ui.f("не число миллисекунд в «{}»", std::slice::from_ref(value)))?;
        let track = tracks
            .iter_mut()
            .find(|track| track.uid == uid)
            .ok_or_else(|| {
                ui.f(
                    "дорожка {} не выбрана — сдвиг некуда применить",
                    &[(uid + 1).to_string()],
                )
            })?;
        track.delay_ms = ms;
    }
    Ok(())
}

fn select_streams(
    ui: &Ui,
    info: &MediaInfo,
    keep: &[usize],
    drop: &[usize],
) -> Result<Vec<trackcut_core::KeepTrack>, String> {
    Ok(select_uids(ui, info, keep, drop)?
        .into_iter()
        .map(trackcut_core::KeepTrack::new)
        .collect())
}
