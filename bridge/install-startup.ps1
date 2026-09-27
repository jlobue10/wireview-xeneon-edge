# Registers the WireView bridge to start (hidden) at login for the current user.
# Run once from this folder:  powershell -ExecutionPolicy Bypass -File install-startup.ps1 [-ExtraArgs "--port 9000"]
# Remove with:                 powershell -ExecutionPolicy Bypass -File install-startup.ps1 -Uninstall
# (..\install.ps1 calls this for you.)
param(
    [switch]$Uninstall,
    [switch]$NoStart,
    [string]$ExtraArgs = ''
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$script = Join-Path $here 'wireview_bridge.py'
$startup = [Environment]::GetFolderPath('Startup')
$lnk = Join-Path $startup 'WireView Bridge.lnk'

function Stop-Daemon {
    Get-CimInstance Win32_Process -Filter "Name LIKE 'python%'" |   # python.exe, pythonw.exe, pythonw3.12.exe (Store)
        Where-Object { $_.CommandLine -like '*wireview_bridge.py*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

if ($Uninstall) {
    Stop-Daemon
    if (Test-Path $lnk) { Remove-Item $lnk; Write-Host "Removed $lnk" } else { Write-Host 'Not installed.' }
    exit 0
}

# Prefer a venv in the repository root (python -m venv venv; venv\Scripts\pip install -r bridge\requirements.txt)
$pythonw = Join-Path (Split-Path -Parent $here) 'venv\Scripts\pythonw.exe'
if (-not (Test-Path $pythonw)) {
    $pythonw = (Get-Command pythonw.exe -ErrorAction SilentlyContinue).Source
    if (-not $pythonw) { $pythonw = Join-Path (Split-Path (Get-Command python.exe -ErrorAction Stop).Source) 'pythonw.exe' }
}
if (-not (Test-Path $pythonw)) { throw "pythonw.exe not found; install Python 3.10+ from python.org" }

$args = '"' + $script + '"'
if ($ExtraArgs) { $args += ' ' + $ExtraArgs }

$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = $pythonw
$s.Arguments = $args
$s.WorkingDirectory = $here
$s.WindowStyle = 7
$s.Description = 'Serves WireView Pro II readings to the Xeneon Edge widgets'
$s.Save()
Write-Host "Installed: $lnk  ($pythonw $args)"
if ($NoStart) { exit 0 }
Stop-Daemon   # replace a running copy so new options take effect
Write-Host "Starting it now..."
Start-Process -FilePath $pythonw -ArgumentList $args -WorkingDirectory $here -WindowStyle Hidden
