[CmdletBinding()]
param(
    [string]$OutputPath,
    [string]$Cargo = 'cargo',
    [string]$GitHubClientId,
    [switch]$Offline
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (!$OutputPath) { $OutputPath = Join-Path $root 'release-local' }
$target = 'x86_64-pc-windows-msvc'
$manifest = Join-Path $root 'portable-installer\Cargo.toml'
$version = [regex]::Match((Get-Content -LiteralPath $manifest -Raw), '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (!$version) { throw 'Installer version not found.' }
$destination = [IO.Path]::GetFullPath((Join-Path $OutputPath "v$version"))
if (Test-Path -LiteralPath $destination) { throw "Release output already exists: $destination" }

function Invoke-Cargo {
    & $Cargo @args
    if ($LASTEXITCODE -ne 0) { throw "Cargo failed with exit code $LASTEXITCODE." }
}

function Get-Sha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        [BitConverter]::ToString($hash.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
    } finally {
        $hash.Dispose()
        $stream.Dispose()
    }
}

$scratch = Join-Path ([IO.Path]::GetTempPath()) ("unity-ipa-release-" + [guid]::NewGuid().ToString('N'))
$source = Join-Path $scratch 'source'
$bundle = Join-Path $scratch 'bundle'
New-Item -ItemType Directory -Path "$source\portable-installer","$source\tools","$source\workflows","$source\licenses",$bundle | Out-Null
$files = @(
    '.gitignore', 'Build-Unity-Ipa.ps1', 'Build-Unity-Ipa.cmd', 'START-HERE.md',
    'tools\UnityIpaExport.cs', 'tools\Build-InstallerRelease.ps1',
    'workflows\unity-ios-from-xcode.yml', 'licenses\LGPL-2.1.txt',
    'portable-installer\Cargo.toml', 'portable-installer\Cargo.lock', 'portable-installer\src',
    'portable-installer\Show-Developer-Mode.cmd'
)
if (Test-Path -LiteralPath (Join-Path $root 'LICENSE')) { $files += 'LICENSE' }
foreach ($relative in $files) {
    Copy-Item -LiteralPath (Join-Path $root $relative) -Destination (Join-Path $source $relative) -Recurse
}

$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$buildFlags = @('-C', 'target-feature=+crt-static', "--remap-path-prefix=$source=/source", "--remap-path-prefix=$cargoHome=/cargo")
$savedFlags = $env:CARGO_ENCODED_RUSTFLAGS
$savedTarget = $env:CARGO_TARGET_DIR
$savedClientId = $env:UNITY_IOS_GITHUB_CLIENT_ID
$cargoArgs = @('--manifest-path', (Join-Path $source 'portable-installer\Cargo.toml'), '--locked', '--target', $target)
if ($Offline) { $cargoArgs += '--offline' }
$env:CARGO_ENCODED_RUSTFLAGS = $buildFlags -join [char]31
$env:CARGO_TARGET_DIR = Join-Path $scratch 'target'
if ($GitHubClientId) { $env:UNITY_IOS_GITHUB_CLIENT_ID = $GitHubClientId }
try {
    Invoke-Cargo test --release @cargoArgs
    Invoke-Cargo build --release @cargoArgs
    $exe = Join-Path $bundle 'Unity-IPA-Installer.exe'
    Copy-Item -LiteralPath (Join-Path $env:CARGO_TARGET_DIR "$target\release\unity-ipa-installer.exe") -Destination $exe
    $packages = Invoke-Cargo tree @cargoArgs --edges 'normal,build' --prefix none --format '{p}|{l}'
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $savedFlags
    $env:CARGO_TARGET_DIR = $savedTarget
    $env:UNITY_IOS_GITHUB_CLIENT_ID = $savedClientId
}

$binary = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($exe))
foreach ($privatePath in @($env:USERPROFILE, $root, $scratch, $cargoHome)) {
    if (!$privatePath) { continue }
    if ($binary.IndexOf($privatePath, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
        $binary.IndexOf($privatePath.Replace('\', '/'), [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw 'The executable contains a local build path. Review the compiler prefix mappings.'
    }
}

foreach ($relative in @('Build-Unity-Ipa.ps1','Build-Unity-Ipa.cmd','START-HERE.md','tools\UnityIpaExport.cs','workflows\unity-ios-from-xcode.yml','licenses\LGPL-2.1.txt')) {
    $copyTo = Join-Path $bundle $relative
    [void][IO.Directory]::CreateDirectory((Split-Path -Parent $copyTo))
    Copy-Item -LiteralPath (Join-Path $source $relative) -Destination $copyTo
}
Copy-Item -LiteralPath (Join-Path $source 'portable-installer\Show-Developer-Mode.cmd') -Destination $bundle
if (Test-Path -LiteralPath (Join-Path $source 'LICENSE')) {
    Copy-Item -LiteralPath (Join-Path $source 'LICENSE') -Destination $bundle
}

$noticeRoot = Join-Path $bundle 'licenses\dependencies'
$thirdParty = Join-Path $source 'third-party'
New-Item -ItemType Directory -Path $noticeRoot,$thirdParty | Out-Null
$registrySrc = Join-Path $cargoHome 'registry\src'
$licenses = foreach ($line in $packages) {
    if ($line -notmatch '^(?<name>[\w-]+) v(?<version>[^\s|]+)(?: \(proc-macro\))?\|(?<license>[^\r\n]*?)(?: \(\*\))?$') { continue }
    $nameVersion = "$($Matches.name)-$($Matches.version)"
    $licenseId = $Matches.license
    $crate = Get-ChildItem -Path "$registrySrc\*\$nameVersion" -Directory | Select-Object -First 1
    if (!$crate) { throw "Cargo registry source missing for $nameVersion" }
    $texts = Get-ChildItem -LiteralPath $crate.FullName -File | Where-Object {
        $_.Name -match '^(licen[sc]e|copying|copyright|notice|unlicense)([._-]|$)'
    }
    if ($texts) {
        $relativeDirectory = Join-Path $noticeRoot $nameVersion
        [void][IO.Directory]::CreateDirectory($relativeDirectory)
        $texts | Copy-Item -Destination $relativeDirectory
    }
    "$nameVersion | $licenseId"
}
if (!$licenses) { throw 'No dependency licenses were found.' }
$lgplDependency = $licenses | Where-Object { $_ -like 'apple-codesign-quick-*' } | Sort-Object -Unique
if (@($lgplDependency).Count -ne 1 -or $lgplDependency -notlike '*LGPL-2.1-or-later*') {
    throw 'The LGPL signing dependency has changed. Review its distribution materials.'
}
$signerVersion = ($lgplDependency -split ' \| ')[0]
$cratePath = Get-ChildItem -Path "$cargoHome\registry\cache\*\$signerVersion.crate" -File | Select-Object -First 1
if (!$cratePath) { throw "Missing exact library source archive $signerVersion.crate" }
$lock = Get-Content -LiteralPath (Join-Path $source 'portable-installer\Cargo.lock') -Raw
$signerNumber = [regex]::Escape($signerVersion.Substring('apple-codesign-quick-'.Length))
$expected = [regex]::Match($lock, 'name = "apple-codesign-quick"\s+version = "' + $signerNumber + '"\s+source = "[^"]+"\s+checksum = "([a-f0-9]{64})"').Groups[1].Value
$actual = Get-Sha256 $cratePath.FullName
if ($expected -cne $actual) { throw 'The LGPL source archive does not match Cargo.lock.' }
Copy-Item -LiteralPath $cratePath.FullName -Destination $thirdParty

$notices = @(
    'Third-party notices',
    '',
    "$signerVersion is used for Mach-O code signing, under LGPL-2.1-or-later.",
    'See licenses/LGPL-2.1.txt for the license. The accompanying source bundle includes',
    'the exact crate from Cargo.lock and instructions to rebuild with a modified copy.',
    '',
    'The dependency licenses available in the crate distributions are under licenses/dependencies.',
    ''
) + @($licenses | Sort-Object -Unique)
$notices | Set-Content -LiteralPath (Join-Path $bundle 'THIRD-PARTY-NOTICES.txt') -Encoding UTF8
Copy-Item -LiteralPath (Join-Path $bundle 'THIRD-PARTY-NOTICES.txt') -Destination $source
Copy-Item -LiteralPath $noticeRoot -Destination (Join-Path $source 'licenses') -Recurse
@"
# Build and relink the Windows installer

Install the Rust MSVC toolchain, MSVC C++ tools, and Windows SDK. From this directory run:

    powershell -ExecutionPolicy Bypass -File tools/Build-InstallerRelease.ps1

Use the Rust version in BUILD-INFO.txt and the supplied Cargo.lock for the release dependency versions.

To replace the LGPL library, extract third-party/$signerVersion.crate with tar, edit the
extracted source, and append this to portable-installer/Cargo.toml:

    [patch.crates-io]
    apple-codesign-quick = { path = "../third-party/$signerVersion" }

Run cargo update --manifest-path portable-installer/Cargo.toml -p apple-codesign-quick
to record the local source, then cargo build --manifest-path portable-installer/Cargo.toml
--release --locked --target $target (as one command).

The public distribution decision and license for the project's own source are separate from the
third-party licenses included here. This bundle is prepared for local review.
"@ | Set-Content -LiteralPath (Join-Path $source 'REBUILD.md') -Encoding UTF8
@(
    "Version: $version",
    "Target: $target",
    (Invoke-Cargo --version),
    (Invoke-Cargo version --verbose | Where-Object { $_ -like 'release:*' }),
    'Rust linker flags: -C target-feature=+crt-static with remapped source and registry paths'
) | Set-Content -LiteralPath (Join-Path $source 'BUILD-INFO.txt') -Encoding UTF8
Copy-Item -LiteralPath (Join-Path $source 'BUILD-INFO.txt') -Destination $bundle
$exeHash = Get-Sha256 $exe
"$exeHash  Unity-IPA-Installer.exe" | Set-Content -LiteralPath "$exe.sha256" -Encoding Ascii

New-Item -ItemType Directory -Path $destination | Out-Null
foreach ($package in @(@($bundle, "Unity-iOS-Tools-v$version-windows-x64.zip"), @($source, "Unity-iOS-Tools-v$version-source.zip"))) {
    $zip = Join-Path $destination $package[1]
    & tar.exe -acf $zip -C $package[0] .
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the release archive.' }
    $hash = Get-Sha256 $zip
    "$hash  $($package[1])" | Set-Content -LiteralPath "$zip.sha256" -Encoding Ascii
}
Write-Host "Local release package: $destination"
$resolvedScratch = (Resolve-Path -LiteralPath $scratch).Path
if ($resolvedScratch -eq [IO.Path]::GetFullPath($scratch) -and
    (Split-Path -Leaf $resolvedScratch) -match '^unity-ipa-release-[a-f0-9]{32}$') {
    Remove-Item -LiteralPath $resolvedScratch -Recurse
}
