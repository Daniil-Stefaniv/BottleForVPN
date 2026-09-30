using System.Diagnostics;

namespace BottleForApps.Infrastructure;

internal sealed record CommandResult(int ExitCode, string StandardOutput, string StandardError);

internal static class CommandRunner
{
    public static Task<CommandResult> RunMacAdminAsync(
        string fileName,
        IEnumerable<string> arguments,
        CancellationToken cancellationToken = default)
    {
        static string Quote(string value) => "'" + value.Replace("'", "'\"'\"'") + "'";
        var command = string.Join(' ', new[] { fileName }.Concat(arguments).Select(Quote));
        var script = $"do shell script {ToAppleScriptString(command)} with administrator privileges";
        return RunAsync("/usr/bin/osascript", ["-e", script], cancellationToken);
    }

    public static async Task<CommandResult> RunAsync(
        string fileName,
        IEnumerable<string> arguments,
        CancellationToken cancellationToken = default)
    {
        var argumentList = arguments.ToArray();
        using var operation = DiagnosticLog.Begin("process.run", "Запуск внешней команды.", new
        {
            fileName,
            arguments = SafeArguments(fileName, argumentList)
        });
        var startInfo = new ProcessStartInfo
        {
            FileName = fileName,
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true
        };

        foreach (var argument in argumentList)
        {
            startInfo.ArgumentList.Add(argument);
        }

        using var process = new Process { StartInfo = startInfo };
        try
        {
            if (!process.Start())
            {
                throw new InvalidOperationException($"Не удалось запустить {fileName}.");
            }

            var stdout = process.StandardOutput.ReadToEndAsync(cancellationToken);
            var stderr = process.StandardError.ReadToEndAsync(cancellationToken);
            await process.WaitForExitAsync(cancellationToken);
            var result = new CommandResult(process.ExitCode, await stdout, await stderr);
            DiagnosticLog.Info("process.result", "Внешняя команда завершена.", new
            {
                fileName,
                result.ExitCode,
                standardOutput = Truncate(result.StandardOutput),
                standardError = Truncate(result.StandardError)
            });
            return result;
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            DiagnosticLog.Error("process.error", "Не удалось выполнить внешнюю команду.", exception, new { fileName });
            throw;
        }
    }

    private static object SafeArguments(string fileName, IReadOnlyList<string> arguments) =>
        Path.GetFileName(fileName).Equals("osascript", StringComparison.OrdinalIgnoreCase)
            ? new { count = arguments.Count, redacted = true }
            : new { count = arguments.Count, values = arguments.Select(Truncate).ToArray(), redacted = false };

    private static string Truncate(string value) => value.Length <= 4000 ? value : value[..4000] + "…";

    private static string ToAppleScriptString(string value) =>
        "\"" + value.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"";
}
