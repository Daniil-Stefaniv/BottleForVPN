using System.Globalization;

namespace BottleForApps.Cli;

internal sealed class Arguments
{
    private readonly Dictionary<string, List<string>> _options;

    private Arguments(
        string command,
        IReadOnlyList<string> positionals,
        Dictionary<string, List<string>> options)
    {
        Command = command;
        Positionals = positionals;
        _options = options;
    }

    public string Command { get; }
    public IReadOnlyList<string> Positionals { get; }

    public static Arguments Parse(string[] args)
    {
        if (args.Length == 0)
        {
            return new Arguments("help", [], new(StringComparer.OrdinalIgnoreCase));
        }

        var positionals = new List<string>();
        var options = new Dictionary<string, List<string>>(StringComparer.OrdinalIgnoreCase);

        for (var i = 1; i < args.Length; i++)
        {
            var token = args[i];
            if (!token.StartsWith("--", StringComparison.Ordinal))
            {
                positionals.Add(token);
                continue;
            }

            var name = token[2..];
            if (string.IsNullOrWhiteSpace(name))
            {
                throw new ArgumentException("Пустое имя параметра.");
            }

            var value = "true";
            if (i + 1 < args.Length && !args[i + 1].StartsWith("--", StringComparison.Ordinal))
            {
                value = args[++i];
            }

            if (!options.TryGetValue(name, out var values))
            {
                values = [];
                options[name] = values;
            }
            values.Add(value);
        }

        return new Arguments(args[0].ToLowerInvariant(), positionals, options);
    }

    public string? Get(string name) =>
        _options.TryGetValue(name, out var values) ? values[^1] : null;

    public IReadOnlyList<string> GetMany(string name) =>
        _options.TryGetValue(name, out var values) ? values : [];

    public string Require(string name) => Get(name) is { Length: > 0 } value
        ? value
        : throw new ArgumentException($"Не указан обязательный параметр --{name}.");

    public int GetInt(string name, int defaultValue)
    {
        var value = Get(name);
        if (value is null)
        {
            return defaultValue;
        }
        return int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var result)
            ? result
            : throw new ArgumentException($"Параметр --{name} должен быть целым числом.");
    }
}
