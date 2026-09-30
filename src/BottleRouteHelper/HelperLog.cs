using System.Text;

namespace BottleRouteHelper;

/// <summary>
/// Минимальный журнал службы: строки с временем в файл рядом с журналом маршрутов
/// и в консоль (launchd/консольный режим). При старте файл урезается до 1 МБ.
/// </summary>
internal static class HelperLog
{
    private const string EnvironmentVariable = "BOTTLE_ROUTE_LOG";
    private const long MaxBytes = 1024 * 1024;
    private static readonly object Sync = new();
    private static readonly string? LogPath = ResolveLogPath();

    public static void Info(string message) => Write("INFO", message);
    public static void Warning(string message) => Write("WARN", message);
    public static void Error(string message) => Write("ERROR", message);

    private static void Write(string level, string message)
    {
        var line = $"{DateTimeOffset.UtcNow:yyyy-MM-dd HH:mm:ss} {level,-5} {message}";
        lock (Sync)
        {
            Console.WriteLine(line);
            if (LogPath is null)
            {
                return;
            }
            try
            {
                File.AppendAllText(LogPath, line + Environment.NewLine, Encoding.UTF8);
            }
            catch (Exception)
            {
                // Файловый журнал не критичен: каталог может быть недоступен в консольном режиме.
            }
        }
    }

    private static string? ResolveLogPath()
    {
        try
        {
            var path = Environment.GetEnvironmentVariable(EnvironmentVariable) is { Length: > 0 } environmentValue
                ? environmentValue
                : OperatingSystem.IsMacOS()
                    ? "/var/db/bottleforapps-helper.log"
                    : Path.Combine(
                        Environment.GetFolderPath(Environment.SpecialFolder.CommonApplicationData),
                        "BottleForApps", "helper.log");
            var directory = Path.GetDirectoryName(path);
            if (!string.IsNullOrEmpty(directory))
            {
                Directory.CreateDirectory(directory);
            }
            var fileInfo = new FileInfo(path);
            if (fileInfo.Exists && fileInfo.Length > MaxBytes)
            {
                fileInfo.Delete();
            }
            return path;
        }
        catch (Exception)
        {
            return null;
        }
    }
}
