# Stops SynCord Server and removes its startup task and firewall rule. Data is kept.
$task = 'SynCord Server'
Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
Get-NetFirewallRule -DisplayName $task -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Get-Process syncord-server -ErrorAction SilentlyContinue | Stop-Process -Force
