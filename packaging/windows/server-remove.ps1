# Stops SynCord Server and removes its startup task and firewall rule. Data is kept.
$taskName = 'SynCord Server'
& schtasks.exe /End /TN $taskName 2>&1 | Out-Null
& schtasks.exe /Delete /TN $taskName /F 2>&1 | Out-Null
Get-NetFirewallRule -DisplayName $taskName -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
& netsh.exe advfirewall firewall delete rule name="$taskName" 2>&1 | Out-Null
Get-Process syncord-server -ErrorAction SilentlyContinue | Stop-Process -Force
exit 0
