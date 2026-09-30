using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net.NetworkInformation;
using BottleForApps.Domain;
using BottleForApps.Infrastructure;
using BottleForApps.Networking;
using BottleForApps.Persistence;
using BottleForApps.Runtime;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Hosting.Server;
using Microsoft.AspNetCore.Hosting.Server.Features;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;

namespace BottleForApps.Web;

internal static class GuiApplication
{
    private sealed record ServerHandle(
        WebApplication App,
        string Address,
        ConcurrentDictionary<string, ConnectionSession> Sessions);

    /// <summary>
    /// Синхронный вход в GUI: вызывается с главного потока, потому что цикл сообщений
    /// нативного окна (macOS) обязан работать на нём.
    /// </summary>
    public static int Run(bool browserMode)
    {
        using var instanceLock = new SingleInstanceLock();
        if (!instanceLock.TryAcquire())
        {
            Console.WriteLine("BottleForApps уже запущен — откройте существующее окно приложения.");
            return 0;
        }

        ServerHandle server;
        try
        {
            var startTask = StartServerAsync();
            if (!startTask.Wait(TimeSpan.FromSeconds(30)))
            {
                Console.Error.WriteLine("Ошибка: локальный сервер интерфейса не запустился за 30 секунд.");
                return 1;
            }
            server = startTask.Result;
        }
        catch (AggregateException exception)
        {
            DiagnosticLog.Error("gui.start.error", "Не удалось запустить локальный сервер интерфейса.", exception.InnerException ?? exception);
            Console.Error.WriteLine($"Ошибка запуска интерфейса: {exception.InnerException?.Message ?? exception.Message}");
            return 1;
        }

        Console.WriteLine($"BottleForApps открыт: {server.Address}");
        Console.WriteLine($"Диагностический журнал: {DiagnosticLog.FilePath}");
        DiagnosticLog.Info("gui.ready", "Графический интерфейс готов.", new { address = server.Address });

        var opened = false;
        if (browserMode)
        {
            OpenBrowser(server.Address);
            opened = true;
        }
        else
        {
            opened = WindowHost.TryRun(
                server.Address,
                activeSessionCount: () => server.Sessions.Values.Count(item => item.State is "starting" or "active"),
                onClosed: () => { });
        }
        if (!opened)
        {
            Console.WriteLine("Нативное окно недоступно, открываю панель в браузере.");
            OpenBrowser(server.Address);
        }

        if (!opened || browserMode)
        {
            // Браузерный режим: окно нет, держим сервер, пока пользователь не прервёт процесс.
            try
            {
                server.App.WaitForShutdownAsync().GetAwaiter().GetResult();
            }
            catch (Exception)
            {
                // Остановка по Ctrl+C — штатная.
            }
        }

        DiagnosticLog.Info("gui.stop", "Окно закрыто, останавливаю сессии и сервер.");
        foreach (var session in server.Sessions.Values)
        {
            session.Stop();
        }
        try
        {
            server.App.StopAsync().GetAwaiter().GetResult();
        }
        catch (Exception exception)
        {
            DiagnosticLog.Warning("gui.stop.error", "Сервер интерфейса остановился с ошибкой: " + exception.Message);
        }
        return 0;
    }

