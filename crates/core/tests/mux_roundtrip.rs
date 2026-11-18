use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use trackcut_core::extract::{ExtractContainer, ExtractPlan};
use trackcut_core::model::{OutputContainer, StreamKind};
use trackcut_core::{
    probe, run_extract, run_mux, ExtraInput, ExtraTrack, KeepTrack, MediaInfo, MuxPlan, ToolPaths,
};

fn tools() -> Option<ToolPaths> {
    let tools = ToolPaths::detect();
    if tools.ffmpeg.is_some() && tools.ffprobe.is_some() {
        Some(tools)
    } else {
        None
    }
}

fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trackcut_test_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(program: &Path, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("не удалось запустить {}: {err}", program.display()));
    assert!(
        output.status.success(),
        "{} {:?} завершился с ошибкой:\n{}",
        program.display(),
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Создаёт MKV: видео + два аудио (rus/eng) + два субтитра (rus/eng).
fn make_fixture(tools: &ToolPaths, dir: &Path) -> PathBuf {
    let input = dir.join("fixture.mkv");
    let sub_ru = dir.join("ru.srt");
    let sub_en = dir.join("en.srt");
    std::fs::write(
        &sub_ru,
        "1\n00:00:00,000 --> 00:00:02,000\nПривет\n\n2\n00:00:02,000 --> 00:00:04,000\nМир\n",
    )
    .unwrap();
    std::fs::write(
        &sub_en,
        "1\n00:00:00,000 --> 00:00:02,000\nHello\n\n2\n00:00:02,000 --> 00:00:04,000\nWorld\n",
    )
    .unwrap();

    let ffmpeg = tools.ffmpeg().unwrap();
    run(
        &ffmpeg,
        &[
            "-hide_banner",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=24:duration=4",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=4",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:duration=4",
            "-i",
            sub_ru.to_str().unwrap(),
            "-i",
            sub_en.to_str().unwrap(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-map",
            "3:s",
            "-map",
            "4:s",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-c:s",
            "srt",
            "-metadata:s:a:0",
            "language=rus",
            "-metadata:s:a:0",
            "title=Дубляж",
            "-metadata:s:a:1",
            "language=eng",
            "-metadata:s:a:1",
            "title=Original",
            "-metadata:s:s:0",
            "language=rus",
            "-metadata:s:s:1",
            "language=eng",
            input.to_str().unwrap(),
        ],
    );
    input
}

fn find_stream(info: &MediaInfo, kind: StreamKind, language: &str) -> usize {
    info.streams
        .iter()
        .position(|stream| {
            stream.kind == kind
                && stream
                    .language
                    .as_deref()
                    .map(|value| value.eq_ignore_ascii_case(language))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("не найдена дорожка {kind:?}/{language}"))
}

#[test]
fn removes_unwanted_audio_and_subtitles_without_reencoding() {
    let Some(tools) = tools() else {
        eprintln!("пропуск: ffmpeg/ffprobe не найдены");
        return;
    };
    let dir = workdir("mkv");
    let input = make_fixture(&tools, &dir);
    let info = probe::probe(&input, &tools).expect("probe исходника");

    assert_eq!(info.kind_count(StreamKind::Video), 1);
    assert_eq!(info.kind_count(StreamKind::Audio), 2);
    assert_eq!(info.kind_count(StreamKind::Subtitle), 2);

    let media_streams: Vec<_> = info
        .streams
        .iter()
        .filter(|stream| matches!(stream.kind, StreamKind::Video | StreamKind::Audio))
        .collect();
    assert!(
        media_streams.iter().all(|stream| stream.bitrate.is_some()),
        "битрейт должен оцениваться даже без статистических тегов: {media_streams:?}"
    );

    let rus_audio = find_stream(&info, StreamKind::Audio, "rus");
    let eng_audio = find_stream(&info, StreamKind::Audio, "eng");
    let eng_sub = find_stream(&info, StreamKind::Subtitle, "eng");

    assert_eq!(
        info.streams[rus_audio].title.as_deref(),
        Some("Дубляж"),
        "название перевода должно читаться из контейнера"
    );

    let output = dir.join("clean.mkv");
    let keep = info
        .streams
        .iter()
        .enumerate()
        .filter(|(uid, stream)| stream.kind.is_selectable() && *uid != eng_audio && *uid != eng_sub)
        .map(|(uid, _)| KeepTrack::new(uid))
        .collect();

    let plan = MuxPlan {
        input: input.clone(),
        output: output.clone(),
        container: OutputContainer::Mkv,
        keep,
        keep_chapters: true,
        keep_tags: true,
        keep_attachments: true,
        convert_subtitles_to_mov_text: false,
        overwrite: true,
        backend: None,
        extras: Vec::new(),
    };

    let outcome = run_mux(
        &info,
        &plan,
        &tools,
        Arc::new(AtomicBool::new(false)),
        |_event| {},
    )
    .expect("mux");

    assert!(outcome.output.exists());
    let cleaned = probe::probe(&outcome.output, &tools).expect("probe результата");
    assert_eq!(cleaned.kind_count(StreamKind::Video), 1);
    assert_eq!(cleaned.kind_count(StreamKind::Audio), 1);
    assert_eq!(cleaned.kind_count(StreamKind::Subtitle), 1);

    let audio = cleaned
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Audio)
        .unwrap();
    assert_eq!(
        audio.language.as_deref().map(str::to_ascii_lowercase),
        Some("rus".to_string()),
        "должна остаться русская дорожка"
    );
}

#[test]
fn remuxes_to_mp4_with_ffmpeg() {
    let Some(tools) = tools() else {
        eprintln!("пропуск: ffmpeg/ffprobe не найдены");
        return;
    };
    let dir = workdir("mp4");
    let input = make_fixture(&tools, &dir);
    let info = probe::probe(&input, &tools).expect("probe исходника");
    let rus_audio = find_stream(&info, StreamKind::Audio, "rus");

    let keep = info
        .streams
        .iter()
        .enumerate()
        .filter(|(uid, stream)| {
            (stream.kind == StreamKind::Video && *uid == 0) || *uid == rus_audio
        })
        .map(|(uid, _)| KeepTrack::new(uid))
        .collect();

    let plan = MuxPlan {
        input: input.clone(),
        output: dir.join("clean.mp4"),
        container: OutputContainer::Mp4,
        keep,
        keep_chapters: false,
        keep_tags: true,
        keep_attachments: false,
        convert_subtitles_to_mov_text: false,
        overwrite: true,
        backend: None,
        extras: Vec::new(),
    };

    let outcome = run_mux(
        &info,
        &plan,
        &tools,
        Arc::new(AtomicBool::new(false)),
        |_event| {},
    )
    .expect("mux mp4");

    let cleaned = probe::probe(&outcome.output, &tools).expect("probe mp4");
    assert_eq!(cleaned.kind_count(StreamKind::Video), 1);
    assert_eq!(cleaned.kind_count(StreamKind::Audio), 1);
    assert_eq!(cleaned.kind_count(StreamKind::Subtitle), 0);
}

#[cfg(test)]
fn ffprobe_stream_count(path: &Path) -> usize {
    let tools = ToolPaths::detect();
    let info = probe::probe(path, &tools).unwrap();
    info.streams.len()
}

#[test]
fn extracts_audio_then_attaches_it_to_another_file() {
    let Some(tools) = tools() else {
        eprintln!("пропуск: ffmpeg/ffprobe не найдены");
        return;
    };
    let dir = workdir("attach");
    let input = make_fixture(&tools, &dir);
    let info = probe::probe(&input, &tools).expect("probe исходника");
    let eng_audio = find_stream(&info, StreamKind::Audio, "eng");

    // 1. Сохраняем английскую дорожку отдельным файлом.
    let track_file = dir.join("eng.mka");
    let extract_plan = ExtractPlan {
        input: input.clone(),
        output: track_file.clone(),
        tracks: vec![eng_audio],
        container: ExtractContainer::Mka,
        overwrite: true,
        backend: None,
    };
    run_extract(
        &info,
        &extract_plan,
        &tools,
        Arc::new(AtomicBool::new(false)),
        |_event| {},
    )
    .expect("извлечение дорожки");

    let track_info = probe::probe(&track_file, &tools).expect("probe mka");
    assert_eq!(track_info.kind_count(StreamKind::Audio), 1);
    assert_eq!(track_info.kind_count(StreamKind::Video), 0);
    assert_eq!(
        track_info.streams[0]
            .language
            .as_deref()
            .map(str::to_ascii_lowercase),
        Some("eng".to_string()),
        "язык должен пережить извлечение"
    );

    // 2. Делаем «второй релиз»: только видео, без звука.
    let video_only = dir.join("video_only.mkv");
    run(
        &tools.ffmpeg().unwrap(),
        &[
            "-hide_banner",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-map",
            "0:v",
            "-c",
            "copy",
            "-an",
            "-sn",
            video_only.to_str().unwrap(),
        ],
    );
    let video_info = probe::probe(&video_only, &tools).expect("probe видео");
    assert_eq!(video_info.kind_count(StreamKind::Audio), 0);

    let keep = video_info
        .streams
        .iter()
        .enumerate()
        .filter(|(_, stream)| stream.kind == StreamKind::Video)
        .map(|(uid, _)| KeepTrack::new(uid))
        .collect();

    let output = dir.join("attached.mkv");
    let plan = MuxPlan {
        input: video_only.clone(),
        output: output.clone(),
        container: OutputContainer::Mkv,
        keep,
        keep_chapters: true,
        keep_tags: true,
        keep_attachments: true,
        convert_subtitles_to_mov_text: false,
        overwrite: true,
        backend: None,
        extras: vec![ExtraInput {
            info: track_info,
            tracks: vec![ExtraTrack {
                uid: 0,
                delay_ms: 0,
                default: Some(true),
                forced: None,
            }],
        }],
    };

    let outcome = run_mux(
        &video_info,
        &plan,
        &tools,
        Arc::new(AtomicBool::new(false)),
        |_event| {},
    )
    .expect("пришивание дорожки");

    let result = probe::probe(&outcome.output, &tools).expect("probe результата");
    assert_eq!(result.kind_count(StreamKind::Video), 1);
    assert_eq!(result.kind_count(StreamKind::Audio), 1);
    let audio = result
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Audio)
        .unwrap();
    assert_eq!(
        audio.language.as_deref().map(str::to_ascii_lowercase),
        Some("eng".to_string())
    );
    assert!(audio.default, "дорожка должна стать default");
    assert!(outcome.output.exists());
}

#[test]
fn ignores_helpers_without_tools() {
    let Some(tools) = tools() else { return };
    let dir = workdir("probe");
    let input = make_fixture(&tools, &dir);
    assert_eq!(ffprobe_stream_count(&input), 5);
}
