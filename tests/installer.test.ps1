# Isolated installer regression checks. Administrative APIs and downloads are
# mocked; only --version is executed, using the binary already built by Cargo.
param([switch]$Child, [string]$Installer, [string]$Directory, [string]$Binary, [switch]$Download, [switch]$ExplicitDir, [switch]$WithLog)
$ErrorActionPreference = 'Stop'
if ($Child) {
    function Get-ScheduledTask { return $null }
    function Get-CimInstance { return @() }
    function Stop-ScheduledTask {}
    function Stop-Process {}
    function Unregister-ScheduledTask {}
    function New-ScheduledTaskAction { Set-Content -LiteralPath (Join-Path $Directory 'task-arguments.txt') -Value ($args -join ' ') }
    function New-ScheduledTaskTrigger {}
    function New-ScheduledTaskPrincipal {}
    function New-ScheduledTaskSettingsSet { return [pscustomobject]@{ ExecutionTimeLimit = '' } }
    function Register-ScheduledTask {}
    function Unblock-File {}
    function Invoke-WebRequest {
        param([switch]$UseBasicParsing, [Parameter(Position=0)][string]$Uri, [string]$OutFile)
        if ($Uri.EndsWith('/install.ps1')) { Copy-Item -LiteralPath $Installer -Destination $OutFile }
        else { Copy-Item -LiteralPath $Binary -Destination $OutFile }
    }
    if ($Download) {
        $hash = (Get-FileHash -LiteralPath $Binary -Algorithm SHA256).Hash
        if ($WithLog) { & $Installer -Dir $Directory -Ref v2.2.0 -Sha256 $hash -NoStart -Log }
        else { & $Installer -Dir $Directory -Ref v2.2.0 -Sha256 $hash -NoStart }
    } elseif ($ExplicitDir) { & $Installer -Uninstall -Dir $Directory }
    else { & $Installer -Uninstall }
    exit 0
}

$repoRoot = Split-Path -Parent $PSScriptRoot
# The installer under test names the release repository and executable; read them from it so
# the checks do not depend on what the checkout directory happens to be called.
$installerText = Get-Content -LiteralPath (Join-Path $repoRoot 'install.ps1') -Raw
$repo = [regex]::Match($installerText, "(?m)^\`$Repo = '([^']+)'").Groups[1].Value
$name = [regex]::Match($installerText, "(?m)^\`$Name = '([^']+)'").Groups[1].Value
$exe = [regex]::Match($installerText, "(?m)^\`$Exe = '([^']+)'").Groups[1].Value
if (-not $repo -or -not $name -or -not $exe) { throw 'Could not read $Repo / $Name / $Exe from install.ps1.' }
$binaryName = if ($IsWindows) { $exe } else { $exe.Replace('.exe', '') }
$binaryPath = Join-Path $repoRoot "target/debug/$binaryName"
if (-not (Test-Path -LiteralPath $binaryPath)) { throw 'Run cargo test first to build the CLI binary.' }
$engine = (Get-Process -Id $PID).Path
$root = Join-Path ([IO.Path]::GetTempPath()) ('wireview-installer-test-' + [guid]::NewGuid().ToString('N'))
$oldLocal = $env:LOCALAPPDATA
$oldTemp = $env:TEMP
New-Item -ItemType Directory -Path $root | Out-Null
$env:LOCALAPPDATA = Join-Path $root 'local'
$env:TEMP = $root

