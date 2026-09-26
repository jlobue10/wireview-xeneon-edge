# Registers the WireView bridge to start (hidden) at login for the current user.
# Run once from this folder:  powershell -ExecutionPolicy Bypass -File install-startup.ps1
# Remove with:                 powershell -ExecutionPolicy Bypass -File install-startup.ps1 -Uninstall
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$script = Join-Path $here 'wireview_bridge.py'
$startup = [Environment]::GetFolderPath('Startup')
$lnk = Join-Path $startup 'WireView Bridge.lnk'

if ($Uninstall) {
    if (Test-Path $lnk) { Remove-Item $lnk; Write-Host "Removed $lnk" } else { Write-Host 'Not installed.' }
    exit 0
}

$pythonw = (Get-Command pythonw.exe -ErrorAction SilentlyContinue).Source
if (-not $pythonw) {
    $python = (Get-Command python.exe -ErrorAction Stop).Source
    $pythonw = Join-Path (Split-Path $python) 'pythonw.exe'
}
if (-not (Test-Path $pythonw)) { throw "pythonw.exe not found; install Python 3.10+ from python.org" }

$ws = New-Object -ComObject WScript.Shell
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = $pythonw
$s.Arguments = '"' + $script + '"'
$s.WorkingDirectory = $here
$s.WindowStyle = 7
$s.Description = 'Serves WireView Pro II readings to the Xeneon Edge widgets'
$s.Save()
Write-Host "Installed: $lnk"
Write-Host "Starting it now..."
Start-Process -FilePath $pythonw -ArgumentList ('"' + $script + '"') -WorkingDirectory $here -WindowStyle Hidden
