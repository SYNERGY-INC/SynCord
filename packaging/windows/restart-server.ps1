# Restarts SynCord Server (for example after editing its settings). Asks for admin rights.
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`""
    exit
}
$taskName = 'SynCord Server'
$exe = Join-Path $PSScriptRoot 'syncord-server.exe'
$dataDir = Join-Path $env:ProgramData 'SynCord Server'
$config = Join-Path $dataDir 'syncord-server.toml'

Write-Host 'Stopping SynCord Server...'
& schtasks.exe /End /TN $taskName 2>&1 | Out-Null
Get-Process syncord-server -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1

& schtasks.exe /Run /TN $taskName 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    # No start-with-Windows task (setup couldn't create it): just start the program.
    Start-Process -FilePath $exe -ArgumentList "`"$config`"" -WorkingDirectory $dataDir -WindowStyle Hidden
}
Start-Sleep -Seconds 2
try {
    $null = Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 'http://127.0.0.1:7878/'
    Write-Host 'SynCord Server is running.' -ForegroundColor Green
} catch {
    Write-Host "SynCord Server didn't start. Use 'Run server in a window' from the Start menu to see why," -ForegroundColor Yellow
    Write-Host "or look at $dataDir\syncord-server.log"
    Read-Host 'Press Enter to close'
    exit 1
}
Start-Sleep -Seconds 2
