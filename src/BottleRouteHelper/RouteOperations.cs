using System.Runtime.Versioning;
using System.Diagnostics;
using System.Net;
using System.Net.Sockets;

namespace BottleRouteHelper;

internal readonly record struct RouteOperationResult(bool Ok, bool Changed, string Detail);

internal static class RouteOperations
{
    public static Task<RouteOperationResult> AddAsync(ValidatedRoute route) =>
        OperatingSystem.IsMacOS()
            ? MacRouteOperations.AddAsync(route)
            : OperatingSystem.IsWindows()
                ? WindowsRouteOperations.AddAsync(route)
                : Task.FromResult(new RouteOperationResult(false, false, "Поддерживаются только macOS и Windows."));

    public static Task<RouteOperationResult> RemoveAsync(ValidatedRoute route) =>
        OperatingSystem.IsMacOS()
            ? MacRouteOperations.RemoveAsync(route)
            : OperatingSystem.IsWindows()
                ? WindowsRouteOperations.RemoveAsync(route)
                : Task.FromResult(new RouteOperationResult(false, false, "Поддерживаются только macOS и Windows."));
}

[SupportedOSPlatform("macos")]
internal static class MacRouteOperations
{
    public static Task<RouteOperationResult> AddAsync(ValidatedRoute route) => RunRouteAsync(route, "add");

    public static Task<RouteOperationResult> RemoveAsync(ValidatedRoute route) => RunRouteAsync(route, "delete");

    private static async Task<RouteOperationResult> RunRouteAsync(ValidatedRoute route, string verb)
    {
        var result = await ProcessRunner.RunAsync("/sbin/route",
        [
            "-n", verb,
            route.Family == AddressFamily.InterNetwork ? "-inet" : "-inet6",
            "-host",
            route.Destination.ToString(),
            FormatGateway(route.Gateway, route.InterfaceName)
        ]);
        var output = (result.StandardOutput + result.StandardError).Trim();
        if (result.ExitCode == 0)
        {
            return new RouteOperationResult(true, Changed: true, output);
        }
        // Идемпотентность: «уже существует» и «отсутствует в таблице» не считаем ошибкой.
        if (output.Contains("File exists", StringComparison.OrdinalIgnoreCase) ||
            output.Contains("not in table", StringComparison.OrdinalIgnoreCase))
        {
            return new RouteOperationResult(true, Changed: false, output);
        }
        return new RouteOperationResult(false, false,
            $"route {verb} завершился с кодом {result.ExitCode}: {output}");
    }

    private static string FormatGateway(IPAddress gateway, string interfaceName) =>
        gateway.AddressFamily == AddressFamily.InterNetworkV6 && gateway.IsIPv6LinkLocal
            ? $"{gateway}%{interfaceName}"
            : gateway.ToString();
}

[SupportedOSPlatform("windows")]
internal static class WindowsRouteOperations
{
    public static async Task<RouteOperationResult> AddAsync(ValidatedRoute route)
    {
        var prefix = Prefix(route);
        var script = string.Join(' ',
            "$existing = Get-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            $"-InterfaceIndex {route.InterfaceIndex}",
            "-ErrorAction SilentlyContinue |",
            $"Where-Object {{ $_.NextHop -eq '{route.Gateway}' }};",
            "if ($existing) { exit 10 };",
            "New-NetRoute",
            $"-DestinationPrefix '{prefix}'",
            $"-InterfaceIndex {route.InterfaceIndex}",
            $"-NextHop '{route.Gateway}'",
            "-RouteMetric 1",
            "-PolicyStore ActiveStore",
            "-ErrorAction Stop | Out-Null");
        var result = await ProcessRunner.RunAsync("powershell.exe",
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]);
        return result.ExitCode switch
        {
            0 => new RouteOperationResult(true, Changed: true, "маршрут создан"),
            10 => new RouteOperationResult(true, Changed: false, "маршрут уже существует"),
            _ => new RouteOperationResult(false, false,
                $"New-NetRoute завершился с кодом {result.ExitCode}: {Detail(result)}")
        };
    }

    public static async Task<RouteOperationResult> RemoveAsync(ValidatedRoute route)
    {
        var script = string.Join(' ',
            "Get-NetRoute",
            $"-DestinationPrefix '{Prefix(route)}'",
            $"-InterfaceIndex {route.InterfaceIndex}",
            "-ErrorAction SilentlyContinue |",
            $"Where-Object {{ $_.NextHop -eq '{route.Gateway}' }} |",
            "Remove-NetRoute -Confirm:$false -ErrorAction Stop | Out-Null");
        var result = await ProcessRunner.RunAsync("powershell.exe",
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]);
        return result.ExitCode == 0
            ? new RouteOperationResult(true, Changed: true, "маршрут удалён")
            : new RouteOperationResult(false, false,
                $"Remove-NetRoute завершился с кодом {result.ExitCode}: {Detail(result)}");
    }

    private static string Prefix(ValidatedRoute route) =>
        route.Destination + (route.Family == AddressFamily.InterNetwork ? "/32" : "/128");

    private static string Detail(ProcessResult result)
    {
        var text = string.IsNullOrWhiteSpace(result.StandardError)
            ? result.StandardOutput.Trim()
            : result.StandardError.Trim();
        return text.Length > 400 ? text[..400] + "…" : text;
    }
}

internal static class ProcessRunner
{
    public static async Task<ProcessResult> RunAsync(string fileName, IReadOnlyList<string> arguments)
    {
        var startInfo = new ProcessStartInfo
        {
            FileName = fileName,
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true
        };
        foreach (var argument in arguments)
        {
            startInfo.ArgumentList.Add(argument);
        }
        using var process = new Process { StartInfo = startInfo };
        process.Start();
        var stdout = process.StandardOutput.ReadToEndAsync();
        var stderr = process.StandardError.ReadToEndAsync();
        try
        {
            await process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(30));
        }
        catch (TimeoutException)
        {
            process.Kill(entireProcessTree: true);
            await process.WaitForExitAsync(CancellationToken.None);
            throw new InvalidOperationException($"{fileName} не завершился за 30 секунд и был остановлен.");
        }
        return new ProcessResult(process.ExitCode, await stdout, await stderr);
    }
}

internal readonly record struct ProcessResult(int ExitCode, string StandardOutput, string StandardError);
