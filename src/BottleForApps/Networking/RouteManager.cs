using System.Net;
using System.Net.Sockets;
using BottleForApps.Infrastructure;
using BottleIpc;

namespace BottleForApps.Networking;

internal sealed record ManagedRoute(
    IPAddress Destination,
    int InterfaceIndex,
    string InterfaceName,
    IPAddress NextHop,
    bool CreatedByBottle);

internal sealed record RouteVerification(
    IPAddress Destination,
    string AddressFamily,
    int InterfaceIndex,
    string InterfaceName,
    IPAddress NextHop,
    bool Verified,
    string Detail);

internal sealed class RouteManager
{
    private readonly List<ManagedRoute> _managedRoutes = [];

    public async Task<RouteVerification> EnsureHostRouteAsync(
        IPAddress destination,
        NetworkAdapterInfo networkAdapter,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        using var operation = DiagnosticLog.Begin("route.ensure", "Добавление и проверка маршрута к VPN endpoint.", new
        {
            destination = destination.ToString(),
            networkAdapter.Name,
            nextHop = nextHop.ToString()
        });
        EnsureSupportedAddress(destination, nameof(destination));
        if (destination.AddressFamily != nextHop.AddressFamily)
        {
            throw new ArgumentException("Endpoint и шлюз должны использовать одно семейство адресов.", nameof(nextHop));
        }

        var interfaceIndex = destination.AddressFamily == AddressFamily.InterNetwork
            ? networkAdapter.InterfaceIndex
            : networkAdapter.Ipv6InterfaceIndex;
        if (interfaceIndex <= 0)
        {
            throw new InvalidOperationException(
                $"Не найден индекс {FamilyName(destination)} для интерфейса {networkAdapter.Name}.");
        }

        var known = _managedRoutes.FirstOrDefault(item =>
            item.Destination.Equals(destination) &&
            item.InterfaceIndex == interfaceIndex &&
            item.NextHop.Equals(nextHop));
        if (known is null)
        {
            var created = await AddRouteAsync(destination, networkAdapter, nextHop, cancellationToken);
            _managedRoutes.Add(new ManagedRoute(
                destination,
                interfaceIndex,
                networkAdapter.Name,
                nextHop,
                created));
        }

        var verification = await VerifyHostRouteAsync(destination, networkAdapter, nextHop, cancellationToken);
        if (!verification.Verified)
        {
            throw new InvalidOperationException(
                $"Маршрут к VPN endpoint {destination} не проходит через {networkAdapter.Name}: {verification.Detail}");
        }
        DiagnosticLog.Info("route.verified", "Маршрут к VPN endpoint проверен.", verification);
        return verification;
    }

    public async Task RemoveCreatedRoutesAsync(CancellationToken cancellationToken = default)
    {
        using var operation = DiagnosticLog.Begin("route.cleanup", "Удаление временных маршрутов.", new
        {
            count = _managedRoutes.Count(item => item.CreatedByBottle)
        });
        foreach (var route in _managedRoutes
                     .Where(item => item.CreatedByBottle)
                     .Reverse()
                     .ToArray())
        {
            await RemoveRouteAsync(route, cancellationToken);
            _managedRoutes.Remove(route);
            DiagnosticLog.Info("route.removed", "Временный маршрут удалён.", route);
        }
    }

