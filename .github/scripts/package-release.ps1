[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('windows-x64', 'linux-x64', 'macos-arm64')]
    [string]$Platform,

    [Parameter(Mandatory)]
    [string]$Binary,

    [Parameter(Mandatory)]
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$project = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
$versionText = & $binaryPath --version
if ($versionText -notmatch '^namemcsniper (?<version>\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$') {
    throw "Unexpected version from $binaryPath"
}
$version = $Matches.version
if ($env:GITHUB_REF_TYPE -eq 'tag' -and $env:GITHUB_REF_NAME -cne "v$version") {
    throw "The release tag must match the executable version v$version."
}

$outputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDirectory)
$output = [IO.Directory]::CreateDirectory($outputPath).FullName
$archiveName = "namemcsniper-v$version-$Platform.zip"
$destination = Join-Path $output $archiveName
if (Test-Path -LiteralPath $destination) {
    throw "The release ZIP already exists at $destination."
}

$tempRoot = (Resolve-Path -LiteralPath ([IO.Path]::GetTempPath())).Path
$scratch = [IO.Directory]::CreateTempSubdirectory('namemcsniper-package-').FullName
try {
    $package = [IO.Directory]::CreateDirectory((Join-Path $scratch 'package')).FullName
    $extracted = [IO.Directory]::CreateDirectory((Join-Path $scratch 'extracted')).FullName
    $archive = Join-Path $scratch $archiveName
    $binaryName = if ($Platform -eq 'windows-x64') { 'namemcsniper.exe' } else { 'namemcsniper' }
    $included = 'LICENSE', 'README.md', 'tokens.txt.example', 'proxies.txt.example'
    $packageBinary = Join-Path $package $binaryName
    Copy-Item -LiteralPath $binaryPath -Destination $packageBinary
    foreach ($file in $included) {
        Copy-Item -LiteralPath (Join-Path $project $file) -Destination $package
    }

    if ($IsWindows) {
        Compress-Archive -Path (Join-Path $package '*') -DestinationPath $archive
        Expand-Archive -LiteralPath $archive -DestinationPath $extracted
    } else {
        & chmod +x $packageBinary
        Push-Location -LiteralPath $package
        try {
            & zip -q $archive $binaryName @included
        } finally {
            Pop-Location
        }
        & unzip -q $archive -d $extracted
    }

    Push-Location -LiteralPath $extracted
    try {
        $packagedBinary = Join-Path $extracted $binaryName
        if ((& $packagedBinary --version) -cne $versionText) {
            throw 'The extracted executable reported a different version.'
        }
        $result = & $packagedBinary simulate
        if (($result -join ' ') -notmatch '\[simulated\] Name changed\.') {
            throw 'The extracted executable did not pass the offline simulation.'
        }
    } finally {
        Pop-Location
    }

    Copy-Item -LiteralPath $archive -Destination $destination
    Write-Output "Verified $destination with an offline simulation."
} finally {
    $resolvedScratch = (Resolve-Path -LiteralPath $scratch).Path
    if ([IO.Path]::GetDirectoryName($resolvedScratch) -ne [IO.Path]::TrimEndingDirectorySeparator($tempRoot)) {
        throw "Unexpected packaging workspace $resolvedScratch"
    }
    Remove-Item -LiteralPath $resolvedScratch -Recurse -Force
}
