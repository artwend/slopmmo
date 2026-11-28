<#
.SYNOPSIS
    Runs a local MMO server, one or more clients, and optionally the protocol inspector.

.DESCRIPTION
    Builds the binaries (unless -NoBuild), starts the server on loopback, starts
    the requested number of clients, and stops everything when the clients exit
    or you press Ctrl+C.

    By default each client opens a game window and runs until you close it.
    Use -Headless to run the fixed scripted client instead (about 20 seconds of
    simulated time).

    With -Capture the clients connect through crates/protocol-inspector, which forwards
    the traffic to the server and prints each parsed packet. Without -Capture
    the clients connect straight to the server.

    Headless clients write their output to target/local-run/client-<slot>.log
    and are printed when the run finishes. Keep -Duration above the scripted
    session length; -Duration is ignored for windowed clients.

    Player identity is assigned by the server; -PlayerBase only numbers the
    client slots in console output and headless log names.

.EXAMPLE
    .\tools\run-local.ps1
    Server plus one windowed client, no capture.

.EXAMPLE
    .\tools\run-local.ps1 -Capture
    Same, with the parsed packet stream in this console.

.EXAMPLE
    .\tools\run-local.ps1 -Headless -Capture -Clients 2
    Two scripted clients, both routed through the protocol inspector.

.EXAMPLE
    .\tools\run-local.ps1 -Headless -Capture -Detail -Duration 45
    One scripted client with expanded packets and a longer safety cap.

.EXAMPLE
    .\tools\run-local.ps1 -Clients 0 -Capture
    Server and protocol-inspector only; connect a client yourself.
.EXAMPLE
    .\tools\run-local.ps1 -c -hl -n 2 -t 45 -d
    Short flags: capture, headless, two clients, 45 s cap, detail.

.NOTES
    Short flags: -n Clients, -sp ServerPort, -cp CapturePort, -c Capture,
    -d Detail, -a NoAcks, -hl Headless, -t Duration, -pb PlayerBase,
    -r Release, -nb NoBuild.
#>
[CmdletBinding()]
param(
    [Alias('n')]
    [ValidateRange(0, 64)]
    [int]$Clients = 1,

    [Alias('sp')]
    [ValidateRange(1, 65535)]
    [int]$ServerPort = 4000,

    [Alias('cp')]
    [ValidateRange(1, 65535)]
    [int]$CapturePort = 4001,

    [Alias('c')]
    [switch]$Capture,

    [Alias('d')]
    [switch]$Detail,

    [Alias('a')]
    [switch]$NoAcks,

    [Alias('hl')]
    [switch]$Headless,

    [Alias('t')]
    [ValidateRange(1, 3600)]
    [double]$Duration = 30,

    [Alias('pb')]
    [ValidateRange(0, 1000000)]
    [int]$PlayerBase = 1,

    [Alias('r')]
    [switch]$Release,

    [Alias('nb')]
    [switch]$NoBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($Capture -and $ServerPort -eq $CapturePort) {
    throw "-ServerPort and -CapturePort must be different."
}
if ($Detail -and -not $Capture) {
    Write-Warning "-Detail only affects protocol-inspector; add -Capture to use it."
}
if ($NoAcks -and -not $Capture) {
    Write-Warning "-NoAcks only affects protocol-inspector; add -Capture to use it."
}
if ($PSBoundParameters.ContainsKey('Duration') -and -not $Headless) {
    Write-Warning "-Duration only applies to -Headless clients."
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path -LiteralPath (Join-Path $RepoRoot 'Cargo.toml'))) {
    throw "Cannot find Cargo.toml in $RepoRoot; keep this script in the repository's tools directory."
}

$profileName = if ($Release) { 'release' } else { 'debug' }
$binDir = Join-Path (Join-Path $RepoRoot 'target') $profileName
$isWindowsHost = $env:OS -eq 'Windows_NT'
$exeSuffix = if ($isWindowsHost) { '.exe' } else { '' }
$serverExe = Join-Path $binDir "server$exeSuffix"
$clientExe = Join-Path $binDir "client$exeSuffix"
$inspectorExe = Join-Path $binDir "protocol-inspector$exeSuffix"

$LogDir = Join-Path (Join-Path $RepoRoot 'target') 'local-run'
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