    private static async Task<ServerHandle> StartServerAsync()
    {
        using var operation = DiagnosticLog.Begin("gui.run", "Запуск графического интерфейса.");
        var webRoot = Path.Combine(AppContext.BaseDirectory, "Web", "wwwroot");
        if (!Directory.Exists(webRoot))
        {
            throw new InvalidOperationException($"Файлы интерфейса не найдены: {webRoot}");
        }

        var builder = WebApplication.CreateBuilder(new WebApplicationOptions
        {
            ContentRootPath = AppContext.BaseDirectory,
            WebRootPath = webRoot
        });
        builder.Logging.ClearProviders();
        builder.WebHost.UseUrls("http://127.0.0.1:0");

        var app = builder.Build();
        var store = new BottleStore();
        var sessions = new ConcurrentDictionary<string, ConnectionSession>(StringComparer.OrdinalIgnoreCase);

        app.Use(async (context, next) =>
        {
            var host = context.Request.Host.Host;
            if (!host.Equals("127.0.0.1", StringComparison.OrdinalIgnoreCase) &&
                !host.Equals("localhost", StringComparison.OrdinalIgnoreCase))
            {
                // Панель слушает только loopback; посторонний Host — признак DNS-rebinding.
                context.Response.StatusCode = StatusCodes.Status403Forbidden;
                DiagnosticLog.Warning("http.host-rejected", "Отклонён запрос с посторонним Host.", data: new { host });
                return;
            }

            var requestTimer = Stopwatch.StartNew();
            var operationName = $"http.{context.Request.Method.ToLowerInvariant()}";
            DiagnosticLog.Info(operationName + ".start", "Получен локальный HTTP-запрос.", new
            {
                method = context.Request.Method,
                path = context.Request.Path.Value
            });
            if (context.Request.Path.StartsWithSegments("/api") &&
                (HttpMethods.IsPost(context.Request.Method) ||
                 HttpMethods.IsDelete(context.Request.Method) ||
                 HttpMethods.IsPut(context.Request.Method)))
            {
                var fetchSite = context.Request.Headers["Sec-Fetch-Site"].ToString();
                if (fetchSite.Equals("cross-site", StringComparison.OrdinalIgnoreCase))
                {
                    context.Response.StatusCode = StatusCodes.Status403Forbidden;
                    DiagnosticLog.Warning("http.rejected", "Отклонён cross-site запрос к API.", data: new
                    {
                        method = context.Request.Method,
                        path = context.Request.Path.Value
                    });
                    return;
                }
            }
            try
            {
                await next();
                DiagnosticLog.Info(operationName + ".finish", "Локальный HTTP-запрос обработан.", new
                {
                    method = context.Request.Method,
                    path = context.Request.Path.Value,
                    statusCode = context.Response.StatusCode,
                    elapsedMilliseconds = requestTimer.ElapsedMilliseconds
                });
            }
            catch (Exception exception)
            {
                DiagnosticLog.Error(operationName + ".error", "Ошибка обработки локального HTTP-запроса.", exception, new
                {
                    method = context.Request.Method,
                    path = context.Request.Path.Value,
                    elapsedMilliseconds = requestTimer.ElapsedMilliseconds
                });
                throw;
            }
        });

        app.UseDefaultFiles();
        app.UseStaticFiles();

        app.MapGet("/api/overview", async (CancellationToken cancellationToken) =>
        {
            var bottles = await store.ListAsync(cancellationToken);
            return Results.Ok(new
            {
                platform = OperatingSystem.IsWindows() ? "Windows" : OperatingSystem.IsMacOS() ? "macOS" : "Другая ОС",
                supported = OperatingSystem.IsWindows() || OperatingSystem.IsMacOS(),
                networkAvailable = NetworkInterface.GetIsNetworkAvailable(),
                elevated = PrivilegeManager.IsElevated(),
                routeHelper = await RouteHelperClient.IsAvailableAsync(cancellationToken),
                configPath = store.DirectoryPath,
                total = bottles.Count,
                active = sessions.Values.Count(item => item.State is "starting" or "active"),
                logPath = DiagnosticLog.FilePath
            });
        });

        app.MapPost("/api/logs/export", async (CancellationToken cancellationToken) =>
        {
            var path = await SupportBundle.CreateAsync(null, cancellationToken);
            return Results.Ok(new { path });
        });

        app.MapGet("/api/bottles", async (CancellationToken cancellationToken) =>
        {
            var bottles = await store.ListAsync(cancellationToken);
            return Results.Ok(bottles.Select(bottle => new
            {
                bottle.Id,
                bottle.Name,
                bottle.VpnExecutable,
                bottle.VpnArguments,
                bottle.VpnEndpoints,
                bottle.RdpHost,
                bottle.RdpPort,
                bottle.VpnInterface,
                bottle.BaseInterface,
                bottle.RelayPort,
                session = sessions.TryGetValue(bottle.Id, out var session) ? session.ToResponse() : null
            }));
        });

        app.MapPost("/api/bottles", async (HttpContext context, CancellationToken cancellationToken) =>
        {
            var input = await context.Request.ReadFromJsonAsync<BottleInput>(cancellationToken);
            if (input is null)
            {
                return Results.BadRequest(new { message = "Не удалось прочитать параметры профиля." });
            }

            try
            {
                var requestedId = string.IsNullOrWhiteSpace(input.Id) ? input.Name ?? string.Empty : input.Id;
                string id;
                try
                {
                    id = BottleStore.MakeId(requestedId);
                }
                catch (ArgumentException)
                {
                    // Имя без латиницы (например, только кириллица) не даёт стабильного id — генерируем свой.
                    id = "profile-" + Guid.NewGuid().ToString("N")[..8];
                }
                var endpoints = input.VpnEndpoints?
                    .Where(item => !string.IsNullOrWhiteSpace(item))
                    .Select(item => item.Trim())
                    .Distinct(StringComparer.OrdinalIgnoreCase)
                    .ToArray() ?? [];
                if (sessions.TryGetValue(id, out var current) && current.State is "starting" or "active")
                {
                    return Results.Conflict(new { message = "Нельзя изменить профиль во время подключения." });
                }
                if (string.IsNullOrWhiteSpace(input.Name) || string.IsNullOrWhiteSpace(input.VpnExecutable) ||
                    string.IsNullOrWhiteSpace(input.RdpHost) || endpoints.Length == 0)
                {
                    return Results.BadRequest(new { message = "Заполните название, VPN-клиент, endpoint и RDP-адрес." });
                }
                if (input.RdpPort is < 1 or > 65535 || input.RelayPort is < 0 or > 65535)
                {
                    return Results.BadRequest(new { message = "Порт должен быть в диапазоне от 1 до 65535." });
                }

                var bottle = new BottleDefinition
                {
                    Id = id,
                    Name = input.Name.Trim(),
                    VpnExecutable = Path.GetFullPath(input.VpnExecutable.Trim()),
                    VpnArguments = input.VpnArguments?.Trim() ?? string.Empty,
                    VpnEndpoints = endpoints,
                    RdpHost = input.RdpHost.Trim(),
                    RdpPort = input.RdpPort,
                    VpnInterface = NullIfWhiteSpace(input.VpnInterface),
                    BaseInterface = ResolveBaseInterfaceAtSave(input.BaseInterface),
                    RelayPort = input.RelayPort
                };
                await store.SaveAsync(bottle, cancellationToken);
                return Results.Ok(new { bottle.Id, bottle.Name });
            }
            catch (Exception exception) when (exception is ArgumentException or InvalidOperationException or NotSupportedException)
            {
                return Results.BadRequest(new { message = exception.Message });
            }
        });

        app.MapDelete("/api/bottles/{id}", (string id) =>
        {
            try
            {
                id = BottleStore.MakeId(id);
                if (sessions.TryGetValue(id, out var session) && session.State is "starting" or "active")
                {
                    return Results.Conflict(new { message = "Сначала завершите активное подключение." });
                }
                store.Delete(id);
                sessions.TryRemove(id, out _);
                return Results.NoContent();
            }
            catch (Exception exception) when (exception is ArgumentException or FileNotFoundException)
            {
                return Results.BadRequest(new { message = exception.Message });
            }
        });

        app.MapPost("/api/bottles/{id}/start", async (string id, CancellationToken cancellationToken) =>
        {
            try
            {
                id = BottleStore.MakeId(id);
                var bottle = await store.LoadAsync(id, cancellationToken);
                if (sessions.TryGetValue(id, out var current) && current.State is "starting" or "active")
                {
                    return Results.Conflict(new { message = "Этот профиль уже подключается." });
                }

                var session = new ConnectionSession();
                sessions[id] = session;
                _ = RunSessionAsync(bottle, session);
                return Results.Accepted($"/api/bottles/{id}/session", session.ToResponse());
            }
            catch (Exception exception) when (exception is ArgumentException or FileNotFoundException)
            {
                return Results.BadRequest(new { message = exception.Message });
            }
        });

        app.MapPost("/api/bottles/{id}/stop", (string id) =>
        {
            id = BottleStore.MakeId(id);
            if (!sessions.TryGetValue(id, out var session) || session.State is not ("starting" or "active"))
            {
                return Results.BadRequest(new { message = "Активное подключение не найдено." });
            }
            session.Stop();
            return Results.Accepted();
        });

        app.MapGet("/api/interfaces", () =>
        {
            var adapters = new NetworkInspector().GetAdapters();
            return Results.Ok(adapters.Select(adapter => new
            {
                adapter.Name,
                adapter.Description,
                type = adapter.Type.ToString(),
                status = adapter.Status.ToString(),
                adapter.InterfaceIndex,
                adapter.Ipv6InterfaceIndex,
                addresses = adapter.Addresses.Select(item => item.ToString()),
                ipv6Addresses = adapter.Ipv6Addresses.Select(item => item.ToString()),
                gateway = adapter.Gateway?.ToString(),
                ipv6Gateway = adapter.Ipv6Gateway?.ToString(),
                physicalCandidate = adapter.Status == OperationalStatus.Up &&
                                    (adapter.Gateway is not null || adapter.Ipv6Gateway is not null) &&
                                    adapter.Type is NetworkInterfaceType.Ethernet or NetworkInterfaceType.Wireless80211
            }));
        });

        app.MapPost("/api/pick-vpn", async (CancellationToken cancellationToken) =>
        {
            CommandResult result;
            if (OperatingSystem.IsMacOS())
            {
                result = await CommandRunner.RunAsync(
                    "/usr/bin/osascript",
                    ["-e", "POSIX path of (choose application with prompt \"Выберите VPN-клиент\")"],
                    cancellationToken);
            }
            else if (OperatingSystem.IsWindows())
            {
                const string script = "Add-Type -AssemblyName System.Windows.Forms; " +
                    "$dialog = New-Object System.Windows.Forms.OpenFileDialog; " +
                    "$dialog.Filter = 'Приложения (*.exe)|*.exe|Все файлы (*.*)|*.*'; " +
                    "if ($dialog.ShowDialog() -eq 'OK') { Write-Output $dialog.FileName } else { exit 2 }";
                result = await CommandRunner.RunAsync(
                    "powershell.exe",
                    ["-NoLogo", "-NoProfile", "-STA", "-Command", script],
                    cancellationToken);
            }
            else
            {
                return Results.BadRequest(new { message = "Выбор файла доступен только на Windows и macOS." });
            }

            if (result.ExitCode == 0 && !string.IsNullOrWhiteSpace(result.StandardOutput))
            {
                return Results.Ok(new { path = result.StandardOutput.Trim() });
            }
            return Results.BadRequest(new { message = "VPN-клиент не выбран." });
        });

        app.MapFallbackToFile("index.html");

        await app.StartAsync();
        var address = app.Services.GetRequiredService<IServer>()
            .Features.Get<IServerAddressesFeature>()?.Addresses.FirstOrDefault();
        if (address is null)
        {
            throw new InvalidOperationException("Локальный адрес интерфейса не получен.");
        }
        return new ServerHandle(app, address, sessions);
    }

