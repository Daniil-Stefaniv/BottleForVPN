using System.IO.Pipes;
using System.Net.Sockets;
using System.Runtime.Versioning;
using System.Text;
using System.Text.Json;
using BottleIpc;

namespace BottleRouteHelper;

/// <summary>
/// Приём соединений от приложения и обработка запросов протокола.
/// macOS — unix-сокет, Windows — именованный канал.
/// </summary>
internal static class HelperServer
{
    public static async Task<int> RunAsync(CancellationToken cancellationToken)
    {
        HelperLog.Info(
            $"bottle-route-helper запущен (pid {Environment.ProcessId}), версия протокола {RouteHelperProtocol.Version}.");
        try
        {
            if (OperatingSystem.IsMacOS())
            {
                await RunUnixAsync(cancellationToken);
            }
            else if (OperatingSystem.IsWindows())
            {
                await RunPipeAsync(cancellationToken);
            }
            else
            {
                HelperLog.Error("Поддерживаются только macOS и Windows.");
                return 1;
            }
            return 0;
        }
        catch (OperationCanceledException)
        {
            return 0;
        }
        catch (Exception exception)
        {
            HelperLog.Error("Критическая ошибка сервера: " + exception);
            return 1;
        }
        finally
        {
            HelperLog.Info("bottle-route-helper остановлен.");
        }
    }

    [SupportedOSPlatform("macos")]
    private static async Task RunUnixAsync(CancellationToken cancellationToken)
    {
        var socketPath = RouteHelperProtocol.UnixSocketPath ?? RouteHelperProtocol.DefaultUnixSocketPath;
        if (File.Exists(socketPath))
        {
            DeleteStaleSocket(socketPath);
        }
        var directory = Path.GetDirectoryName(socketPath);
        if (!string.IsNullOrEmpty(directory))
        {
            Directory.CreateDirectory(directory);
        }

        using var server = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        server.Bind(new UnixDomainSocketEndPoint(socketPath));
        server.Listen(16);
        RestrictSocketPermissions(socketPath);
        HelperLog.Info($"Ожидаю соединения на unix-сокете {socketPath}.");

        while (!cancellationToken.IsCancellationRequested)
        {
            Socket client;
            try
            {
                client = await server.AcceptAsync(cancellationToken);
            }
            catch (OperationCanceledException)
            {
                break;
            }
            _ = HandleSocketClientAsync(client, cancellationToken);
        }
    }

    private static void DeleteStaleSocket(string socketPath)
    {
        try
        {
            using var probe = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
            probe.Connect(new UnixDomainSocketEndPoint(socketPath));
            probe.Close();
            throw new InvalidOperationException(
                $"Другой экземпляр bottle-route-helper уже слушает {socketPath}.");
        }
        catch (SocketException)
        {
            // Соединение не получилось — это файл, оставшийся после падения предыдущего экземпляра.
        }
        File.Delete(socketPath);
    }

    [SupportedOSPlatform("macos")]
    private static void RestrictSocketPermissions(string socketPath)
    {
        try
        {
            // Чтение и запись только владельцу (root) и группе admin.
            File.SetUnixFileMode(socketPath,
                UnixFileMode.UserRead | UnixFileMode.UserWrite |
                UnixFileMode.GroupRead | UnixFileMode.GroupWrite);
            if (MacInterop.SetAdminGroup(socketPath))
            {
                HelperLog.Info("Сокет принадлежит root:admin с правами 0660.");
            }
            else
            {
                HelperLog.Warning("Не удалось сменить группу сокета на admin; действует режим владельца root.");
            }
        }
        catch (Exception exception)
        {
            HelperLog.Warning("Не удалось ограничить права сокета: " + exception.Message);
        }
    }

    [SupportedOSPlatform("windows")]
    private static async Task RunPipeAsync(CancellationToken cancellationToken)
    {
        HelperLog.Info($"Ожидаю соединения на именованном канале {RouteHelperProtocol.WindowsPipeName}.");
        var clients = new List<Task>();
        while (!cancellationToken.IsCancellationRequested)
        {
            NamedPipeServerStream pipe;
            try
            {
                pipe = WindowsPipeServer.Create();
            }
            catch (Exception exception)
            {
                HelperLog.Error("Не создать именованный канал: " + exception.Message);
                throw;
            }

            try
            {
                await pipe.WaitForConnectionAsync(cancellationToken);
            }
            catch (OperationCanceledException)
            {
                await pipe.DisposeAsync();
                break;
            }
            clients.Add(HandleStreamAsync(pipe, cancellationToken));
            clients.RemoveAll(task => task.IsCompleted);
        }
        try
        {
            await Task.WhenAll(clients);
        }
        catch (Exception)
        {
            // Отдельные ошибки клиентов уже записаны в журнал.
        }
    }

    private static async Task HandleSocketClientAsync(Socket client, CancellationToken cancellationToken)
    {
        if (OperatingSystem.IsMacOS() && !MacInterop.IsPeerAuthorized(client))
        {
            HelperLog.Warning("Соединение отклонено: клиент не входит в группу admin.");
            client.Close();
            return;
        }
        await HandleStreamAsync(new NetworkStream(client, ownsSocket: false), cancellationToken);
        client.Close();
    }

    private static async Task HandleStreamAsync(Stream stream, CancellationToken cancellationToken)
    {
        try
        {
            using var closeOnCancel = cancellationToken.Register(() => TryClose(stream));
            // BOM выключен: протокол — построчный JSON, лишние байты в начале строки недопустимы.
            var encoding = new UTF8Encoding(encoderShouldEmitUTF8Identifier: false);
            using var reader = new StreamReader(stream, encoding, false, bufferSize: 4096, leaveOpen: true);
            using var writer = new StreamWriter(stream, encoding, bufferSize: 4096, leaveOpen: true)
            {
                AutoFlush = true
            };

            while (!cancellationToken.IsCancellationRequested)
            {
                string? line;
                using var readTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
                readTimeout.CancelAfter(TimeSpan.FromSeconds(30));
                try
                {
                    line = await reader.ReadLineAsync(readTimeout.Token);
                }
                catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
                {
                    break;
                }
                if (string.IsNullOrEmpty(line))
                {
                    break;
                }

                RouteHelperResponse response;
                if (line.Length > RouteHelperProtocol.MaxLineBytes)
                {
                    response = new RouteHelperResponse(false, Error: "Запрос превышает допустимый размер.");
                    await writer.WriteLineAsync(JsonSerializer.Serialize(response, RouteHelperProtocol.Json));
                    break;
                }
                try
                {
                    var request = JsonSerializer.Deserialize<RouteHelperRequest>(line, RouteHelperProtocol.Json);
                    response = request is null
                        ? new RouteHelperResponse(false, Error: "Пустой запрос.")
                        : await RequestProcessor.HandleAsync(request);
                }
                catch (JsonException exception)
                {
                    response = new RouteHelperResponse(false, Error: "Некорректный JSON: " + exception.Message);
                }
                await writer.WriteLineAsync(JsonSerializer.Serialize(response, RouteHelperProtocol.Json));
            }
        }
        catch (IOException)
        {
        }
        catch (ObjectDisposedException)
        {
        }
        catch (Exception exception)
        {
            HelperLog.Error("Ошибка обработки клиента: " + exception.Message);
        }
        finally
        {
            TryClose(stream);
        }
    }

    private static void TryClose(Stream stream)
    {
        try
        {
            stream.Close();
        }
        catch (Exception)
        {
        }
    }
}
