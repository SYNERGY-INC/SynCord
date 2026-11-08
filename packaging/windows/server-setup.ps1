# Registers SynCord Server to start with Windows and opens the firewall for it.
# Run by the installer (as administrator).
param(
    [Parameter(Mandatory)] [string] $Exe,
    [Parameter(Mandatory)] [string] $DataDir
)
$ErrorActionPreference = 'Stop'
$task = 'SynCord Server'
$config = Join-Path $DataDir 'syncord-server.toml'

New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
# Let normal users edit the settings file (it holds the invite code and server name).
icacls $DataDir /grant '*S-1-5-32-545:(OI)(CI)M' /T /Q | Out-Null

# Firewall: allow the server program in, whatever port it's set to (TCP for chat, UDP for voice).
Get-NetFirewallRule -DisplayName $task -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName $task -Direction Inbound -Program $Exe -Action Allow -Profile Any | Out-Null

# Start at boot as SYSTEM, never time out, restart if it crashes, run on battery too.
$action = New-ScheduledTaskAction -Execute $Exe -Argument "`"$config`"" -WorkingDirectory $DataDir
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName $task -Action $action -Trigger $trigger -Principal $principal -Settings $settings `
    -Description 'SynCord chat server' -Force | Out-Null

Start-ScheduledTask -TaskName $task
# Wait for the first start to write the settings file (with a fresh invite code).
for ($i = 0; $i -lt 20 -and -not (Test-Path $config); $i++) { Start-Sleep -Milliseconds 500 }
