# Installer for wireview-xeneon-edge (WireView Pro II -> Xeneon Edge bridge).
#
# From anywhere (downloads the latest main branch into %LOCALAPPDATA%\wireview-xeneon-edge):
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/main/install.ps1 | iex"
# With options:
#   powershell -ExecutionPolicy Bypass -c "& ([scriptblock]::Create((irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/main/install.ps1))) -ExtraArgs '--port 9000'"
# Pin what gets installed. Fetch the bootstrap from the SAME tag, otherwise main's installer
# runs before anything is verified:
#   powershell -ExecutionPolicy Bypass -c "& ([scriptblock]::Create((irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/v1.0.1/install.ps1))) -Ref v1.0.1 -Sha256 <hash from the release notes>"
# Fully verified: download the release zip, compare its SHA-256 with the release notes, expand
# it, and run install.ps1 from the extracted folder (installs in place; -Ref/-Sha256 unused).
# From a clone (installs in place):
#   powershell -ExecutionPolicy Bypass -File install.ps1 [-ExtraArgs '--port 9000'] [-NoStart]
# Remove:
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#
# What it does: finds Python 3.10+ (installs it with winget if missing), creates a venv,
# installs pyserial, registers a per-user Scheduled Task that runs the bridge
# at logon (no admin rights needed), and starts it. Re-running updates and restarts.
param(
    [string]$ExtraArgs = '',
    [string]$Dir = '',
    [string]$Ref = '',
    [string]$Sha256 = '',
    [switch]$Uninstall,
    [switch]$NoStart
)

$ErrorActionPreference = 'Stop'
$Repo = 'jlobue10/wireview-xeneon-edge'
$Name = 'wireview-xeneon-edge'
$Main = 'bridge/wireview_bridge.py'
$TaskName = 'WireView Bridge'
$LegacyShortcut = Join-Path ([Environment]::GetFolderPath('Startup')) 'WireView Bridge.lnk'

function Say($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

function Stop-Daemon {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue | Stop-ScheduledTask -ErrorAction SilentlyContinue
    Get-CimInstance Win32_Process -Filter "Name LIKE 'python%'" |   # python.exe, pythonw.exe, pythonw3.12.exe (Store)
        Where-Object { $_.CommandLine -like "*$(Join-Path $Dir $Main)*" } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

# --- where to install -------------------------------------------------------
$scriptDir = if ($PSCommandPath) { Split-Path -Parent $PSCommandPath } else { '' }
$inPlace = $scriptDir -and (Test-Path (Join-Path $scriptDir $Main))
if (-not $Dir) { $Dir = if ($inPlace) { $scriptDir } else { Join-Path $env:LOCALAPPDATA $Name } }

if ($Uninstall) {
    Say "Stopping $Name"
    Stop-Daemon
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Say "Removed scheduled task '$TaskName'"
    }
    if (Test-Path $LegacyShortcut) { Remove-Item $LegacyShortcut }
    if (-not $inPlace -and (Test-Path $Dir)) {
        if (Test-Path (Join-Path $Dir $Main)) { Remove-Item -Recurse -Force $Dir; Say "Removed $Dir" }
        else { Say "Left $Dir alone: it does not contain $Main" }
    }
    Say 'Uninstalled.'
    exit 0
}

# --- get the files ----------------------------------------------------------
if ($inPlace -and ($Ref -or $Sha256)) {
    Say "Installing in place from $scriptDir; -Ref/-Sha256 apply only to downloads (verify the archive you extracted instead)."
}
if (-not $inPlace) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    if (-not $Ref) {
        # Default to the latest tagged release. Never fall back to main silently: a lookup
        # failure must not turn a release install into a development-branch install.
        try { $Ref = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest").tag_name }
        catch { throw "Could not look up the latest release of $Repo ($($_.Exception.Message)). Pass -Ref <tag> to choose one, or -Ref main for the development branch." }
        if (-not $Ref) { throw "GitHub returned no release tag for $Repo. Pass -Ref <tag> or -Ref main." }
    }
    Say "Downloading $Repo@$Ref to $Dir"
    # Fresh private staging directory; nothing predictable is reused between runs.
    $stage = Join-Path $env:TEMP ("$Name-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        $zip = Join-Path $stage 'src.zip'
        Invoke-WebRequest -UseBasicParsing "https://github.com/$Repo/archive/$Ref.zip" -OutFile $zip
        $hash = (Get-FileHash $zip -Algorithm SHA256).Hash
        Say "Archive SHA256: $hash"
        if ($Sha256 -and $hash -ne $Sha256.ToUpper()) { throw "SHA256 mismatch: expected $Sha256, got $hash" }
        $tmp = Join-Path $stage 'extract'
        Expand-Archive $zip -DestinationPath $tmp
        $src = Get-ChildItem $tmp | Select-Object -First 1
        New-Item -ItemType Directory -Force $Dir | Out-Null
        # Keep an existing venv; refresh everything else.
        Get-ChildItem $src.FullName | ForEach-Object { Copy-Item -Recurse -Force $_.FullName $Dir }
    } finally {
        Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
    }
}
Set-Location $Dir

# --- python -----------------------------------------------------------------
function Find-Python {
    # Resolve through PATH only; unlike cmd.exe this never picks up a python.exe from the current directory.
    foreach ($cand in @(@('py', '-3'), @('python'), @('python3'))) {
        $cmd = Get-Command $cand[0] -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $cmd) { continue }
        $exe = $cmd.Source
        $extra = @($cand | Select-Object -Skip 1)
        try {
            $v = & $exe @extra -c 'import sys;print(sys.version_info[0]*100+sys.version_info[1])' 2>$null
            if ($LASTEXITCODE -eq 0 -and [int]$v -ge 310) { return @{ Exe = $exe; Args = $extra } }
        } catch {}
    }
    return $null
}
$py = Find-Python
if (-not $py) {
    Say 'Python 3.10+ not found; installing with winget'
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        throw 'winget is not available. Install Python 3.10+ from https://www.python.org/downloads/ (tick "Add to PATH") and run this again.'
    }
    winget install -e --id Python.Python.3.12 --accept-package-agreements --accept-source-agreements --silent
    $env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')
    $py = Find-Python
    if (-not $py) { throw 'Python was installed but is not on PATH yet. Open a new terminal and run this again.' }
}
Say "Using Python: $($py.Exe) $($py.Args)"

