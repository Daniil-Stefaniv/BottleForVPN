using System.Runtime.InteropServices;
using BottleRouteHelper;

if (args.Length > 0 && args[0] is not ("run" or "console"))
{
    if (args[0] is "help" or "--help" or "-h")
    {
        Console.WriteLine("bottle-route-helper — привилегированная служба маршрутов BottleForApps.");
        Console.WriteLine("Обычно запускается системой (launchd на macOS, SCM на Windows).");
        Console.WriteLine("Для ручного запуска в консоли: bottle-route-helper run");
        return 0;
    }
    Console.Error.WriteLine($"Неизвестный аргумент '{args[0]}'. Используйте bottle-route-helper [run].");
    return 1;
}

// Запуск SCM: сервис всегда стартует без аргументов и в неинтерактивной сессии.
if (OperatingSystem.IsWindows() && !Environment.UserInteractive)
{
    return WindowsServiceHost.Run();
}

using var cancellation = new CancellationTokenSource();
Console.CancelKeyPress += (_, eventArgs) =>
{
    eventArgs.Cancel = true;
    cancellation.Cancel();
};
if (OperatingSystem.IsMacOS())
{
    PosixSignalRegistration.Create(PosixSignal.SIGTERM, (_) => cancellation.Cancel());
}
return await HelperServer.RunAsync(cancellation.Token);