    private static async Task RunSessionAsync(BottleDefinition bottle, ConnectionSession session)
    {
        try
        {
            session.Report("Проверяю права и сетевое окружение…");
            await PrivilegeManager.EnsureAsync(session.Token);
            await new BottleRunner().RunAsync(bottle, session.Token, session.Report, session.ReportRoute);
            session.State = "finished";
            session.Report("Сессия завершена.");
        }
        catch (OperationCanceledException)
        {
            session.State = "finished";
            session.Report("Подключение остановлено.");
        }
        catch (Exception exception)
        {
            session.State = "error";
            session.Error = exception.Message;
            DiagnosticLog.Error("session.error", "Сессия GUI завершилась с ошибкой.", exception, new { bottle.Id });
            session.Report("Ошибка: " + exception.Message);
        }
    }

    private static string? NullIfWhiteSpace(string? value) => string.IsNullOrWhiteSpace(value) ? null : value.Trim();

    // Явно указанный интерфейс проверяем сразу, чтобы опечатка всплыла при сохранении.
    // Пустое поле остаётся null и пересчитывается при каждом запуске профиля.
    private static string? ResolveBaseInterfaceAtSave(string? value) =>
        string.IsNullOrWhiteSpace(value)
            ? null
            : new NetworkInspector().FindBaseAdapter(value.Trim()).Name;

