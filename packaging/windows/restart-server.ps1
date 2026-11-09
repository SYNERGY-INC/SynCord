# Restarts Celerity Server (for example after editing its settings). Asks for admin rights.
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`""
    exit
}
$taskName = 'Celerity Server'
$exe = Join-Path $PSScriptRoot 'celerity-server.exe'
$dataDir = Join-Path $env:ProgramData 'Celerity Server'
$config = Join-Path $dataDir 'celerity-server.toml'

Write-Host 'Stopping Celerity Server...'
& schtasks.exe /End /TN $taskName 2>&1 | Out-Null
Get-Process celerity-server -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1

& schtasks.exe /Run /TN $taskName 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    # No start-with-Windows task (setup couldn't create it): just start the program.
    Start-Process -FilePath $exe -ArgumentList "`"$config`"" -WorkingDirectory $dataDir -WindowStyle Hidden
}
Start-Sleep -Seconds 2
try {
    $null = Invoke-WebRequest -UseBasicParsing -TimeoutSec 3 'http://127.0.0.1:7878/'
    Write-Host 'Celerity Server is running.' -ForegroundColor Green
} catch {
    Write-Host "Celerity Server didn't start. Use 'Run server in a window' from the Start menu to see why," -ForegroundColor Yellow
    Write-Host "or look at $dataDir\celerity-server.log"
    Read-Host 'Press Enter to close'
    exit 1
}
Start-Sleep -Seconds 2
