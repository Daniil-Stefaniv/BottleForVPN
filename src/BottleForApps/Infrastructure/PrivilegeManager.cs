using System.Security.Principal;
using BottleIpc;

namespace BottleForApps.Infrastructure;

internal static class PrivilegeManager
{
    public static async Task EnsureAsync(CancellationToken cancellationToken)
    {
        using var operation = DiagnosticLog.Begin("privileges.check", "Проверка системных прав.");
        if (OperatingSystem.IsWindows())
        {
            using var identity = WindowsIdentity.GetCurrent();
            if (new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator))
            {
                DiagnosticLog.Info("privileges.windows", "Процесс запущен с правами администратора.");
                return;
            }
        }

        if (await RouteHelperClient.IsAvailableAsync(cancellationToken))
        {
            DiagnosticLog.Info("privileges.helper", "Служба маршрутов доступна, повышение прав не требуется.");
            return;
        }

        if (OperatingSystem.IsWindows())
        {
            throw new UnauthorizedAccessException(
                "Служба маршрутов BottleForApps недоступна, а процесс запущен без прав администратора. " +
                "Переустановите приложение, чтобы служба была зарегистрирована, или запустите терминал от имени администратора.");
        }

        if (OperatingSystem.IsMacOS())
        {
            Console.WriteLine("Служба маршрутов недоступна; macOS может запросить разрешение на изменение маршрутов...");
            var result = await CommandRunner.RunMacAdminAsync("/usr/bin/true", [], cancellationToken);
            if (result.ExitCode != 0)
            {
                throw new UnauthorizedAccessException(
                    "Не удалось получить право на изменение таблицы маршрутов: " + result.StandardError.Trim());
            }
            DiagnosticLog.Info("privileges.macos", "Разрешение на изменение маршрутов получено (без службы).");
            return;
        }

        throw new PlatformNotSupportedException("Запуск поддерживается только на Windows и macOS.");
    }

    /// <summary>
    /// Просит службу удалить все созданные ею маршруты (после аварийного завершения приложения).
    /// Без службы на macOS потребует пароль администратора.
    /// </summary>
    public static async Task<bool> CleanupOrphanedRoutesAsync(CancellationToken cancellationToken)
    {
        var response = await RouteHelperClient.TrySendAsync(
            new RouteHelperRequest("cleanup"), TimeSpan.FromSeconds(30), cancellationToken);
        if (response is { Ok: true })
        {
            return response.Removed > 0;
        }
        RouteHelperClient.MarkUnavailable();
        DiagnosticLog.Warning("route.cleanup.unavailable", "Служба маршрутов недоступна, осиротевшие маршруты не очищены.");
        return false;
    }

    public static bool IsElevated()
    {
        if (!OperatingSystem.IsWindows())
        {
            return false;
        }
        using var identity = WindowsIdentity.GetCurrent();
        return new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator);
    }
}
