using System.Text.Json;
using System.Text.RegularExpressions;
using BottleForApps.Domain;

namespace BottleForApps.Persistence;

internal sealed partial class BottleStore
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        WriteIndented = true
    };

    private readonly string _directory;

    public BottleStore(string? directory = null)
    {
        _directory = directory ?? ResolveDefaultDirectory();
    }

    public string DirectoryPath => _directory;

    public async Task SaveAsync(BottleDefinition bottle, CancellationToken cancellationToken = default)
    {
        ValidateId(bottle.Id);
        Directory.CreateDirectory(_directory);
        var json = JsonSerializer.Serialize(bottle, JsonOptions);
        await File.WriteAllTextAsync(GetPath(bottle.Id), json, cancellationToken);
    }

    public async Task<BottleDefinition> LoadAsync(string id, CancellationToken cancellationToken = default)
    {
        ValidateId(id);
        var path = GetPath(id);
        if (!File.Exists(path))
        {
            throw new FileNotFoundException($"Бутылка '{id}' не найдена.", path);
        }

        await using var stream = File.OpenRead(path);
        return await JsonSerializer.DeserializeAsync<BottleDefinition>(stream, JsonOptions, cancellationToken)
               ?? throw new InvalidDataException($"Конфигурация '{id}' пуста или повреждена.");
    }

    public async Task<IReadOnlyList<BottleDefinition>> ListAsync(CancellationToken cancellationToken = default)
    {
        if (!Directory.Exists(_directory))
        {
            return [];
        }

        var result = new List<BottleDefinition>();
        foreach (var path in Directory.EnumerateFiles(_directory, "*.json").Order(StringComparer.OrdinalIgnoreCase))
        {
            await using var stream = File.OpenRead(path);
            var bottle = await JsonSerializer.DeserializeAsync<BottleDefinition>(stream, JsonOptions, cancellationToken);
            if (bottle is not null)
            {
                result.Add(bottle);
            }
        }
        return result;
    }

    public void Delete(string id)
    {
        ValidateId(id);
        var path = GetPath(id);
        if (!File.Exists(path))
        {
            throw new FileNotFoundException($"Бутылка '{id}' не найдена.", path);
        }
        File.Delete(path);
    }

    public static string MakeId(string value)
    {
        var id = Regex.Replace(value.Trim().ToLowerInvariant(), "[^a-z0-9_-]+", "-").Trim('-');
        if (string.IsNullOrEmpty(id))
        {
            throw new ArgumentException("Имя должно содержать хотя бы одну латинскую букву или цифру.");
        }
        return id;
    }

    private string GetPath(string id) => Path.Combine(_directory, id + ".json");

    private static string ResolveDefaultDirectory()
    {
        var overridePath = Environment.GetEnvironmentVariable("BOTTLE_HOME");
        if (!string.IsNullOrWhiteSpace(overridePath))
        {
            return Path.GetFullPath(overridePath);
        }

        return Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "BottleForApps",
            "bottles");
    }

    private static void ValidateId(string id)
    {
        if (!ValidId().IsMatch(id))
        {
            throw new ArgumentException("Некорректный идентификатор бутылки.", nameof(id));
        }
    }

    [GeneratedRegex("^[a-z0-9][a-z0-9_-]*$", RegexOptions.CultureInvariant)]
    private static partial Regex ValidId();
}
