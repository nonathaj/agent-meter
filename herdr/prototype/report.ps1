$ErrorActionPreference = 'Stop'
$herdr = if ($env:HERDR_BIN_PATH) { $env:HERDR_BIN_PATH } else { 'herdr' }
$accounts = agent-meter list --json | ConvertFrom-Json
$threshold = 90
function Span([TimeSpan]$t) {
  if ($t.TotalDays -ge 1) { return "{0}d{1}h" -f [int][math]::Floor($t.TotalDays), $t.Hours }
  if ($t.TotalHours -ge 1) { return "{0}h{1:00}m" -f [int][math]::Floor($t.TotalHours), $t.Minutes }
  return "{0}m" -f [int][math]::Max(1, $t.Minutes)
}
$active = @{}
foreach ($a in $accounts) { if ($a.active) { $active[$a.provider] = $a } }
$panes = (& $herdr pane list | ConvertFrom-Json).result.panes
foreach ($p in $panes) {
  $kind = $p.agent
  if ($kind -ne 'claude' -and $kind -ne 'codex') { continue }
  $a = $active[$kind]
  if (-not $a -or -not $a.usage) { continue }
  $used = [math]::Round($a.usage.usedPercent)
  $top = $a.usage.windows | Where-Object counted | Sort-Object usedPercent -Descending | Select-Object -First 1
  $text = "$used%"
  if ($top.resetsAt) { $text += " · resets " + (Span (([datetime]$top.resetsAt).ToUniversalTime() - [datetime]::UtcNow)) }
  $level = if ($used -ge $threshold) { 'hot' } elseif ($used -ge $threshold - 20) { 'warn' } else { 'ok' }
  $cmd = @('pane','report-metadata',$p.pane_id,'--source','agent-meter','--token',"meter_account=$($a.id)")
  foreach ($l in 'ok','warn','hot') {
    if ($l -eq $level) { $cmd += @('--token', "meter_$l=$text") } else { $cmd += @('--clear-token', "meter_$l") }
  }
  $cmd += @('--ttl-ms','900000')
  & $herdr @cmd | Out-Null
}