    private static void OpenBrowser(string address)
    {
        try
        {
            if (OperatingSystem.IsWindows())
            {
                Process.Start(new ProcessStartInfo(address) { UseShellExecute = true });
            }
            else
            {
                Process.Start(OperatingSystem.IsMacOS() ? "open" : "xdg-open", address);
            }
        }
        catch (Exception exception)
        {
            Console.Error.WriteLine($"Не удалось открыть браузер автоматически: {exception.Message}");
        }
    }

    private sealed class ConnectionSession
    {
        private readonly CancellationTokenSource _cancellation = new();
        private readonly object _sync = new();
        private readonly List<string> _messages = [];
        private readonly Dictionary<string, RoutePathStatus> _routes = new(StringComparer.OrdinalIgnoreCase);

        public string State { get; set; } = "starting";
        public string? Error { get; set; }
        public CancellationToken Token => _cancellation.Token;

        public void Report(string message)
        {
            lock (_sync)
            {
                if (message.StartsWith("VPN-интерфейс:", StringComparison.Ordinal))
                {
                    State = "active";
                }
                _messages.Add(message);
                if (_messages.Count > 20) _messages.RemoveAt(0);
            }
        }

        public void Stop() => _cancellation.Cancel();

        public void ReportRoute(RoutePathStatus route)
        {
            lock (_sync)
            {
                _routes[$"{route.Endpoint}|{route.Address}"] = route;
            }
        }

        public object ToResponse()
        {
            lock (_sync)
            {
                return new
                {
                    state = State,
                    error = Error,
                    messages = _messages.ToArray(),
                    routes = _routes.Values.OrderBy(item => item.Endpoint).ThenBy(item => item.Address).ToArray()
                };
            }
        }
    }

    private sealed record BottleInput(
        string? Id,
        string? Name,
        string? VpnExecutable,
        string? VpnArguments,
        IReadOnlyList<string>? VpnEndpoints,
        string? RdpHost,
        int RdpPort = 3389,
        string? VpnInterface = null,
        string? BaseInterface = null,
        int RelayPort = 0);
}
