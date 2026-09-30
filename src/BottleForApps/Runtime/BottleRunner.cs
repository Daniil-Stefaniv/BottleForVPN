using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using BottleForApps.Domain;
using BottleForApps.Networking;
using BottleForApps.Infrastructure;

namespace BottleForApps.Runtime;

internal sealed record RoutePathStatus(
    string Endpoint,
    string Address,
    string AddressFamily,
    string InterfaceName,
    string Gateway,
    bool Verified,
    string Detail);

internal sealed class BottleRunner
{
    private readonly NetworkInspector _network = new();

    public async Task RunAsync(
        BottleDefinition bottle,
        CancellationToken cancellationToken,
        Action<string>? report = null,
        Action<RoutePathStatus>? reportRoute = null)
    {
        void Log(string message)
        {
            Console.WriteLine(message);
            DiagnosticLog.Info("session.progress", message, new { bottle.Id });
            report?.Invoke(message);
        }

        using var operation = DiagnosticLog.Begin("session.run", "Запуск профиля.", new
        {
            bottle.Id,
            bottle.Name,
            bottle.VpnExecutable,
            vpnArgumentsPresent = !string.IsNullOrWhiteSpace(bottle.VpnArguments),
            bottle.VpnEndpoints,
            bottle.RdpHost,
            bottle.RdpPort,
            bottle.VpnInterface,
            bottle.BaseInterface,
            bottle.RelayPort
        });
        EnsureSupportedPlatform();
        Validate(bottle);

        var baseAdapter = _network.FindBaseAdapter(bottle.BaseInterface);
        var before = _network.GetAdapters().ToDictionary(item => item.Id, StringComparer.OrdinalIgnoreCase);
        var routes = new RouteManager();
        var knownAddresses = new HashSet<IPAddress>();

        Log($"Базовая сеть: {baseAdapter.Name}, IPv4-шлюз {baseAdapter.Gateway?.ToString() ?? "—"}, " +
            $"IPv6-шлюз {baseAdapter.Ipv6Gateway?.ToString() ?? "—"}, ifIndex {baseAdapter.InterfaceIndex}");

        try
        {
            await EnsureEndpointRoutesAsync(
                bottle, baseAdapter, routes, knownAddresses, Log, reportRoute, cancellationToken);

            using var refreshCancellation = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            var refreshTask = RefreshEndpointRoutesAsync(
                bottle, baseAdapter, routes, knownAddresses, Log, reportRoute, refreshCancellation.Token);
            var sessionTask = RunVpnAndRdpAsync(bottle, baseAdapter, before, Log, refreshCancellation.Token);
            try
            {
                var completed = await Task.WhenAny(sessionTask, refreshTask);
                await completed;
                if (completed == refreshTask)
                {
                    throw new InvalidOperationException("Контроль маршрутов неожиданно завершился.");
                }
            }
            finally
            {
                await refreshCancellation.CancelAsync();
                try
                {
                    await refreshTask;
                }
                catch (OperationCanceledException) when (refreshCancellation.IsCancellationRequested)
                {
                    // Expected when the RDP session or the user stops the bottle.
                }
                try
                {
                    await sessionTask;
                }
                catch (OperationCanceledException) when (refreshCancellation.IsCancellationRequested)
                {
                    // Expected when route monitoring fails or the session ends.
                }
            }
        }
        finally
        {
            Log("Завершаю сессию и удаляю временные маршруты...");
            using var cleanupTimeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
            try
            {
                await routes.RemoveCreatedRoutesAsync(cleanupTimeout.Token);
            }
            catch (Exception exception)
            {
                DiagnosticLog.Error("route.cleanup.error", "Не удалось полностью очистить маршруты.", exception, new { bottle.Id });
                Console.Error.WriteLine($"Не удалось полностью очистить маршруты: {exception.Message}");
            }
        }
    }

    private async Task RunVpnAndRdpAsync(
        BottleDefinition bottle,
        NetworkAdapterInfo baseAdapter,
        IReadOnlyDictionary<string, NetworkAdapterInfo> before,
        Action<string> log,
        CancellationToken cancellationToken)
    {
        var vpnProcess = StartVpn(bottle);
        log(vpnProcess is null
            ? "VPN-клиент запущен. Выполните подключение в его интерфейсе."
            : $"VPN-клиент запущен, PID {vpnProcess.Id}. Выполните подключение, если оно не автоматическое.");

        var vpnAdapter = await _network.WaitForVpnAdapterAsync(
            before,
            bottle.VpnInterface,
            baseAdapter.InterfaceIndex,
            TimeSpan.FromSeconds(120),
            cancellationToken);
        log($"VPN-интерфейс: {vpnAdapter.Name}, ifIndex {vpnAdapter.InterfaceIndex}");

        var rdpAddresses = await NetworkInspector.ResolveIpv4Async(bottle.RdpHost, cancellationToken);
        var rdpAddress = rdpAddresses[0];
        log($"RDP: {bottle.RdpHost} -> {rdpAddress}:{bottle.RdpPort}");

        await using var relay = new TcpRelay(
            rdpAddress,
            bottle.RdpPort,
            vpnAdapter.InterfaceIndex,
            bottle.RelayPort);
        relay.Start();
        log($"Релей: 127.0.0.1:{relay.LocalPort} -> {rdpAddress}:{bottle.RdpPort} через {vpnAdapter.Name}");

        var rdpFile = await new RdpLauncher().LaunchAsync(bottle, relay.LocalPort, cancellationToken);
        try
        {
            log("RDP-сессия активна.");
            await relay.WaitForSessionEndAsync(cancellationToken);
        }
        finally
        {
            if (rdpFile is not null && File.Exists(rdpFile))
            {
                File.Delete(rdpFile);
            }
        }
    }