    /// <summary>
    /// Добавление маршрута: сначала через службу bottle-route-helper (без прав
    /// администратора), при её недоступности — прежним способом платформы.
    /// </summary>
    private static async Task<bool> AddRouteAsync(
        IPAddress destination,
        NetworkAdapterInfo networkAdapter,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        var response = await RouteHelperClient.TrySendAsync(new RouteHelperRequest(
            "add",
            Family: destination.AddressFamily == AddressFamily.InterNetwork ? "inet" : "inet6",
            Destination: destination.ToString(),
            InterfaceName: networkAdapter.Name,
            InterfaceIndex: GetInterfaceIndex(networkAdapter, destination),
            Gateway: nextHop.ToString()), TimeSpan.FromSeconds(8), cancellationToken);
        if (response is not null)
        {
            if (!response.Ok)
            {
                throw new InvalidOperationException(
                    $"Служба маршрутов отказала: {response.Error ?? "без деталей"}");
            }
            return response.Created == true;
        }
        RouteHelperClient.MarkUnavailable();
        DiagnosticLog.Warning("route.helper-fallback", "Служба маршрутов недоступна, используется прежний способ.");
        return OperatingSystem.IsMacOS()
            ? await AddMacRouteAsync(destination, networkAdapter, nextHop, cancellationToken)
            : OperatingSystem.IsWindows()
                ? await AddWindowsRouteAsync(destination, networkAdapter, nextHop, cancellationToken)
                : throw new PlatformNotSupportedException("Маршруты поддерживаются только на Windows и macOS.");
    }

    private static async Task RemoveRouteAsync(ManagedRoute route, CancellationToken cancellationToken)
    {
        var response = await RouteHelperClient.TrySendAsync(new RouteHelperRequest(
            "del",
            Family: route.Destination.AddressFamily == AddressFamily.InterNetwork ? "inet" : "inet6",
            Destination: route.Destination.ToString(),
            InterfaceName: route.InterfaceName,
            InterfaceIndex: route.InterfaceIndex,
            Gateway: route.NextHop.ToString()), TimeSpan.FromSeconds(8), cancellationToken);
        if (response is not null)
        {
            if (!response.Ok)
            {
                throw new InvalidOperationException(
                    $"Служба маршрутов отказала: {response.Error ?? "без деталей"}");
            }
            return;
        }
        RouteHelperClient.MarkUnavailable();
        if (OperatingSystem.IsMacOS())
        {
            await RemoveMacRouteAsync(route, cancellationToken);
        }
        else
        {
            await RemoveWindowsRouteAsync(route, cancellationToken);
        }
    }

    private static async Task<bool> AddWindowsRouteAsync(
        IPAddress destination,
        NetworkAdapterInfo networkAdapter,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        var prefix = GetPrefix(destination);
        var script = string.Join(' ',
            "$existing = Get-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            $"-InterfaceIndex {GetInterfaceIndex(networkAdapter, destination)}",
            "-ErrorAction SilentlyContinue |",
            $"Where-Object {{ $_.NextHop -eq '{nextHop}' }};",
            "if ($existing) { exit 10 };",
            "New-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            $"-InterfaceIndex {GetInterfaceIndex(networkAdapter, destination)}",
            $"-NextHop '{nextHop}'",
            "-RouteMetric 1",
            "-PolicyStore ActiveStore",
            "-ErrorAction Stop | Out-Null");

        var result = await RunPowerShellAsync(script, cancellationToken);
        if (result.ExitCode == 10)
        {
            return false;
        }
        EnsureSuccess(result, $"Не удалось добавить маршрут {prefix}.");
        return true;
    }

    private static async Task<bool> AddMacRouteAsync(
        IPAddress destination,
        NetworkAdapterInfo networkAdapter,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        var result = await CommandRunner.RunMacAdminAsync(
            "/sbin/route",
            [
                "-n", "add", GetMacFamily(destination), "-host",
                destination.ToString(), FormatMacGateway(nextHop, networkAdapter.Name)
            ],
            cancellationToken);

        var combinedOutput = result.StandardOutput + result.StandardError;
        if (result.ExitCode != 0 && combinedOutput.Contains("File exists", StringComparison.OrdinalIgnoreCase))
        {
            return false;
        }
        EnsureSuccess(result, $"Не удалось добавить маршрут {GetPrefix(destination)} через {networkAdapter.Name}.");
        return true;
    }

