using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using BottleForApps.Infrastructure;

namespace BottleForApps.Networking;

internal sealed class TcpRelay : IAsyncDisposable
{
    // Native values from ws2ipdef.h (Windows) and netinet/in.h (Darwin).
    private const SocketOptionName WindowsIpUnicastInterface = (SocketOptionName)31;
    private const SocketOptionName MacOsIpBoundInterface = (SocketOptionName)25;

    private readonly IPAddress _destination;
    private readonly int _destinationPort;
    private readonly int _interfaceIndex;
    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _shutdown = new();
    private readonly List<Task> _connections = [];
    private readonly TaskCompletionSource _firstConnection = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private Task? _acceptLoop;
    private int _activeConnections;
    private long _lastConnectionActivity = Stopwatch.GetTimestamp();

    public TcpRelay(IPAddress destination, int destinationPort, int interfaceIndex, int localPort)
    {
        if (destination.AddressFamily != AddressFamily.InterNetwork)
        {
            throw new NotSupportedException("В MVP RDP-релей поддерживает только IPv4.");
        }
        if (destinationPort is < 1 or > 65535 || localPort is < 0 or > 65535)
        {
            throw new ArgumentOutOfRangeException(nameof(destinationPort));
        }

        _destination = destination;
        _destinationPort = destinationPort;
        _interfaceIndex = interfaceIndex;
        _listener = new TcpListener(IPAddress.Loopback, localPort);
    }

    public int LocalPort => ((IPEndPoint)_listener.LocalEndpoint).Port;

    public void Start()
    {
        _listener.Start();
        DiagnosticLog.Info("relay.start", "Локальный TCP-релей запущен.", new
        {
            localPort = LocalPort,
            destination = _destination.ToString(),
            destinationPort = _destinationPort,
            interfaceIndex = _interfaceIndex
        });
        _acceptLoop = AcceptLoopAsync(_shutdown.Token);
    }

    public async Task WaitForSessionEndAsync(CancellationToken cancellationToken)
    {
        await _firstConnection.Task.WaitAsync(TimeSpan.FromMinutes(2), cancellationToken);
        while (!cancellationToken.IsCancellationRequested)
        {
            var idleTime = Stopwatch.GetElapsedTime(Interlocked.Read(ref _lastConnectionActivity));
            if (Volatile.Read(ref _activeConnections) == 0 && idleTime >= TimeSpan.FromSeconds(5))
            {
                return;
            }
            await Task.Delay(TimeSpan.FromMilliseconds(500), cancellationToken);
        }
    }

    private async Task AcceptLoopAsync(CancellationToken cancellationToken)
    {
        try
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                var client = await _listener.AcceptTcpClientAsync(cancellationToken);
                DiagnosticLog.Info("relay.connection.accept", "Принято локальное RDP-соединение.", new
                {
                    remoteEndpoint = client.Client.RemoteEndPoint?.ToString(),
                    localPort = LocalPort
                });
                Interlocked.Increment(ref _activeConnections);
                Interlocked.Exchange(ref _lastConnectionActivity, Stopwatch.GetTimestamp());
                _firstConnection.TrySetResult();
                var task = ForwardSafelyAsync(client, cancellationToken);
                lock (_connections)
                {
                    _connections.Add(task);
                    _connections.RemoveAll(item => item.IsCompleted);
                }
            }
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
        }
        catch (SocketException) when (cancellationToken.IsCancellationRequested)
        {
        }
    }

    private async Task ForwardSafelyAsync(TcpClient incoming, CancellationToken cancellationToken)
    {
        try
        {
            await ForwardAsync(incoming, cancellationToken);
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
        }
        catch (Exception exception)
        {
            DiagnosticLog.Error("relay.connection.error", "Ошибка RDP-релея.", exception, new
            {
                destination = _destination.ToString(),
                destinationPort = _destinationPort,
                interfaceIndex = _interfaceIndex
            });
            Console.Error.WriteLine($"Ошибка RDP-релея: {exception.Message}");
        }
        finally
        {
            Interlocked.Decrement(ref _activeConnections);
            Interlocked.Exchange(ref _lastConnectionActivity, Stopwatch.GetTimestamp());
        }
    }

    private async Task ForwardAsync(TcpClient incoming, CancellationToken cancellationToken)
    {
        using (incoming)
        using (var outgoing = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp))
        using (var connectionCancellation = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
        {
            BindToInterface(outgoing);
            DiagnosticLog.Info("relay.connection.connect", "Соединение привязано к VPN-интерфейсу.", new
            {
                destination = _destination.ToString(),
                destinationPort = _destinationPort,
                interfaceIndex = _interfaceIndex
            });
            await outgoing.ConnectAsync(new IPEndPoint(_destination, _destinationPort), cancellationToken);
            DiagnosticLog.Info("relay.connection.connected", "Соединение с RDP-сервером установлено.");

            using var outgoingStream = new NetworkStream(outgoing, ownsSocket: false);
            var incomingStream = incoming.GetStream();
            var upstream = incomingStream.CopyToAsync(outgoingStream, connectionCancellation.Token);
            var downstream = outgoingStream.CopyToAsync(incomingStream, connectionCancellation.Token);
            await Task.WhenAny(upstream, downstream);
            await connectionCancellation.CancelAsync();
            try
            {
                await Task.WhenAll(upstream, downstream);
            }
            catch (OperationCanceledException) when (connectionCancellation.IsCancellationRequested)
            {
            }
        }
    }

    private void BindToInterface(Socket socket)
    {
        if (OperatingSystem.IsWindows())
        {
            // Winsock expects IP_UNICAST_IF in network byte order.
            socket.SetSocketOption(
                SocketOptionLevel.IP,
                WindowsIpUnicastInterface,
                IPAddress.HostToNetworkOrder(_interfaceIndex));
            return;
        }
        if (OperatingSystem.IsMacOS())
        {
            socket.SetSocketOption(SocketOptionLevel.IP, MacOsIpBoundInterface, _interfaceIndex);
            return;
        }
        throw new PlatformNotSupportedException("Привязка релея поддерживается только на Windows и macOS.");
    }

    public async ValueTask DisposeAsync()
    {
        DiagnosticLog.Info("relay.stop", "Остановка TCP-релея.", new { localPort = LocalPort });
        await _shutdown.CancelAsync();
        _listener.Stop();
        if (_acceptLoop is not null)
        {
            await _acceptLoop;
        }

        Task[] connections;
        lock (_connections)
        {
            connections = _connections.ToArray();
        }
        try
        {
            await Task.WhenAll(connections).WaitAsync(TimeSpan.FromSeconds(2));
        }
        catch (TimeoutException)
        {
        }
        catch (OperationCanceledException)
        {
        }
        catch (Exception) when (_shutdown.IsCancellationRequested)
        {
        }
        _shutdown.Dispose();
        DiagnosticLog.Info("relay.stopped", "TCP-релей остановлен.");
    }
}
