using System.Diagnostics;
using System.Net;
using BottleForApps.Domain;
using BottleForApps.Networking;

namespace BottleForApps.Runtime;

internal sealed class BottleRunner
{
    private readonly NetworkInspector _network = new();

    public async Task RunAsync(BottleDefinition bottle, CancellationToken cancellationToken)
    {
        EnsureWindows();
        Validate(bottle);

        var baseAdapter = _network.FindBaseAdapter(bottle.BaseInterface);
        var baseGateway = baseAdapter.Gateway
                          ?? throw new InvalidOperationException("У базового интерфейса отсутствует IPv4-шлюз.");
        var before = _network.GetAdapters().ToDictionary(item => item.Id, StringComparer.OrdinalIgnoreCase);
        var routes = new RouteManager();

        Console.WriteLine($"Базовая сеть: {baseAdapter.Name}, шлюз {baseGateway}, ifIndex {baseAdapter.InterfaceIndex}");

        try
        {
            foreach (var endpoint in bottle.VpnEndpoints)
            {
                var addresses = await NetworkInspector.ResolveIpv4Async(endpoint, cancellationToken);
                foreach (var address in addresses)
                {
                    Console.WriteLine($"Закрепляю VPN endpoint {address} за физической сетью...");
                    await routes.AddHostRouteAsync(address, baseAdapter.InterfaceIndex, baseGateway, cancellationToken);
                }
            }

            var vpnProcess = StartVpn(bottle);
            Console.WriteLine(vpnProcess is null
                ? "VPN-клиент запущен. Выполните подключение в его интерфейсе."
                : $"VPN-клиент запущен, PID {vpnProcess.Id}. Выполните подключение, если оно не автоматическое.");

            var vpnAdapter = await _network.WaitForVpnAdapterAsync(
                before,
                bottle.VpnInterface,
                baseAdapter.InterfaceIndex,
                TimeSpan.FromSeconds(120),
                cancellationToken);
            Console.WriteLine($"VPN-интерфейс: {vpnAdapter.Name}, ifIndex {vpnAdapter.InterfaceIndex}");

            var rdpAddresses = await NetworkInspector.ResolveIpv4Async(bottle.RdpHost, cancellationToken);
            var rdpAddress = rdpAddresses[0];
            Console.WriteLine($"RDP: {bottle.RdpHost} -> {rdpAddress}:{bottle.RdpPort}");

            await using var relay = new TcpRelay(
                rdpAddress,
                bottle.RdpPort,
                vpnAdapter.InterfaceIndex,
                bottle.RelayPort);
            relay.Start();
            Console.WriteLine($"Релей: 127.0.0.1:{relay.LocalPort} -> {rdpAddress}:{bottle.RdpPort} через {vpnAdapter.Name}");

            using var rdp = Process.Start(new ProcessStartInfo
            {
                FileName = "mstsc.exe",
                UseShellExecute = true,
                ArgumentList = { $"/v:127.0.0.1:{relay.LocalPort}" }
            }) ?? throw new InvalidOperationException("Не удалось запустить mstsc.exe.");

            Console.WriteLine("Маршруты будут удалены после закрытия RDP. Для отмены нажмите Ctrl+C.");
            await rdp.WaitForExitAsync(cancellationToken);
        }
        finally
        {
            Console.WriteLine("Удаляю временные маршруты...");
            using var cleanupTimeout = new CancellationTokenSource(TimeSpan.FromSeconds(15));
            try
            {
                await routes.RemoveCreatedRoutesAsync(cleanupTimeout.Token);
            }
            catch (Exception exception)
            {
                Console.Error.WriteLine($"Не удалось полностью очистить маршруты: {exception.Message}");
            }
        }
    }

    private static Process? StartVpn(BottleDefinition bottle)
    {
        if (!File.Exists(bottle.VpnExecutable))
        {
            throw new FileNotFoundException("VPN-клиент не найден.", bottle.VpnExecutable);
        }

        var startInfo = new ProcessStartInfo
        {
            FileName = bottle.VpnExecutable,
            Arguments = bottle.VpnArguments,
            UseShellExecute = true,
            WorkingDirectory = Path.GetDirectoryName(bottle.VpnExecutable) ?? Environment.CurrentDirectory
        };
        return Process.Start(startInfo);
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

    private static void EnsureWindows()
    {
        if (!OperatingSystem.IsWindows())
        {
            throw new PlatformNotSupportedException("Запуск бутылок поддерживается только на Windows 10/11.");
        }
    }
}