    private static async Task<RouteVerification> VerifyHostRouteAsync(
        IPAddress destination,
        NetworkAdapterInfo networkAdapter,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        if (OperatingSystem.IsWindows())
        {
            var script = string.Join(' ',
                "$match = Find-NetRoute",
                $"-RemoteIPAddress '{destination}'",
                "-ErrorAction SilentlyContinue -ErrorVariable lookupError | Select-Object -First 1;",
                "if ($lookupError) { exit 12 };",
                "if (-not $match) { exit 11 };",
                "Write-Output \"$($match.InterfaceIndex)|$($match.NextHop)\"");
            var result = await RunPowerShellAsync(script, cancellationToken);
            if (result.ExitCode == 11)
            {
                return FailedVerification(destination, networkAdapter, nextHop, "маршрут не найден");
            }
            if (result.ExitCode == 0)
            {
                var parts = result.StandardOutput.Trim().Split('|', 2);
                var actualIndex = parts.Length > 0 && int.TryParse(parts[0], out var index) ? index : -1;
                var actualGateway = parts.Length > 1 ? NormalizeGateway(parts[1]) : string.Empty;
                var verified = actualIndex == GetInterfaceIndex(networkAdapter, destination) && GatewayEquals(actualGateway, nextHop);
                var detail = $"ifIndex {actualIndex}, шлюз {actualGateway}";
                return new RouteVerification(destination, FamilyName(destination), actualIndex,
                    networkAdapter.Name, nextHop, verified, detail);
            }

            // Find-NetRoute не выполнился (например, без прав администратора) —
            // сверяем таблицу маршрутов напрямую через Get-NetRoute.
            return await VerifyWindowsByTableAsync(destination, networkAdapter, nextHop, cancellationToken);
        }

        if (OperatingSystem.IsMacOS())
        {
            var result = await CommandRunner.RunAsync(
                "/sbin/route",
                ["-n", "get", GetMacFamily(destination), destination.ToString()],
                cancellationToken);
            if (result.ExitCode != 0)
            {
                return FailedVerification(destination, networkAdapter, nextHop, ResultDetail(result, "маршрут не найден"));
            }

            var values = result.StandardOutput.Split('\n', StringSplitOptions.RemoveEmptyEntries)
                .Select(line => line.Split(':', 2, StringSplitOptions.TrimEntries))
                .Where(parts => parts.Length == 2)
                .ToDictionary(parts => parts[0], parts => parts[1], StringComparer.OrdinalIgnoreCase);
            values.TryGetValue("interface", out var actualInterface);
            values.TryGetValue("gateway", out var actualGateway);
            actualGateway = NormalizeGateway(actualGateway ?? string.Empty);
            var verified = string.Equals(actualInterface, networkAdapter.Name, StringComparison.OrdinalIgnoreCase) &&
                           GatewayEquals(actualGateway, nextHop);
            var detail = $"интерфейс {actualInterface ?? "не определён"}, шлюз {actualGateway}";
            return new RouteVerification(destination, FamilyName(destination), GetInterfaceIndex(networkAdapter, destination),
                actualInterface ?? networkAdapter.Name, nextHop, verified, detail);
        }

        throw new PlatformNotSupportedException("Проверка маршрутов поддерживается только на Windows и macOS.");
    }

    private static async Task<RouteVerification> VerifyWindowsByTableAsync(
        IPAddress destination,
        NetworkAdapterInfo networkAdapter,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        var prefix = GetPrefix(destination);
        var script = string.Join(' ',
            "$route = Get-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            "-ErrorAction SilentlyContinue |",
            $"Where-Object {{ $_.NextHop -eq '{nextHop}' -and $_.InterfaceIndex -eq {GetInterfaceIndex(networkAdapter, destination)} }} |",
            "Select-Object -First 1;",
            "if ($route) { Write-Output 'found' } else { exit 11 }");
        var result = await RunPowerShellAsync(script, cancellationToken);
        return result.ExitCode == 0
            ? new RouteVerification(destination, FamilyName(destination), GetInterfaceIndex(networkAdapter, destination),
                networkAdapter.Name, nextHop, true, $"маршрут {prefix} подтверждён в таблице")
            : FailedVerification(destination, networkAdapter, nextHop, "маршрут не найден в таблице");
    }

