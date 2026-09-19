<#
.SYNOPSIS
    为 arona-rs 获取 DX12 着色器编译器依赖（DXC：dxcompiler.dll + dxil.dll）。

.DESCRIPTION
    管理面板的 wgpu(DX12) 后端需要一个着色器编译器把 WGSL 编译成 DXIL。可选两条路：
      1) 系统自带的 FXC(d3dcompiler_47.dll)：不用带任何文件，但微软已停止演进，
         且部分精简安装的 Windows Server 里根本没有这个 DLL；
      2) 随程序附带 DXC：启动时由 wgpu 动态加载 exe 同级的 dxcompiler.dll/dxil.dll，
         这是候选链里最优的一档（见 crates/arona/src/gui/mod.rs 的 wgpu_candidates）。
    本脚本负责第 2 条：从微软 DirectXShaderCompiler 的官方 Release 下载 zip，
    只取 x64 的两个 DLL 放到仓库根目录的 dxc\，由 `cargo dist` 复制到 exe 同级、
    `cargo installer` 打进安装包 —— 目标机因此仍然不需要安装任何东西。

    产物不入库（.gitignore 已忽略 /dxc/），需要时重新跑本脚本即可。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/fetch-dxc.ps1
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts/fetch-dxc.ps1 -Force
#>
[CmdletBinding()]
param(
    # DXC 版本号（https://github.com/Microsoft/DirectXShaderCompiler/releases）
    [string]$Tag = "v1.9.2607",
    # Release 里的 zip 文件名（随日期变化，跟 $Tag 一起改）
    [string]$Archive = "dxc_2026_07_29.zip",
    # 输出目录，默认仓库根目录下的 dxc\
    [string]$Dest = "",
    # 已存在时也重新下载
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Dest)) {
    $Dest = Join-Path $root "dxc"
}
$Dest = [System.IO.Path]::GetFullPath($Dest)

$files = @("dxcompiler.dll", "dxil.dll")
$already = ($files | ForEach-Object { Test-Path (Join-Path $Dest $_) }) -notcontains $false
if ($already -and -not $Force) {
    Write-Host "[dxc] 已存在，跳过下载: $Dest" -ForegroundColor Green
    Write-Host "[dxc] 需要重新下载请加 -Force"
    exit 0
}

$url = "https://github.com/microsoft/DirectXShaderCompiler/releases/download/$Tag/$Archive"
# 工作目录放工程内（与最终目标同盘，也避开 %TEMP% 被重定向/锁定的情况）
$tmp = Join-Path $root ("target\dxc-fetch-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    $zip = Join-Path $tmp $Archive
    Write-Host "[dxc] 下载 $url"
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing -TimeoutSec 900
    $watch.Stop()
    $size = [math]::Round((Get-Item $zip).Length / 1MB, 1)
    Write-Host ("[dxc] 下载完成: {0} MB, 耗时 {1} 秒" -f $size, [int]$watch.Elapsed.TotalSeconds)

    # 校验一下拿到的确实是 zip（PK\3\4 魔数），避免把错误页面当成压缩包
    $magic = [System.IO.File]::ReadAllBytes($zip)[0..3]
    if (($magic | ForEach-Object { $_.ToString("X2") }) -join "" -ne "504B0304") {
        throw "下载到的文件不是 zip 压缩包: $zip"
    }

    $extract = Join-Path $tmp "out"
    New-Item -ItemType Directory -Force -Path $extract | Out-Null

    # 这个 zip 的条目名用反斜杠分隔（bin\x64\dxcompiler.dll），Expand-Archive 处理不好，
    # 所以优先用系统自带的 bsdtar（它把反斜杠当路径分隔符）。必须写全路径：
    # 开发机上 PATH 里常排着 Git 的 GNU tar，它不认 zip，还会去连名为 "E:" 的远程主机。
    $sysTar = Join-Path ${env:SystemRoot} "System32\tar.exe"
    $extracted = $false
    if (Test-Path $sysTar) {
        Write-Host "[dxc] 解压(bsdtar): $sysTar"
        & $sysTar -xf $zip -C $extract
        $extracted = $LASTEXITCODE -eq 0
        if (-not $extracted) { Write-Host "[dxc] bsdtar 失败(退出码 $LASTEXITCODE)，改用 Expand-Archive" }
    }
    if (-not $extracted) {
        Expand-Archive -LiteralPath $zip -DestinationPath $extract -Force
    }

    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    foreach ($name in $files) {
        # 只取 x64：递归找同名文件，再按路径里的 x64 过滤（zip 里同时有 x86/arm64）
        $found = Get-ChildItem -LiteralPath $extract -Recurse -Filter $name -File |
            Where-Object { $_.FullName -match "\\x64\\" } |
            Select-Object -First 1
        if (-not $found) {
            throw "解压结果里没有 x64/$name（$Tag/$Archive 的目录结构可能变了）"
        }
        Copy-Item -LiteralPath $found.FullName -Destination (Join-Path $Dest $name) -Force
        Write-Host ("[dxc] {0}: {1} MB" -f $name, [math]::Round($found.Length / 1MB, 1))
    }

    $total = [math]::Round(((Get-ChildItem $Dest -File | Measure-Object Length -Sum).Sum / 1MB), 1)
    Write-Host "[dxc] 完成: $Dest (共 $total MB)" -ForegroundColor Green
    Write-Host "[dxc] cargo dist 会把这两个 dll 复制到 exe 同级，cargo installer 会打进安装包"
}
finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
