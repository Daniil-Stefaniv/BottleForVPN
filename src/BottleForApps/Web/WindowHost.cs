using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using BottleForApps.Infrastructure;
using Photino.NET;

namespace BottleForApps.Web;

/// <summary>
/// Нативное окно Photino вокруг локального веб-интерфейса.
/// WaitForClose выполняет цикл сообщений и поэтому должен вызываться с главного потока.
/// </summary>
internal static class WindowHost
{
    public static bool IsSupported => OperatingSystem.IsWindows() || OperatingSystem.IsMacOS();

    /// <summary>
    /// Открывает окно и блокирует поток до его закрытия.
    /// Возвращает false, если окно создать не удалось (вызывающий код откроет браузер).
    /// </summary>
    public static bool TryRun(
        string url,
        Func<int> activeSessionCount,
        Action onClosed)
    {
        try
        {
            if (OperatingSystem.IsWindows())
            {
                WindowsConsole.Hide();
            }

            var iconPath = Path.Combine(AppContext.BaseDirectory, "bottle.ico");
            var window = new PhotinoWindow()
                .SetTitle("BottleForApps")
                .SetSize(1180, 760)
                .Center()
                .SetResizable(true)
                .SetGrantBrowserPermissions(true)
                .Load(new Uri(url));
            // Иконку задаём только при наличии файла: пустая строка вызывает исключение.
            if (File.Exists(iconPath))
            {
                window.SetIconFile(iconPath);
            }

            window.RegisterWindowClosingHandler((_, _) =>
            {
                var active = activeSessionCount();
                if (active == 0)
                {
                    return true;
                }
                var answer = window.ShowMessage(
                    "BottleForApps",
                    $"Активных подключений: {active}. Закрыть приложение и остановить их?",
                    PhotinoDialogButtons.YesNo,
                    PhotinoDialogIcon.Warning);
                return answer == PhotinoDialogResult.Yes;
            });
            window.RegisterWindowCreatedHandler((_, _) => Console.WriteLine("Окно BottleForApps открыто."));
            window.WaitForClose();
            onClosed();
            return true;
        }
        catch (Exception exception)
        {
            DiagnosticLog.Error("gui.window.error", "Не удалось открыть нативное окно.", exception);
            return false;
        }
    }
}

[SupportedOSPlatform("windows")]
internal static class WindowsConsole
{
    private const int SwHide = 0;

    [DllImport("kernel32.dll")]
    private static extern IntPtr GetConsoleWindow();

    [DllImport("user32.dll")]
    private static extern bool ShowWindow(IntPtr hWnd, int command);

    public static void Hide()
    {
        var console = GetConsoleWindow();
        if (console != IntPtr.Zero)
        {
            ShowWindow(console, SwHide);
        }
    }
}
