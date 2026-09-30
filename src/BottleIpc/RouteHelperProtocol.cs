using System.Text.Json;

namespace BottleIpc;

/// <summary>
/// Запрос к привилегированной службе маршрутов.
/// Op: ping | add | del | cleanup.
/// </summary>
public sealed record RouteHelperRequest(
    string Op,
    string? Family = null,
    string? Destination = null,
    string? InterfaceName = null,
    int? InterfaceIndex = null,
    string? Gateway = null,
    string? Owner = null);

public sealed record RouteHelperResponse(
    bool Ok,
    bool? Created = null,
    int? Removed = null,
    string? Error = null,
    string? Detail = null,
    int? Version = null);

/// <summary>
/// Общий протокол приложения BottleForApps и службы bottle-route-helper:
/// одна JSON-строка (UTF-8, завершение \n) на запрос и одну строку на ответ.
/// </summary>
public static class RouteHelperProtocol
{
    public const int Version = 1;
    public const string DefaultUnixSocketPath = "/var/run/bottleforapps-routed.sock";
    public const string DefaultWindowsPipeName = "bottleforapps-route";
    public const string UnixSocketPathEnvironmentVariable = "BOTTLE_ROUTE_SOCK";
    public const string WindowsPipeNameEnvironmentVariable = "BOTTLE_ROUTE_PIPE";
    public const int MaxLineBytes = 4096;

    public static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);

    /// <summary>Путь к unix-сокету (macOS); переопределяется переменной окружения для отладки.</summary>
    public static string? UnixSocketPath =>
        Environment.GetEnvironmentVariable(UnixSocketPathEnvironmentVariable) is { Length: > 0 } environmentValue
            ? environmentValue
            : OperatingSystem.IsWindows()
                ? null
                : DefaultUnixSocketPath;

    /// <summary>Имя именованного канала (Windows); переопределяется переменной окружения для отладки.</summary>
    public static string WindowsPipeName =>
        Environment.GetEnvironmentVariable(WindowsPipeNameEnvironmentVariable) is { Length: > 0 } environmentValue
            ? environmentValue
            : DefaultWindowsPipeName;
}
