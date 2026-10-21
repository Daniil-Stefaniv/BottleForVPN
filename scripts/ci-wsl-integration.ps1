# ci-wsl-integration.ps1 — WSL-интеграционный сценарий CI (задача 18, M1).
#
# Полный вертикальный срез env-бутылки на windows-раннере:
#   1) импорт rootfs-апплайенса как дистрибутива `bottleforapps-ci`;
#   2) подъём упрощённого WG-стенда (по мотивам теста wg_handshake, T10)
#      ВНУТРИ той же WSL-VM: namespace `srv` с veth 10.77.253.0/30,
#      wireguard-сервер 10.99.0.1:51820 (ключи генерируются в VM через
#      `wg genkey`/`wg pubkey`), endpoint клиента = IP eth0 VM
#      (UDP 51820 на eth0 DNAT-ится в srv);
#   3) профиль office-ci (kind=env, wireguard, socks5=0, forwards=[])
#      во временном DataDir;
#   4) запуск bottled, `bottle start` → handshake туннеля → `up`;
#   5) SOCKS5-negotiate 05 01 00 → 05 00 (как в юнит-тестах T16)
#      на 127.0.0.1:<порт из status>;
#   6) `bottle stop` → бутылки в status нет, порт закрыт.
#
# Допущения стенда (проверяются поведением, CI-джоба continue-on-error):
#   - все дистрибутивы одной WSL2-VM делят общий network namespace,
#     поэтому veth/DNAT из `bottleforapps-ci` видны бутылке, поднятой
#     агентом в дистрибутиве демона `bottleforapps`;
#   - boot.command rootfs стартует bottle-agent при первом обращении
#     к дистрибутиву: у `bottleforapps-ci` его приходится останавливать
#     (он занял бы 0.0.0.0:7777 в общем namespace до старта агента демона).
#
# PowerShell 5.1-совместим. Любой шаг падает с понятным сообщением и
# exit 1; teardown выполняется всегда (finally).

param(
    # Тарболл rootfs аплаенса (артефакт джобы appliance-build).
    [Parameter(Mandatory = $true)][string]$Rootfs,
    # Путь к bottled.exe (cargo build --release -p daemon).
    [Parameter(Mandatory = $true)][string]$Bottled,
    # Путь к bottle.exe (cargo build --release -p cli).
    [Parameter(Mandatory = $true)][string]$Bottle,
    # Каталог данных демона (профили, токен, VHD дистрибутивов).
    [Parameter(Mandatory = $true)][string]$DataDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# --- Константы сценария ------------------------------------------------------

# Дистрибутив стенда (импортируется из того же rootfs, что и дистрибутив
# демона `bottleforapps`; не пересекается с ним по имени).
$CiDistro = 'bottleforapps-ci'
# Идентификатор тестовой бутылки (профиль + namespace bottle-office-ci).
$BottleId = 'office-ci'
# Внутренние адреса WG-стенда: сервер 10.99.0.1, клиент 10.99.0.2.
$WgSrvAddr = '10.99.0.1'
$WgCliAddr = '10.99.0.2'
# Порт SOCKS5, который bottled пробросит на 127.0.0.1 (из status).
$Socks5Port = 0

# Процесс демона (для teardown).
$DaemonProcess = $null
# Логи демона — печатаются в диагностику при провале.
$DaemonStdout = Join-Path $DataDir 'bottled-stdout.log'
$DaemonStderr = Join-Path $DataDir 'bottled-stderr.log'

# --- Вспомогательные функции ---------------------------------------------------

# Запускает внешнюю команду, сводит stdout/stderr в строку; ненулевой код
# возврата — фатальная ошибка с текстом вывода.
#
# $ErrorActionPreference временно ослабляется: в PS 5.1 перенаправление
# stderr (2>&1) при 'Stop' превращает любой вывод в stderr NativeCommandError.
function Invoke-Tool {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$Description
    )
    Write-Host "==> $Description"
    $ErrorActionPreference = 'Continue'
    $output = & $FilePath @Arguments 2>&1
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) {
        $text = ($output | Out-String)
        throw "$Description : «$FilePath $($Arguments -join ' ')» — код $code`n$text"
    }
    return ($output | Out-String)
}

# Выполняет команду в дистрибутиве стенда: `wsl -d <ci> --exec <args>`.
function Invoke-Wsl {
    param(
        [string[]]$Arguments,
        [string]$Description
    )
    $all = @('-d', $CiDistro, '--exec') + $Arguments
    return Invoke-Tool -FilePath 'wsl.exe' -Arguments $all -Description $Description
}

