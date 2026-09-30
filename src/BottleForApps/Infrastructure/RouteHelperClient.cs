using System.IO.Pipes;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using BottleIpc;

namespace BottleForApps.Infrastructure;

/// <summary>
/// Клиент службы маршрутов bottle-route-helper. TrySendAsync возвращает null,
/// если служба недоступна: вызывающий код переходит на прежний способ (osascript/PowerShell).
/// </summary>
internal static class RouteHelperClient
{
    private static readonly SemaphoreSlim PingLock = new(1, 1);
    private static bool? _cachedAvailable;
    private static long _cachedAt;

    /// <summary>Пинг службы с кэшированием на 30 секунд.</summary>
    public static async Task<bool> IsAvailableAsync(CancellationToken cancellationToken)
    {
        if (_cachedAvailable is { } cached &&
            Environment.TickCount64 - Volatile.Read(ref _cachedAt) < 30_000)
        {
            return cached;
        }
        await PingLock.WaitAsync(cancellationToken);
        try
        {
            if (_cachedAvailable is { } recent &&
                Environment.TickCount64 - _cachedAt < 30_000)
            {
                return recent;
            }
            var response = await TrySendAsync(
                new RouteHelperRequest("ping"), TimeSpan.FromSeconds(2), CancellationToken.None);
            var available = response is { Ok: true };
            _cachedAvailable = available;
            _cachedAt = Environment.TickCount64;
            return available;
        }
        finally
        {
            PingLock.Release();
        }
    }

    public static void MarkUnavailable()
    {
        _cachedAvailable = false;
        _cachedAt = Environment.TickCount64;
    }

    public static async Task<RouteHelperResponse?> TrySendAsync(
        RouteHelperRequest request,
        TimeSpan timeout,
        CancellationToken cancellationToken)
    {
        try
        {
            if (OperatingSystem.IsMacOS())
            {
                var socketPath = RouteHelperProtocol.UnixSocketPath;
                if (socketPath is null || !File.Exists(socketPath))
                {
                    return null;
                }
                using var timeoutCancellation = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
                timeoutCancellation.CancelAfter(timeout);
                using var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
                await socket.ConnectAsync(new UnixDomainSocketEndPoint(socketPath), timeoutCancellation.Token);
                return await ExchangeAsync(
                    new NetworkStream(socket, ownsSocket: false), request, timeoutCancellation.Token);
            }
            if (OperatingSystem.IsWindows())
            {
                using var pipe = new NamedPipeClientStream(
                    ".", RouteHelperProtocol.WindowsPipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
                await Task.Run(() => pipe.Connect((int)timeout.TotalMilliseconds), cancellationToken);
                return await ExchangeAsync(pipe, request, cancellationToken);
            }
            return null;
        }
        catch (Exception exception) when (exception is IOException or SocketException or TimeoutException or
                                          OperationCanceledException or UnauthorizedAccessException)
        {
            DiagnosticLog.Warning("route.helper-unreachable", "Служба маршрутов недоступна.", data: new
            {
                request.Op,
                exception.Message
            });
            return null;
        }
    }

    private static async Task<RouteHelperResponse?> ExchangeAsync(
        Stream stream,
        RouteHelperRequest request,
        CancellationToken cancellationToken)
    {
        try
        {
            // BOM выключен: протокол — построчный JSON, лишние байты в начале строки недопустимы.
            var encoding = new UTF8Encoding(encoderShouldEmitUTF8Identifier: false);
            using var writer = new StreamWriter(stream, encoding, bufferSize: 4096, leaveOpen: true)
            {
                AutoFlush = true
            };
            using var reader = new StreamReader(stream, encoding, false, bufferSize: 4096, leaveOpen: true);

            var payload = JsonSerializer.Serialize(request, RouteHelperProtocol.Json);
            if (payload.Length > RouteHelperProtocol.MaxLineBytes)
            {
                throw new InvalidOperationException("Запрос к службе маршрутов превышает допустимый размер.");
            }
            await writer.WriteLineAsync(payload.AsMemory(), cancellationToken);

            using var readTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            readTimeout.CancelAfter(TimeSpan.FromSeconds(5));
            var line = await reader.ReadLineAsync(readTimeout.Token)
                       ?? throw new IOException("Служба маршрутов закрыла соединение без ответа.");
            return JsonSerializer.Deserialize<RouteHelperResponse>(line, RouteHelperProtocol.Json);
        }
        finally
        {
            try
            {
                stream.Close();
            }
            catch (IOException)
            {
            }
        }
    }
}
