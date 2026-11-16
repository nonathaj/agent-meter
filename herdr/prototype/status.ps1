$accounts = agent-meter list --json | ConvertFrom-Json
$parts = foreach ($a in $accounts | Where-Object active) {
  $name = if ($a.provider -eq 'claude') { 'Claude' } else { 'Codex' }
  "$name $($a.id) $([math]::Round($a.usage.usedPercent))%"
}
($parts -join '  ')