# Выводит хвост журнала демона (диагностика при провале).
function Show-DaemonLog {
    foreach ($log in @($DaemonStdout, $DaemonStderr)) {
        if (Test-Path $log) {
            Write-Host "----- $log (последние строки) -----"
            Get-Content $log -Tail 40 | ForEach-Object { Write-Host $_ }
        }
    }
}

# Снимок состояния обоих дистрибутивов VM (диагностика при провале):
# жив ли bottle-agent, кто слушает :7777 в общем network namespace,
# адреса интерфейсов. Отвечает на главный вопрос сценария — почему
# «агент аплаенса не ответил за 30 s».
function Show-ApplianceState {
    foreach ($distro in @('bottleforapps', $CiDistro)) {
        Write-Host "----- дистрибутив ${distro}: ps / слушатели / адреса -----"
        $ErrorActionPreference = 'Continue'
        $probe = & wsl.exe -d $distro --exec /bin/sh -c 'ps; echo ---; ss -ltnp; echo ---; ip -o addr' 2>&1
        $ErrorActionPreference = 'Stop'
        Write-Host ($probe | Out-String)
    }
}

# SOCKS5-negotiate: TCP к 127.0.0.1:$Port, метод «без аутентификации»
# 05 01 00 → ожидаем 05 00 (та же проверка, что в юнит-тестах relay T16).
function Invoke-Socks5Negotiate {
    param([int]$Port)
    $client = New-Object System.Net.Sockets.TcpClient
    $connect = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
    if (-not $connect.AsyncWaitHandle.WaitOne(10000)) {
        $client.Close()
        throw "таймаут TCP-подключения к 127.0.0.1:$Port"
    }
    try {
        $client.EndConnect($connect)
    }
    catch {
        $client.Close()
        throw "TCP к 127.0.0.1:$Port не установился: $($_.Exception.Message)"
    }
    try {
        $stream = $client.GetStream()
        $stream.WriteTimeout = 10000
        $stream.ReadTimeout = 10000
        $request = [byte[]](5, 1, 0)
        $stream.Write($request, 0, $request.Length)
        $stream.Flush()
        $reply = New-Object byte[] 2
        $read = $stream.Read($reply, 0, 2)
        if ($read -lt 2) {
            throw "SOCKS5: короткий ответ negotiate ($read байт вместо 2)"
        }
        if ($reply[0] -ne 5 -or $reply[1] -ne 0) {
            throw ("SOCKS5: некорректный ответ negotiate: {0:X2} {1:X2} (ожидались 05 00)" -f $reply[0], $reply[1])
        }
    }
    finally {
        $client.Close()
    }
}

# Пытается подключиться к 127.0.0.1:$Port. $true — соединение УСТАНОВИЛОСЬ
# (порт всё ещё слушается), $false — отказ/таймаут (порт закрыт).
function Test-TcpStillOpen {
    param([int]$Port)
    $client = New-Object System.Net.Sockets.TcpClient
    $connect = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
    if (-not $connect.AsyncWaitHandle.WaitOne(5000)) {
        $client.Close()
        return $false
    }
    try {
        $client.EndConnect($connect)
        $client.Close()
        return $true
    }
    catch {
        $client.Close()
        return $false
    }
}

# --- Скрипт стенда WG-сервера (выполняется в VM как root) ---------------------

# Пробрасывается в VM одним аргументом в base64: никаких проблем
# с кавычками PS → sh и зависимостей от /mnt/c или wslpath.
$StandScript = @'
set -eu

# (1) Остановить boot-агента ЭТОГО дистрибутива: boot.command стартует
#     его при первом обращении, а он занимает 0.0.0.0:7777 в общем
#     network namespace VM — агент дистрибутива демона не сможет слушать.
waited=0
while [ "$waited" -lt 15 ]; do
    if ss -ltn | grep -q ':7777 '; then break; fi
    sleep 1; waited=$((waited + 1))
done
pkill -f 'bottle-agent serve' 2>/dev/null || true
sleep 1
if ss -ltn | grep -q ':7777 '; then
    echo 'FATAL: порт 7777 всё ещё занят после остановки boot-агента' >&2
    exit 1
fi

# (2) Форвардинг (путь бутылка -> DNAT -> srv) и мягкий rp_filter.
echo 1 > /proc/sys/net/ipv4/ip_forward
echo 0 > /proc/sys/net/ipv4/conf/all/rp_filter 2>/dev/null || true

