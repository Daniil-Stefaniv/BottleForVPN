using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;

namespace BottleForApps.Networking;

internal sealed record NetworkAdapterInfo(
    string Id,
    string Name,
    string Description,
    NetworkInterfaceType Type,
    OperationalStatus Status,
    int InterfaceIndex,
    IReadOnlyList<IPAddress> Addresses,
    IPAddress? Gateway);

internal sealed class NetworkInspector
{
    public IReadOnlyList<NetworkAdapterInfo> GetAdapters() =>
        NetworkInterface.GetAllNetworkInterfaces()
            .Select(ToInfo)
            .Where(adapter => adapter.InterfaceIndex > 0)
            .OrderBy(adapter => adapter.Name, StringComparer.OrdinalIgnoreCase)
            .ToArray();

    public NetworkAdapterInfo FindBaseAdapter(string? requestedName)
    {
        var adapters = GetAdapters();
        if (!string.IsNullOrWhiteSpace(requestedName))
        {
            return FindByName(adapters, requestedName)
                   ?? throw new InvalidOperationException($"Базовый интерфейс '{requestedName}' не найден.");
        }

        var localAddress = FindDefaultLocalAddress();
        var selected = adapters
            .Where(adapter =>
                adapter.Status == OperationalStatus.Up &&
                adapter.Gateway is not null &&
                adapter.Type is NetworkInterfaceType.Ethernet or NetworkInterfaceType.Wireless80211)
            .OrderByDescending(adapter => BaseAdapterScore(adapter, localAddress))
            .FirstOrDefault();

        return selected ?? throw new InvalidOperationException(
            "Не удалось определить физический интерфейс с интернет-шлюзом. Укажите --base-interface.");
    }

    public NetworkAdapterInfo? FindByName(string requestedName) => FindByName(GetAdapters(), requestedName);

    public async Task<NetworkAdapterInfo> WaitForVpnAdapterAsync(
        IReadOnlyDictionary<string, NetworkAdapterInfo> before,
        string? requestedName,
        int baseInterfaceIndex,
        TimeSpan timeout,
        CancellationToken cancellationToken)
    {
        var deadline = DateTimeOffset.UtcNow + timeout;
        while (DateTimeOffset.UtcNow < deadline)
        {
            cancellationToken.ThrowIfCancellationRequested();
            var adapters = GetAdapters();

            if (!string.IsNullOrWhiteSpace(requestedName))
            {
                var configured = FindByName(adapters, requestedName);
                if (configured?.Status == OperationalStatus.Up)
                {
                    return configured;
                }
            }
            else
            {
                var candidate = adapters.FirstOrDefault(adapter =>
                    adapter.InterfaceIndex != baseInterfaceIndex &&
                    adapter.Status == OperationalStatus.Up &&
                    (!before.TryGetValue(adapter.Id, out var old) || old.Status != OperationalStatus.Up));
                if (candidate is not null)
                {
                    return candidate;
                }
            }

            await Task.Delay(TimeSpan.FromSeconds(1), cancellationToken);
        }

        throw new TimeoutException(
            requestedName is null
                ? "VPN-интерфейс не появился. Подключите VPN или задайте --vpn-interface."
                : $"VPN-интерфейс '{requestedName}' не перешёл в состояние Up.");
    }

    public static async Task<IReadOnlyList<IPAddress>> ResolveIpv4Async(
        string host,
        CancellationToken cancellationToken)
    {
        if (IPAddress.TryParse(host, out var address))
        {
            if (address.AddressFamily != AddressFamily.InterNetwork)
            {
                throw new NotSupportedException("В MVP поддерживается только IPv4.");
            }
            return [address];
        }

        var addresses = (await Dns.GetHostAddressesAsync(host, cancellationToken))
            .Where(item => item.AddressFamily == AddressFamily.InterNetwork)
            .Distinct()
            .ToArray();
        return addresses.Length > 0
            ? addresses
            : throw new InvalidOperationException($"Имя '{host}' не имеет IPv4-адресов.");
    }

    private static NetworkAdapterInfo ToInfo(NetworkInterface networkInterface)
    {
        var properties = networkInterface.GetIPProperties();
        int index;
        try
        {
            index = properties.GetIPv4Properties()?.Index ?? -1;
        }
        catch (NetworkInformationException)
        {
            index = -1;
        }

        return new NetworkAdapterInfo(
            networkInterface.Id,
            networkInterface.Name,
            networkInterface.Description,
            networkInterface.NetworkInterfaceType,
            networkInterface.OperationalStatus,
            index,
            properties.UnicastAddresses
                .Where(item => item.Address.AddressFamily == AddressFamily.InterNetwork)
                .Select(item => item.Address)
                .ToArray(),
            properties.GatewayAddresses
                .Select(item => item.Address)
                .FirstOrDefault(item => item.AddressFamily == AddressFamily.InterNetwork && !item.Equals(IPAddress.Any)));
    }

    private static NetworkAdapterInfo? FindByName(
        IEnumerable<NetworkAdapterInfo> adapters,
        string requestedName) => adapters.FirstOrDefault(adapter =>
            adapter.Name.Equals(requestedName, StringComparison.OrdinalIgnoreCase) ||
            adapter.Description.Equals(requestedName, StringComparison.OrdinalIgnoreCase));

    private static IPAddress FindDefaultLocalAddress()
    {
        using var socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        socket.Connect("1.1.1.1", 53);
        return ((IPEndPoint)socket.LocalEndPoint!).Address;
    }

    private static int BaseAdapterScore(NetworkAdapterInfo adapter, IPAddress currentDefaultAddress)
    {
        var text = (adapter.Name + " " + adapter.Description).ToLowerInvariant();
        var looksVirtual = new[] { "vpn", "tunnel", "tap", "tun ", "wintun", "wireguard", "virtual", "hyper-v" }
            .Any(text.Contains);

        var score = adapter.Type == NetworkInterfaceType.Wireless80211 ? 100 : 50;
        if (adapter.Addresses.Contains(currentDefaultAddress))
        {
            score += 10;
        }
        if (looksVirtual)
        {
            score -= 1000;
        }
        return score;
    }
}
