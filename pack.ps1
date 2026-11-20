# Собирает релизную поставку TrackCut: портативный ZIP и установщик (Inno Setup).
#
# Использование:
#   .\pack.ps1                 # сборка + dist\TrackCut-<ver>-portable.zip + setup.exe
#   .\pack.ps1 -SkipBuild      # взять уже собранные exe из target\release
#   .\pack.ps1 -NoSetup        # только ZIP
#   .\pack.ps1 -Iscc "C:\...\ISCC.exe"
#
# Внутрь кладётся только то, что нужно программе: ffmpeg, ffprobe, mkvmerge,
# mkvextract. Остальное из tools (ffplay, GUI MKVToolNix, документация) не попадает.

param(
    [switch]$SkipBuild,
    [switch]$NoSetup,
    [string]$Iscc = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

function Fail([string]$message) {
    Write-Host "Ошибка: $message" -ForegroundColor Red
    exit 1
}

# --- версия из Cargo.toml ---------------------------------------------------
$cargo = Get-Content (Join-Path $root "Cargo.toml") -Raw
$workspacePackage = [regex]::Match($cargo, '(?s)\[workspace\.package\](.*?)(\r?\n\[|\z)').Groups[1].Value
$version = [regex]::Match($workspacePackage, 'version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $version) { Fail "не удалось прочитать version из Cargo.toml" }

if (Get-Process trackcut -ErrorAction SilentlyContinue) {
    Fail "закройте trackcut.exe перед сборкой (файл занят)"
}

$dist = Join-Path $root "dist"
$stageName = "TrackCut-$version"
$stage = Join-Path $dist $stageName

# --- release-сборка ---------------------------------------------------------
if (-not $SkipBuild) {
    $mingwCandidates = @(
        (Join-Path $env:LOCALAPPDATA "trackcut-tools\mingw64\bin"),
        (Join-Path $root "tools\mingw\mingw64\bin")
    )
    $mingw = $mingwCandidates | Where-Object { Test-Path (Join-Path $_ "gcc.exe") } | Select-Object -First 1
    $env:PATH = "$(Join-Path $env:USERPROFILE '.cargo\bin');$env:PATH"
    if ($mingw) {
        $env:PATH = "$mingw;$env:PATH"
        $env:CC = "gcc"
        $env:CXX = "g++"
    }
    Write-Host "Сборка release..." -ForegroundColor Cyan
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { Fail "cargo build --release завершился с кодом $LASTEXITCODE" }
}

# --- подготовка папки -------------------------------------------------------
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stage "licenses") | Out-Null

function CopyRequired([string]$from, [string]$to) {
    if (-not (Test-Path $from)) { Fail "не найден файл: $from" }
    $dir = Split-Path -Parent $to
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    Copy-Item $from $to -Force
}

CopyRequired (Join-Path $root "target\release\trackcut.exe") (Join-Path $stage "trackcut.exe")
CopyRequired (Join-Path $root "target\release\trackcut-cli.exe") (Join-Path $stage "trackcut-cli.exe")
CopyRequired (Join-Path $root "README.md") (Join-Path $stage "README.md")
CopyRequired (Join-Path $root "installer\THIRD-PARTY-NOTICES.txt") (Join-Path $stage "THIRD-PARTY-NOTICES.txt")
CopyRequired (Join-Path $root "LICENSE-MIT") (Join-Path $stage "LICENSE-MIT")
CopyRequired (Join-Path $root "LICENSE-APACHE") (Join-Path $stage "LICENSE-APACHE")

CopyRequired (Join-Path $root "tools\ffmpeg\bin\ffmpeg.exe") (Join-Path $stage "tools\ffmpeg\bin\ffmpeg.exe")
CopyRequired (Join-Path $root "tools\ffmpeg\bin\ffprobe.exe") (Join-Path $stage "tools\ffmpeg\bin\ffprobe.exe")
CopyRequired (Join-Path $root "tools\mkvtoolnix\mkvmerge.exe") (Join-Path $stage "tools\mkvtoolnix\mkvmerge.exe")
CopyRequired (Join-Path $root "tools\mkvtoolnix\mkvextract.exe") (Join-Path $stage "tools\mkvtoolnix\mkvextract.exe")

CopyRequired (Join-Path $root "tools\ffmpeg\LICENSE.txt") (Join-Path $stage "licenses\ffmpeg.txt")
CopyRequired (Join-Path $root "tools\mkvtoolnix\doc\COPYING.txt") (Join-Path $stage "licenses\mkvtoolnix.txt")

# Портативный режим: настройки пишутся рядом с программой.
New-Item -ItemType File -Path (Join-Path $stage "portable.ini") | Out-Null

# --- ZIP --------------------------------------------------------------------
$zip = Join-Path $dist "$stageName-portable.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Write-Host "Упаковка $stageName-portable.zip..." -ForegroundColor Cyan
if (Get-Command tar.exe -ErrorAction SilentlyContinue) {
    & tar.exe -a -c -f $zip -C $dist $stageName
    if ($LASTEXITCODE -ne 0) { Fail "tar завершился с кодом $LASTEXITCODE" }
} else {
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
}

# --- установщик -------------------------------------------------------------
$setup = Join-Path $dist "$stageName-setup.exe"
if (-not $NoSetup) {
    if (-not $Iscc) {
        $candidates = @(
            (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
            (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
            (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe")
        )
        $Iscc = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    }
    if (-not $Iscc) {
        Write-Host "ISCC.exe не найден — установщик пропущен." -ForegroundColor Yellow
        Write-Host "Поставьте Inno Setup 6: winget install JRSoftware.InnoSetup" -ForegroundColor Yellow
    } else {
        Remove-Item $setup -ErrorAction SilentlyContinue
        Write-Host "Сборка установщика..." -ForegroundColor Cyan
        & $Iscc "/DMyAppVersion=$version" "/DStageDir=$stage" "/O$dist" (Join-Path $root "installer\TrackCut.iss")
        if ($LASTEXITCODE -ne 0) { Fail "ISCC завершился с кодом $LASTEXITCODE" }
    }
}

# --- контрольные суммы ------------------------------------------------------
$artifacts = @($zip, $setup) | Where-Object { Test-Path $_ }
$lines = foreach ($file in $artifacts) {
    $hash = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
    $name = [IO.Path]::GetFileName($file)
    "{0}  {1}" -f $hash, $name
}
$sums = Join-Path $dist "SHA256SUMS.txt"
Set-Content -Path $sums -Value $lines -Encoding ASCII

# --- итог -------------------------------------------------------------------
Write-Host ""
Write-Host "Готово, версия $version" -ForegroundColor Green
Write-Host ("Папка:    {0} ({1:N0} МБ)" -f $stage, ((Get-ChildItem $stage -Recurse -File | Measure-Object Length -Sum).Sum / 1MB))
foreach ($file in $artifacts) {
    Write-Host ("Файл:     {0} ({1:N0} МБ)" -f ([IO.Path]::GetFileName($file)), ((Get-Item $file).Length / 1MB))
}
Write-Host "Суммы:    dist\SHA256SUMS.txt"
