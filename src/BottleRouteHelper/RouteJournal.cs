using System.Text.Json;

namespace BottleRouteHelper;

/// <summary>Запись о временном маршруте, созданном службой.</summary>
internal sealed record JournaledRoute(
    string Family,
    string Destination,
    string Gateway,
    string InterfaceName,
    int InterfaceIndex,
    string? Owner,
    DateTimeOffset CreatedAtUtc);

/// <summary>
/// Журнал созданных маршрутов на диске: переживает перезапуск службы и позволяет
/// удалить «осиротевшие» маршруты командой cleanup после аварийного завершения приложения.
/// </summary>
internal sealed class RouteJournal
{
    private const string EnvironmentVariable = "BOTTLE_ROUTE_JOURNAL";
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web)
    {
        WriteIndented = true
    };

    public static readonly RouteJournal Current = new(ResolveDefaultPath());

    private readonly string _path;
    private readonly object _sync = new();
    private List<JournaledRoute> _routes = [];

    public RouteJournal(string path)
    {
        _path = path;
        Load();
    }

    public IReadOnlyList<JournaledRoute> Snapshot()
    {
        lock (_sync)
        {
            return _routes.ToArray();
        }
    }

    public void Remember(JournaledRoute route)
    {
        lock (_sync)
        {
            _routes.RemoveAll(item =>
                item.Destination == route.Destination &&
                item.InterfaceIndex == route.InterfaceIndex &&
                item.Gateway == route.Gateway);
            _routes.Add(route);
            Save();
        }
    }

    public void Forget(System.Net.IPAddress destination, System.Net.IPAddress gateway, int interfaceIndex)
    {
        lock (_sync)
        {
            _routes.RemoveAll(item =>
                item.Destination == destination.ToString() &&
                item.Gateway == gateway.ToString() &&
                item.InterfaceIndex == interfaceIndex);
            Save();
        }
    }

    public void Clear()
    {
        lock (_sync)
        {
            _routes.Clear();
            Save();
        }
    }

    private void Load()
    {
        try
        {
            if (!File.Exists(_path))
            {
                return;
            }
            using var stream = File.OpenRead(_path);
            _routes = JsonSerializer.Deserialize<List<JournaledRoute>>(stream, JsonOptions) ?? [];
        }
        catch (Exception exception)
        {
            HelperLog.Warning($"Не удалось прочитать журнал маршрутов ({exception.Message}); начинаю с пустого.");
            _routes = [];
            try
            {
                File.Move(_path, _path + ".bad", overwrite: true);
            }
            catch (Exception)
            {
            }
        }
    }

    private void Save()
    {
        try
        {
            var directory = Path.GetDirectoryName(_path);
            if (!string.IsNullOrEmpty(directory))
            {
                Directory.CreateDirectory(directory);
            }
            var temporary = _path + ".tmp";
            File.WriteAllText(temporary, JsonSerializer.Serialize(_routes, JsonOptions));
            File.Move(temporary, _path, overwrite: true);
        }
        catch (Exception exception)
        {
            // Журнал — вспомогательный механизм; невозможность записи не должна ронять службу.
            HelperLog.Warning("Не удалось сохранить журнал маршрутов: " + exception.Message);
        }
    }

    private static string ResolveDefaultPath() =>
        Environment.GetEnvironmentVariable(EnvironmentVariable) is { Length: > 0 } environmentValue
            ? environmentValue
            : OperatingSystem.IsMacOS()
                ? "/var/db/bottleforapps-routes.json"
                : Path.Combine(
                    Environment.GetFolderPath(Environment.SpecialFolder.CommonApplicationData),
                    "BottleForApps", "routes.json");
}
