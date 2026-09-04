using System.Net;
using System.Net.Sockets;

namespace BottleForApps.Networking;

internal sealed class TcpRelay : IAsyncDisposable
{
    // IP_UNICAST_IF from ws2ipdef.h. The value is intentionally explicit because
    // older .NET reference assemblies do not expose SocketOptionName.UnicastInterface.
    private const SocketOptionName IpUnicastInterface = (SocketOptionName)31;

    private readonly IPAddress _destination;
    private readonly int _destinationPort;
    private readonly int _interfaceIndex;
    private readonly TcpListener _listener;
    private readonly CancellationTokenSource _shutdown = new();
    private readonly List<Task> _connections = [];
    private Task? _acceptLoop;

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
        _acceptLoop = AcceptLoopAsync(_shutdown.Token);
    }

    private async Task AcceptLoopAsync(CancellationToken cancellationToken)
    {
        try
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                var client = await _listener.AcceptTcpClientAsync(cancellationToken);
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
            Console.Error.WriteLine($"Ошибка RDP-релея: {exception.Message}");
        }
    }

    private async Task ForwardAsync(TcpClient incoming, CancellationToken cancellationToken)
    {
        using (incoming)
        using (var outgoing = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp))
        using (var connectionCancellation = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
        {
            // Winsock expects the IPv4 interface index in network byte order.
            outgoing.SetSocketOption(
                SocketOptionLevel.IP,
                IpUnicastInterface,
                IPAddress.HostToNetworkOrder(_interfaceIndex));
            await outgoing.ConnectAsync(new IPEndPoint(_destination, _destinationPort), cancellationToken);

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

    public async ValueTask DisposeAsync()
    {
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
    }
}
