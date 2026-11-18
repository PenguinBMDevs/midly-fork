# P2 性能矩阵：版本 × parallel/serial × 线程 × 素材
#
# 用途：一键复现「版本对照 + 并行/串行 + 线程数 + 素材形态」的完整矩阵，
#       结果由 `midly-perf bench --out` 追加到同一个 TSV（列见 bench::tsv_header）。
#
# 设计约束（与 HANDOFF §5 一致）：
# - 每个 (版本, 模式) 只编译一次，二进制缓存在 perf/target/matrix/；
# - 版本切换靠 `--config patch.crates-io.lumino-midly.path=...`，不修改任何 Cargo.toml；
# - 基线 0.6.3/0.6.4 没有 `_collect_` API → 构建时关闭 `collect-api` 特性；
#   0.6.3 还没有 `text_events` → 同时关闭 `text-events`；
# - 素材不进仓库；gen-* 缺失时用 fork 构建自动生成；
# - 机器信息 / 负载 / 构建命令写入同名 .meta.txt（结论必须能追溯到环境）。
#
# 示例：
#   .\matrix.ps1                                        # 全矩阵（4 版本 × 2 模式 × 3 线程 × 5 素材）
#   .\matrix.ps1 -Versions fork,0.6.5 -Threads 1,8 -Materials pi,text
#   .\matrix.ps1 -NoBuild                               # 复用已缓存的二进制
#   .\matrix.ps1 -Modes parallel -Callback realistic     # 只测并行 + 下游模拟

[CmdletBinding()]
param(
    [string[]]$Versions = @('fork', '0.6.5', '0.6.4', '0.6.3'),
    [string[]]$Modes = @('parallel', 'serial'),
    [string[]]$Threads = @('1', '8', '16'),
    [string[]]$Materials = @('pi', 'clementi', 'control', 'sysex', 'text'),
    [string]$Callback = 'minimal',
    [string]$Runs = '15',
    [string]$Warmup = '3',
    [string]$Out = '',
    [switch]$NoBuild
)

$ErrorActionPreference = 'Continue'

