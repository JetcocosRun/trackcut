use std::path::Path;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{AttachmentInfo, MediaInfo, StreamInfo, StreamKind};
use crate::sys::hidden_command;
use crate::tools::ToolPaths;

/// Читает метаданные файла. Всегда пытается использовать ffprobe,
/// а для mkvmerge-совместимых контейнеров дополнительно `mkvmerge -J`.
pub fn probe(path: &Path, tools: &ToolPaths) -> Result<MediaInfo> {
    if !path.is_file() {
        return Err(Error::Unsupported(format!(
            "файл не найден: {}",
            path.display()
        )));
    }

    let ffprobe_json = match tools.ffprobe.as_deref() {
        Some(ffprobe) => match run_json(
            ffprobe,
            &[
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-show_chapters",
                "-of",
                "json",
            ],
            path,
        ) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::warn!("ffprobe не смог прочитать {}: {err}", path.display());
                None
            }
        },
        None => None,
    };

    let mkv_json = match tools.mkvmerge.as_deref() {
        Some(mkvmerge) => match run_json(mkvmerge, &["-J"], path) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::debug!("mkvmerge -J не подходит для {}: {err}", path.display());
                None
            }
        },
        None => None,
    };

    if ffprobe_json.is_none() && mkv_json.is_none() {
        return Err(Error::Other(
            "не удалось прочитать метаданные: проверьте, что установлены ffprobe и mkvmerge"
                .to_string(),
        ));
    }

    let mut info = MediaInfo::new(path);

    if let Some(json) = &mkv_json {
        apply_mkvmerge(&mut info, json);
    }
    if let Some(json) = &ffprobe_json {
        apply_ffprobe(&mut info, json);
    }

    if info.streams.is_empty() {
        return Err(Error::Unsupported(
            "в файле не найдено ни одной дорожки".to_string(),
        ));
    }

    // В Matroska битрейт и размер часто не хранятся. Оцениваем по пакетам
    // первых секунд — это быстро и не требует чтения всего файла.
    estimate_missing_bitrates(&mut info, path, tools);

    Ok(info)
}

fn estimate_missing_bitrates(info: &mut MediaInfo, input: &Path, tools: &ToolPaths) {
    let needs_estimate = info.streams.iter().any(|stream| {
        matches!(stream.kind, StreamKind::Video | StreamKind::Audio) && stream.bitrate.is_none()
    });
    if !needs_estimate {
        return;
    }
    let Some(ffprobe) = tools.ffprobe.as_deref() else {
        return;
    };

    let json = match run_json(
        ffprobe,
        &[
            "-v",
            "error",
            "-read_intervals",
            "%+6",
            "-show_entries",
            "packet=stream_index,size,pts_time,dts_time",
            "-of",
            "json",
        ],
        input,
    ) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!("не удалось оценить битрейт {}: {err}", input.display());
            return;
        }
    };

    let Some(packets) = json.get("packets").and_then(Value::as_array) else {
        return;
    };

    // stream_index -> (байты, пакеты, минимальное и максимальное время)
    let mut stats: std::collections::HashMap<usize, (u64, u64, f64, f64)> =
        std::collections::HashMap::new();
    for packet in packets {
        let Some(index) = packet
            .get("stream_index")
            .and_then(value_as_u64)
            .map(|value| value as usize)
        else {
            continue;
        };
        let size = packet.get("size").and_then(value_as_u64).unwrap_or(0);
        let time = packet
            .get("pts_time")
            .and_then(value_as_f64)
            .or_else(|| packet.get("dts_time").and_then(value_as_f64));
        let Some(time) = time else { continue };

        let entry = stats
            .entry(index)
            .or_insert((0, 0, f64::INFINITY, f64::NEG_INFINITY));
        entry.0 += size;
        entry.1 += 1;
        entry.2 = entry.2.min(time);
        entry.3 = entry.3.max(time);
    }

    for stream in &mut info.streams {
        if stream.bitrate.is_some() {
            continue;
        }
        if !matches!(stream.kind, StreamKind::Video | StreamKind::Audio) {
            continue;
        }
        let Some(index) = stream.ff_index else {
            continue;
        };
        let Some((bytes, count, first, last)) = stats.get(&index).copied() else {
            continue;
        };
        let span = last - first;
        if count < 2 || span <= 0.0 {
            continue;
        }
        let bitrate = (bytes as f64 * 8.0 / span) as u64;
        if bitrate > 1000 {
            stream.bitrate = Some(bitrate);
        }
    }
}

