# Sets up SynCord Server: firewall rule, start-with-Windows task, and starts it.
# Run by the installer as administrator. Each step is independent and has a
# fallback, so one problem (for example another antivirus managing the
# firewall) doesn't stop the rest. Everything is logged to setup.log.
#
# Exit code is a sum of problems (0 = all good):
#   2 = firewall rule couldn't be added
#   4 = couldn't register the start-with-Windows task (server started directly instead)
#   8 = the server isn't answering
param(
    [Parameter(Mandatory)] [string] $Exe,
    [Parameter(Mandatory)] [string] $DataDir
)
$ErrorActionPreference = 'Continue'
$taskName = 'SynCord Server'
$config = Join-Path $DataDir 'syncord-server.toml'
$problems = 0

New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
$log = Join-Path $DataDir 'setup.log'
function Log([string] $msg) {
    $line = "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')  $msg"
    Write-Output $line
    Add-Content -Path $log -Value $line -ErrorAction SilentlyContinue
}
Log "---- Setting up SynCord Server ($Exe) on $([Environment]::OSVersion.VersionString), PowerShell $($PSVersionTable.PSVersion)"

# 1. Let normal users edit the settings file (it holds the invite code and server name).
& icacls.exe $DataDir /grant '*S-1-5-32-545:(OI)(CI)M' /T /Q 2>&1 | Out-Null
if ($LASTEXITCODE -eq 0) { Log 'Data folder ready' } else { Log "WARN: couldn't change folder permissions (icacls exit $LASTEXITCODE)" }

# 2. Firewall: allow the server program in (TCP for chat, UDP for voice).
$firewall = $false
try {
    Get-NetFirewallRule -DisplayName $taskName -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
    New-NetFirewallRule -DisplayName $taskName -Direction Inbound -Program $Exe -Action Allow -Profile Any -ErrorAction Stop | Out-Null
    $firewall = $true
} catch {
    Log "New-NetFirewallRule failed: $($_.Exception.Message)"
}
if (-not $firewall) {
    & netsh.exe advfirewall firewall delete rule name="$taskName" 2>&1 | Out-Null
    $out = & netsh.exe advfirewall firewall add rule name="$taskName" dir=in action=allow program="$Exe" enable=yes profile=any 2>&1
    $firewall = ($LASTEXITCODE -eq 0)
    if (-not $firewall) { Log "netsh failed: $out" }
}
if ($firewall) { Log 'Firewall rule added' } else { Log 'WARN: no firewall rule (another firewall or antivirus may be in charge)'; $problems += 2 }

# 3. Start with Windows, as the built-in SYSTEM account (by SID, so it works in every
#    Windows language), never time out, keep running on battery, restart after a crash.
function Esc([string] $s) { [Security.SecurityElement]::Escape($s) }
$xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>SynCord chat server</Description></RegistrationInfo>
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>S-1-5-18</UserId><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>$(Esc $Exe)</Command>
      <Arguments>"$(Esc $config)"</Arguments>
      <WorkingDirectory>$(Esc $DataDir)</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"@
$task = $false
try {
    Register-ScheduledTask -TaskName $taskName -Xml $xml -Force -ErrorAction Stop | Out-Null
    $task = $true
} catch {
    Log "Register-ScheduledTask failed: $($_.Exception.Message)"
}
if (-not $task) {
    $xmlFile = Join-Path $env:TEMP 'syncord-server-task.xml'
    $xml | Out-File -FilePath $xmlFile -Encoding Unicode
    $out = & schtasks.exe /Create /TN $taskName /XML $xmlFile /F 2>&1
    $task = ($LASTEXITCODE -eq 0)
    if (-not $task) { Log "schtasks failed: $out" }
    Remove-Item $xmlFile -ErrorAction SilentlyContinue
}

# 4. Start it now.
if ($task) {
    Log 'Start-with-Windows task registered'
    & schtasks.exe /Run /TN $taskName 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        try { Start-ScheduledTask -TaskName $taskName -ErrorAction Stop } catch { Log "Couldn't start the task: $($_.Exception.Message)" }
    }
} else {
    Log 'WARN: could not register the start-with-Windows task; starting the server directly instead'
    $problems += 4
    Start-Process -FilePath $Exe -ArgumentList "`"$config`"" -WorkingDirectory $DataDir -WindowStyle Hidden
}

# 5. Check it's answering.
$port = 7878
for ($i = 0; $i -lt 20 -and -not (Test-Path $config); $i++) { Start-Sleep -Milliseconds 500 }
if ((Test-Path $config) -and ((Get-Content $config -Raw) -match 'bind\s*=\s*"[^"]*:(\d+)"')) { $port = [int]$Matches[1] }
$running = $false
for ($i = 0; $i -lt 20 -and -not $running; $i++) {
    try {
        $reply = Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$port/"
        $running = $reply.Content -match 'SynCord'
    } catch { Start-Sleep -Milliseconds 500 }
}
if ($running) {
    Log "Server is running on port $port"
} else {
    $problems += 8
    Log "WARN: the server isn't answering on port $port"
    $serverLog = Join-Path $DataDir 'syncord-server.log'
    if (Test-Path $serverLog) { Get-Content $serverLog -Tail 5 | ForEach-Object { Log "  server log: $_" } }
}
Log "Done (problems: $problems)"
exit $problems
