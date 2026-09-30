using System.IO.Compression;
using System.Reflection;
using System.Text.Json;

namespace BottleForApps.Infrastructure;

internal static class SupportBundle
{
    public static async Task<string> CreateAsync(string? requestedPath, CancellationToken cancellationToken)
    {
        var path = string.IsNullOrWhiteSpace(requestedPath)
            ? Path.Combine(DiagnosticLog.DirectoryPath, $"bottle-support-{DateTimeOffset.Now:yyyyMMdd-HHmmss}.zip")
            : Path.GetFullPath(requestedPath);
        if (!path.EndsWith(".zip", StringComparison.OrdinalIgnoreCase))
        {
            path += ".zip";
        }

        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        DiagnosticLog.Info("support.export", "Создание диагностического архива.", new { path });

        await using var output = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.None);
        using var archive = new ZipArchive(output, ZipArchiveMode.Create, leaveOpen: true);

        var logFiles = Directory.Exists(DiagnosticLog.DirectoryPath)
            ? Directory.EnumerateFiles(DiagnosticLog.DirectoryPath, "bottle-*.log")
                .OrderByDescending(File.GetLastWriteTimeUtc)
                .Take(10)
                .ToArray()
            : [];
        foreach (var logPath in logFiles)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var entry = archive.CreateEntry("logs/" + Path.GetFileName(logPath), CompressionLevel.Optimal);
            await using var destination = entry.Open();
            await using var source = new FileStream(logPath, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            await source.CopyToAsync(destination, cancellationToken);
        }

        var diagnostics = archive.CreateEntry("system.json", CompressionLevel.Optimal);
        await using (var stream = diagnostics.Open())
        {
            await JsonSerializer.SerializeAsync(stream, new
            {
                generatedAt = DateTimeOffset.Now,
                applicationVersion = Assembly.GetExecutingAssembly().GetName().Version?.ToString(),
                operatingSystem = Environment.OSVersion.ToString(),
                framework = System.Runtime.InteropServices.RuntimeInformation.FrameworkDescription,
                processArchitecture = System.Runtime.InteropServices.RuntimeInformation.ProcessArchitecture.ToString(),
                osArchitecture = System.Runtime.InteropServices.RuntimeInformation.OSArchitecture.ToString(),
                processorCount = Environment.ProcessorCount,
                networkAvailable = System.Net.NetworkInformation.NetworkInterface.GetIsNetworkAvailable(),
                logDirectory = DiagnosticLog.DirectoryPath
            }, cancellationToken: cancellationToken);
        }

        var readme = archive.CreateEntry("README.txt", CompressionLevel.Optimal);
        await using (var writer = new StreamWriter(readme.Open()))
        {
            await writer.WriteAsync(
                "Диагностический архив BottleForApps.\n" +
                "Он содержит технические журналы запусков и сведения о системе.\n" +
                "Пароли приложение не хранит и не записывает, однако журналы содержат пути, имена интерфейсов, IP-адреса и VPN endpoint.\n");
        }

        DiagnosticLog.Info("support.export.complete", "Диагностический архив создан.", new { path, logCount = logFiles.Length });
        return path;
    }
}
