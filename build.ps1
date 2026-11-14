# Собирает TrackCut.
# Использование: .\build.ps1 [-Release]
param(
    [switch]$Release
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"

# MinGW: сначала портативный рядом с профилем, затем tools\mingw, затем gcc из PATH.
$mingwCandidates = @(
    (Join-Path $env:LOCALAPPDATA "trackcut-tools\mingw64\bin"),
    (Join-Path $root "tools\mingw\mingw64\bin")
)
$mingw = $mingwCandidates | Where-Object { Test-Path (Join-Path $_ "gcc.exe") } | Select-Object -First 1
if (-not $mingw) {
    $gcc = Get-Command gcc -ErrorAction SilentlyContinue
    if ($gcc) { $mingw = Split-Path -Parent $gcc.Source }
}

$env:PATH = "$cargoBin;$env:PATH"
if ($mingw) {
    $env:PATH = "$mingw;$env:PATH"
    $env:CC = "gcc"
    $env:CXX = "g++"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Не найден cargo. Установите Rust: https://rustup.rs"
}

$argsList = @("build")
if ($Release) { $argsList += "--release" }

Push-Location $root
try {
    & cargo @argsList
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}

$profile = if ($Release) { "release" } else { "debug" }
Write-Host ""
Write-Host "Готово: $root\target\$profile\trackcut.exe"
Write-Host "CLI:    $root\target\$profile\trackcut-cli.exe"
