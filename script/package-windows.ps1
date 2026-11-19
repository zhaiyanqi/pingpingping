#Requires -Version 5.1
[CmdletBinding()]
param(
    [ValidateSet('Both', 'Setup', 'Portable')]
    [string]$Package = 'Both',
    [switch]$StandardRelease,
    [string]$IsccPath
)

$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne 'Win32NT') {
    throw 'Run this script on Windows with the x64 MSVC build tools installed.'
}

if ($Package -ne 'Portable') {
    if (-not $IsccPath) {
        $IsccPath = (Get-Command ISCC.exe -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1).Source
    }
    if (-not $IsccPath) {
        $installRoots = @(${env:ProgramFiles(x86)}, $env:ProgramFiles)
        if ($env:LOCALAPPDATA) { $installRoots += Join-Path $env:LOCALAPPDATA 'Programs' }
        foreach ($installRoot in $installRoots) {
            if (-not $installRoot) { continue }
            foreach ($folder in @('Inno Setup 6', 'Inno Setup 7')) {
                $candidate = Join-Path $installRoot "$folder\ISCC.exe"
                if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                    $IsccPath = $candidate
                    break
                }
            }
            if ($IsccPath) { break }
        }
    }
    if (-not $IsccPath -or -not (Test-Path -LiteralPath $IsccPath -PathType Leaf)) {
        throw 'Install Inno Setup 6.3 or later, or set -IsccPath to the compiler ISCC.exe.'
    }
    $IsccPath = (Get-Item -LiteralPath $IsccPath).FullName
}

$root = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'
$buildScript = Join-Path $PSScriptRoot 'build-windows-release.bat'
if ($StandardRelease) {
    $buildProfile = 'release'
    $artifactSuffix = 'windows-x64-release'
    $buildArguments = @('--no-pause')
} else {
    $buildProfile = 'release-small'
    $artifactSuffix = 'windows-x64'
    $buildArguments = @('--size', '--no-pause')
}

& $buildScript @buildArguments
if ($LASTEXITCODE -ne 0) { throw "Windows build failed (exit code $LASTEXITCODE)." }

$manifest = Join-Path $root 'Cargo.toml'
$metadata = & rustup run "stable-$target" cargo metadata --locked --no-deps --format-version 1 --manifest-path $manifest
if ($LASTEXITCODE -ne 0) { throw "Cargo metadata failed (exit code $LASTEXITCODE)." }
$app = ($metadata | ConvertFrom-Json).packages | Where-Object { $_.name -eq 'pingpingping' }
if (-not $app.version) { throw 'Could not find the PingPingPing version in Cargo metadata.' }

$name = "PingPingPing-$($app.version)-$artifactSuffix"
$exe = Join-Path $root "target\$target\$buildProfile\pingpingping.exe"
$packaging = Join-Path $root 'packaging'
$dist = Join-Path $root 'dist\windows'
$staging = Join-Path $dist ('.package-' + [Guid]::NewGuid().ToString('N'))
$payload = Join-Path $staging 'payload'

try {
    # Stage a single build and finish every requested package before publishing any of them.
    # 使用同一份构建产物，所有选定的包都生成后再移入输出目录。
    [void][IO.Directory]::CreateDirectory($payload)
    Copy-Item -LiteralPath $exe -Destination $payload
    $artifacts = @()

    if ($Package -ne 'Portable') {
        $spec = Join-Path $packaging 'windows-setup.iss'
        $binary = Join-Path $payload 'pingpingping.exe'
        & $IsccPath "/DAppVersion=$($app.version)" "/DAppBinary=$binary" "/O$staging" "/F$name-setup" $spec
        if ($LASTEXITCODE -ne 0) { throw "Inno Setup failed (exit code $LASTEXITCODE)." }
        $artifacts += "$name-setup.exe"
    }

    if ($Package -ne 'Setup') {
        Copy-Item -LiteralPath (Join-Path $packaging 'Launch-PingPingPing.bat') -Destination $payload
        $archive = Join-Path $staging "$name-portable.zip"
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::CreateFromDirectory($payload, $archive, 'Optimal', $false)
        $artifacts += "$name-portable.zip"
    }

    $packages = foreach ($artifact in $artifacts) {
        Get-Item -LiteralPath (Join-Path $staging $artifact)
    }
    foreach ($file in $packages) {
        $destination = Join-Path $dist $file.Name
        $sizeMiB = $file.Length / 1MB
        Move-Item -LiteralPath $file.FullName -Destination $destination -Force
        Write-Host ('Built {0} ({1:N2} MiB)' -f $destination, $sizeMiB)
    }
} finally {
    if (Test-Path -LiteralPath $staging) {
        Remove-Item -LiteralPath $staging -Recurse -Force
    }
}
