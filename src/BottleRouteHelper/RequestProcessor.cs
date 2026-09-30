using System.Net;
using System.Net.Sockets;
using System.Text.RegularExpressions;
using BottleIpc;

namespace BottleRouteHelper;

internal readonly record struct ValidatedRoute(
    AddressFamily Family,
    IPAddress Destination,
    IPAddress Gateway,
    string InterfaceName,
    int InterfaceIndex,
    string? Owner);

/// <summary>
/// Разбор, строгая валидация и выполнение запросов. Служба работает от root/SYSTEM,
/// поэтому принимает только операции с маршрутами и только корректные аргументы.
/// </summary>
internal static partial class RequestProcessor
{
    public static async Task<RouteHelperResponse> HandleAsync(RouteHelperRequest request)
    {
        HelperLog.Info("op=" + request.Op +
                       (request.Destination is null ? string.Empty : $" dest={request.Destination}") +
                       (request.Owner is null ? string.Empty : $" owner={request.Owner}"));
        try
        {
            switch (request.Op)
            {
                case "ping":
                    return new RouteHelperResponse(
                        true,
                        Version: RouteHelperProtocol.Version,
                        Detail: OperatingSystem.IsMacOS() ? "macOS" : OperatingSystem.IsWindows() ? "Windows" : "?");

                case "add":
                {
                    var route = Validate(request);
                    var result = await RouteOperations.AddAsync(route);
                    if (result is { Ok: true, Changed: true })
                    {
                        RouteJournal.Current.Remember(new JournaledRoute(
                            FamilyName(route.Family),
                            route.Destination.ToString(),
                            route.Gateway.ToString(),
                            route.InterfaceName,
                            route.InterfaceIndex,
                            route.Owner,
                            DateTimeOffset.UtcNow));
                    }
                    return result.Ok
                        ? new RouteHelperResponse(true, Created: result.Changed, Detail: result.Detail)
                        : new RouteHelperResponse(false, Error: result.Detail);
                }

                case "del":
                {
                    var route = Validate(request);
                    var result = await RouteOperations.RemoveAsync(route);
                    if (result.Ok)
                    {
                        RouteJournal.Current.Forget(route.Destination, route.Gateway, route.InterfaceIndex);
                    }
                    return result.Ok
                        ? new RouteHelperResponse(true, Removed: result.Changed ? 1 : 0, Detail: result.Detail)
                        : new RouteHelperResponse(false, Error: result.Detail);
                }

                case "cleanup":
                {
                    var removed = 0;
                    foreach (var entry in RouteJournal.Current.Snapshot())
                    {
                        if (!TryRouteFromJournal(entry, out var route))
                        {
                            continue;
                        }
                        try
                        {
                            var result = await RouteOperations.RemoveAsync(route);
                            if (result.Ok)
                            {
                                removed++;
                            }
                            else
                            {
                                HelperLog.Warning($"cleanup: {entry.Destination}: {result.Detail}");
                            }
                        }
                        catch (Exception exception)
                        {
                            HelperLog.Warning($"cleanup: {entry.Destination}: {exception.Message}");
                        }
                    }
                    RouteJournal.Current.Clear();
                    return new RouteHelperResponse(true, Removed: removed);
                }

                default:
                    return new RouteHelperResponse(false, Error: $"Неизвестная операция '{request.Op}'.");
            }
        }
        catch (Exception exception) when (exception is ArgumentException or InvalidOperationException or
                                          PlatformNotSupportedException or SocketException)
        {
            return new RouteHelperResponse(false, Error: exception.Message);
        }
    }

    private static ValidatedRoute Validate(RouteHelperRequest request)
    {
        if (string.IsNullOrEmpty(request.Destination) || !IPAddress.TryParse(request.Destination, out var destination))
        {
            throw new ArgumentException("Некорректный адрес назначения.");
        }
        if (string.IsNullOrEmpty(request.Gateway) || !IPAddress.TryParse(request.Gateway, out var gateway))
        {
            throw new ArgumentException("Некорректный адрес шлюза.");
        }
        var family = request.Family switch
        {
            "inet" => AddressFamily.InterNetwork,
            "inet6" => AddressFamily.InterNetworkV6,
            _ => throw new ArgumentException("Family должен быть inet или inet6.")
        };
        if (destination.AddressFamily != family || gateway.AddressFamily != family)
        {
            throw new ArgumentException("Family не соответствует адресам назначения и шлюза.");
        }
        if (!IsAllowedDestination(destination))
        {
            throw new ArgumentException("Недопустимый адрес назначения (loopback, unspecified или multicast).");
        }

        var interfaceName = request.InterfaceName ?? string.Empty;
        if (interfaceName.Length is < 1 or > 64 || !InterfaceNamePattern().IsMatch(interfaceName))
        {
            throw new ArgumentException("Некорректное имя интерфейса.");
        }
        if (request.InterfaceIndex is not > 0)
        {
            throw new ArgumentException("Некорректный индекс интерфейса.");
        }

        var owner = request.Owner?.Trim();
        if (!string.IsNullOrEmpty(owner))
        {
            owner = owner[..Math.Min(64, owner.Length)];
        }
        return new ValidatedRoute(family, destination, gateway, interfaceName, request.InterfaceIndex.Value,
            string.IsNullOrEmpty(owner) ? null : owner);
    }

    private static bool TryRouteFromJournal(JournaledRoute entry, out ValidatedRoute route)
    {
        route = default;
        if (!IPAddress.TryParse(entry.Destination, out var destination) ||
            !IPAddress.TryParse(entry.Gateway, out var gateway) ||
            destination.AddressFamily != gateway.AddressFamily)
        {
            HelperLog.Warning($"cleanup: повреждённая запись журнала для {entry.Destination}.");
            return false;
        }
        route = new ValidatedRoute(
            destination.AddressFamily,
            destination,
            gateway,
            string.IsNullOrEmpty(entry.InterfaceName) ? "en0" : entry.InterfaceName,
            entry.InterfaceIndex > 0 ? entry.InterfaceIndex : 1,
            entry.Owner);
        return true;
    }

    private static bool IsAllowedDestination(IPAddress address)
    {
        if (IPAddress.IsLoopback(address) ||
            address.Equals(IPAddress.Any) ||
            address.Equals(IPAddress.IPv6Any) ||
            address.IsIPv6Multicast)
        {
            return false;
        }
        var bytes = address.GetAddressBytes();
        return address.AddressFamily != AddressFamily.InterNetwork || bytes[0] is not (>= 224 and <= 239);
    }

    private static string FamilyName(AddressFamily family) =>
        family == AddressFamily.InterNetwork ? "inet" : "inet6";

    [GeneratedRegex("^[A-Za-z0-9 .:_\\-()]{1,64}$", RegexOptions.CultureInvariant)]
    private static partial Regex InterfaceNamePattern();
}
