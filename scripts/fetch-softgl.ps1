<#
.SYNOPSIS
    为 arona-rs 获取「软件 OpenGL(softgl)」兜底依赖（Mesa llvmpipe）。

.DESCRIPTION
    Windows Server / 没有显卡驱动的虚拟机只有 GDI 自带的 OpenGL 1.1，而 egui 需要
    OpenGL 2.0+；一旦 wgpu 的 DX12/WARP 也不可用（例如系统缺少 d3d12.dll），管理面板
    在这类机器上根本打不开。把 Mesa 的软件渲染 opengl32.dll 放到 exe 同级的 softgl\
    目录即可用 CPU 把界面画出来（详见 crates/arona/src/runtime/softgl.rs）。

    下载 mesa-dist-win 的 MSVC 发布包，只解出 x64 需要的三个文件：
      opengl32.dll         Mesa 的 WGL 前端
      libgallium_wgl.dll   gallium 驱动（含 llvmpipe 软件光栅化）
      dxil.dll             给 Mesa 的 d3d12 驱动签名用（非必需，一起带上更稳）

    产物不入库（.gitignore 已忽略 /softgl/），需要时重新跑本脚本即可。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1 -Force
#>
[CmdletBinding()]
param(
    # mesa-dist-win 的版本号（https://github.com/pal1000/mesa-dist-win/releases）
    [string]$Version = "26.2.0",
    # 输出目录，默认仓库根目录下的 softgl\
    [string]$Dest = "",
    # 已存在时也重新下载
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Dest)) {
    $Dest = Join-Path $root "softgl"
}
$Dest = [System.IO.Path]::GetFullPath($Dest)

$files = @("opengl32.dll", "libgallium_wgl.dll", "dxil.dll")
$already = (Test-Path (Join-Path $Dest "opengl32.dll")) -and (Test-Path (Join-Path $Dest "libgallium_wgl.dll"))
if ($already -and -not $Force) {
    Write-Host "[softgl] 已存在，跳过下载: $Dest" -ForegroundColor Green
    Write-Host "[softgl] 需要重新下载请加 -Force"
    exit 0
}

$archiveName = "mesa3d-$Version-release-msvc.7z"
$url = "https://github.com/pal1000/mesa-dist-win/releases/download/$Version/$archiveName"
# 工作目录放工程内(与最终目标同盘, 也避免 %TEMP% 被重定向/锁定时 7z 打不开下载好的包)
$tmp = Join-Path $root ("target\softgl-fetch-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    $archive = Join-Path $tmp $archiveName
    Write-Host "[softgl] 下载 $url"
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing -TimeoutSec 900
    $watch.Stop()
    $size = [math]::Round((Get-Item $archive).Length / 1MB, 1)
    Write-Host ("[softgl] 下载完成: {0} MB, 耗时 {1} 秒" -f $size, [int]$watch.Elapsed.TotalSeconds)

    # 校验一下拿到的确实是 7z 包(7z 魔数)，避免把错误页面当成压缩包
    $magic = [System.IO.File]::ReadAllBytes($archive)[0..5]
    if (($magic | ForEach-Object { $_.ToString("X2") }) -join "" -ne "377ABCAF271C") {
        throw "下载到的文件不是 7z 压缩包: $archive"
    }

    $extract = Join-Path $tmp "out"
    New-Item -ItemType Directory -Force -Path $extract | Out-Null

    $sevenZip = $null
    foreach ($candidate in @(
            (Join-Path ${env:ProgramFiles} "7-Zip\7z.exe"),
            (Join-Path ${env:ProgramFiles(x86)} "7-Zip\7z.exe"),
            (Get-Command 7z.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -ErrorAction SilentlyContinue)
        )) {
        if ($candidate -and (Test-Path $candidate)) { $sevenZip = $candidate; break }
    }

    if ($sevenZip) {
        Write-Host "[softgl] 解压(7-Zip): $sevenZip"
        & $sevenZip e $archive -o"$extract" ($files | ForEach-Object { "x64\$_" }) -y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "7z 解压失败, 退出码 $LASTEXITCODE" }
    }
    else {
        # Windows 10+ 自带的 bsdtar 也能解 7z
        Write-Host "[softgl] 解压(tar)"
        & tar -xf $archive -C $extract ($files | ForEach-Object { "x64/$_" })
        if ($LASTEXITCODE -ne 0) { throw "tar 解压失败, 退出码 $LASTEXITCODE" }
    }

    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    $copied = 0
    foreach ($name in $files) {
        $source = Join-Path $extract $name
        if (Test-Path $source) {
            Copy-Item -LiteralPath $source -Destination (Join-Path $Dest $name) -Force
            $copied++
        }
        elseif ($name -eq "dxil.dll") {
            Write-Host "[softgl] 跳过可选文件 $name"
        }
        else {
            throw "解压结果缺少 $name"
        }
    }
    if ($copied -lt 2) { throw "解压结果不完整" }

    $total = [math]::Round(((Get-ChildItem $Dest -File | Measure-Object Length -Sum).Sum / 1MB), 1)
    Write-Host "[softgl] 完成: $Dest (共 $total MB)" -ForegroundColor Green
    Write-Host "[softgl] 之后运行 arona-rs 时, 前几个渲染后端卡死或失败会自动切到 llvmpipe; 也可显式加 --softgl"
}
finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}