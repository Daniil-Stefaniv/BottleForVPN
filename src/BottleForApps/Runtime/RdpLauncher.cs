using System.Diagnostics;
using System.Text;
using BottleForApps.Domain;
using BottleForApps.Infrastructure;

namespace BottleForApps.Runtime;

internal sealed class RdpLauncher
{
    public async Task<string?> LaunchAsync(
        BottleDefinition bottle,
        int localPort,
        CancellationToken cancellationToken)
    {
        using var operation = DiagnosticLog.Begin("rdp.launch", "Запуск RDP-клиента.", new { bottle.Id, localPort });
        if (OperatingSystem.IsWindows())
        {
            _ = Process.Start(new ProcessStartInfo
            {
                FileName = "mstsc.exe",
                UseShellExecute = true,
                ArgumentList = { $"/v:127.0.0.1:{localPort}" }
            }) ?? throw new InvalidOperationException("Не удалось запустить mstsc.exe.");
            DiagnosticLog.Info("rdp.launched", "mstsc.exe запущен.", new { bottle.Id, localPort });
            return null;
        }

        if (OperatingSystem.IsMacOS())
        {
            var directory = Path.Combine(Path.GetTempPath(), "BottleForApps");
            Directory.CreateDirectory(directory);
            var path = Path.Combine(directory, $"{bottle.Id}-{Environment.ProcessId}.rdp");
            var contents = string.Join("\r\n",
                $"full address:s:127.0.0.1:{localPort}",
                "prompt for credentials:i:1",
                "administrative session:i:0",
                "redirectclipboard:i:1",
                "audiomode:i:0",
                string.Empty);
            await File.WriteAllTextAsync(path, contents, new UTF8Encoding(encoderShouldEmitUTF8Identifier: false), cancellationToken);

            var result = await CommandRunner.RunAsync("/usr/bin/open", [path], cancellationToken);
            if (result.ExitCode != 0)
            {
                File.Delete(path);
                throw new InvalidOperationException(
                    "Не удалось открыть RDP-файл. Установите Windows App. " + result.StandardError.Trim());
            }
            DiagnosticLog.Info("rdp.launched", "RDP-файл открыт в macOS.", new { bottle.Id, path });
            return path;
        }

        throw new PlatformNotSupportedException("RDP-клиент поддерживается только на Windows и macOS.");
    }
}
