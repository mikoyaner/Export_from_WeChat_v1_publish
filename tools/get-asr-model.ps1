<#
.SYNOPSIS
  下载 SenseVoice 的 sherpa-onnx 版模型，放进 vendor\models\sensevoice\（语音转写是可选功能）。

.DESCRIPTION
  需要两个文件：model.int8.onnx（约 239 MB）+ tokens.txt（约 316 KB）。
  ⚠ 必须是 **sherpa-onnx 转好的** ONNX。ModelScope 上 FunASR 的原始模型（.pt）或它的
    SenseVoiceSmall-onnx 是另一套图接口，本项目用的 sherpa-onnx-node 加载不了。

  三种用法：
    1) 自动（推荐）  .\tools\get-asr-model.ps1
       从 k2-fsa/sherpa-onnx 的 asr-models release 里自动找 sense-voice 的包。
    2) 给直链        .\tools\get-asr-model.ps1 -Url https://.../sherpa-onnx-sense-voice-xxxx.tar.bz2
    3) 已经下好了    .\tools\get-asr-model.ps1 -Tarball D:\downloads\xxx.tar.bz2
       已经解压了    .\tools\get-asr-model.ps1 -FromDir D:\downloads\sherpa-onnx-sense-voice-xxxx

.PARAMETER Dest
  目标目录，默认 <项目根>\vendor\models\sensevoice。

.PARAMETER Force
  目标文件已存在时也重新安装。

.EXAMPLE
  .\tools\get-asr-model.ps1
.EXAMPLE
  .\tools\get-asr-model.ps1 -Tarball D:\downloads\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2
#>
[CmdletBinding()]
param(
    [string]$Url = "",
    [string]$Tarball = "",
    [string]$FromDir = "",
    [string]$Dest = "",
    [switch]$Force
)

$ErrorActionPreference = "Stop"
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

$root = Split-Path -Parent $PSScriptRoot
if (-not $Dest) { $Dest = Join-Path $root "vendor\models\sensevoice" }
$need = @("model.int8.onnx", "tokens.txt")

function Say($msg, $color = "Gray") { Write-Host $msg -ForegroundColor $color }
function Fail($msg) { Write-Host "`n[失败] $msg" -ForegroundColor Red; exit 1 }

