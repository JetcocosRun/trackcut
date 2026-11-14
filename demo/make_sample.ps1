# Генерирует demo\sample.mkv: 12 секунд видео, два аудио (rus/eng) и два субтитра (rus/eng).
# Требуется ffmpeg в tools\ffmpeg\bin.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$demo = Join-Path $root "demo"
$ffmpeg = Join-Path $root "tools\ffmpeg\bin\ffmpeg.exe"

if (-not (Test-Path $ffmpeg)) {
    throw "Не найден ffmpeg: $ffmpeg"
}

& $ffmpeg -hide_banner -y `
    -f lavfi -i "testsrc2=size=640x360:rate=24:duration=12" `
    -f lavfi -i "sine=frequency=440:duration=12" `
    -f lavfi -i "sine=frequency=660:duration=12" `
    -i (Join-Path $demo "ru.srt") `
    -i (Join-Path $demo "en.srt") `
    -map 0:v -map 1:a -map 2:a -map 3:s -map 4:s `
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p `
    -c:a aac -b:a 128k -c:s srt `
    -metadata:s:v:0 title="Видео" `
    -metadata:s:a:0 language=rus -metadata:s:a:0 title="Дубляж (пример)" `
    -metadata:s:a:1 language=eng -metadata:s:a:1 title="Original" `
    -metadata:s:s:0 language=rus -metadata:s:s:0 title="Русские" `
    -metadata:s:s:1 language=eng -metadata:s:s:1 title="English" `
    (Join-Path $demo "sample.mkv")

if ($LASTEXITCODE -ne 0) {
    throw "ffmpeg завершился с кодом $LASTEXITCODE"
}

Write-Host "Готово: $(Join-Path $demo 'sample.mkv')"
