$ErrorActionPreference = "Stop"
$projectRoot = Split-Path -Parent $PSScriptRoot
$project = Join-Path $projectRoot "src\BottleForApps\BottleForApps.csproj"
$output = Join-Path $projectRoot "artifacts\win-x64"

dotnet publish $project `
    --configuration Release `
    --runtime win-x64 `
    --self-contained false `
    --output $output

Write-Host "Published to $output"
