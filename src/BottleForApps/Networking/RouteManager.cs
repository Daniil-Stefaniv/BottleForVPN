using System.Net;
using BottleForApps.Infrastructure;

namespace BottleForApps.Networking;

internal sealed record ManagedRoute(IPAddress Destination, int InterfaceIndex, IPAddress NextHop);

internal sealed class RouteManager
{
    private readonly List<ManagedRoute> _createdRoutes = [];

    public async Task AddHostRouteAsync(
        IPAddress destination,
        int interfaceIndex,
        IPAddress nextHop,
        CancellationToken cancellationToken)
    {
        EnsureIpv4(destination, nameof(destination));
        EnsureIpv4(nextHop, nameof(nextHop));

        var prefix = destination + "/32";
        var script = string.Join(' ',
            "$existing = Get-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            $"-InterfaceIndex {interfaceIndex}",
            "-ErrorAction SilentlyContinue |",
            $"Where-Object {{ $_.NextHop -eq '{nextHop}' }};",
            "if ($existing) { exit 10 };",
            "New-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            $"-InterfaceIndex {interfaceIndex}",
            $"-NextHop '{nextHop}'",
            "-RouteMetric 1",
            "-PolicyStore ActiveStore",
            "-ErrorAction Stop | Out-Null");

        var result = await RunPowerShellAsync(script, cancellationToken);
        if (result.ExitCode == 10)
        {
            return;
        }
        EnsureSuccess(result, $"Не удалось добавить маршрут {prefix}.");
        _createdRoutes.Add(new ManagedRoute(destination, interfaceIndex, nextHop));
    }

    public async Task RemoveCreatedRoutesAsync(CancellationToken cancellationToken = default)
    {
        foreach (var route in _createdRoutes.AsEnumerable().Reverse().ToArray())
        {
            var prefix = route.Destination + "/32";
            var script = string.Join(' ',
                "Get-NetRoute",
                $"-DestinationPrefix '{prefix}'",
                $"-InterfaceIndex {route.InterfaceIndex}",
                "-ErrorAction SilentlyContinue |",
                $"Where-Object {{ $_.NextHop -eq '{route.NextHop}' }} |",
                "Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue");
            await RunPowerShellAsync(script, cancellationToken);
            _createdRoutes.Remove(route);
        }
    }

    private static Task<CommandResult> RunPowerShellAsync(string script, CancellationToken cancellationToken) =>
        CommandRunner.RunAsync(
            "powershell.exe",
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script],
            cancellationToken);

    private static void EnsureSuccess(CommandResult result, string message)
    {
        if (result.ExitCode != 0)
        {
            var detail = string.IsNullOrWhiteSpace(result.StandardError)
                ? result.StandardOutput.Trim()
                : result.StandardError.Trim();
            throw new InvalidOperationException($"{message} {detail}".Trim());
        }
    }

    private static void EnsureIpv4(IPAddress address, string parameterName)
    {
        if (address.AddressFamily != System.Net.Sockets.AddressFamily.InterNetwork)
        {
            throw new ArgumentException("В MVP поддерживаются только IPv4-маршруты.", parameterName);
        }
    }
}