fn run_json(program: &Path, args: &[&str], input: &Path) -> Result<Value> {
    let output = hidden_command(program)
        .args(args)
        .arg(input)
        .output()
        .map_err(|source| Error::Spawn {
            program: program.display().to_string(),
            source,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|err| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Error::Other(format!(
            "{}: не удалось разобрать JSON: {err}; stderr: {}",
            program.display(),
            stderr.trim()
        ))
    })?;

    // mkvmerge возвращает 1 при предупреждениях, но JSON при этом валиден.
    let code = output.status.code().unwrap_or(-1);
    if code > 1 && value.get("files").is_none() && value.get("streams").is_none() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::CommandFailed {
            program: program.display().to_string(),
            code,
            stderr: stderr.trim().to_string(),
        });
    }

    Ok(value)
}

fn apply_mkvmerge(info: &mut MediaInfo, json: &Value) {
    // mkvmerge 9x/103+ (identification_format_version >= 20) отдаёт дорожки
    // и главы на верхнем уровне; старые версии — внутри files[0].
    let file = json
        .get("files")
        .and_then(|f| f.as_array())
        .and_then(|f| f.first());

    if let Some(container) = json
        .get("container")
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
    {
        info.container = container.to_string();
    }

    if let Some(duration_ns) = json
        .get("container")
        .and_then(|c| c.get("properties"))
        .and_then(|p| p.get("duration"))
        .and_then(Value::as_u64)
    {
        let ms = duration_ns / 1_000_000;
        info.duration_ms = Some(info.duration_ms.unwrap_or(0).max(ms));
    }

    let tracks = json
        .get("tracks")
        .and_then(Value::as_array)
        .or_else(|| file.and_then(|f| f.get("tracks")).and_then(Value::as_array));

    if let Some(tracks) = tracks {
        for track in tracks {
            let kind = match track.get("type").and_then(Value::as_str) {
                Some("video") => StreamKind::Video,
                Some("audio") => StreamKind::Audio,
                Some("subtitles") => StreamKind::Subtitle,
                Some("buttons") => StreamKind::Button,
                _ => StreamKind::Other,
            };
            let props = track.get("properties").unwrap_or(&Value::Null);

            let codec_id = props
                .get("codec_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut stream = StreamInfo {
                kind,
                mkv_id: track.get("id").and_then(Value::as_u64).map(|v| v as u32),
                codec_raw: if codec_id.is_empty() {
                    track
                        .get("codec")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                } else {
                    codec_id
                },
                codec: track
                    .get("codec")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                ..StreamInfo::default()
            };

            stream.language = props
                .get("language")
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty() && *v != "und")
                .or_else(|| props.get("language_ietf").and_then(Value::as_str))
                .map(|v| v.to_string());

            stream.title = props
                .get("track_name")
                .and_then(Value::as_str)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());

            stream.default = props
                .get("default_track")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            stream.forced = props
                .get("forced_track")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            stream.channels = number(props, &["audio_channels", "num_channels"]).map(|v| v as u8);
            stream.sample_rate = number(props, &["audio_sampling_frequency"]).map(|v| v as u32);
            stream.channel_layout = text(props, &["channel_layout"]);

            if let Some(dimensions) = props.get("pixel_dimensions").and_then(Value::as_str) {
                if let Some((w, h)) = dimensions.split_once('x') {
                    if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                        stream.resolution = Some((w, h));
                    }
                }
            }

            if let Some(duration) = number(props, &["default_duration"]) {
                if duration > 0 {
                    stream.fps = Some(1_000_000_000.0 / duration as f64);
                }
            }

            if let Some(tag_duration) = props.get("tag_duration").and_then(Value::as_str) {
                if let Some(ms) = parse_hhmmss_ms(tag_duration) {
                    stream.duration_ms = Some(ms);
                    info.duration_ms = Some(info.duration_ms.unwrap_or(0).max(ms));
                }
            }

            info.streams.push(stream);
        }
    }

    info.chapters = chapter_count(json)
        .or_else(|| file.and_then(chapter_count))
        .unwrap_or(0);

    if let Some(attachments) = json
        .get("attachments")
        .and_then(Value::as_array)
        .or_else(|| {
            file.and_then(|f| f.get("attachments"))
                .and_then(Value::as_array)
        })
    {
        for attachment in attachments {
            info.attachments.push(AttachmentInfo {
                id: attachment.get("id").and_then(Value::as_u64).unwrap_or(0),
                file_name: attachment
                    .get("file_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                mime_type: attachment
                    .get("content_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                size: attachment.get("size").and_then(Value::as_u64).unwrap_or(0),
            });
        }
    }
}

/// `chapters` бывает числом (старый формат) или массивом (новый).
fn chapter_count(value: &Value) -> Option<usize> {
    match value.get("chapters") {
        Some(Value::Array(items)) => Some(items.len()),
        Some(other) => other.as_u64().map(|count| count as usize),
        None => None,
    }
}

fn apply_ffprobe(info: &mut MediaInfo, json: &Value) {
    let duration_ms = json
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(value_as_f64)
        .map(|secs| (secs * 1000.0) as u64);
    if duration_ms.is_some() {
        info.duration_ms = duration_ms;
    }

    if info.container.is_empty() {
        if let Some(name) = json
            .get("format")
            .and_then(|f| f.get("format_name"))
            .and_then(Value::as_str)
        {
            info.container = name.to_string();
        }
    }

    if let Some(tags) = json
        .get("format")
        .and_then(|f| f.get("tags"))
        .and_then(Value::as_object)
    {
        for (key, value) in tags {
            if let Some(value) = value.as_str() {
                info.tags.insert(key.clone(), value.to_string());
            }
        }
    }

    if let Some(chapters) = json.get("chapters").and_then(Value::as_array) {
        if !chapters.is_empty() {
            info.chapters = chapters.len();
        }
    }

    let ff_streams = json
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut used = vec![false; ff_streams.len()];

    // Обогащаем дорожки, полученные из mkvmerge, метаданными ffprobe.
    for stream in &mut info.streams {
        let position = ff_streams
            .iter()
            .enumerate()
            .position(|(index, candidate)| !used[index] && ff_kind(candidate) == stream.kind);
        if let Some(index) = position {
            used[index] = true;
            enrich(stream, &ff_streams[index]);
        }
    }

    // Если mkvmerge недоступен — строим список из ffprobe.
    if info.streams.is_empty() {
        for stream in &ff_streams {
            let kind = ff_kind(stream);
            let mut info_stream = StreamInfo {
                kind,
                ff_index: stream
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize),
                ..StreamInfo::default()
            };
            enrich(&mut info_stream, stream);
            if kind == StreamKind::Attachment {
                info.attachments.push(AttachmentInfo {
                    id: info_stream.ff_index.unwrap_or(0) as u64,
                    file_name: info_stream.title.clone().unwrap_or_default(),
                    mime_type: info_stream.codec_raw.clone(),
                    size: 0,
                });
            } else {
                info.streams.push(info_stream);
            }
        }
    }
}

