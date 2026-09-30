# Installer for wireview-xeneon-edge (WireView Pro II -> Xeneon Edge bridge).
#
# From anywhere (downloads the latest release into %LOCALAPPDATA%\wireview-xeneon-edge):
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/main/install.ps1 | iex"
# With options:
#   powershell -ExecutionPolicy Bypass -c "& ([scriptblock]::Create((irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/main/install.ps1))) -ExtraArgs '--port 9000'"
# Pin what gets installed. Fetch the bootstrap from the SAME tag, otherwise main's installer
# runs before anything is verified:
#   powershell -ExecutionPolicy Bypass -c "& ([scriptblock]::Create((irm https://raw.githubusercontent.com/jlobue10/wireview-xeneon-edge/v2.0.0/install.ps1))) -Ref v2.0.0 -Sha256 <hash from the release notes>"
# Fully verified: download wireview-bridge.exe and install.ps1 from the release, compare the
# executable's SHA-256 with the release notes, and run install.ps1 from that folder (installs the
# executable next to it; -Ref/-Sha256 unused).
# Remove:
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#
# What it does: downloads wireview-bridge.exe (one self-contained file, nothing else to install),
# checks its SHA-256, registers a per-user Scheduled Task that runs the bridge at logon (no admin
# rights needed), and starts it. Re-running updates and restarts. An older Python-based install in
# the same folder is replaced.
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
$Exe = 'wireview-bridge.exe'
$TaskName = 'WireView Bridge'
$Description = 'Serves Thermal Grizzly WireView Pro II readings to the Xeneon Edge widgets'
$BaseArgs = ''
# What the Python releases (1.x) put in the install folder.
$LegacyMain = 'bridge/wireview_bridge.py'
$LegacyItems = @('venv', 'bridge', 'docs', 'tests', '.github', '.gitignore', 'LICENSE', 'README.md', 'install.ps1')
$startup = [Environment]::GetFolderPath('Startup')
$LegacyShortcut = if ($startup) { Join-Path $startup 'WireView Bridge.lnk' } else { '' }   # older installs used a Startup shortcut

function Say($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }

function Stop-Daemon {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue | Stop-ScheduledTask -ErrorAction SilentlyContinue
    $target = Join-Path $Dir $Exe
    Get-CimInstance Win32_Process -Filter "Name = '$Exe'" |
        Where-Object { $_.ExecutablePath -and ($_.ExecutablePath -ieq $target) } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    # A 1.x install runs under python.exe, pythonw.exe or pythonw3.12.exe (Store).
    Get-CimInstance Win32_Process -Filter "Name LIKE 'python%'" |
        Where-Object { $_.CommandLine -like "*$(Join-Path $Dir $LegacyMain)*" } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

function Remove-Legacy {
    # Only a folder that really holds the Python release is cleaned, and only the names it shipped.
    if (-not (Test-Path (Join-Path $Dir $LegacyMain))) { return }
    Say 'Removing the files of the Python-based 1.x install'
    foreach ($item in $LegacyItems) {
        $p = Join-Path $Dir $item
        if (Test-Path $p) { Remove-Item -Recurse -Force $p -ErrorAction SilentlyContinue }
    }
}

# --- where to install -------------------------------------------------------
$scriptDir = if ($PSCommandPath) { Split-Path -Parent $PSCommandPath } else { '' }
$inPlace = $scriptDir -and (Test-Path (Join-Path $scriptDir $Exe))
if (-not $Dir) { $Dir = if ($inPlace) { $scriptDir } else { Join-Path $env:LOCALAPPDATA $Name } }

if ($Uninstall) {
    Say "Stopping $Name"
    Stop-Daemon
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Say "Removed scheduled task '$TaskName'"
    }
    if ($LegacyShortcut -and (Test-Path $LegacyShortcut)) { Remove-Item $LegacyShortcut }
    if (-not $inPlace -and (Test-Path $Dir)) {
        Remove-Legacy
        $target = Join-Path $Dir $Exe
        if (Test-Path $target) { Remove-Item -Force $target; Say "Removed $target" }
        if (-not (Get-ChildItem -Force $Dir)) { Remove-Item -Force $Dir; Say "Removed $Dir" }
        else { Say "Left $Dir alone: it holds files this installer did not put there" }
    }
    Say 'Uninstalled.'
    exit 0
}

