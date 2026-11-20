#Requires -Version 5.1
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Path)

$ErrorActionPreference = 'Stop'
$signTool = $env:PINGPINGPING_SIGNTOOL_PATH
if (-not (Test-Path -LiteralPath $signTool -PathType Leaf)) {
    throw "SignTool executable not found: $signTool"
}
[string[]]$signArguments = ConvertFrom-Json -InputObject $env:PINGPINGPING_SIGNTOOL_ARGS
& $signTool sign /fd SHA256 /td SHA256 @signArguments $Path
if ($LASTEXITCODE -ne 0) { throw "Signing failed for $Path (exit code $LASTEXITCODE)." }

& $signTool verify /pa /tw $Path
if ($LASTEXITCODE -ne 0) { throw "Signature verification failed for $Path (exit code $LASTEXITCODE)." }
