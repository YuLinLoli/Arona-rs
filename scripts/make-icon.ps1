<#
  从学生立绘生成 Windows 可执行文件用的多尺寸 .ico。

  处理流程: 取原图指定位置的正方形区域(默认取上半部分) -> 按各尺寸高质量缩放
           -> 组装成多尺寸 ICO(全部为 32bpp DIB 条目, 兼容性最好; 可用 -PngThreshold 让大尺寸改用 PNG)。

  用法:
    pwsh -File scripts/make-icon.ps1
    pwsh -File scripts/make-icon.ps1 -Source "路径\阿罗娜.png" -CropY 60

  参数 CropX/CropY/CropSide 对应在原图上裁切的区域；仓库内的源立绘 900x1200，
  默认裁 x=150,y=0,边长 600（取原图上半部分的正方形）。
#>
param(
  [string]$Source = "assets/source/阿罗娜.png",
  [string]$Out = "assets/arona.ico",
  [int]$CropX = 150,
  [int]$CropY = 0,
  [int]$CropSide = 600,
  [int[]]$Sizes = @(16, 24, 32, 48, 64, 128, 256),
  # 大于等于该尺寸的条目改用 PNG 压缩(体积小); 默认 4096 表示全部使用 DIB
  [int]$PngThreshold = 4096,
  [string]$PreviewPng = "assets/arona.png"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $PSScriptRoot
function Resolve-Path2([string]$p) { if ([System.IO.Path]::IsPathRooted($p)) { $p } else { Join-Path $root $p } }

$source = Resolve-Path2 $Source
$outIco = Resolve-Path2 $Out
$preview = Resolve-Path2 $PreviewPng
if (-not (Test-Path $source)) { throw "找不到源图片: $source" }
[System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($outIco)) | Out-Null

$image = [System.Drawing.Image]::FromFile($source)
Write-Host "源图片: $source ($($image.Width)x$($image.Height))"
if ($CropX + $CropSide -gt $image.Width -or $CropY + $CropSide -gt $image.Height) {
  throw "裁切区域越界: x=$CropX y=$CropY side=$CropSide"
}

function Resize-Square([System.Drawing.Image]$img, [int]$x, [int]$y, [int]$side, [int]$size) {
  $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
  $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
  $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
  $dst = New-Object System.Drawing.Rectangle 0, 0, $size, $size
  $srcRect = New-Object System.Drawing.Rectangle $x, $y, $side, $side
  $g.DrawImage($img, $dst, $srcRect, [System.Drawing.GraphicsUnit]::Pixel)
  $g.Dispose()
  return $bmp
}

function Get-BgraBytes([System.Drawing.Bitmap]$bmp) {
  $rect = New-Object System.Drawing.Rectangle 0, 0, $bmp.Width, $bmp.Height
  $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  try {
    $len = [Math]::Abs($data.Stride) * $bmp.Height
    $buf = New-Object byte[] $len
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buf, 0, $len)
    return $buf
  } finally { $bmp.UnlockBits($data) }
}

function Get-DibEntry([System.Drawing.Bitmap]$bmp) {
  $w = $bmp.Width; $h = $bmp.Height
  $pixels = Get-BgraBytes $bmp
  $stride = $w * 4
  $ms = New-Object System.IO.MemoryStream
  $bw = New-Object System.IO.BinaryWriter $ms
  # BITMAPINFOHEADER: 高度写 2 倍(ICO 约定: XOR 位图 + AND 掩码)
  $bw.Write([uint32]40); $bw.Write([int32]$w); $bw.Write([int32]($h * 2))
  $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]0)
  $bw.Write([uint32]($stride * $h)); $bw.Write([int32]0); $bw.Write([int32]0)
  $bw.Write([uint32]0); $bw.Write([uint32]0)
  # XOR 位图: 自下而上
  for ($row = $h - 1; $row -ge 0; $row--) { $bw.Write($pixels, $row * $stride, $stride) }
  # AND 掩码: 每行按 4 字节对齐, 32bpp 时全部为 0(由 alpha 决定透明)
  $maskStride = [int](([Math]::Floor(($w + 31) / 32)) * 4)
  $mask = New-Object byte[] ($maskStride * $h)
  $bw.Write($mask, 0, $mask.Length)
  $bw.Flush()
  $bytes = $ms.ToArray()
  $bw.Dispose(); $ms.Dispose()
  return $bytes
}

function Get-PngEntry([System.Drawing.Bitmap]$bmp) {
  $ms = New-Object System.IO.MemoryStream
  $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  $bytes = $ms.ToArray()
  $ms.Dispose()
  return $bytes
}

# 默认全部使用 DIB 条目(32bpp + AND 掩码), 兼容性最好
$entries = @()
foreach ($size in $Sizes) {
  $bmp = Resize-Square $image $CropX $CropY $CropSide $size
  if ($size -ge $PngThreshold) { $bytes = Get-PngEntry $bmp } else { $bytes = Get-DibEntry $bmp }
  $entries += [pscustomobject]@{ Size = $size; Bytes = $bytes }
  $kind = if ($size -ge $PngThreshold) { "PNG" } else { "DIB" }
  Write-Host ("  尺寸 {0,3}: {1,6} 字节 ({2})" -f $size, $bytes.Length, $kind)
  if ($size -eq 256) { $bmp.Save($preview, [System.Drawing.Imaging.ImageFormat]::Png) }
  $bmp.Dispose()
}
$image.Dispose()

$fh = [System.IO.File]::Create($outIco)
$bw = New-Object System.IO.BinaryWriter $fh
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$entries.Count)
$offset = 6 + 16 * $entries.Count
foreach ($e in $entries) {
  $dim = if ($e.Size -ge 256) { 0 } else { $e.Size }
  $bw.Write([byte]$dim); $bw.Write([byte]$dim)
  $bw.Write([byte]0); $bw.Write([byte]0)
  $bw.Write([uint16]1); $bw.Write([uint16]32)
  $bw.Write([uint32]$e.Bytes.Length); $bw.Write([uint32]$offset)
  $offset += $e.Bytes.Length
}
foreach ($e in $entries) { $bw.Write($e.Bytes, 0, $e.Bytes.Length) }
$bw.Flush(); $bw.Dispose(); $fh.Dispose()

Write-Host "已生成: $outIco ($((Get-Item $outIco).Length) 字节)"
Write-Host "预览图: $preview"