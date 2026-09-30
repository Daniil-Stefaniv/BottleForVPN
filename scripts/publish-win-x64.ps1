$ErrorActionPreference = "Stop"
$projectRoot = Split-Path -Parent $PSScriptRoot
$project = Join-Path $projectRoot "src\BottleForApps\BottleForApps.csproj"
$helperProject = Join-Path $projectRoot "src\BottleRouteHelper\BottleRouteHelper.csproj"
$output = Join-Path $projectRoot "artifacts\win-x64"
$helperOutput = Join-Path $projectRoot "artifacts\win-x64-helper"
$release = Join-Path $projectRoot "artifacts\release"
$stage = Join-Path $projectRoot "artifacts\staging\windows\BottleForApps-Windows-x64"

dotnet publish $project `
    --configuration Release `
    --runtime win-x64 `
    --self-contained true `
    -p:NuGetAudit=false `
    -p:PublishSingleFile=true `
    -p:DebugType=None `
    --output $output

dotnet publish $helperProject `
    --configuration Release `
    --runtime win-x64 `
    --self-contained true `
    -p:NuGetAudit=false `
    -p:PublishSingleFile=true `
    -p:DebugType=None `
    --output $helperOutput

Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $stage "app") | Out-Null
New-Item -ItemType Directory -Force -Path $release | Out-Null
Copy-Item (Join-Path $output "*") (Join-Path $stage "app") -Recurse -Force
Copy-Item (Join-Path $helperOutput "bottle-route-helper.exe") (Join-Path $stage "app") -Force
Copy-Item (Join-Path $projectRoot "installer\windows\Install-BottleForApps.ps1") $stage
Copy-Item (Join-Path $projectRoot "installer\windows\Uninstall-BottleForApps.ps1") $stage
Copy-Item (Join-Path $projectRoot "installer\README-RU.txt") $stage
$archive = Join-Path $release "BottleForApps-Windows-x64.zip"
Remove-Item $archive -Force -ErrorAction SilentlyContinue
Compress-Archive -Path $stage -DestinationPath $archive

Write-Host "Installer package: $archive"
