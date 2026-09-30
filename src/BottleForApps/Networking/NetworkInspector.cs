using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using BottleForApps.Infrastructure;

namespace BottleForApps.Networking;

internal sealed record NetworkAdapterInfo(
    string Id,
    string Name,
    string Description,
    NetworkInterfaceType Type,
    OperationalStatus Status,
    int InterfaceIndex,
    int Ipv6InterfaceIndex,
    IReadOnlyList<IPAddress> Addresses,
    IReadOnlyList<IPAddress> Ipv6Addresses,
    IPAddress? Gateway,
    IPAddress? Ipv6Gateway);

internal sealed class NetworkInspector
{
    public IReadOnlyList<NetworkAdapterInfo> GetAdapters()
    {
        var adapters = NetworkInterface.GetAllNetworkInterfaces()
            .Select(ToInfo)
            .Where(adapter => adapter.InterfaceIndex > 0)
            .OrderBy(adapter => adapter.Name, StringComparer.OrdinalIgnoreCase)
            .ToArray();
        DiagnosticLog.Info("network.adapters", "Список сетевых интерфейсов прочитан.", new
        {
            count = adapters.Length,
            adapters = adapters.Select(item => new
            {
                item.Name,
                item.Description,
                status = item.Status.ToString(),
                type = item.Type.ToString(),
                item.InterfaceIndex,
                item.Ipv6InterfaceIndex,
                addresses = item.Addresses.Select(address => address.ToString()),
                ipv6Addresses = item.Ipv6Addresses.Select(address => address.ToString()),
                gateway = item.Gateway?.ToString(),
                ipv6Gateway = item.Ipv6Gateway?.ToString()
            })
        });
        return adapters;
    }

    public NetworkAdapterInfo FindBaseAdapter(string? requestedName)
    {
        using var operation = DiagnosticLog.Begin("network.base-adapter", "Выбор основного сетевого интерфейса.", new { requestedName });
        var adapters = GetAdapters();
        if (!string.IsNullOrWhiteSpace(requestedName))
        {
            var requested = FindByName(adapters, requestedName)
                            ?? throw new InvalidOperationException($"Базовый интерфейс '{requestedName}' не найден.");
            if (requested.Status != OperationalStatus.Up ||
                requested.Type is not (NetworkInterfaceType.Ethernet or NetworkInterfaceType.Wireless80211) ||
                (requested.Gateway is null && requested.Ipv6Gateway is null))
            {
                throw new InvalidOperationException(
                    $"Интерфейс '{requestedName}' не является активным физическим Wi-Fi/Ethernet со шлюзом.");
            }
            DiagnosticLog.Info("network.base-adapter.selected", "Выбран указанный основной интерфейс.", AdapterData(requested));
            return requested;
        }

        var localAddress = FindDefaultLocalAddress();
        var selected = adapters
            .Where(adapter =>
                adapter.Status == OperationalStatus.Up &&
                (adapter.Gateway is not null || adapter.Ipv6Gateway is not null) &&
                adapter.Type is NetworkInterfaceType.Ethernet or NetworkInterfaceType.Wireless80211)
            .OrderByDescending(adapter => BaseAdapterScore(adapter, localAddress))
            .FirstOrDefault();

        if (selected is null)
        {
            throw new InvalidOperationException(
                "Не удалось определить физический интерфейс с интернет-шлюзом. Укажите --base-interface.");
        }
        DiagnosticLog.Info("network.base-adapter.selected", "Основной интерфейс выбран автоматически.", AdapterData(selected));
        return selected;
    }

    public NetworkAdapterInfo? FindByName(string requestedName) => FindByName(GetAdapters(), requestedName);

