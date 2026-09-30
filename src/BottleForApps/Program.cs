using BottleForApps.Cli;
using BottleForApps.Infrastructure;
using BottleForApps.Web;

DiagnosticLog.Initialize();
AppDomain.CurrentDomain.UnhandledException += (_, eventArgs) =>
{
    if (eventArgs.ExceptionObject is Exception exception)
    {
        DiagnosticLog.Error("app.unhandled", "Необработанное исключение.", exception, new { eventArgs.IsTerminating });
    }
};
TaskScheduler.UnobservedTaskException += (_, eventArgs) =>
{
    DiagnosticLog.Error("app.unobserved-task", "Необработанное исключение фоновой задачи.", eventArgs.Exception);
};

try
{
    // GUI должен получить главный поток: нативное окно (macOS) требует цикл сообщений на нём.
    if (args.Length == 0 || args[0].Equals("gui", StringComparison.OrdinalIgnoreCase))
    {
        var browserMode = args.Any(argument => argument.Equals("browser", StringComparison.OrdinalIgnoreCase));
        return GuiApplication.Run(browserMode);
    }
    return Application.RunAsync(args).GetAwaiter().GetResult();
}
catch (Exception exception)
{
    DiagnosticLog.Error("app.fatal", "Критическая ошибка приложения.", exception);
    Console.Error.WriteLine($"Критическая ошибка: {exception.Message}");
    Console.Error.WriteLine($"Журнал: {DiagnosticLog.FilePath}");
    return 1;
}
finally
{
    DiagnosticLog.Shutdown();
}