    private static async Task RefreshEndpointRoutesAsync(
        BottleDefinition bottle,
        NetworkAdapterInfo baseAdapter,
        RouteManager routes,
        HashSet<IPAddress> knownAddresses,
        Action<string> log,
        Action<RoutePathStatus>? reportRoute,
        CancellationToken cancellationToken)
    {
        while (true)
        {
            await Task.Delay(TimeSpan.FromSeconds(30), cancellationToken);
            try
            {
                await EnsureEndpointRoutesAsync(
                    bottle, baseAdapter, routes, knownAddresses, log, reportRoute, cancellationToken);
            }
            catch (SocketException exception)
            {
                log($"DNS endpoint временно недоступен, повторю проверку через 30 секунд: {exception.Message}");
            }
        }
    }

    private static async Task EnsureEndpointRoutesAsync(
        BottleDefinition bottle,
        NetworkAdapterInfo baseAdapter,
        RouteManager routes,
        HashSet<IPAddress> knownAddresses,
        Action<string> log,
        Action<RoutePathStatus>? reportRoute,
        CancellationToken cancellationToken)
    {
        foreach (var endpoint in bottle.VpnEndpoints)
        {
            var addresses = await NetworkInspector.ResolveEndpointAddressesAsync(endpoint, cancellationToken);
            foreach (var address in addresses)
            {
                var gateway = address.AddressFamily == AddressFamily.InterNetwork
                    ? baseAdapter.Gateway
                    : baseAdapter.Ipv6Gateway;
                if (gateway is null)
                {
                    throw new InvalidOperationException(
                        $"VPN endpoint {endpoint} разрешился в {address}, но у интерфейса " +
                        $"{baseAdapter.Name} нет {FamilyName(address)}-шлюза. Запуск остановлен, чтобы endpoint не ушёл через главный VPN.");
                }

                var isNewAddress = knownAddresses.Add(address);
                if (isNewAddress)
                {
                    log($"Закрепляю VPN endpoint {endpoint} ({address}) за физической сетью...");
                }
                try
                {
                    var verification = await routes.EnsureHostRouteAsync(address, baseAdapter, gateway, cancellationToken);
                    if (isNewAddress)
                    {
                        log($"Маршрут проверен: {address} → {baseAdapter.Name} → {gateway} ({verification.AddressFamily})");
                    }
                    reportRoute?.Invoke(new RoutePathStatus(
                        endpoint,
                        address.ToString(),
                        verification.AddressFamily,
                        baseAdapter.Name,
                        gateway.ToString(),
                        verification.Verified,
                        verification.Detail));
                }
                catch (Exception exception) when (exception is not OperationCanceledException)
                {
                    reportRoute?.Invoke(new RoutePathStatus(
                        endpoint,
                        address.ToString(),
                        FamilyName(address),
                        baseAdapter.Name,
                        gateway.ToString(),
                        false,
                        exception.Message));
                    throw;
                }
            }
        }
    }

    private static string FamilyName(IPAddress address) =>
        address.AddressFamily == AddressFamily.InterNetwork ? "IPv4" : "IPv6";

    private static Process? StartVpn(BottleDefinition bottle)
    {
        DiagnosticLog.Info("vpn.launch", "Запуск VPN-клиента.", new
        {
            bottle.Id,
            executable = bottle.VpnExecutable,
            argumentsPresent = !string.IsNullOrWhiteSpace(bottle.VpnArguments)
        });
        if (!File.Exists(bottle.VpnExecutable) && !Directory.Exists(bottle.VpnExecutable))
        {
            throw new FileNotFoundException("VPN-клиент не найден.", bottle.VpnExecutable);
        }

        if (OperatingSystem.IsMacOS() && bottle.VpnExecutable.EndsWith(".app", StringComparison.OrdinalIgnoreCase))
        {
            if (!string.IsNullOrWhiteSpace(bottle.VpnArguments))
            {
                throw new InvalidOperationException(
                    "Для VPN в формате .app параметры запуска не поддерживаются. " +
                    "Укажите исполняемый файл из Contents/MacOS, если параметры необходимы.");
            }
            var openInfo = new ProcessStartInfo
            {
                FileName = "/usr/bin/open",
                UseShellExecute = false
            };
            openInfo.ArgumentList.Add("-a");
            openInfo.ArgumentList.Add(bottle.VpnExecutable);
            var opened = Process.Start(openInfo);
            DiagnosticLog.Info("vpn.launched", "VPN-приложение передано LaunchServices.", new { bottle.Id, processId = opened?.Id });
            return opened;
        }

        var startInfo = new ProcessStartInfo
        {
            FileName = bottle.VpnExecutable,
            Arguments = bottle.VpnArguments,
            UseShellExecute = OperatingSystem.IsWindows(),
            WorkingDirectory = Path.GetDirectoryName(bottle.VpnExecutable) ?? Environment.CurrentDirectory
        };
        var process = Process.Start(startInfo);
        DiagnosticLog.Info("vpn.launched", "Процесс VPN-клиента запущен.", new { bottle.Id, processId = process?.Id });
        return process;
    }

    private static void Validate(BottleDefinition bottle)
    {
        if (bottle.VpnEndpoints.Count == 0)
        {
            throw new InvalidOperationException("Не указан ни один VPN endpoint.");
        }
        if (bottle.RdpPort is < 1 or > 65535 || bottle.RelayPort is < 0 or > 65535)
        {
            throw new InvalidOperationException("Некорректный номер порта.");
        }
    }

    private static void EnsureSupportedPlatform()
    {
        if (!OperatingSystem.IsWindows() && !OperatingSystem.IsMacOS())
        {
            throw new PlatformNotSupportedException("Запуск бутылок поддерживается только на Windows и macOS.");
        }
    }
}