    public async Task<NetworkAdapterInfo> WaitForVpnAdapterAsync(
        IReadOnlyDictionary<string, NetworkAdapterInfo> before,
        string? requestedName,
        int baseInterfaceIndex,
        TimeSpan timeout,
        CancellationToken cancellationToken)
    {
        using var operation = DiagnosticLog.Begin("network.vpn-adapter.wait", "Ожидание VPN-интерфейса.", new
        {
            requestedName,
            baseInterfaceIndex,
            timeoutSeconds = timeout.TotalSeconds
        });
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
                    DiagnosticLog.Info("network.vpn-adapter.selected", "Указанный VPN-интерфейс активен.", AdapterData(configured));
                    return configured;
                }
            }
            else
            {
                var candidate = adapters
                    .Where(adapter =>
                        adapter.InterfaceIndex != baseInterfaceIndex &&
                        adapter.Status == OperationalStatus.Up &&
                        (!before.TryGetValue(adapter.Id, out var old) || AdapterChanged(old, adapter)))
                    .OrderByDescending(VpnAdapterScore)
                    .FirstOrDefault();
                if (candidate is not null)
                {
                    DiagnosticLog.Info("network.vpn-adapter.selected", "VPN-интерфейс выбран автоматически.", AdapterData(candidate));
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
        DiagnosticLog.Info("dns.resolve-ipv4.start", "Разрешение IPv4-адреса.", new { host });
        if (IPAddress.TryParse(host, out var address))
        {
            if (address.AddressFamily != AddressFamily.InterNetwork)
            {
                throw new NotSupportedException("В MVP поддерживается только IPv4.");
            }
            DiagnosticLog.Info("dns.resolve-ipv4.finish", "Использован заданный IPv4-адрес.", new { host, address = address.ToString() });
            return [address];
        }

        var addresses = (await Dns.GetHostAddressesAsync(host, cancellationToken))
            .Where(item => item.AddressFamily == AddressFamily.InterNetwork)
            .Distinct()
            .ToArray();
        if (addresses.Length == 0)
        {
            throw new InvalidOperationException($"Имя '{host}' не имеет IPv4-адресов.");
        }
        DiagnosticLog.Info("dns.resolve-ipv4.finish", "IPv4-адреса разрешены.", new { host, addresses = addresses.Select(item => item.ToString()) });
        return addresses;
    }

    public static async Task<IReadOnlyList<IPAddress>> ResolveEndpointAddressesAsync(
        string host,
        CancellationToken cancellationToken)
    {
        DiagnosticLog.Info("dns.resolve-endpoint.start", "Разрешение VPN endpoint.", new { host });
        if (IPAddress.TryParse(host, out var address))
        {
            if (address.AddressFamily is not (AddressFamily.InterNetwork or AddressFamily.InterNetworkV6))
            {
                throw new NotSupportedException("Поддерживаются только IPv4 и IPv6 endpoint.");
            }
            DiagnosticLog.Info("dns.resolve-endpoint.finish", "Использован заданный IP-адрес endpoint.", new { host, address = address.ToString() });
            return [address];
        }

        var addresses = (await Dns.GetHostAddressesAsync(host, cancellationToken))
            .Where(item => item.AddressFamily is AddressFamily.InterNetwork or AddressFamily.InterNetworkV6)
            .Distinct()
            .ToArray();
        if (addresses.Length == 0)
        {
            throw new InvalidOperationException($"Имя '{host}' не имеет IP-адресов.");
        }
        DiagnosticLog.Info("dns.resolve-endpoint.finish", "Адреса VPN endpoint разрешены.", new { host, addresses = addresses.Select(item => item.ToString()) });
        return addresses;
    }

    private static object AdapterData(NetworkAdapterInfo adapter) => new
    {
        adapter.Name,
        adapter.Description,
        status = adapter.Status.ToString(),
        adapter.InterfaceIndex,
        adapter.Ipv6InterfaceIndex,
        gateway = adapter.Gateway?.ToString(),
        ipv6Gateway = adapter.Ipv6Gateway?.ToString()
    };

    private static NetworkAdapterInfo ToInfo(NetworkInterface networkInterface)
    {
        var properties = networkInterface.GetIPProperties();
        int index;
        int ipv6Index;
        try
        {
            index = properties.GetIPv4Properties()?.Index ?? -1;
        }
        catch (NetworkInformationException)
        {
            index = -1;
        }
        try
        {
            ipv6Index = properties.GetIPv6Properties()?.Index ?? -1;
        }
        catch (NetworkInformationException)
        {
            ipv6Index = -1;
        }

        return new NetworkAdapterInfo(
            networkInterface.Id,
            networkInterface.Name,
            networkInterface.Description,
            networkInterface.NetworkInterfaceType,
            networkInterface.OperationalStatus,
            index,
            ipv6Index,
            properties.UnicastAddresses
                .Where(item => item.Address.AddressFamily == AddressFamily.InterNetwork)
                .Select(item => item.Address)
                .ToArray(),
            properties.UnicastAddresses
                .Where(item => item.Address.AddressFamily == AddressFamily.InterNetworkV6)
                .Select(item => item.Address)
                .ToArray(),
            properties.GatewayAddresses
                .Select(item => item.Address)
                .FirstOrDefault(item => item.AddressFamily == AddressFamily.InterNetwork && !item.Equals(IPAddress.Any)),
            properties.GatewayAddresses
                .Select(item => item.Address)
                .FirstOrDefault(item => item.AddressFamily == AddressFamily.InterNetworkV6 && !item.Equals(IPAddress.IPv6Any)));
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

    private static bool AdapterChanged(NetworkAdapterInfo oldAdapter, NetworkAdapterInfo newAdapter) =>
        oldAdapter.Status != OperationalStatus.Up ||
        !oldAdapter.Addresses.OrderBy(item => item.ToString(), StringComparer.Ordinal)
            .SequenceEqual(newAdapter.Addresses.OrderBy(item => item.ToString(), StringComparer.Ordinal));

    private static int VpnAdapterScore(NetworkAdapterInfo adapter)
    {
        var text = (adapter.Name + " " + adapter.Description).ToLowerInvariant();
        var score = adapter.Type is NetworkInterfaceType.Tunnel or NetworkInterfaceType.Ppp ? 100 : 0;
        if (adapter.Name.StartsWith("utun", StringComparison.OrdinalIgnoreCase))
        {
            score += 100;
        }
        if (new[] { "vpn", "tunnel", "tap", "wintun", "wireguard" }.Any(text.Contains))
        {
            score += 50;
        }
        return score;
    }
}