if (-not $NoBuild) {
    $cargoArgs = @('build')
    if ($Release) {
        $cargoArgs += '--release'
    }
    $cargoArgs += @('-p', 'server', '-p', 'client')
    if ($Capture) {
        $cargoArgs += @('-p', 'protocol-inspector')
    }
    Write-Host "building: cargo $($cargoArgs -join ' ')" -ForegroundColor DarkGray
    Push-Location $RepoRoot
    try {
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

$required = @($serverExe, $clientExe)
if ($Capture) {
    $required += $inspectorExe
}
foreach ($path in $required) {
    if (-not (Test-Path -LiteralPath $path)) {
        throw "missing binary: $path (run without -NoBuild)"
    }
}

function Wait-UdpReady {
    param(
        [int]$Port,
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds = 10
    )

    if (-not (Get-Command Get-NetUDPEndpoint -ErrorAction SilentlyContinue)) {
        # No cmdlet (non-Windows); give the process time to bind and move on.
        Start-Sleep -Milliseconds 1000
        return -not $Process.HasExited
    }

    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($Process.HasExited) {
            return $false
        }
        $endpoint = Get-NetUDPEndpoint -LocalPort $Port -ErrorAction SilentlyContinue |
            Where-Object { $_.OwningProcess -eq $Process.Id } |
            Select-Object -First 1
        if ($null -ne $endpoint) {
            return $true
        }
        Start-Sleep -Milliseconds 100
    }
    return $false
}

function Show-LogTail {
    param(
        [string]$Path,
        [int]$Lines = 20
    )
    if (Test-Path -LiteralPath $Path) {
        $content = Get-Content -LiteralPath $Path -Tail $Lines -ErrorAction SilentlyContinue
        if ($content) {
            Write-Host "--- $Path ---" -ForegroundColor DarkYellow
            $content | ForEach-Object { Write-Host $_ }
        }
    }
}

function Stop-Child {
    param(
        [System.Diagnostics.Process]$Process,
        [string]$Label
    )
    if ($null -ne $Process -and -not $Process.HasExited) {
        Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
        Write-Host "stopped $Label (pid $($Process.Id))" -ForegroundColor DarkGray
    }
}

$server = $null
$relay = $null
$clientRuns = @()
$failed = $false
$clientTarget = if ($Capture) { "127.0.0.1:$CapturePort" } else { "127.0.0.1:$ServerPort" }

try {
    $serverOut = Join-Path $LogDir 'server.log'
    $serverErr = Join-Path $LogDir 'server.err.log'
    Remove-Item -LiteralPath $serverOut, $serverErr -ErrorAction SilentlyContinue
    $server = Start-Process -FilePath $serverExe `
        -ArgumentList @('--listen', "127.0.0.1:$ServerPort") `
        -WorkingDirectory $RepoRoot `
        -RedirectStandardOutput $serverOut `
        -RedirectStandardError $serverErr `
        -PassThru -WindowStyle Hidden
    Write-Host "server   127.0.0.1:$ServerPort (pid $($server.Id), log $serverErr)"
    if (-not (Wait-UdpReady -Port $ServerPort -Process $server)) {
        Show-LogTail -Path $serverErr -Lines 30
        throw "server did not bind 127.0.0.1:$ServerPort"
    }

    if ($Capture) {
        $captureArgs = @('--listen', "127.0.0.1:$CapturePort", '--server', "127.0.0.1:$ServerPort")
        if ($Detail) {
            $captureArgs += '--detail'
        }
        if ($NoAcks) {
            $captureArgs += '--no-acks'
        }
        $relay = Start-Process -FilePath $inspectorExe `
            -ArgumentList $captureArgs `
            -WorkingDirectory $RepoRoot `
            -PassThru -NoNewWindow
        Write-Host "capture  127.0.0.1:$CapturePort -> 127.0.0.1:$ServerPort (pid $($relay.Id))"
        if (-not (Wait-UdpReady -Port $CapturePort -Process $relay)) {
            throw "protocol-inspector did not bind 127.0.0.1:$CapturePort"
        }
    }

    for ($index = 0; $index -lt $Clients; $index++) {
        $slot = $PlayerBase + $index
        if ($Headless) {
            $log = Join-Path $LogDir "client-$slot.log"
            $err = Join-Path $LogDir "client-$slot.err.log"
            Remove-Item -LiteralPath $log, $err -ErrorAction SilentlyContinue
            $seconds = $Duration.ToString([System.Globalization.CultureInfo]::InvariantCulture)
            $process = Start-Process -FilePath $clientExe `
                -ArgumentList @('--headless', '--connect', $clientTarget, '--seconds', $seconds) `
                -WorkingDirectory $RepoRoot `
                -RedirectStandardOutput $log `
                -RedirectStandardError $err `
                -PassThru -WindowStyle Hidden
            $clientRuns += [pscustomobject]@{
                Process  = $process
                Slot     = $slot
                Log      = $log
                Err      = $err
                Headless = $true
            }
            Write-Host "client $slot -> $clientTarget (headless, pid $($process.Id), log $log)"
        } else {
            $process = Start-Process -FilePath $clientExe `
                -ArgumentList @('--connect', $clientTarget) `
                -WorkingDirectory $RepoRoot `
                -PassThru
            $clientRuns += [pscustomobject]@{
                Process  = $process
                Slot     = $slot
                Log      = $null
                Err      = $null
                Headless = $false
            }
            Write-Host "client $slot -> $clientTarget (windowed, pid $($process.Id))"
        }
    }

    Write-Host ""
    Write-Host "running; press Ctrl+C to stop everything." -ForegroundColor DarkGray

    if ($clientRuns.Count -eq 0) {
        Write-Host "no clients launched; connect manually to $clientTarget" -ForegroundColor DarkGray
        while (-not $server.HasExited -and ($null -eq $relay -or -not $relay.HasExited)) {
            Start-Sleep -Milliseconds 250
        }
    } else {
        while ($true) {
            $alive = @($clientRuns | Where-Object { -not $_.Process.HasExited })
            if ($alive.Count -eq 0) {
                break
            }
            if ($server.HasExited) {
                Write-Warning "server exited with code $($server.ExitCode)"
                Show-LogTail -Path $serverErr -Lines 30
                break
            }
            if ($null -ne $relay -and $relay.HasExited) {
                Write-Warning "protocol-inspector exited with code $($relay.ExitCode)"
                break
            }
            Start-Sleep -Milliseconds 250
        }
    }

    # If the server or protocol-inspector died mid-run, stop clients that are still
    # sending so their logs can be collected and the run can finish cleanly.
    foreach ($run in $clientRuns) {
        if (-not $run.Process.HasExited) {
            Stop-Child -Process $run.Process -Label "client $($run.Slot)"
            $run.Process.WaitForExit(2000) | Out-Null
        }
    }

    foreach ($run in $clientRuns) {
        $exitCode = if ($run.Process.HasExited) { $run.Process.ExitCode } else { -1 }
        if (-not $run.Headless) {
            Write-Host "client $($run.Slot) window closed (exit $exitCode)" -ForegroundColor DarkGray
            continue
        }
        Write-Host ""
        Write-Host "=== client $($run.Slot) (exit $exitCode) ===" -ForegroundColor Cyan
        if ($run.Log) {
            $lines = Get-Content -LiteralPath $run.Log -ErrorAction SilentlyContinue
            if ($lines) {
                $lines | ForEach-Object { Write-Host $_ }
            }
        }
        if ($run.Err) {
            $errLines = Get-Content -LiteralPath $run.Err -ErrorAction SilentlyContinue
            if ($errLines) {
                Write-Host "--- stderr ---" -ForegroundColor DarkYellow
                $errLines | ForEach-Object { Write-Host $_ }
            }
        }
        if ($exitCode -ne 0) {
            $failed = $true
            Write-Warning "client $($run.Slot) failed with exit code $exitCode"
        }
    }

    if ($server.HasExited -and $server.ExitCode -ne 0) {
        $failed = $true
        Write-Warning "server exited with code $($server.ExitCode)"
    }
    if ($null -ne $relay -and $relay.HasExited -and $relay.ExitCode -ne 0) {
        $failed = $true
        Write-Warning "protocol-inspector exited with code $($relay.ExitCode)"
    }
} catch {
    Write-Host "ERROR: $($_.Exception.Message)" -ForegroundColor Red
    $failed = $true
} finally {
    foreach ($run in $clientRuns) {
        Stop-Child -Process $run.Process -Label "client $($run.Slot)"
    }
    Stop-Child -Process $relay -Label 'protocol-inspector'
    Stop-Child -Process $server -Label 'server'
}

if ($failed) {
    Write-Host "run failed" -ForegroundColor Red
    exit 1
}
Write-Host "run finished" -ForegroundColor Green
exit 0