# -File 调用时逗号参数是单个字符串：统一在这里拆分 + 校验。
$Versions = @($Versions | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Modes = @($Modes | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Threads = @($Threads | ForEach-Object { $_ -split ',' } | Where-Object { $_ } | ForEach-Object { [int]$_ })
$Materials = @($Materials | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
foreach ($m in $Modes) {
    if ($m -notin @('parallel', 'serial')) { throw "未知模式: $m（可选 parallel|serial）" }
}
if ($Callback -notin @('minimal', 'realistic', 'realistic-par')) {
    throw "未知 callback: $Callback（可选 minimal|realistic|realistic-par）"
}
$Runs = [int]$Runs
$Warmup = [int]$Warmup
if ($Threads.Count -eq 0) { throw 'Threads 不能为空' }
$Perf = $PSScriptRoot
$Repo = Split-Path $Perf -Parent
$BinDir = Join-Path $Perf 'target\matrix'
$Baselines = 'F:\midly-baselines'
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $Out) { $Out = Join-Path $Perf "results\matrix-$Stamp.tsv" }
$MetaPath = [IO.Path]::ChangeExtension($Out, '.meta.txt')
$ErrorPath = "$Out.errors.txt"

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path $Out -Parent) | Out-Null

function Resolve-Material([string]$name) {
    switch ($name) {
        'pi' { Join-Path $Repo 'test-asset\Pi.mid' }
        'clementi' { Join-Path $Repo 'test-asset\Clementi.mid' }
        'control' { Join-Path $Repo 'test-asset\generated\gen-control-dense.mid' }
        'sysex' { Join-Path $Repo 'test-asset\generated\gen-sysex-dense.mid' }
        'text' { Join-Path $Repo 'test-asset\generated\gen-text-dense.mid' }
        'giant' { Join-Path $Repo 'test-asset\generated\gen-single-giant.mid' }
        'start210k' {
            $f = Get-ChildItem (Join-Path $Repo 'test-asset') -Filter 'StarT audio 210k*.mid' |
                Select-Object -First 1
            if (-not $f) { throw '找不到 StarT audio 210k 素材' }
            $f.FullName
        }
        default { throw "未知素材: $name（可选 pi|clementi|control|sysex|text|giant|start210k）" }
    }
}

function Get-Features([string]$ver, [string]$mode) {
    $f = @()
    if ($mode -eq 'parallel') { $f += 'parallel' }
    if ($ver -ne '0.6.3') { $f += 'text-events' }
    if ($ver -eq 'fork') { $f += 'collect-api' }
    $f -join ','
}

# ── 机器信息（结论必须能追溯到环境）────────────────────────────────────────
$Cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name
$Cores = (Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum
$Load = (Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
$Os = (Get-CimInstance Win32_OperatingSystem).Caption
$Commit = (& git -C $Repo rev-parse --short HEAD 2>$null) -join ''
$Rustc = (& rustc --version) -join ''
@(
    "timestamp=$Stamp"
    "host=$env:COMPUTERNAME os=$Os"
    "cpu=$Cpu logical=$Cores"
    "load_at_start=$Load%"
    "repo=$Repo commit=$Commit"
    "rustc=$Rustc"
    "callback=$Callback warmup=$Warmup runs=$Runs"
    "versions=$($Versions -join ',') modes=$($Modes -join ',') threads=$($Threads -join ',') materials=$($Materials -join ',')"
    "out=$Out"
) | Set-Content -Path $MetaPath -Encoding utf8

if ($Load -gt 25) {
    Write-Warning "CPU 负载 ${Load}%：矩阵结果会整体偏高，解读时只能看同批相对值（见 HANDOFF §9）。"
}

# ── 素材：缺失的 gen-* 用 fork 构建生成 ────────────────────────────────────
$MaterialPaths = @($Materials | ForEach-Object { Resolve-Material $_ })
$Missing = $MaterialPaths | Where-Object { -not (Test-Path $_) }
if ($Missing) {
    Write-Host '[matrix] 生成缺失素材 ...'
    $genExe = Join-Path $BinDir 'fork-parallel.exe'
    if (-not (Test-Path $genExe)) {
        & cargo build --release --bin midly-perf --manifest-path (Join-Path $Perf 'Cargo.toml') `
            --no-default-features --features 'parallel,text-events,collect-api'
        if ($LASTEXITCODE -ne 0) { throw '生成器构建失败' }
        Copy-Item (Join-Path $Perf 'target\release\midly-perf.exe') $genExe -Force
    }
    & $genExe gen (Join-Path $Repo 'test-asset\generated') | Out-Null
    if ($LASTEXITCODE -ne 0) { throw '素材生成失败' }
    $Missing = $MaterialPaths | Where-Object { -not (Test-Path $_) }
    if ($Missing) { throw "素材仍缺失: $($Missing -join ', ')" }
}

# ── 构建：每个 (版本, 模式) 一次，二进制缓存 ──────────────────────────────
# 基线构建会改写 Cargo.lock（patch 源），先备份、构建后还原，保证默认构建仍指向 fork。
$LockPath = Join-Path $Perf 'Cargo.lock'
$LockBackup = Join-Path $env:TEMP "midly-perf-Cargo.lock.$Stamp.bak"
if (Test-Path $LockPath) { Copy-Item $LockPath $LockBackup -Force }
$Labels = @()
foreach ($ver in $Versions) {
    foreach ($mode in $Modes) {
        $label = "$ver-$mode"
        $Labels += $label
        $exe = Join-Path $BinDir "$label.exe"
        if ($NoBuild -and (Test-Path $exe)) {
            Write-Host "[matrix] 复用 $label.exe"
            continue
        }
        $feat = Get-Features $ver $mode
        $buildArgs = @('build', '--release', '--bin', 'midly-perf', '--manifest-path',
            (Join-Path $Perf 'Cargo.toml'), '--no-default-features', '--features', $feat)
        if ($ver -ne 'fork') {
            $baseline = "$($Baselines -replace '\\', '/')/lumino-midly-$ver"
            if (-not (Test-Path $baseline)) { throw "基线不存在: $baseline" }
            $buildArgs += @('--config', "patch.crates-io.lumino-midly.path='$baseline'")
        }
        Write-Host "[matrix] 构建 $label（features=$feat）..."
        & cargo @buildArgs 2>&1 | Select-Object -Last 2 | ForEach-Object { "    $_" }
        if ($LASTEXITCODE -ne 0) { throw "构建失败: $label" }
        Copy-Item (Join-Path $Perf 'target\release\midly-perf.exe') $exe -Force
    }
}
Add-Content -Path $MetaPath -Encoding utf8 -Value "labels=$($Labels -join ',')"
if (Test-Path $LockBackup) {
    Copy-Item $LockBackup $LockPath -Force
    Remove-Item $LockBackup -Force
    Write-Host '[matrix] Cargo.lock 已还原（基线构建会改写 patch 源）'
}

# ── 运行矩阵 ───────────────────────────────────────────────────────────────
$Errors = @()
$Total = $Labels.Count * $MaterialPaths.Count * $Threads.Count
$Done = 0
foreach ($label in $Labels) {
    $exe = Join-Path $BinDir "$label.exe"
    if (-not (Test-Path $exe)) { throw "缺少二进制: $exe（去掉 -NoBuild 重新构建）" }
    foreach ($mat in $MaterialPaths) {
        $matName = Split-Path $mat -Leaf
        foreach ($t in $Threads) {
            $Done++
            Write-Host ("[matrix] {0}/{1} {2} | {3} | t={4}" -f $Done, $Total, $label, $matName, $t)
            $benchArgs = @('bench', $mat, '--threads', $t, '--callback', $Callback,
                '--warmup', $Warmup, '--runs', $Runs, '--label', $label, '--out', $Out)
            $log = & $exe @benchArgs 2>&1
            if ($LASTEXITCODE -ne 0) {
                $msg = "$label`t$matName`tt=$t`t$($log -join ' ')"
                $Errors += $msg
                Write-Warning "失败: $msg"
            }
        }
    }
}

if ($Errors.Count -gt 0) {
    $Errors | Set-Content -Path $ErrorPath -Encoding utf8
    Write-Warning "有 $($Errors.Count) 个组合失败，详见 $ErrorPath"
    exit 1
}

Write-Host "[matrix] 完成：$Out"
Write-Host "[matrix] 元信息：$MetaPath"
