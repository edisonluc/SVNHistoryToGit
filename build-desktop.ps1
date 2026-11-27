[CmdletBinding()]
param(
    [string]$OutputDirectory = '',
    [string]$Go = 'go'
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $PSScriptRoot 'dist/SVNHistoryToGit'
}
$destination = [System.IO.Path]::GetFullPath($OutputDirectory)
$previousPath = $env:PATH
$previousGoFlags = $env:GOFLAGS

# Publish by replacement so an open desktop window can finish using its old image.
function Copy-ReleaseBinary([string]$Source, [string]$Target) {
    $temporary = Join-Path (Split-Path -Parent $Target) ([IO.Path]::GetRandomFileName())
    try {
        [IO.File]::Copy($Source, $temporary)
        if ([IO.File]::Exists($Target)) {
            [IO.File]::Replace($temporary, $Target, [System.Management.Automation.Language.NullString]::Value)
        } else {
            [IO.File]::Move($temporary, $Target)
        }
    } finally {
        [IO.File]::Delete($temporary)
    }
}

Push-Location $PSScriptRoot
try {
    $metadataJson = cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw '无法读取 Rust 工作区' }
    $metadata = $metadataJson | ConvertFrom-Json
    $targetDir = $metadata.target_directory
    $cliPackage = $metadata.packages | Where-Object name -eq 'svn-history-to-git-cli'
    $manifest = Get-Content -Raw -Encoding UTF8 -LiteralPath 'desktop/mygo.json' | ConvertFrom-Json
    if ($manifest.version -cne $cliPackage.version) { throw 'desktop/mygo.json 和 Cargo.toml 的版本号必须一致' }
    cargo build --locked --release -p svn-history-to-git-cli
    if ($LASTEXITCODE -ne 0) { throw 'Rust 核心构建失败' }

    $goBin = (Get-Command $Go -CommandType Application).Source
    $env:PATH = (Split-Path -Parent $goBin) + [IO.Path]::PathSeparator + $previousPath
    $env:GOFLAGS = "$previousGoFlags -buildvcs=false".Trim()
    $goArch = & $goBin env GOARCH
    if ($LASTEXITCODE -ne 0) { throw '无法读取目标架构' }
    $mygoVersion = & $goBin -C desktop list -mod=readonly -m -f '{{.Version}}' github.com/egoist/mygo
    if ($LASTEXITCODE -ne 0) { throw '无法读取 MyGo 版本' }
    $release = Join-Path $targetDir 'release'
    & $goBin -C desktop run "github.com/egoist/mygo/cmd/mygo@$mygoVersion" build -platform "windows/$goArch" -skip-installer -o $release
    if ($LASTEXITCODE -ne 0) { throw 'MyGo 桌面端构建失败' }
    $nativeExe = Join-Path $release 'SVNHistoryToGit.exe'
    Copy-ReleaseBinary (Join-Path $release "windows-$goArch/SVNHistoryToGit.exe") $nativeExe

    $distParent = Split-Path -Parent $destination
    $legacyPaths = @(
        (Join-Path $release 'SVN2Git.exe'),
        (Join-Path $release "windows-$goArch/SVN2Git.exe"),
        (Join-Path $release 'svn2git-cli.exe'),
        (Join-Path $release 'svn2git-cli.d'),
        (Join-Path $release 'svn2git_cli.pdb'),
        (Join-Path $distParent 'SVN2Git')
    )
    foreach ($legacy in $legacyPaths) {
        if (Test-Path -LiteralPath $legacy) {
            Remove-Item -LiteralPath $legacy -Recurse -Force
        }
    }
    Get-ChildItem -Path $distParent -Filter 'SVN2Git*.zip' -File -ErrorAction SilentlyContinue | Remove-Item -Force

    [void][System.IO.Directory]::CreateDirectory($destination)
    Copy-ReleaseBinary $nativeExe (Join-Path $destination 'SVNHistoryToGit.exe')
    Copy-ReleaseBinary (Join-Path $release 'svn-history-to-git-cli.exe') (Join-Path $destination 'svn-history-to-git-cli.exe')
    $archive = Join-Path (Split-Path -Parent $destination) "SVNHistoryToGit-v$($manifest.version)-windows-$goArch.zip"
    $binaries = @((Join-Path $destination 'SVNHistoryToGit.exe'), (Join-Path $destination 'svn-history-to-git-cli.exe'))
    $packageFiles = $binaries + (Join-Path $PSScriptRoot 'LICENSE')
    Compress-Archive -LiteralPath $packageFiles -DestinationPath $archive -Force
    Get-Item -LiteralPath ($binaries + $archive)
} finally {
    $env:GOFLAGS = $previousGoFlags
    $env:PATH = $previousPath
    Pop-Location
}
