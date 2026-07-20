$f = 'probe-rs-tools/src/bin/probe-rs/cmd/dap_server/backend/rpc.rs'
$l = Get-Content $f
# Delete lines 761..1196 (1-indexed) = indices 760..1195
$keep = $l[0..759] + $l[1196..($l.Count - 1)]
Set-Content $f -Value $keep