# --- venv + packages ---------------------------------------------------------
$venvPy = Join-Path $Dir 'venv\Scripts\python.exe'
$pythonw = Join-Path $Dir 'venv\Scripts\pythonw.exe'
if (-not (Test-Path $venvPy)) {
    Say 'Creating virtual environment'
    & $py.Exe @($py.Args) -m venv "$Dir\venv"
    if ($LASTEXITCODE -ne 0) { throw 'venv creation failed' }
}
Say 'Installing packages (pyserial)'
$pipExtra = if ($env:WIREVIEW_PIP_ARGS) { $env:WIREVIEW_PIP_ARGS -split ' ' } else { @() }
& $venvPy -m pip install --disable-pip-version-check -q @pipExtra -r (Join-Path $Dir 'bridge/requirements.txt')
if ($LASTEXITCODE -ne 0) { throw 'pip install failed' }

# --- run at logon (per-user scheduled task) ----------------------------------
$daemonArgs = '"' + (Join-Path $Dir $Main) + '"'
if ($ExtraArgs) { $daemonArgs += ' ' + $ExtraArgs }

Say "Registering scheduled task '$TaskName' (runs at logon)"
Stop-Daemon
if (Test-Path $LegacyShortcut) { Remove-Item $LegacyShortcut }   # older installs used a Startup shortcut
$action = New-ScheduledTaskAction -Execute $pythonw -Argument $daemonArgs -WorkingDirectory $Dir
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
$settings.ExecutionTimeLimit = 'PT0S'   # no 3-day cap; it is a daemon
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Description 'Serves Thermal Grizzly WireView Pro II readings to the Xeneon Edge widgets' -Force | Out-Null

if (-not $NoStart) {
    Say 'Starting it now'
    Start-ScheduledTask -TaskName $TaskName
}

Write-Host ''
Say 'Done.'
Write-Host "  Installed in : $Dir"
Write-Host "  Runs         : $pythonw $daemonArgs"
Write-Host '  Bridge       : http://localhost:8765/api/wireview'
Write-Host '  Readings     : straight from the WireView over USB. Close the Thermal Grizzly WireView app'
Write-Host '                 (and disable its auto-start) so the COM port is free. No HWiNFO needed.'
Write-Host '  iCUE         : select the Xeneon Edge, add an iFrame widget, paste one of'
Write-Host '                   http://localhost:8765/per-wire/'
Write-Host '                   http://localhost:8765/total-current/'
Write-Host '                   http://localhost:8765/total-power/'
Write-Host "  Manage       : Task Scheduler > '$TaskName'   |   uninstall: install.ps1 -Uninstall"
