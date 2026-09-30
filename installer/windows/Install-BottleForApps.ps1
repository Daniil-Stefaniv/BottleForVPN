$ErrorActionPreference = "Stop"

$source = Join-Path $PSScriptRoot "app"
if (-not (Test-Path (Join-Path $source "bottle.exe"))) {
    throw "Файл app\bottle.exe не найден. Распакуйте установочный архив полностью."
}

$installDirectory = Join-Path $env:LOCALAPPDATA "Programs\BottleForApps"
$startMenuDirectory = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\BottleForApps"
$desktopDirectory = [Environment]::GetFolderPath("Desktop")
$serviceName = "BottleForAppsRouteHelper"

New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
New-Item -ItemType Directory -Force -Path $startMenuDirectory | Out-Null
Copy-Item (Join-Path $source "*") $installDirectory -Recurse -Force
Copy-Item (Join-Path $PSScriptRoot "Uninstall-BottleForApps.ps1") $installDirectory -Force

$shell = New-Object -ComObject WScript.Shell
$iconPath = Join-Path $installDirectory "bottle.ico"
$applicationShortcut = $shell.CreateShortcut((Join-Path $startMenuDirectory "BottleForApps.lnk"))
$applicationShortcut.TargetPath = Join-Path $installDirectory "bottle.exe"
$applicationShortcut.WorkingDirectory = $installDirectory
$applicationShortcut.Description = "BottleForApps"
if (Test-Path $iconPath) { $applicationShortcut.IconLocation = "$iconPath, 0" }
$applicationShortcut.Save()

$desktopShortcut = $shell.CreateShortcut((Join-Path $desktopDirectory "BottleForApps.lnk"))
$desktopShortcut.TargetPath = Join-Path $installDirectory "bottle.exe"
$desktopShortcut.WorkingDirectory = $installDirectory
$desktopShortcut.Description = "BottleForApps"
if (Test-Path $iconPath) { $desktopShortcut.IconLocation = "$iconPath, 0" }
$desktopShortcut.Save()

$uninstallShortcut = $shell.CreateShortcut((Join-Path $startMenuDirectory "Удалить BottleForApps.lnk"))
$uninstallShortcut.TargetPath = "powershell.exe"
$uninstallShortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $installDirectory 'Uninstall-BottleForApps.ps1')`""
$uninstallShortcut.WorkingDirectory = $installDirectory
$uninstallShortcut.Save()

# Служба маршрутов: единственное действие, требующее прав администратора.
# Появится один запрос UAC; после него приложению повышение прав больше не нужно.
$helperPath = Join-Path $installDirectory "bottle-route-helper.exe"
if (Test-Path $helperPath) {
    $innerCommand = @"
`$ErrorActionPreference = 'Continue'
sc.exe stop $serviceName | Out-Null
sc.exe delete $serviceName | Out-Null
sc.exe create $serviceName binPath= "`"`"$helperPath`"`"" start= auto obj= LocalSystem DisplayName= "BottleForApps Route Helper"
sc.exe description $serviceName "Локальная служба BottleForApps: управляет временными маршрутами VPN endpoint."
sc.exe failure $serviceName reset= 86400 actions= restart/10000/restart/30000/restart/60000
sc.exe start $serviceName
"@
    try {
        $process = Start-Process powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden `
            -ArgumentList "-NoProfile", "-Command", $innerCommand
        if ($process.ExitCode -ne 0) {
            throw "код $($process.ExitCode)"
        }
        Write-Host "Служба маршрутов BottleForApps зарегистрирована и запущена."
    }
    catch {
        Write-Warning "Не удалось зарегистрировать службу маршрутов ($($_.Exception.Message))."
        Write-Warning "Приложение будет работать, но при запуске подключения потребует права администратора."
        Write-Warning "Переустановите приложение и подтвердите запрос прав, чтобы включить службу."
    }
}
else {
    Write-Warning "bottle-route-helper.exe не найден в пакете: приложение попросит права администратора при запуске подключения."
}

Write-Host "BottleForApps установлен в $installDirectory"
Write-Host "Ярлыки созданы в меню Пуск и на рабочем столе."