# (3) Остатки возможного прошлого прогона стенда.
ip netns del srv 2>/dev/null || true
ip link del bfa-t-h 2>/dev/null || true

# (4) srv-namespace + veth 10.77.253.0/30 (по мотивам T10):
#     bfa-t-h (10.77.253.2) в default ns, bfa-t-s (10.77.253.1) в srv.
ip netns add srv
ip link add bfa-t-h type veth peer name bfa-t-s
ip link set bfa-t-s netns srv
ip addr add 10.77.253.2/30 dev bfa-t-h
ip link set bfa-t-h up
ip netns exec srv ip addr add 10.77.253.1/30 dev bfa-t-s
ip netns exec srv ip link set bfa-t-s up
ip netns exec srv ip link set lo up
ip netns exec srv ip route add default via 10.77.253.2

# (5) Пары ключей WireGuard — генерация прямо в VM.
srv_priv="$(wg genkey)"
srv_pub="$(printf '%s\n' "$srv_priv" | wg pubkey)"
cli_priv="$(wg genkey)"
cli_pub="$(printf '%s\n' "$cli_priv" | wg pubkey)"

# (6) WG-сервер в srv: wireguard-go, ListenPort 51820, addr 10.99.0.1/32,
#     AllowedIPs клиента 10.99.0.2/32.
ip netns exec srv wireguard-go bfa-wgsrv >/dev/null 2>&1 &
ready=0
waited=0
while [ "$waited" -lt 15 ]; do
    if ip netns exec srv wg show bfa-wgsrv >/dev/null 2>&1; then ready=1; break; fi
    sleep 1; waited=$((waited + 1))
done
if [ "$ready" != 1 ]; then
    echo 'FATAL: wireguard-go в srv не готов за 15 с' >&2
    exit 1
fi
printf '%s\n' \
    '[Interface]' \
    "PrivateKey = $srv_priv" \
    'ListenPort = 51820' \
    '' \
    '[Peer]' \
    "PublicKey = $cli_pub" \
    'AllowedIPs = 10.99.0.2/32' \
    | ip netns exec srv wg setconf bfa-wgsrv /dev/stdin
ip netns exec srv ip link set bfa-wgsrv up
ip netns exec srv ip addr add 10.99.0.1/32 dev bfa-wgsrv
ip netns exec srv ip route add 10.99.0.2/32 dev bfa-wgsrv

# (7) Endpoint клиента = IP eth0 VM: UDP 51820 на этот адрес DNAT-ится
#     в srv (зашифрованный трафик туннеля идёт «через физику» VM).
#     IP — через iproute2: в Alpine/BusyBox `hostname -I` отсутствует.
vm_ip="$(ip -4 -o addr show dev eth0 | awk '{split($4,a,"/"); print a[1]}')"
iptables -t nat -A PREROUTING -d "$vm_ip" -p udp --dport 51820 \
    -j DNAT --to-destination 10.77.253.1:51820
iptables -A FORWARD -d 10.77.253.0/30 -j ACCEPT
iptables -A FORWARD -s 10.77.253.0/30 -j ACCEPT

# (8) Маркеры для PowerShell-обёртки (вывод парсится по префиксам).
echo "BFA-VM-IP=$vm_ip"
echo "BFA-CLI-PRIV=$cli_priv"
echo "BFA-SRV-PUB=$srv_pub"
echo 'BFA-STAND-OK'
'@

# --- Основной сценарий ---------------------------------------------------------

