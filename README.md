# BottleForApps

Лёгкий Windows-MVP для одновременного использования нескольких связок VPN + RDP без виртуальных машин.

Приложение решает две задачи:

1. До запуска VPN добавляет временные `/32`-маршруты к публичным VPN endpoint через физический шлюз. Поэтому следующий VPN не пытается подключиться через уже поднятый туннель.
2. Запускает локальный TCP-релей для RDP и привязывает его исходящий Winsock-сокет к выбранному VPN-интерфейсу через `IP_UNICAST_IF`.

Дополнительная нагрузка создаётся только небольшим .NET-процессом и копированием RDP TCP-потока. VM, контейнеров и собственного сетевого драйвера нет.

## Ограничения MVP

- Windows 10/11, IPv4 и RDP over TCP.
- Команда `start` требует права администратора для `New-NetRoute`.
- VPN-клиент должен создать или активировать отдельный сетевой интерфейс.
- В конфигурации нужно указать все публичные endpoint VPN. Если клиент выбирает адрес из большого динамического пула, укажите DNS-имя и дополнительные имена через повторяющийся `--vpn-endpoint`.
- VPN с жёстким kill switch на уровне WFP может блокировать второй VPN независимо от таблицы маршрутов.
- UDP-транспорт RDP через MVP не ретранслируется; `mstsc` переходит на TCP.
- VPN-клиент после закрытия RDP автоматически не отключается: универсального API для сторонних клиентов нет.

## Сборка

Требуется [.NET 8 SDK](https://dotnet.microsoft.com/download/dotnet/8.0).

```powershell
dotnet build .\BottleForApps.sln
dotnet publish .\src\BottleForApps\BottleForApps.csproj `
  -c Release `
  -r win-x64 `
  --self-contained false `
  -o .\artifacts\win-x64
```

Исполняемый файл будет называться `bottle.exe`.

## Первый запуск

Посмотреть сетевые интерфейсы:

```powershell
.\bottle.exe interfaces
```

Создать бутылку:

```powershell
.\bottle.exe create office-a `
  --name "Офис A" `
  --vpn "C:\Program Files\Vendor VPN\vpn.exe" `
  --vpn-endpoint vpn.company-a.example `
  --vpn-interface "Vendor VPN Adapter" `
  --rdp 10.20.30.15
```

`--vpn-interface` должен совпадать с именем или описанием из команды `interfaces`. Его можно не указывать: тогда программа выберет интерфейс, который перешёл в состояние `Up` после запуска VPN-клиента.

Если VPN использует несколько endpoint:

```powershell
.\bottle.exe create office-b `
  --vpn "C:\VPN\client.exe" `
  --vpn-endpoint vpn1.example.com `
  --vpn-endpoint vpn2.example.com `
  --rdp 10.30.0.20
```

Проверить окружение и профили:

```powershell
.\bottle.exe doctor
.\bottle.exe list
.\bottle.exe show office-a
```

Запустить PowerShell от имени администратора, затем:

```powershell
.\bottle.exe start office-a
```

Программа откроет VPN-клиент и будет ждать подключения не более 120 секунд. После появления VPN-интерфейса она запустит `mstsc.exe` через локальный релей. Временные маршруты удаляются после закрытия RDP или по `Ctrl+C`.

Для нескольких бутылок запустите каждую в отдельном окне терминала.

## Параметры

```text
--vpn <path>              путь к VPN-клиенту
--vpn-args <text>         аргументы запуска клиента
--vpn-endpoint <host>     публичный адрес VPN, параметр повторяемый
--vpn-interface <name>    интерфейс VPN
--base-interface <name>   физический интернет-интерфейс
--rdp <host>              внутренний адрес RDP
--rdp-port <port>         по умолчанию 3389
--relay-port <port>       0 означает автоматический выбор
```

Профили находятся в `%LOCALAPPDATA%\BottleForApps\bottles`. Для переноса каталога установите переменную среды `BOTTLE_HOME`.

## Как проверить изоляцию

1. Создайте две бутылки с разными VPN.
2. Запустите первую и подключите её VPN.
3. Не закрывая первый RDP, запустите вторую бутылку в другом административном терминале.
4. Выполните `Get-NetRoute -AddressFamily IPv4` и убедитесь, что endpoint обоих VPN имеют `/32`-маршруты через физический интерфейс.
5. Проверьте в журналах VPN, что второй туннель подключился к публичному endpoint напрямую.

Если второй клиент всё равно не подключается, временно отключите его kill switch или опцию вида `Block connections without VPN`. Если это запрещено корпоративной политикой, данный клиент нельзя совместить с другим без отдельного сетевого стека.

## Безопасность маршрутов

Создаются только маршруты в `ActiveStore`, поэтому они не являются постоянными. При аварийном завершении процесса они могут оставаться до перезагрузки или отключения интерфейса. Просмотреть их можно так:

```powershell
Get-NetRoute -AddressFamily IPv4 | Sort-Object DestinationPrefix
```

Удалять маршрут вручную следует только после проверки его destination, interface index и next hop:

```powershell
Remove-NetRoute -DestinationPrefix "203.0.113.10/32" -InterfaceIndex 12
```
