# shell-remote one-line agent install script (Windows / PowerShell)
# DO NOT run directly — use:
#   run:      irm <relay>/agent/install.ps1 | iex
#   download: & ([scriptblock]::Create((irm <relay>/agent/install.ps1))) --download-only
#
# Sources are tried in order: GitHub release directly, then the GitHub CDN
# mirrors (same list and order as DEFAULT_CDN_BASE_URLS in
# src/relay/artifact_store.rs), then this relay's own /download endpoint (only
# served when the relay runs with --download-dir). Each source prints live
# progress (% / bytes / speed) on one self-refreshing line; a source that fails
# or stalls is abandoned and the next one is tried.
#
# Written for Windows PowerShell 5.1: no Invoke-WebRequest (it buffers the whole
# body, so there is no progress to show), no `??`, no ternary, no -Parallel.

$ErrorActionPreference = "Stop"

# Win10's .NET 4.x does not always enable TLS 1.2 by default; without it the
# handshake with github.com fails outright.
[Net.ServicePointManager]::SecurityProtocol = `
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

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
$PART = "$BIN.part"

$BASE = "https://github.com/zzttzzmyswy/shell-remote/releases/latest/download"

# Tried top to bottom. A CDN entry is a prefix mirror: mirror origin + the full
# official URL ("https://gh-proxy.com/$BASE"), not origin + relative path. Bare
# github.com needs no mirror, so it is entry #1. Label "relay" is expanded to
# "relay <host>" when printed.
#
# SHELL_REMOTE_INSTALL_URLS overrides the whole list with space-separated full
# URLs (label = URL host). TESTING ONLY; unset in production.
$URLS = @(
    @{ Label = "GitHub";                   Url = "$BASE/$ASSET" }
    @{ Label = "CDN gh-proxy.com";         Url = "https://gh-proxy.com/$BASE/$ASSET" }
    @{ Label = "CDN edgeone.gh-proxy.org"; Url = "https://edgeone.gh-proxy.org/$BASE/$ASSET" }
    @{ Label = "CDN hk.gh-proxy.org";      Url = "https://hk.gh-proxy.org/$BASE/$ASSET" }
    @{ Label = "CDN ghproxy.net";          Url = "https://ghproxy.net/$BASE/$ASSET" }
    @{ Label = "relay";                    Url = "$RELAY_URL/download/$ASSET" }
)
if ($env:SHELL_REMOTE_INSTALL_URLS) {
    $URLS = @()
    foreach ($u in $env:SHELL_REMOTE_INSTALL_URLS.Split(" ")) {
        if ($u) { $URLS += @{ Label = ([uri]$u).Host; Url = $u } }
    }
}

function Format-Size {
    param([double]$Bytes)
    if ($Bytes -ge 1048576) { return "{0:N1} MB" -f ($Bytes / 1048576) }
    if ($Bytes -ge 1024) { return "{0:N0} KB" -f ($Bytes / 1024) }
    return "{0:N0} B" -f $Bytes
}

# Save-RemoteFile downloads $Url to $Dest with live progress and the same
# failure policy as the shell script: 10s to connect, abort once the average
# over a 20s window drops below 10 KB/s, 600s absolute cap. Throws on failure;
# the caller deletes $Dest. Returns the byte count.
function Save-RemoteFile {
    param([string]$Url, [string]$Dest, [string]$Label)

    $req = [System.Net.HttpWebRequest]::Create($Url)
    $req.Timeout = 10000                 # connect + first response byte
    $req.ReadWriteTimeout = 20000        # per-read: a dead socket fails here
    $req.AllowAutoRedirect = $true
    $req.MaximumAutomaticRedirections = 5
    $req.UserAgent = "shell-remote-install"
    $req.Accept = "*/*"

    $resp = $null
    $in = $null
    $out = $null
    $progressed = $false
    try {
        try {
            $resp = $req.GetResponse()
        } catch [System.Net.WebException] {
            $e = $_.Exception
            if ($e.Status -eq [System.Net.WebExceptionStatus]::Timeout) { throw "connect timed out" }
            if ($e.Response) {
                $code = [int]([System.Net.HttpWebResponse]$e.Response).StatusCode
                throw "http $code"
            }
            throw $e.Message
        }
        $total = $resp.ContentLength
        $in = $resp.GetResponseStream()
        $out = [System.IO.File]::Create($Dest)

        $buf = New-Object byte[] 65536
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $got = 0
        # Low-speed window: bytes and elapsed at the start of the current window.
        $winBytes = 0
        $winStart = 0.0
        $lastTick = 0.0
        $speed = 0.0
        while ($true) {
            $n = $in.Read($buf, 0, $buf.Length)
            if ($n -le 0) { break }
            $out.Write($buf, 0, $n)
            $got += $n
            $el = $sw.Elapsed.TotalSeconds

            if (($el - $winStart) -ge 20) {
                $rate = ($got - $winBytes) / ($el - $winStart)
                if ($rate -lt 10240) {
                    throw ("too slow ({0:N0} KB/s over 20s)" -f ($rate / 1024))
                }
                $winStart = $el
                $winBytes = $got
            }
            if ($el -ge 600) { throw "exceeded 600s limit" }

            # Refresh at most 4x/s; speed is the current window's average.
            if (($el - $lastTick) -ge 0.25) {
                $span = $el - $winStart
                if ($span -ge 0.5) { $speed = ($got - $winBytes) / $span }
                if ($total -gt 0) {
                    $pct = 100.0 * $got / $total
                    $line = "`r[shell-remote]   {0,5:N1}%  {1} / {2}  {3}/s" -f `
                        $pct, (Format-Size $got), (Format-Size $total), (Format-Size $speed)
                } else {
                    $line = "`r[shell-remote]   {0}  {1}/s" -f (Format-Size $got), (Format-Size $speed)
                }
                Write-Host -NoNewline $line
                $progressed = $true
                $lastTick = $el
            }
        }
        $sw.Stop()
        if ($progressed) { Write-Host "" }
        return @{ Got = $got; Seconds = $sw.Elapsed.TotalSeconds }
    } catch {
        # Close out the self-refreshing progress line so the failure message
        # starts on a line of its own instead of being appended to it.
        if ($progressed) { Write-Host "" }
        throw
    } finally {
        if ($out) { $out.Close() }
        if ($in) { $in.Close() }
        if ($resp) { $resp.Close() }
    }
}