    private static RouteVerification FailedVerification(
        IPAddress destination,
        NetworkAdapterInfo adapter,
        IPAddress nextHop,
        string detail) => new(destination, FamilyName(destination), GetInterfaceIndex(adapter, destination),
        adapter.Name, nextHop, false, detail);

    private static async Task RemoveWindowsRouteAsync(ManagedRoute route, CancellationToken cancellationToken)
    {
        var script = string.Join(' ',
            "Get-NetRoute",
            $"-DestinationPrefix '{GetPrefix(route.Destination)}'",
            $"-InterfaceIndex {route.InterfaceIndex}",
            "-ErrorAction SilentlyContinue |",
            $"Where-Object {{ $_.NextHop -eq '{route.NextHop}' }} |",
            "Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue");
        await RunPowerShellAsync(script, cancellationToken);
    }

    private static async Task RemoveMacRouteAsync(ManagedRoute route, CancellationToken cancellationToken)
    {
        var result = await CommandRunner.RunMacAdminAsync(
            "/sbin/route",
            [
                "-n", "delete", GetMacFamily(route.Destination), "-host",
                route.Destination.ToString(), FormatMacGateway(route.NextHop, route.InterfaceName)
            ],
            cancellationToken);
        var combinedOutput = result.StandardOutput + result.StandardError;
        if (result.ExitCode != 0 && !combinedOutput.Contains("not in table", StringComparison.OrdinalIgnoreCase))
        {
            EnsureSuccess(result, $"Не удалось удалить маршрут {GetPrefix(route.Destination)}.");
        }
    }

    private static Task<CommandResult> RunPowerShellAsync(string script, CancellationToken cancellationToken) =>
        CommandRunner.RunAsync(
            "powershell.exe",
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script],
            cancellationToken);

    private static string FormatMacGateway(IPAddress gateway, string interfaceName) =>
        gateway.AddressFamily == AddressFamily.InterNetworkV6 && gateway.IsIPv6LinkLocal
            ? $"{gateway}%{interfaceName}"
            : gateway.ToString();

    private static string NormalizeGateway(string value)
    {
        var normalized = value.Trim();
        var scopeIndex = normalized.IndexOf('%');
        return scopeIndex >= 0 ? normalized[..scopeIndex] : normalized;
    }

    private static bool GatewayEquals(string actual, IPAddress expected) =>
        IPAddress.TryParse(actual, out var parsed) &&
        parsed.AddressFamily == expected.AddressFamily &&
        parsed.GetAddressBytes().SequenceEqual(expected.GetAddressBytes());

    private static int GetInterfaceIndex(NetworkAdapterInfo adapter, IPAddress destination) =>
        destination.AddressFamily == AddressFamily.InterNetwork
            ? adapter.InterfaceIndex
            : adapter.Ipv6InterfaceIndex;

    private static string GetPrefix(IPAddress address) =>
        address + (address.AddressFamily == AddressFamily.InterNetwork ? "/32" : "/128");

    private static string GetMacFamily(IPAddress address) =>
        address.AddressFamily == AddressFamily.InterNetwork ? "-inet" : "-inet6";

    private static string FamilyName(IPAddress address) =>
        address.AddressFamily == AddressFamily.InterNetwork ? "IPv4" : "IPv6";

    private static string ResultDetail(CommandResult result, string fallback)
    {
        var detail = string.IsNullOrWhiteSpace(result.StandardError)
            ? result.StandardOutput.Trim()
            : result.StandardError.Trim();
        return string.IsNullOrWhiteSpace(detail) ? fallback : detail;
    }

    private static void EnsureSuccess(CommandResult result, string message)
    {
        if (result.ExitCode != 0)
        {
            throw new InvalidOperationException($"{message} {ResultDetail(result, string.Empty)}".Trim());
        }
    }

    private static void EnsureSupportedAddress(IPAddress address, string parameterName)
    {
        if (address.AddressFamily is not (AddressFamily.InterNetwork or AddressFamily.InterNetworkV6))
        {
            throw new ArgumentException("Поддерживаются только IPv4 и IPv6 маршруты.", parameterName);
        }
    }
}
