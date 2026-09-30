using System.Diagnostics;
using System.Reflection;
using System.Text.Json;

namespace BottleForApps.Infrastructure;

internal static class DiagnosticLog
{
    private static readonly object Sync = new();
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase
    };

    private static StreamWriter? _writer;

    public static string SessionId { get; } = Guid.NewGuid().ToString("N")[..12];
    public static string DirectoryPath { get; private set; } = string.Empty;
    public static string FilePath { get; private set; } = string.Empty;

    public static void Initialize()
    {
        lock (Sync)
        {
            if (_writer is not null)
            {
                return;
            }

            DirectoryPath = ResolveDirectory();
            try
            {
                Directory.CreateDirectory(DirectoryPath);
                DeleteExpiredLogs(DirectoryPath);
                FilePath = Path.Combine(
                    DirectoryPath,
                    $"bottle-{DateTimeOffset.Now:yyyyMMdd-HHmmss}-{Environment.ProcessId}.log");
                var stream = new FileStream(FilePath, FileMode.CreateNew, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete);
                _writer = new StreamWriter(stream) { AutoFlush = true };
            }
            catch (Exception exception)
            {
                DirectoryPath = Path.Combine(Path.GetTempPath(), "BottleForApps", "logs");
                Directory.CreateDirectory(DirectoryPath);
                FilePath = Path.Combine(DirectoryPath, $"bottle-{SessionId}.log");
                var stream = new FileStream(FilePath, FileMode.Create, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete);
                _writer = new StreamWriter(stream) { AutoFlush = true };
                Write("warning", "log.fallback", "Основной каталог журналов недоступен; используется временный.", exception);
            }
        }

        Info("app.start", "BottleForApps запущен.", new
        {
            version = Assembly.GetExecutingAssembly().GetName().Version?.ToString(),
            processId = Environment.ProcessId,
            operatingSystem = Environment.OSVersion.ToString(),
            framework = System.Runtime.InteropServices.RuntimeInformation.FrameworkDescription,
            architecture = System.Runtime.InteropServices.RuntimeInformation.ProcessArchitecture.ToString(),
            executable = Environment.ProcessPath,
            argumentCount = Environment.GetCommandLineArgs().Length - 1
        });
    }

    public static void Info(string operation, string message, object? data = null) =>
        Write("info", operation, message, data: data);

    public static void Warning(string operation, string message, Exception? exception = null, object? data = null) =>
        Write("warning", operation, message, exception, data);

    public static void Error(string operation, string message, Exception exception, object? data = null) =>
        Write("error", operation, message, exception, data);

    public static IDisposable Begin(string operation, string message, object? data = null)
    {
        Info(operation + ".start", message, data);
        return new OperationScope(operation);
    }

    public static void Shutdown()
    {
        lock (Sync)
        {
            if (_writer is null)
            {
                return;
            }
            Write("info", "app.stop", "BottleForApps завершён.");
            _writer.Dispose();
            _writer = null;
        }
    }

    private static void Write(
        string level,
        string operation,
        string message,
        Exception? exception = null,
        object? data = null)
    {
        lock (Sync)
        {
            if (_writer is null)
            {
                return;
            }

            try
            {
                var entry = new
                {
                    timestamp = DateTimeOffset.Now.ToString("O"),
                    level,
                    sessionId = SessionId,
                    threadId = Environment.CurrentManagedThreadId,
                    operation,
                    message,
                    data,
                    exception = exception is null ? null : new
                    {
                        type = exception.GetType().FullName,
                        exception.Message,
                        exception.StackTrace,
                        inner = FlattenInnerExceptions(exception)
                    }
                };
                _writer.WriteLine(JsonSerializer.Serialize(entry, JsonOptions));
            }
            catch
            {
                // Diagnostics must never break the operation being diagnosed.
            }
        }
    }

    private static IReadOnlyList<object> FlattenInnerExceptions(Exception exception)
    {
        var result = new List<object>();
        for (var inner = exception.InnerException; inner is not null; inner = inner.InnerException)
        {
            result.Add(new { type = inner.GetType().FullName, inner.Message, inner.StackTrace });
        }
        return result;
    }

    private static string ResolveDirectory()
    {
        var configured = Environment.GetEnvironmentVariable("BOTTLE_LOG_DIR");
        if (!string.IsNullOrWhiteSpace(configured))
        {
            return Path.GetFullPath(configured);
        }

        return Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "BottleForApps",
            "logs");
    }

    private static void DeleteExpiredLogs(string directory)
    {
        var cutoff = DateTime.UtcNow.AddDays(-30);
        foreach (var path in Directory.EnumerateFiles(directory, "bottle-*.log"))
        {
            try
            {
                if (File.GetLastWriteTimeUtc(path) < cutoff)
                {
                    File.Delete(path);
                }
            }
            catch
            {
                // Old logs are best-effort cleanup.
            }
        }
    }

    private sealed class OperationScope(string operation) : IDisposable
    {
        private readonly Stopwatch _stopwatch = Stopwatch.StartNew();

        public void Dispose() => Info(
            operation + ".finish",
            "Операция завершена.",
            new { elapsedMilliseconds = _stopwatch.ElapsedMilliseconds });
    }
}
