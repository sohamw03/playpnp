param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$PlaypnpArgs
)

$ErrorActionPreference = 'Stop'

Get-Command cargo -ErrorAction Stop | Out-Null

Write-Host 'Stopping playpnp...'
& playpnp stop
if ($LASTEXITCODE -ne 0) { throw "playpnp stop failed (exit $LASTEXITCODE)" }

Write-Host 'Installing...'
& cargo install --path $PSScriptRoot
if ($LASTEXITCODE -ne 0) { throw "cargo install failed (exit $LASTEXITCODE)" }

Write-Host 'Starting playpnp...'
& playpnp @PlaypnpArgs
if ($LASTEXITCODE -ne 0) { throw "playpnp start failed (exit $LASTEXITCODE)" }

& playpnp status
