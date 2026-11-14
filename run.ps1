# Запускает GUI. По умолчанию — release-сборка, если она есть.
# Использование: .\run.ps1 [путь к видео]
param(
    [string]$File
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$mingw = Join-Path $env:LOCALAPPDATA "trackcut-tools\mingw64\bin"
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
$tools = Join-Path $root "tools"
$ffmpeg = Join-Path $tools "ffmpeg\bin"
$mkvtoolnix = Join-Path $tools "mkvtoolnix"
$extra = @($cargoBin) + @($ffmpeg, $mkvtoolnix | Where-Object { Test-Path $_ })
if (Test-Path $mingw) { $extra = @($mingw) + $extra }
$env:PATH = (($extra + $env:PATH) -join ";")

$release = Join-Path $root "target\release\trackcut.exe"
$debug = Join-Path $root "target\debug\trackcut.exe"
$exe = if (Test-Path $release) { $release } elseif (Test-Path $debug) { $debug } else { throw "Сначала соберите проект: .\build.ps1" }

if ($File) {
    Start-Process -FilePath $exe -ArgumentList "`"$File`"" -WorkingDirectory $root
} else {
    Start-Process -FilePath $exe -WorkingDirectory $root
}
