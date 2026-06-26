# shell-remote one-line agent install script (Windows / PowerShell)
# DO NOT run directly — use:
#   run:      irm <relay>/agent/install.ps1 | iex
#   download: & ([scriptblock]::Create((irm <relay>/agent/install.ps1))) --download-only

$ErrorActionPreference = "Stop"
# Speed up Invoke-WebRequest by suppressing the progress bar (PS 5.1).
$ProgressPreference = "SilentlyContinue"

$RELAY_URL = "__RELAY_URL__"

# Only the x86_64.exe build is published; ARM64 Windows 11 runs it via x64 emulation.
$ASSET = "shell-remote-x86_64.exe"

# --download-only: save the binary to the current directory and do not run it.
$downloadOnly = $false
$passArgs = @()
foreach ($a in $args) {
    if ($a -eq "--download-only") { $downloadOnly = $true }
    else { $passArgs += $a }
}

if ($downloadOnly) {
    $BIN = Join-Path (Get-Location) "shell-remote.exe"
} else {
    $BIN = Join-Path $env:TEMP "shell-remote-$PID.exe"
}

$BASE = "https://github.com/zzttzzmyswy/shell-remote/releases/latest/download"
$URLS = @(
    "$BASE/$ASSET",
    "https://edgeone.gh-proxy.com/$BASE/$ASSET",
    "https://hk.gh-proxy.com/$BASE/$ASSET",
    "https://gh-proxy.com/$BASE/$ASSET",
    "https://gh.llkk.cc/$BASE/$ASSET"
)

Write-Host "[shell-remote] downloading $ASSET..."

$ok = $false
foreach ($url in $URLS) {
    try {
        Invoke-WebRequest -Uri $url -OutFile $BIN -UseBasicParsing -TimeoutSec 60
        if ((Test-Path $BIN) -and (Get-Item $BIN).Length -gt 0) {
            Write-Host "[shell-remote] downloaded via $(([uri]$url).Host)"
            $ok = $true
            break
        }
    } catch {
        # try next mirror
    }
}

if (-not $ok) {
    Write-Host "[shell-remote] download failed - all sources unreachable"
    exit 1
}

if ($downloadOnly) {
    Write-Host "[shell-remote] saved to $BIN (not executed)"
    exit 0
}

try {
    Write-Host "[shell-remote] starting agent..."
    & $BIN agent --relay-url $RELAY_URL @passArgs
} finally {
    Remove-Item $BIN -ErrorAction SilentlyContinue
    Write-Host "[shell-remote] cleaned up"
}
