[CmdletBinding()]
param(
    [string]$ProjectPath,
    [string]$UnityEditor,
    [string]$Repository,
    [switch]$ExportOnly,
    [switch]$SkipInstall
)
$ErrorActionPreference = 'Stop'

function Find-UnityEditor([string]$Version) {
    $hub = Join-Path $env:APPDATA 'UnityHub'
    $roots = @((Join-Path $env:ProgramFiles 'Unity\Hub\Editor'))
    $custom = Join-Path $hub 'secondaryInstallPath.json'
    if (Test-Path -LiteralPath $custom) {
        $roots += (Get-Content -LiteralPath $custom -Raw | ConvertFrom-Json)
    }
    $locations = @()
    $registered = Join-Path $hub 'editors.json'
    if (Test-Path -LiteralPath $registered) {
        $entry = (Get-Content -LiteralPath $registered -Raw | ConvertFrom-Json).PSObject.Properties[$Version]
        if ($entry) { $locations += @($entry.Value.location) }
    }
    $locations += @($roots | Where-Object { $_ } | ForEach-Object {
        Join-Path $_ "$Version\Editor\Unity.exe"
    })
    $locations | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } | Select-Object -First 1
}

function Invoke-GitHub {
    & $gh @args
    if ($LASTEXITCODE -ne 0) { throw "GitHub 操作失败（退出码 $LASTEXITCODE）。请检查上方提示。" }
}

function Test-GitHubApi([string]$Endpoint) {
    $ErrorActionPreference = 'Continue'
    & $gh api --silent $Endpoint 2>$null
    $LASTEXITCODE -eq 0
}

function Test-GitHubLogin {
    $ErrorActionPreference = 'Continue'
    & $gh auth status --hostname github.com *> $null
    $LASTEXITCODE -eq 0
}

if (!$ProjectPath) {
    Add-Type -AssemblyName System.Windows.Forms
    $picker = New-Object System.Windows.Forms.FolderBrowserDialog
    $picker.Description = '选择 Unity 工程文件夹（里面有 Assets 和 ProjectSettings）'
    try {
        if ($picker.ShowDialog() -ne [Windows.Forms.DialogResult]::OK) { return }
        $ProjectPath = $picker.SelectedPath
    } finally {
        $picker.Dispose()
    }
}
$project = (Resolve-Path -LiteralPath $ProjectPath).Path
$versionPath = Join-Path $project 'ProjectSettings\ProjectVersion.txt'
if (!(Test-Path -LiteralPath $versionPath -PathType Leaf) -or
    !(Test-Path -LiteralPath (Join-Path $project 'Assets') -PathType Container)) {
    throw '请选择同时包含 Assets 和 ProjectSettings 的 Unity 工程根目录。'
}
$version = [regex]::Match((Get-Content -LiteralPath $versionPath -Raw), '(?m)^m_EditorVersion:\s*(\S+)').Groups[1].Value
if (!$version) { throw '无法读取此项目的 Unity 编辑器版本。' }
if (!$UnityEditor) { $UnityEditor = Find-UnityEditor $version }
if (!$UnityEditor -or !(Test-Path -LiteralPath $UnityEditor -PathType Leaf)) {
    throw "请打开 Unity Hub，安装项目所需的 Unity $version，并勾选 iOS Build Support。装好后再双击本工具。"
}
$UnityEditor = (Resolve-Path -LiteralPath $UnityEditor).Path
if (!(Test-Path -LiteralPath (Join-Path (Split-Path -Parent $UnityEditor) 'Data\PlaybackEngines\iOSSupport'))) {
    throw "请在 Unity Hub 的 Installs 找到 Unity $version，选择 Add modules，添加 iOS Build Support。"
}