function Fixture([string]$path, [string]$markerRepo = '') {
    New-Item -ItemType Directory -Path $path -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $repoRoot 'install.ps1') -Destination (Join-Path $path 'install.ps1')
    Set-Content -LiteralPath (Join-Path $path $exe) -Value 'Fixture only; never executed.'
    Set-Content -LiteralPath (Join-Path $path 'user-notes.txt') -Value 'Keep me.'
    if ($markerRepo) {
        @{ schema = 1; repo = $markerRepo; exe = $exe } | ConvertTo-Json -Compress |
            Set-Content -LiteralPath (Join-Path $path '.wireview-install.json')
    }
}
function Run-Uninstall([string]$path, [switch]$Explicit) {
    $params = @('-NoProfile', '-File', $PSCommandPath, '-Child', '-Installer', (Join-Path $path 'install.ps1'), '-Directory', $path)
    if ($Explicit) { $params += '-ExplicitDir' }
    & $engine @params
    if ($LASTEXITCODE -ne 0) { throw "Uninstall failed for $path" }
}
function Check([bool]$condition, [string]$message) { if (-not $condition) { throw $message } }
try {
    $managed = Join-Path $root 'downloaded-custom'
    & $engine -NoProfile -File $PSCommandPath -Child -Download -Installer (Join-Path $repoRoot 'install.ps1') -Directory $managed -Binary $binaryPath
    Check ($LASTEXITCODE -eq 0) 'Mocked download install failed'
    $marker = Get-Content -LiteralPath (Join-Path $managed '.wireview-install.json') -Raw | ConvertFrom-Json
    Check ($marker.repo -eq $repo -and $marker.exe -eq $exe -and $marker.schema -eq 1) 'Install marker missing or incorrect'
    $taskArgs = Get-Content -LiteralPath (Join-Path $managed 'task-arguments.txt') -Raw
    Check ($taskArgs -notmatch '--csv-log') 'CSV logging must be off unless -Log is given'

    # -Log adds the CSV log, in logs\ under the install folder unless -LogDir says otherwise.
    $logged = Join-Path $root 'downloaded-logged'
    & $engine -NoProfile -File $PSCommandPath -Child -Download -WithLog -Installer (Join-Path $repoRoot 'install.ps1') -Directory $logged -Binary $binaryPath
    Check ($LASTEXITCODE -eq 0) 'Mocked download install with -Log failed'
    $taskArgs = Get-Content -LiteralPath (Join-Path $logged 'task-arguments.txt') -Raw
    $expectedLogDir = Join-Path $logged 'logs'
    Check ($taskArgs -like "*--csv-log `"$expectedLogDir`"*") "-Log did not pass --csv-log for $expectedLogDir (got: $taskArgs)"

    # A copied installer has a binary beside it. Selecting a different
    # destination must still download and verify, even if that destination
    # already holds an older executable.
    foreach ($existing in @($false, $true)) {
        $elsewhere = Join-Path $root "explicit-destination-$existing"
        if ($existing) {
            New-Item -ItemType Directory -Path $elsewhere | Out-Null
            Set-Content -LiteralPath (Join-Path $elsewhere $exe) -Value 'Old fixture only; must be replaced before execution.'
        }
        & $engine -NoProfile -File $PSCommandPath -Child -Download -Installer (Join-Path $managed 'install.ps1') -Directory $elsewhere -Binary $binaryPath
        Check ($LASTEXITCODE -eq 0) 'An adjacent binary suppressed the explicit-destination download'
        Check ((Get-FileHash -LiteralPath (Join-Path $elsewhere $exe)).Hash -eq (Get-FileHash -LiteralPath $binaryPath).Hash) 'Explicit destination did not receive the verified binary'
        Check (Test-Path -LiteralPath (Join-Path $elsewhere '.wireview-install.json')) 'Explicit destination was not marked as managed'
    }
    Set-Content -LiteralPath (Join-Path $managed 'user-notes.txt') -Value 'Keep me.'
    Run-Uninstall $managed
    Check (-not (Test-Path -LiteralPath (Join-Path $managed $exe))) 'Copied installer left the managed executable'
    Check (-not (Test-Path -LiteralPath (Join-Path $managed 'install.ps1'))) 'Managed installer was not removed'
    Check (-not (Test-Path -LiteralPath (Join-Path $managed '.wireview-install.json'))) 'Managed marker was not removed'
    Check (Test-Path -LiteralPath (Join-Path $managed 'user-notes.txt')) 'Uninstall removed user files'

    $legacy = Join-Path $env:LOCALAPPDATA $name
    Fixture $legacy
    Run-Uninstall $legacy
    Check (-not (Test-Path -LiteralPath (Join-Path $legacy $exe))) 'Older default install left its executable'
    Check (Test-Path -LiteralPath (Join-Path $legacy 'user-notes.txt')) 'Older install removed user files'

    $source = Join-Path $root 'source'
    Fixture $source
    Run-Uninstall $source
    Check (Test-Path -LiteralPath (Join-Path $source $exe)) 'In-place executable should be preserved'
    Check (Test-Path -LiteralPath (Join-Path $source 'install.ps1')) 'In-place installer should be preserved'

    $custom = Join-Path $root 'older-custom'
    Fixture $custom
    Run-Uninstall $custom -Explicit
    Check (-not (Test-Path -LiteralPath (Join-Path $custom $exe))) 'Explicit custom uninstall left its executable'
    Check (Test-Path -LiteralPath (Join-Path $custom 'user-notes.txt')) 'Explicit custom uninstall removed user files'

    $foreign = Join-Path $root 'foreign-marker'
    Fixture $foreign 'someone/another-repo'
    Run-Uninstall $foreign
    Check (Test-Path -LiteralPath (Join-Path $foreign $exe)) 'Foreign marker should not authorise deletion'
    Write-Host 'PASS: managed download, -Log, explicit destinations with adjacent binaries, copied installer, older default/custom install, source files and user files.'
} finally {
    $env:LOCALAPPDATA = $oldLocal
    $env:TEMP = $oldTemp
    Remove-Item -LiteralPath $root -Recurse -Force
}