# A mirror that answers with an HTML error page yields a 200 with a non-empty
# body, so check the PE magic ("MZ") before trusting the file.
function Test-PeFile {
    param([string]$Path)
    if (-not (Test-Path $Path)) { return $false }
    $fs = [System.IO.File]::OpenRead($Path)
    try {
        if ($fs.Length -lt 2) { return $false }
        $b = New-Object byte[] 2
        [void]$fs.Read($b, 0, 2)
        return ($b[0] -eq 0x4D -and $b[1] -eq 0x5A)
    } finally {
        $fs.Close()
    }
}

Write-Host "[shell-remote] downloading $ASSET..."

$ok = $false
$i = 0
foreach ($src in $URLS) {
    $i++
    $label = $src.Label
    $url = $src.Url
    $hostName = ([uri]$url).Host
    if ($label -eq "relay") { $label = "relay $hostName" }

    Write-Host "[shell-remote] [$i/$($URLS.Count)] trying $label ($hostName)"
    if (Test-Path $PART) { Remove-Item $PART -Force -ErrorAction SilentlyContinue }
    try {
        $r = Save-RemoteFile -Url $url -Dest $PART -Label $label
        if (-not (Test-PeFile $PART)) {
            throw "not a PE binary (mirror error page?)"
        }
        Move-Item -Force $PART $BIN
        $secs = [math]::Max($r.Seconds, 1)
        Write-Host ("[shell-remote] downloaded {0} in {1:N0}s (avg {2}/s) from {3}" -f `
            (Format-Size $r.Got), $secs, (Format-Size ($r.Got / $secs)), $label)
        $ok = $true
        break
    } catch {
        if (Test-Path $PART) { Remove-Item $PART -Force -ErrorAction SilentlyContinue }
        Write-Host "[shell-remote] $label failed: $($_.Exception.Message), trying next source"
    }
}

if (-not $ok) {
    Write-Host "[shell-remote] download failed - no valid binary obtained from any source"
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
    Remove-Item $BIN -Force -ErrorAction SilentlyContinue
    Write-Host "[shell-remote] cleaned up"
}