if (!$ExportOnly) {
    $ghCommand = Get-Command gh.exe -ErrorAction SilentlyContinue
    if ($ghCommand) {
        $gh = $ghCommand.Source
    } else {
        $gh = Join-Path $env:ProgramFiles 'GitHub CLI\gh.exe'
        if (!(Test-Path -LiteralPath $gh -PathType Leaf)) {
            throw '请安装 GitHub CLI（https://cli.github.com/），再双击此工具。'
        }
    }
    $installer = Join-Path $PSScriptRoot 'Unity-IPA-Installer.exe'
    if (!$SkipInstall -and !(Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw '找不到 Unity-IPA-Installer.exe。请解压完整的工具包。'
    }
}

$tag = 'unity-ios-{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), ([guid]::NewGuid().ToString('N').Substring(0, 8))
$work = Join-Path $env:LOCALAPPDATA "UnityIpaWorkflow\Builds\$tag"
$export = Join-Path $work 'Xcode'
$log = Join-Path $work 'unity-export.log'
$archive = Join-Path $work 'unity-ios-xcode.tar.gz'

New-Item -ItemType Directory -Path $work | Out-Null
$editorFolder = Join-Path $project 'Assets\Editor'
$bridge = Join-Path $editorFolder 'UnityIpaWorkflowExport.cs'
if ((Test-Path -LiteralPath $bridge) -or (Test-Path -LiteralPath "$bridge.meta")) {
    throw "工程中已存在 $bridge。请先确认它的用途。"
}
$createdEditorFolder = !(Test-Path -LiteralPath $editorFolder)
if ($createdEditorFolder) {
    New-Item -ItemType Directory -Path $editorFolder | Out-Null
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'tools\UnityIpaExport.cs') -Destination $bridge
$previousOutput = $env:UNITY_IPA_XCODE_OUTPUT
try {
    $env:UNITY_IPA_XCODE_OUTPUT = $export
    Write-Host "Unity 正在导出 iOS 工程。日志位于 $log"
    $unityArgs = "-batchmode -quit -nographics -projectPath `"$project`" -buildTarget iOS -executeMethod UnityIpaWorkflowExport.Run -logFile `"$log`""
    $unity = Start-Process -FilePath $UnityEditor -ArgumentList $unityArgs -WindowStyle Hidden -Wait -PassThru
    if ($unity.ExitCode -ne 0 -or !(Test-Path -LiteralPath (Join-Path $export 'Unity-iPhone.xcodeproj\project.pbxproj'))) {
        throw "Unity 导出失败。请关闭正在编辑此项目的 Unity 窗口，并查看日志 $log"
    }
} finally {
    $env:UNITY_IPA_XCODE_OUTPUT = $previousOutput
    foreach ($file in @($bridge, "$bridge.meta")) {
        if (Test-Path -LiteralPath $file -PathType Leaf) { Remove-Item -LiteralPath $file }
    }
    if ($createdEditorFolder -and !(Get-ChildItem -LiteralPath $editorFolder -Force)) {
        Remove-Item -LiteralPath $editorFolder
        if (Test-Path -LiteralPath "$editorFolder.meta" -PathType Leaf) {
            Remove-Item -LiteralPath "$editorFolder.meta"
        }
    }
}

Write-Host '正在打包 Xcode 工程...'
& tar.exe -czf $archive -C $export .
if ($LASTEXITCODE -ne 0) { throw '无法打包 Xcode 工程。' }
$sha = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "Xcode 归档已保存到 $archive"
if ($ExportOnly) { return }

if (!(Test-GitHubLogin)) {
    Invoke-GitHub auth login --hostname github.com --web --scopes workflow
}
$owner = Invoke-GitHub api user --jq .login
if (!$Repository) { $Repository = "$owner/unity-ios-builds" }
if ($Repository -notmatch '^[a-zA-Z0-9][a-zA-Z0-9-]*/[a-zA-Z0-9_.-]+$') {
    throw 'GitHub 仓库请使用 owner/repo 格式。'
}
if ((Read-Host "将 Xcode 工程上传到 GitHub 私有仓库 $Repository 并启动 macOS 构建。按 Enter 继续，输入其他字符取消")) {
    return
}
if (!(Test-GitHubApi "repos/$Repository")) {
    $parts = $Repository.Split('/')
    if ($parts[0] -ine $owner) { throw '找不到目标仓库，或当前 GitHub 账户没有权限。' }
    $null = Invoke-GitHub api --method POST user/repos -f "name=$($parts[1])" -F private=true -F auto_init=true
}
$repo = Invoke-GitHub api "repos/$Repository" | ConvertFrom-Json
if (!$repo.private) { throw '请使用私有仓库保存 Xcode 导出物。' }
$ref = $repo.default_branch
$workflow = 'unity-ios-from-xcode.yml'
$endpoint = "repos/$Repository/contents/.github/workflows/$workflow"
$localWorkflow = Join-Path $PSScriptRoot "workflows\$workflow"
if (Test-GitHubApi $endpoint) {
    $existing = Invoke-GitHub api $endpoint --jq .content
    $remoteBytes = [Convert]::FromBase64String(($existing -join ''))
    $localBytes = [IO.File]::ReadAllBytes($localWorkflow)
    $remoteText = [Text.Encoding]::UTF8.GetString($remoteBytes)
    $localText = [Text.Encoding]::UTF8.GetString($localBytes)
    if (($remoteText -replace "\r\n", "`n") -cne ($localText -replace "\r\n", "`n")) {
        throw '私有仓库中已有不同版本的同名构建工作流。请查看该文件或选用另一个私有仓库。'
    }
} else {
    $workflowContent = [Convert]::ToBase64String([IO.File]::ReadAllBytes($localWorkflow))
    try {
        $null = Invoke-GitHub api --method PUT $endpoint -f 'message=Add Unity iOS build workflow' -f "content=$workflowContent" -f "branch=$ref"
    } catch {
        throw '无法安装 GitHub 工作流。请确认有仓库写入权限。已有 CLI 登录时，可运行 gh auth refresh --hostname github.com --scopes workflow 后重试。'
    }
}

for ($attempt = 0; $attempt -lt 5; $attempt++) {
    if (Test-GitHubApi "repos/$Repository/actions/workflows/$workflow") { break }
    if ($attempt -eq 4) { throw 'GitHub 尚未启用该构建工作流。请打开仓库的 Actions 页面检查一次。' }
    Start-Sleep -Seconds 2
}
Write-Host '正在上传 Xcode 工程并启动 GitHub Actions...'
Invoke-GitHub release create $tag $archive --repo $Repository --target $ref --title $tag --prerelease --notes 'Private Unity Xcode export for an iPhone test build.'
$dispatch = Invoke-GitHub api --method POST "repos/$Repository/actions/workflows/$workflow/dispatches" `
    -H 'X-GitHub-Api-Version: 2026-03-10' -f "ref=$ref" -f "inputs[release_tag]=$tag" -f "inputs[archive_sha256]=$sha"
$run = $dispatch | ConvertFrom-Json
if (!$run.workflow_run_id) { throw "GitHub 未返回运行编号。请在 https://github.com/$Repository/actions 查看此次构建。" }
Write-Host "正在等待 GitHub 编译 $($run.html_url)"
Invoke-GitHub run watch $run.workflow_run_id --repo $Repository --exit-status --interval 10
$download = Join-Path $work 'IPA'
Invoke-GitHub run download $run.workflow_run_id --repo $Repository --name unity-ios-unsigned-ipa --dir $download
$ipa = Join-Path $download 'unity-ios-unsigned.ipa'
$manifest = (Get-Content -LiteralPath "$ipa.sha256" -Raw).Trim()
$actual = (Get-FileHash -LiteralPath $ipa -Algorithm SHA256).Hash.ToLowerInvariant()
if ($manifest -cne "$actual  unity-ios-unsigned.ipa") { throw '下载的 IPA 未通过 SHA-256 校验。' }
Write-Host "IPA 已保存到 $ipa"
if (!$SkipInstall) {
    [void](Read-Host '请用数据线连接并解锁 iPhone，准备好后按 Enter')
    & $installer $ipa
    if ($LASTEXITCODE -ne 0) { throw "此 IPA 已保存，可直接双击安装器重新安装。$ipa" }
}
