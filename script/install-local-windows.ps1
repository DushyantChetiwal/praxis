<#
.SYNOPSIS
    Builds this fork and installs it beside the released Zed, without an installer.

.DESCRIPTION
    The Inno Setup bundler (script/bundle-windows.ps1) produces a real installer,
    but it builds into its own `--target` directory from scratch and costs upwards
    of 100 GB of transient disk. This does the part that matters for using the
    editor day to day: build, then copy the three files Zed needs to run.

    `conpty.dll` and `OpenConsole.exe` have to sit beside `zed.exe` or the
    built-in terminal fails to start, which is why they are copied too.

    What you do NOT get, versus the installer: a Start Menu entry, file
    associations, the `zed` CLI on PATH, and the Explorer context-menu shell
    extension.

.PARAMETER Profile
    Cargo profile. `release-fast` (the default) is fully optimised but skips the
    single-threaded LTO link that makes `release` take roughly twice as long.

.PARAMETER Run
    Launch the editor when the install finishes.
#>
[CmdletBinding()]
Param(
    [Parameter()][string]$Profile = 'release-fast',
    [Parameter()][switch]$Run
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    # The channel decides the install directory and, at runtime, the user data
    # directory. Keeping them aligned is what stops this build treating an
    # installed stable Zed's settings and threads as its own.
    $channel = (Get-Content 'crates/zed/RELEASE_CHANNEL' -Raw).Trim()
    $appName = if ($channel -eq 'stable') { 'Zed' } else { "Zed $((Get-Culture).TextInfo.ToTitleCase($channel))" }
    $dest = Join-Path $env:LOCALAPPDATA "Programs\$appName"

    Write-Host "Channel : $channel"
    Write-Host "Profile : $Profile"
    Write-Host "Target  : $dest"
    Write-Host ''

    Write-Host 'Building...' -ForegroundColor Cyan
    cargo build --profile $Profile --package zed
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }

    # `dev` is cargo's directory name for the `dev` profile; every other profile
    # uses its own name.
    $outDir = if ($Profile -eq 'dev') { 'target/debug' } else { "target/$Profile" }

    # A running copy holds a lock on its own exe, so it has to go first.
    Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($dest, [StringComparison]::OrdinalIgnoreCase) } |
        ForEach-Object {
            Write-Host "Stopping running instance (PID $($_.Id))" -ForegroundColor Yellow
            Stop-Process -Id $_.Id -Force
        }
    Start-Sleep -Seconds 3

    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    foreach ($file in 'zed.exe', 'conpty.dll', 'OpenConsole.exe') {
        $src = Join-Path $outDir $file
        if (-not (Test-Path $src)) { throw "Missing build output: $src" }
        Copy-Item $src (Join-Path $dest $file) -Force
        $mb = [math]::Round((Get-Item $src).Length / 1MB, 1)
        Write-Host ("  {0,-18} {1,7} MB" -f $file, $mb)
    }

    Write-Host ''
    Write-Host "Installed to $dest" -ForegroundColor Green
    Write-Host "User data in $(Join-Path $env:LOCALAPPDATA $appName)"

    if ($Run) {
        Start-Process -FilePath (Join-Path $dest 'zed.exe') | Out-Null
        Write-Host 'Launched.'
    }
}
finally {
    Pop-Location
}