function Test-Sherpa {
    $voice = Join-Path $root "voice2text.cjs"
    $node = Get-Command node -ErrorAction SilentlyContinue
    if (-not $node) { Say "`n[提示] 没找到 node，跳过加载自检（装好 Node.js 后可手动跑：node voice2text.cjs）" Yellow; return }
    if (-not (Test-Path $voice)) { return }

    $vendorNM = Join-Path $root "vendor\node_modules"
    if (-not (Test-Path (Join-Path $vendorNM "sherpa-onnx-node"))) {
        Say "`n[下一步] vendor 里缺 JS 依赖，执行：" Yellow
        Say "  npm --prefix vendor install silk-wasm sherpa-onnx-node" Cyan
        return
    }

    Say "`n加载自检（node voice2text.cjs，无参数只打印来源）…"
    # PS5.1 会把原生命令的 stderr 变成 ErrorRecord，在 Stop 下直接终止脚本；
    # 交给 cmd 在它那一层做 2>&1，PowerShell 就只看到普通文本。
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $out = (& cmd.exe /c "`"$($node.Source)`" `"$voice`" 2>&1" | Out-String)
    $ErrorActionPreference = $prev
    foreach ($ln in ($out -split "\r?\n")) {
        if ($ln.Trim()) { Say ("  " + $ln.Trim()) }
    }
    if ($out -match "模型来自 随包 vendor") {
        Say "`n[通过] 模型与依赖都已就位，语音转写可用。" Green
    }
    else {
        Say "`n[注意] 自检里没看到「模型来自 随包 vendor」，把上面几行贴出来排查。" Yellow
    }
}

Say "== SenseVoice 模型安装（sherpa-onnx 版）==" Cyan
Say "目标目录：$Dest"

# ── 0. 已经装好就不重复下（除非 -Force）─────────────────────────
$have = @($need | Where-Object { Test-Path (Join-Path $Dest $_) })
if ($have.Count -eq $need.Count -and -not $Force) {
    Say "`n两个文件都已存在，跳过下载。要重装请加 -Force。" Green
    foreach ($n in $need) {
        $p = Join-Path $Dest $n
        $mb = [math]::Round((Get-Item $p).Length / 1MB, 1)
        Say ("  {0,-18} {1,8} MB" -f $n, $mb)
    }
    $null = Test-Sherpa
    exit 0
}

# ── 1. 找来源 ─────────────────────────────────────────────────
$tmpRoot = Join-Path ([IO.Path]::GetTempPath()) ("sensevoice_" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Force -Path $tmpRoot | Out-Null

$source = $null          # 'tarball' | 'dir'
$tarballPath = ""

if ($FromDir) {
    if (-not (Test-Path $FromDir)) { Fail "找不到目录：$FromDir" }
    $source = "dir"
}
elseif ($Tarball) {
    if (-not (Test-Path $Tarball)) { Fail "找不到压缩包：$Tarball" }
    $tarballPath = (Resolve-Path $Tarball).Path
    $source = "tarball"
}
else {
    if (-not $Url) {
        Say "`n未指定来源，尝试从 GitHub 找最新包（k2-fsa/sherpa-onnx 的 asr-models release）…"
        try {
            $rel = Invoke-RestMethod -UseBasicParsing -Headers @{ "User-Agent" = "reply-assistant" } `
                -Uri "https://api.github.com/repos/k2-fsa/sherpa-onnx/releases/tags/asr-models"
            $asset = $rel.assets | Where-Object { $_.name -like "sherpa-onnx-sense-voice-*" -and $_.name -like "*.tar.bz2" } |
                Sort-Object name -Descending | Select-Object -First 1
            if ($asset) {
                $Url = $asset.browser_download_url
                Say ("找到：" + $asset.name) Green
            }
        }
        catch {
            Say ("查 GitHub API 失败：" + $_.Exception.Message) Yellow
        }
        if (-not $Url) {
            Say "`n没能自动找到下载地址。请打开下面页面，下载名字形如" DarkYellow
            Say "  sherpa-onnx-sense-voice-*.tar.bz2" DarkYellow
            Say "的附件，然后重跑：.\tools\get-asr-model.ps1 -Tarball <你下载的文件>" DarkYellow
            Say "  https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models" Cyan
            Fail "需要手动指定来源（-Url 或 -Tarball）"
        }
    }
    $tarballPath = Join-Path $tmpRoot "model.tar.bz2"
    Say "`n下载：$Url"
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $tarballPath
    }
    catch {
        Fail ("下载失败：" + $_.Exception.Message + "`n  换用 -Tarball（先把文件下好）或 -Url 指定镜像试试。")
    }
    $source = "tarball"
}

# ── 2. 解包 / 定位文件 ────────────────────────────────────────
$searchRoot = $FromDir
if ($source -eq "tarball") {
    $extractDir = Join-Path $tmpRoot "x"
    New-Item -ItemType Directory -Force -Path $extractDir | Out-Null
    Say "`n解包（$([math]::Round((Get-Item $tarballPath).Length/1MB,1)) MB）…"
    $ok = $false
    foreach ($flag in @("-xjf", "-xf")) {          # bz2 不认就退回自动识别
        & tar.exe $flag $tarballPath -C $extractDir 2>$null
        if ($LASTEXITCODE -eq 0) { $ok = $true; break }
    }
    if (-not $ok) {
        Fail "解包失败。请手动用 7-Zip 解压，然后：.\tools\get-asr-model.ps1 -FromDir <解压出的目录>"
    }
    $searchRoot = $extractDir
}

$onnxInt8 = Get-ChildItem -Path $searchRoot -Recurse -File -Filter "model.int8.onnx" | Select-Object -First 1
if (-not $onnxInt8) {
    $onnxFp32 = Get-ChildItem -Path $searchRoot -Recurse -File -Filter "model.onnx" | Select-Object -First 1
    if ($onnxFp32) {
        Say "`n包里没有 model.int8.onnx（量化版），只有 model.onnx（fp32，大很多）。" Yellow
        Say "仍可以用，但会慢一些、占更多内存。继续用 model.onnx。" Yellow
        $onnxInt8 = $onnxFp32
    }
}
$tokens = Get-ChildItem -Path $searchRoot -Recurse -File -Filter "tokens.txt" | Select-Object -First 1
if (-not $onnxInt8) { Fail "包里没找到 model.int8.onnx / model.onnx，下载的可能是别的模型。" }
if (-not $tokens) { Fail "包里没找到 tokens.txt。" }

# ── 3. 安装 + 校验 ───────────────────────────────────────────
New-Item -ItemType Directory -Force -Path $Dest | Out-Null
Copy-Item $onnxInt8.FullName (Join-Path $Dest "model.int8.onnx") -Force
Copy-Item $tokens.FullName    (Join-Path $Dest "tokens.txt") -Force

$onnxSize = (Get-Item (Join-Path $Dest "model.int8.onnx")).Length
$tokSize = (Get-Item (Join-Path $Dest "tokens.txt")).Length
Say "`n已安装：" Green
Say ("  model.int8.onnx  {0,9:N0} 字节  ({1} MB)" -f $onnxSize, [math]::Round($onnxSize/1MB,1))
Say ("  tokens.txt       {0,9:N0} 字节" -f $tokSize)
Say ("  SHA256(model)    " + (Get-FileHash (Join-Path $Dest "model.int8.onnx") -Algorithm SHA256).Hash.ToLower())

if ($onnxSize -lt 50MB) { Say "`n[警告] 模型只有 $([math]::Round($onnxSize/1MB,1)) MB，正常约 230~250 MB，可能下错了包。" Yellow }
if ($tokSize -lt 1KB) { Fail "tokens.txt 太小，包不完整。" }

# 清理临时目录（下载/解包产物）
Remove-Item -Recurse -Force $tmpRoot -ErrorAction SilentlyContinue

# ── 4. 依赖与加载自检 ────────────────────────────────────────
Test-Sherpa

Say "`n完成。用法：正常发「回复 某人 30」，语音会被离线转写成文字再交给模型；" Cyan
Say "状态行出现「语音转写 N 条」即生效。" Cyan
