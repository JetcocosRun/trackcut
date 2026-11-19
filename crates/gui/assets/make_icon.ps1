# Draws the TrackCut icon and writes:
#   icon.ico        - multi-size icon embedded into trackcut.exe
#   icon.png        - 512x512 preview for the README
#   icon_128.rgba   - raw RGBA used as the window icon at runtime
# Run: powershell -NoProfile -ExecutionPolicy Bypass -File make_icon.ps1
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$size = 1024
$bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
$g.Clear([System.Drawing.Color]::Transparent)

function New-RoundedPath([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}

# Background: dark rounded square with a soft vertical gradient.
$rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
$bgPath = New-RoundedPath 30 30 964 964 208
$bgBrush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
    $rect,
    [System.Drawing.Color]::FromArgb(255, 13, 20, 36),
    [System.Drawing.Color]::FromArgb(255, 32, 45, 67),
    90.0)
$g.FillPath($bgBrush, $bgPath)
$borderPen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(150, 71, 85, 105), 10)
$g.DrawPath($borderPen, $bgPath)

# Three track bars; the middle one is cut, the gap is where the blade passes.
$bars = @(
    @{ y = 250; color = [System.Drawing.Color]::FromArgb(255, 226, 232, 240) },
    @{ y = 452; color = [System.Drawing.Color]::FromArgb(255, 148, 163, 184) },
    @{ y = 654; color = [System.Drawing.Color]::FromArgb(255, 100, 116, 139) }
)
$left = 150.0
$width = 724.0
$height = 120.0
$gapLeft = 446.0
$gapRight = 578.0

$brush0 = New-Object System.Drawing.SolidBrush($bars[0].color)
$brush1 = New-Object System.Drawing.SolidBrush($bars[1].color)
$brush2 = New-Object System.Drawing.SolidBrush($bars[2].color)

$g.FillPath($brush0, (New-RoundedPath $left $bars[0].y $width $height 60))
$g.FillPath($brush1, (New-RoundedPath $left $bars[1].y ($gapLeft - $left) $height 60))
$g.FillPath($brush1, (New-RoundedPath $gapRight $bars[1].y ($left + $width - $gapRight) $height 60))
$g.FillPath($brush2, (New-RoundedPath $left $bars[2].y $width $height 60))

# The cut: a blue blade with a light core, crossing the gap diagonally.
$shadowPen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(90, 2, 6, 23), 96)
$shadowPen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$shadowPen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$g.DrawLine($shadowPen, 630, 198, 410, 842)

$bladePen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(255, 96, 165, 250), 64)
$bladePen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$bladePen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$g.DrawLine($bladePen, 622, 190, 402, 834)

$corePen = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(230, 219, 234, 254), 18)
$corePen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
$corePen.EndCap = [System.Drawing.Drawing2D.LineCap]::Round
$g.DrawLine($corePen, 622, 190, 402, 834)

$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$pngs = @{}
foreach ($s in $sizes) {
    $small = New-Object System.Drawing.Bitmap($s, $s, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $sg = [System.Drawing.Graphics]::FromImage($small)
    $sg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $sg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $sg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $sg.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $sg.DrawImage($bmp, (New-Object System.Drawing.Rectangle(0, 0, $s, $s)))
    $sg.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $small.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngs[$s] = $ms.ToArray()
    $ms.Dispose()
    if ($s -eq 256) {
        [System.IO.File]::WriteAllBytes((Join-Path $here 'icon.png'), $pngs[$s])
    }
    $small.Dispose()
}

# 512x512 preview for the README.
$preview = New-Object System.Drawing.Bitmap(512, 512, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$pg = [System.Drawing.Graphics]::FromImage($preview)
$pg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$pg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$pg.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
$pg.DrawImage($bmp, (New-Object System.Drawing.Rectangle(0, 0, 512, 512)))
$pg.Dispose()
$preview.Save((Join-Path $here 'icon.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$preview.Dispose()

# ICO container with PNG-compressed entries (Vista+).
$icoPath = Join-Path $here 'icon.ico'
$stream = [System.IO.File]::Create($icoPath)
$writer = New-Object System.IO.BinaryWriter($stream)
$writer.Write([UInt16]0)
$writer.Write([UInt16]1)
$writer.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
foreach ($s in $sizes) {
    $len = $pngs[$s].Length
    $dim = if ($s -ge 256) { 0 } else { $s }
    $writer.Write([byte]$dim)
    $writer.Write([byte]$dim)
    $writer.Write([byte]0)
    $writer.Write([byte]0)
    $writer.Write([UInt16]1)
    $writer.Write([UInt16]32)
    $writer.Write([UInt32]$len)
    $writer.Write([UInt32]$offset)
    $offset += $len
}
foreach ($s in $sizes) {
    $writer.Write([byte[]]$pngs[$s])
}
$writer.Close()
$stream.Close()

# Raw RGBA for the runtime window icon (128x128).
$raw = New-Object System.Drawing.Bitmap(128, 128, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$rg = [System.Drawing.Graphics]::FromImage($raw)
$rg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$rg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
$rg.DrawImage($bmp, (New-Object System.Drawing.Rectangle(0, 0, 128, 128)))
$rg.Dispose()
$bytes = New-Object byte[] (128 * 128 * 4)
$index = 0
for ($y = 0; $y -lt 128; $y++) {
    for ($x = 0; $x -lt 128; $x++) {
        $c = $raw.GetPixel($x, $y)
        $bytes[$index] = $c.R
        $bytes[$index + 1] = $c.G
        $bytes[$index + 2] = $c.B
        $bytes[$index + 3] = $c.A
        $index += 4
    }
}
[System.IO.File]::WriteAllBytes((Join-Path $here 'icon_128.rgba'), $bytes)
$raw.Dispose()

$g.Dispose()
$bmp.Dispose()
Write-Output ("wrote icon.ico ({0:N0} bytes), icon.png, icon_128.rgba" -f (Get-Item $icoPath).Length)
