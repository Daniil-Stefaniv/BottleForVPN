namespace BottleForApps.Domain;

public sealed record BottleDefinition
{
    public required string Id { get; init; }
    public required string Name { get; init; }
    public required string VpnExecutable { get; init; }
    public string VpnArguments { get; init; } = string.Empty;
    public required IReadOnlyList<string> VpnEndpoints { get; init; }
    public required string RdpHost { get; init; }
    public int RdpPort { get; init; } = 3389;
    public string? VpnInterface { get; init; }
    public string? BaseInterface { get; init; }
    public int RelayPort { get; init; }
}