# --- get the executable -----------------------------------------------------
if ($inPlace -and ($Ref -or $Sha256)) {
    Say "Installing $Exe from $scriptDir; -Ref/-Sha256 apply only to downloads (verify the file you downloaded instead)."
}
if (-not $inPlace) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    if (-not $Ref) {
        # Default to the latest tagged release, and stop if it cannot be found: a lookup failure
        # must not turn into installing something else.
        try { $Ref = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest").tag_name }
        catch { throw "Could not look up the latest release of $Repo ($($_.Exception.Message)). Pass -Ref <tag> to choose one." }
        if (-not $Ref) { throw "GitHub returned no release tag for $Repo. Pass -Ref <tag>." }
    }
    if ($Ref -notmatch '^[A-Za-z0-9._-]+$') { throw "-Ref '$Ref' is not a release tag." }
    Say "Downloading $Repo $Ref to $Dir"
    # Fresh private staging directory; nothing predictable is reused between runs.
    $stage = Join-Path $env:TEMP ("$Name-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        $base = "https://github.com/$Repo/releases/download/$Ref"
        $download = Join-Path $stage $Exe
        try { Invoke-WebRequest -UseBasicParsing "$base/$Exe" -OutFile $download }
        catch { throw "Release $Ref of $Repo has no $Exe ($($_.Exception.Message)). Releases before 2.0.0 were Python; use their own install.ps1." }
        $hash = (Get-FileHash $download -Algorithm SHA256).Hash
        Say "$Exe SHA256: $hash"
        if ($Sha256) {
            if ($hash -ne $Sha256.Trim().ToUpper()) { throw "SHA256 mismatch: expected $Sha256, got $hash" }
        } else {
            # Without -Sha256 this only catches a damaged download: the list comes from the same
            # release as the file. Pass -Sha256 from the release notes to check what you reviewed.
            $sums = Join-Path $stage 'SHA256SUMS'
            Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS" -OutFile $sums
            $line = Get-Content $sums | Where-Object { $_ -match "^([0-9A-Fa-f]{64})\s+\*?$([regex]::Escape($Exe))\s*$" } | Select-Object -First 1
            if (-not $line) { throw "SHA256SUMS of release $Ref does not list $Exe" }
            $expected = ($line -split '\s+')[0].ToUpper()
            if ($hash -ne $expected) { throw "SHA256 mismatch: the release lists $expected, the download is $hash" }
        }
        Stop-Daemon
        New-Item -ItemType Directory -Force $Dir | Out-Null
        Remove-Legacy
        Copy-Item -Force $download (Join-Path $Dir $Exe)
        # Keep that release's installer next to the executable, so `install.ps1 -Uninstall`
        # works from the install folder later. Not security-relevant, so a failure is only noted.
        $script = Join-Path $stage 'install.ps1'
        try {
            Invoke-WebRequest -UseBasicParsing "$base/install.ps1" -OutFile $script
            Copy-Item -Force $script (Join-Path $Dir 'install.ps1')
        } catch { Say "Could not fetch install.ps1 from release $Ref; to uninstall later, run this installer again with -Uninstall." }
    } finally {
        Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
    }
}
$exePath = Join-Path $Dir $Exe
if (-not (Test-Path $exePath)) { throw "$exePath is missing" }
Unblock-File $exePath -ErrorAction SilentlyContinue
$version = (& $exePath --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { throw "$exePath does not run (exit code $LASTEXITCODE)" }
Say "Installed $version"

# --- run at logon (per-user scheduled task) ----------------------------------
$daemonArgs = (@($BaseArgs, $ExtraArgs) | Where-Object { $_ }) -join ' '

Say "Registering scheduled task '$TaskName' (runs at logon)"
Stop-Daemon
if ($LegacyShortcut -and (Test-Path $LegacyShortcut)) { Remove-Item $LegacyShortcut }
$actionArgs = @{ Execute = $exePath; WorkingDirectory = $Dir }
if ($daemonArgs) { $actionArgs.Argument = $daemonArgs }
$action = New-ScheduledTaskAction @actionArgs
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
$settings.ExecutionTimeLimit = 'PT0S'   # no 3-day cap; it is a daemon
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Description $Description -Force | Out-Null

if (-not $NoStart) {
    Say 'Starting it now'
    Start-ScheduledTask -TaskName $TaskName
}

Write-Host ''
Say 'Done.'
Write-Host "  Installed in : $Dir"
Write-Host "  Runs         : $exePath $daemonArgs"
Write-Host '  Bridge       : http://localhost:8765/api/wireview'
Write-Host '  Readings     : straight from the WireView over USB. Close the Thermal Grizzly WireView app'
Write-Host '                 (and disable its auto-start) so the COM port is free. No HWiNFO needed.'
Write-Host '  iCUE         : select the Xeneon Edge, add an iFrame widget, paste one of'
Write-Host '                   http://localhost:8765/per-wire/'
Write-Host '                   http://localhost:8765/total-current/'
Write-Host '                   http://localhost:8765/total-power/'
Write-Host "  Manage       : Task Scheduler > '$TaskName'   |   uninstall: install.ps1 -Uninstall"