fn enrich(stream: &mut StreamInfo, ff: &Value) {
    if stream.ff_index.is_none() {
        stream.ff_index = ff.get("index").and_then(Value::as_u64).map(|v| v as usize);
    }
    if stream.kind == StreamKind::Other {
        stream.kind = ff_kind(ff);
    }

    if stream.codec_raw.is_empty() || stream.codec_raw == "unknown" {
        if let Some(codec_name) = ff.get("codec_name").and_then(Value::as_str) {
            stream.codec_raw = codec_name.to_string();
        }
    }
    if stream.codec.is_empty() {
        if let Some(codec_name) = ff.get("codec_name").and_then(Value::as_str) {
            stream.codec = codec_name.to_string();
        }
    }

    if let Some(tags) = ff.get("tags").and_then(Value::as_object) {
        for (key, value) in tags {
            if let Some(value) = value.as_str() {
                stream.tags.insert(key.clone(), value.to_string());
            }
        }
        if stream.title.is_none() {
            stream.title = tags
                .get("title")
                .and_then(Value::as_str)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
        }
        if stream.language.is_none() {
            stream.language = tags
                .get("language")
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty() && *v != "und")
                .map(|v| v.to_string());
        }
    }

    if let Some(disposition) = ff.get("disposition") {
        let default = disposition
            .get("default")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let forced = disposition
            .get("forced")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if default > 0 {
            stream.default = true;
        }
        if forced > 0 {
            stream.forced = true;
        }
    }

    if stream.channels.is_none() {
        stream.channels = ff.get("channels").and_then(Value::as_u64).map(|v| v as u8);
    }
    if stream.channel_layout.is_none() {
        stream.channel_layout = ff
            .get("channel_layout")
            .and_then(Value::as_str)
            .map(|v| v.to_string());
    }
    if stream.sample_rate.is_none() {
        stream.sample_rate = ff
            .get("sample_rate")
            .and_then(value_as_u64)
            .map(|v| v as u32);
    }
    if stream.resolution.is_none() {
        let width = ff.get("width").and_then(Value::as_u64);
        let height = ff.get("height").and_then(Value::as_u64);
        if let (Some(width), Some(height)) = (width, height) {
            stream.resolution = Some((width as u32, height as u32));
        }
    }
    if stream.fps.is_none() {
        stream.fps = ff
            .get("r_frame_rate")
            .and_then(Value::as_str)
            .and_then(parse_fps);
    }
    if stream.bitrate.is_none() {
        stream.bitrate = ff
            .get("bit_rate")
            .and_then(value_as_u64)
            .or_else(|| tags_number(stream, &["BPS", "BPS-eng", "BPS-rus"]));
    }
    if stream.duration_ms.is_none() {
        stream.duration_ms = stream
            .tags
            .iter()
            .find(|(key, _)| {
                key.eq_ignore_ascii_case("DURATION")
                    || key.to_ascii_uppercase().starts_with("DURATION-")
            })
            .and_then(|(_, value)| parse_hhmmss_ms(value));
    }
    if stream.size_bytes.is_none() {
        stream.size_bytes = tags_number(stream, &["NUMBER_OF_BYTES", "NUMBER_OF_BYTES-eng"]);
    }
    if stream.delay_ms.is_none() {
        stream.delay_ms = ff
            .get("start_time")
            .and_then(value_as_f64)
            .map(|secs| (secs * 1000.0) as i64)
            .filter(|ms| *ms > 0);
    }
}

