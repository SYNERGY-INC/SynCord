# Restarts SynCord Server (for example after editing its settings). Asks for admin rights.
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`""
    exit
}
$task = 'SynCord Server'
Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
Get-Process syncord-server -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
Start-ScheduledTask -TaskName $task
Write-Host 'SynCord Server restarted.'
Start-Sleep -Seconds 2