try {
    Write-Host "=== WSL-интеграционный сценарий BottleForApps (M1) ==="
    Write-Host "Rootfs:   $Rootfs"
    Write-Host "Bottled:  $Bottled"
    Write-Host "Bottle:   $Bottle"
    Write-Host "DataDir:  $DataDir"

    # (0) Preflight: всё, что сценарий запускает, должно существовать.
    foreach ($path in @($Rootfs, $Bottled, $Bottle)) {
        if (-not (Test-Path $path)) {
            throw "не найден обязательный файл: $path"
        }
    }
    # Идемпотентность: прошлый DataDir может содержать профиль с чужими
    # ключами стенда — начинаем с чистого каталога.
    if (Test-Path $DataDir) {
        Remove-Item $DataDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path (Join-Path $DataDir 'bottles') -Force | Out-Null

    # wsl.exe пишет UTF-16LE; WSL_UTF8=1 + UTF8-декодирование консоли
    # дают читаемый вывод (тот же приём, что в daemon/src/appliance.rs).
    $env:WSL_UTF8 = '1'
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8

    # (1) Импорт дистрибутива стенда; остаток прошлого прогона сносится
    #     молча (unregister отсутствующего дистрибутива — ошибка, её
    #     игнорируем).
    Write-Host "==> Снос остатка $CiDistro (если был)"
    $ErrorActionPreference = 'Continue'
    & wsl.exe --unregister $CiDistro 2>&1 | Out-Null
    $ErrorActionPreference = 'Stop'

    Invoke-Tool -FilePath 'wsl.exe' `
        -Arguments @('--import', $CiDistro, (Join-Path $DataDir 'vm'), $Rootfs) `
        -Description "Импорт $CiDistro из rootfs"

    # (2) Стенд WG-сервера внутри VM (первый exec будит дистрибутив и
    #     запускает boot.command — стенд сам останавливает boot-агента).
    # Стенд-скрипт — POSIX sh (bash в appliance нет). `tr -d '\r'`
    # обязателен: checkout на windows-раннере ставит CRLF, и `set -eu\r`
    # умирал «set: line 1: illegal option -».
    $standB64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($StandScript))
    $standOut = Invoke-Wsl `
        -Arguments @('/bin/sh', '-c', "echo $standB64 | base64 -d | tr -d '\r' | sh") `
        -Description 'Подъём WG-стенда в VM (srv-netns + wireguard + DNAT)'

    if ($standOut -notmatch 'BFA-STAND-OK') {
        throw "стенд не доложил готовность (нет BFA-STAND-OK):`n$standOut"
    }
    $vmIp = [regex]::Match($standOut, 'BFA-VM-IP=(\S+)').Groups[1].Value
    $cliPriv = [regex]::Match($standOut, 'BFA-CLI-PRIV=([A-Za-z0-9+/=]+)').Groups[1].Value
    $srvPub = [regex]::Match($standOut, 'BFA-SRV-PUB=([A-Za-z0-9+/=]+)').Groups[1].Value
    if (-not $vmIp -or -not $cliPriv -or -not $srvPub) {
        throw "стенд не вернул параметры (vmIp='$vmIp', ключи пусты?):`n$standOut"
    }
    Write-Host "    VM IP: $vmIp; endpoint стенда: ${vmIp}:51820"

    # (3) Профиль office-ci + WG-конфиг клиента (ключи стенда).
    #     Формат — спека §5.1; config — имя файла-копии в хранилище.
    $wgConfig = @"
[Interface]
Address = $WgCliAddr/32
PrivateKey = $cliPriv

[Peer]
PublicKey = $srvPub
Endpoint = ${vmIp}:51820
AllowedIPs = $WgSrvAddr/32
PersistentKeepalive = 5
"@
    $profileToml = @"
id = "$BottleId"
name = "CI Office"
kind = "env"

[vpn]
protocol = "wireguard"
config = "$BottleId.conf"

[expose]
socks5 = 0
forwards = []
"@
    $bottlesDir = Join-Path $DataDir 'bottles'
    # ascii (не utf8): PS 5.1 пишет UTF-8 c BOM, а BOM ломает TOML-разбор
    # демона; содержимое обоих файлов — чистый ASCII.
    Set-Content -Path (Join-Path $bottlesDir "$BottleId.conf") -Value $wgConfig -Encoding ascii
    Set-Content -Path (Join-Path $bottlesDir "$BottleId.toml") -Value $profileToml -Encoding ascii
    Write-Host "==> Профиль $BottleId записан в $bottlesDir"

    # (4) Демон: Start-Process (не ждём завершения), вывод — в журналы.
    #     Пути quoted: DataDir раннера может содержать пробелы.
    $DaemonProcess = Start-Process -FilePath $Bottled `
        -ArgumentList @("--data-dir `"$DataDir`"", "--rootfs `"$Rootfs`"") `
        -RedirectStandardOutput $DaemonStdout `
        -RedirectStandardError $DaemonStderr `
        -PassThru -WindowStyle Hidden
    Write-Host "==> bottled запущен (pid $($DaemonProcess.Id)); жду named pipe ~10 с"

    # Ожидание канала: `bottle list` выходит с кодом 0, когда демон отвечает.
    $daemonReady = $false
    foreach ($attempt in 1..20) {
        $ErrorActionPreference = 'Continue'
        $probe = & $Bottle list 2>&1
        $code = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        if ($code -eq 0) { $daemonReady = $true; break }
        if ($DaemonProcess.HasExited) {
            throw "bottled завершился при старте (код $($DaemonProcess.ExitCode))"
        }
        Start-Sleep -Milliseconds 500
    }
    if (-not $daemonReady) {
        throw "демон не поднял канал \\.\pipe\bottled за 10 с"
    }
    Write-Host "    демон отвечает; список бутылок:"
    Write-Host ($probe | Out-String)

    # (5) start: импорт дистрибутива демона, handshake туннеля ≤30 с —
    #     в CLI заложен таймаут ответа 40 с, ждём его целиком.
    Invoke-Tool -FilePath $Bottle -Arguments @('start', $BottleId) `
        -Description "bottle start $BottleId (туннель + relay)"

    # (6) status: состояние up и порт SOCKS5.
    $status = Invoke-Tool -FilePath $Bottle -Arguments @('status') `
        -Description 'bottle status'
    Write-Host $status
    if ($status -notmatch "$BottleId\s+up") {
        throw "status не показывает «$BottleId  up»:`n$status"
    }
    $portMatch = [regex]::Match($status, 'socks5\s+127\.0\.0\.1:(\d+)')
    if (-not $portMatch.Success) {
        throw "в status нет строки socks5 127.0.0.1:<порт>:`n$status"
    }
    $Socks5Port = [int]$portMatch.Groups[1].Value
    Write-Host "    SOCKS5: 127.0.0.1:$Socks5Port"

    # (7) SOCKS5-negotiate через весь путь: 127.0.0.1 (bottled) → VM →
    #     relay внутри namespace бутылки. Несколько попыток — мост
    #     агента поднимается асинхронно.
    $negotiated = $false
    $lastNegotiateError = $null
    foreach ($attempt in 1..3) {
        try {
            Invoke-Socks5Negotiate -Port $Socks5Port
            $negotiated = $true
            break
        }
        catch {
            $lastNegotiateError = $_
            Start-Sleep -Seconds 2
        }
    }
    if (-not $negotiated) {
        throw "SOCKS5-negotiate на 127.0.0.1:$Socks5Port не прошёл за 3 попытки: $($lastNegotiateError.Exception.Message)"
    }
    Write-Host "    negotiate 05 00 получен — путь до relay бутылки работает"

    # (8) stop: бутылки в status больше нет, слушатель закрыт.
    Invoke-Tool -FilePath $Bottle -Arguments @('stop', $BottleId) `
        -Description "bottle stop $BottleId"

    $statusAfter = Invoke-Tool -FilePath $Bottle -Arguments @('status') `
        -Description 'bottle status (после stop)'
    Write-Host $statusAfter
    if ($statusAfter -match $BottleId) {
        throw "после stop бутылка $BottleId всё ещё в status:`n$statusAfter"
    }
    if (Test-TcpStillOpen -Port $Socks5Port) {
        throw "порт 127.0.0.1:$Socks5Port всё ещё слушается после stop"
    }
    Write-Host "    порт $Socks5Port закрыт (в соединении отказано)"

    Write-Host '=== СЦЕНАРИЙ ПРОЙДЕН ==='
    exit 0
}
catch {
    # Диагностика при провале: состояние WSL, журналы демона, исходная
    # ошибка. EAP ослаблен до конца блока: Write-Error при 'Stop' бросил
    # бы terminating-ошибку до exit 1.
    $ErrorActionPreference = 'Continue'
    Write-Host '=== СЦЕНАРИЙ ПРОВАЛЕН ==='
    & wsl.exe -l -v 2>&1 | Out-String | ForEach-Object { Write-Host $_ }
    Show-DaemonLog
    Show-ApplianceState
    Write-Error $_
    exit 1
}
finally {
    # Teardown (любой исход): остановить демон, убрать дистрибутивы стенда
    # и демона. Ошибки очистки не затирают результат сценария.
    $ErrorActionPreference = 'Continue'
    if ($DaemonProcess -and -not $DaemonProcess.HasExited) {
        Stop-Process -Id $DaemonProcess.Id -Force -ErrorAction SilentlyContinue
    }
    & wsl.exe --unregister $CiDistro 2>&1 | Out-Null
    # Дистрибутив демона импортируется в <DataDir>\appliance-vm — тоже наш.
    & wsl.exe --unregister 'bottleforapps' 2>&1 | Out-Null
    $ErrorActionPreference = 'Stop'
}