fn tags_number(stream: &StreamInfo, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| stream.tags.get(*key))
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn ff_kind(stream: &Value) -> StreamKind {
    match stream.get("codec_type").and_then(Value::as_str) {
        Some("video") => StreamKind::Video,
        Some("audio") => StreamKind::Audio,
        Some("subtitle") => StreamKind::Subtitle,
        Some("attachment") => StreamKind::Attachment,
        _ => StreamKind::Other,
    }
}

fn text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(|v| v.to_string())
    })
}

fn number(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(value_as_u64))
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|v| v.parse::<u64>().ok()))
        .or_else(|| value.as_f64().map(|v| v as u64))
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|v| v.parse::<f64>().ok()))
}

fn parse_fps(value: &str) -> Option<f64> {
    if let Some((num, den)) = value.split_once('/') {
        let num = num.parse::<f64>().ok()?;
        let den = den.parse::<f64>().ok()?;
        if den > 0.0 {
            return Some(num / den);
        }
    }
    value.parse::<f64>().ok()
}

fn parse_hhmmss_ms(value: &str) -> Option<u64> {
    let mut parts = value.split(':');
    let hours: u64 = parts.next()?.trim().parse().ok()?;
    let minutes: u64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;
    Some(((hours * 3600 + minutes * 60) as f64 * 1000.0 + seconds * 1000.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_new_mkvmerge_format_with_track_ids() {
        let json: Value = serde_json::from_str(
            r#"{
              "container": { "type": "Matroska", "properties": { "duration": 12023000000 } },
              "chapters": [],
              "attachments": [
                { "id": 0, "file_name": "font.ttf", "content_type": "font/ttf", "size": 1024 }
              ],
              "tracks": [
                {
                  "id": 0,
                  "type": "video",
                  "codec": "AVC/H.264/MPEG-4p10",
                  "properties": {
                    "codec_id": "V_MPEG4/ISO/AVC",
                    "default_track": false,
                    "tag_duration": "00:00:12.000000000",
                    "pixel_dimensions": "1920x1080"
                  }
                },
                {
                  "id": 2,
                  "type": "audio",
                  "codec": "AAC",
                  "properties": {
                    "codec_id": "A_AAC",
                    "language": "rus",
                    "track_name": "Дубляж",
                    "audio_channels": 2,
                    "audio_sampling_frequency": 48000,
                    "default_track": true
                  }
                }
              ]
            }"#,
        )
        .unwrap();

        let mut info = MediaInfo::new("x.mkv");
        apply_mkvmerge(&mut info, &json);

        assert_eq!(info.container, "Matroska");
        assert_eq!(info.duration_ms, Some(12_023));
        assert_eq!(info.streams.len(), 2);
        assert_eq!(info.streams[0].mkv_id, Some(0));
        assert_eq!(info.streams[1].mkv_id, Some(2));
        assert_eq!(info.streams[1].title.as_deref(), Some("Дубляж"));
        assert_eq!(info.streams[1].channels, Some(2));
        assert_eq!(info.streams[0].codec_display(), "H.264");
        assert_eq!(info.streams[0].resolution, Some((1920, 1080)));
        assert_eq!(info.attachments.len(), 1);
        assert_eq!(info.attachments[0].file_name, "font.ttf");
    }

    #[test]
    fn reads_legacy_mkvmerge_format() {
        let json: Value = serde_json::from_str(
            r#"{
              "container": { "type": "Matroska" },
              "files": [
                {
                  "chapters": 4,
                  "tracks": [
                    {
                      "id": 1,
                      "type": "audio",
                      "codec": "AAC",
                      "properties": { "codec_id": "A_AAC", "language": "eng" }
                    }
                  ]
                }
              ]
            }"#,
        )
        .unwrap();

        let mut info = MediaInfo::new("old.mkv");
        apply_mkvmerge(&mut info, &json);

        assert_eq!(info.streams.len(), 1);
        assert_eq!(info.streams[0].mkv_id, Some(1));
        assert_eq!(info.chapters, 4);
    }
}
