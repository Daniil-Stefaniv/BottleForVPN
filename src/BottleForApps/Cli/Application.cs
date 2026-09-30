using System.Net.NetworkInformation;
using BottleForApps.Domain;
using BottleForApps.Infrastructure;
using BottleForApps.Networking;
using BottleForApps.Persistence;
using BottleForApps.Runtime;

namespace BottleForApps.Cli;

internal static class Application
{
    public static async Task<int> RunAsync(string[] rawArguments)
    {
        var commandName = rawArguments.FirstOrDefault()?.ToLowerInvariant() ?? "help";
        using var operation = DiagnosticLog.Begin("cli.command", "Запуск команды CLI.", new
        {
            command = commandName,
            argumentCount = rawArguments.Length
        });
        using var cancellation = new CancellationTokenSource();
        Console.CancelKeyPress += (_, eventArgs) =>
        {
            eventArgs.Cancel = true;
            cancellation.Cancel();
        };

        try
        {
            var arguments = Arguments.Parse(rawArguments);
            var store = new BottleStore();

            switch (arguments.Command)
            {
                case "create":
                    await CreateAsync(arguments, store, cancellation.Token);
                    break;
                case "list":
                    await ListAsync(store, cancellation.Token);
                    break;
                case "show":
                    await ShowAsync(arguments, store, cancellation.Token);
                    break;
                case "delete":
                    Delete(arguments, store);
                    break;
                case "start":
                    await StartAsync(arguments, store, cancellation.Token);
                    break;
                case "interfaces":
                    PrintInterfaces();
                    break;
                case "routes":
                    await RoutesAsync(arguments, cancellation.Token);
                    break;
                case "doctor":
                    await DoctorAsync(store, cancellation.Token);
                    break;
                case "logs":
                    await LogsAsync(arguments, cancellation.Token);
                    break;
                case "help":
                case "--help":
                case "-h":
                    PrintHelp();
                    break;
                default:
                    throw new ArgumentException($"Неизвестная команда '{arguments.Command}'. Используйте bottle help.");
            }

            return 0;
        }
        catch (OperationCanceledException)
        {
            DiagnosticLog.Warning("cli.cancel", "Операция CLI отменена.", data: new { command = commandName });
            Console.Error.WriteLine("Операция отменена.");
            return 130;
        }
        catch (Exception exception)
        {
            DiagnosticLog.Error("cli.error", "Команда CLI завершилась с ошибкой.", exception, new { command = commandName });
            Console.Error.WriteLine($"Ошибка: {exception.Message}");
            Console.Error.WriteLine($"Журнал: {DiagnosticLog.FilePath}");
            return 1;
        }
    }

    private static async Task CreateAsync(
        Arguments arguments,
        BottleStore store,
        CancellationToken cancellationToken)
    {
        if (arguments.Positionals.Count == 0)
        {
            throw new ArgumentException("Укажите идентификатор: bottle create <id> ...");
        }

        var id = BottleStore.MakeId(arguments.Positionals[0]);
        var endpoints = arguments.GetMany("vpn-endpoint");
        if (endpoints.Count == 0)
        {
            throw new ArgumentException("Укажите хотя бы один --vpn-endpoint.");
        }

        var bottle = new BottleDefinition
        {
            Id = id,
            Name = arguments.Get("name") ?? id,
            VpnExecutable = Path.GetFullPath(arguments.Require("vpn")),
            VpnArguments = arguments.Get("vpn-args") ?? string.Empty,
            VpnEndpoints = endpoints,
            RdpHost = arguments.Require("rdp"),
            RdpPort = arguments.GetInt("rdp-port", 3389),
            VpnInterface = arguments.Get("vpn-interface"),
            // Пустое --base-interface остаётся null и пересчитывается при каждом запуске.
            BaseInterface = ResolveBaseInterfaceAtCreate(arguments.Get("base-interface")),
            RelayPort = arguments.GetInt("relay-port", 0)
        };

        await store.SaveAsync(bottle, cancellationToken);
        Console.WriteLine($"Создана бутылка '{id}': {Path.Combine(store.DirectoryPath, id + ".json")}");
    }

    private static async Task ListAsync(BottleStore store, CancellationToken cancellationToken)
    {
        var bottles = await store.ListAsync(cancellationToken);
        if (bottles.Count == 0)
        {
            Console.WriteLine($"Бутылок пока нет. Каталог: {store.DirectoryPath}");
            return;
        }

        foreach (var bottle in bottles)
        {
            Console.WriteLine($"{bottle.Id,-20} {bottle.Name} -> {bottle.RdpHost}:{bottle.RdpPort}");
        }
    }

    private static async Task ShowAsync(
        Arguments arguments,
        BottleStore store,
        CancellationToken cancellationToken)
    {
        var bottle = await store.LoadAsync(RequireId(arguments), cancellationToken);
        Console.WriteLine($"ID:             {bottle.Id}");
        Console.WriteLine($"Название:       {bottle.Name}");
        Console.WriteLine($"VPN:            {bottle.VpnExecutable} {bottle.VpnArguments}".TrimEnd());
        Console.WriteLine($"VPN endpoints:  {string.Join(", ", bottle.VpnEndpoints)}");
        Console.WriteLine($"VPN interface:  {bottle.VpnInterface ?? "авто"}");
        Console.WriteLine($"Base interface: {bottle.BaseInterface ?? "авто"}");
        Console.WriteLine($"RDP:            {bottle.RdpHost}:{bottle.RdpPort}");
        Console.WriteLine($"Relay port:     {(bottle.RelayPort == 0 ? "авто" : bottle.RelayPort)}");
    }

    private static void Delete(Arguments arguments, BottleStore store)
    {
        var id = RequireId(arguments);
        store.Delete(id);
        Console.WriteLine($"Бутылка '{id}' удалена.");
    }

    private static async Task StartAsync(
        Arguments arguments,
        BottleStore store,
        CancellationToken cancellationToken)
    {
        var bottle = await store.LoadAsync(RequireId(arguments), cancellationToken);
        await PrivilegeManager.EnsureAsync(cancellationToken);
        await new BottleRunner().RunAsync(bottle, cancellationToken);
    }

    private static void PrintInterfaces()
    {
        var adapters = new NetworkInspector().GetAdapters();
        foreach (var adapter in adapters)
        {
            Console.WriteLine(
                $"{adapter.InterfaceIndex,4}  {adapter.Status,-8} {adapter.Type,-18} {adapter.Name}");
            Console.WriteLine(
                $"      IPv4: {string.Join(", ", adapter.Addresses)}; gateway: {adapter.Gateway?.ToString() ?? "-"}");
            Console.WriteLine(
                $"      IPv6: {string.Join(", ", adapter.Ipv6Addresses)}; gateway: {adapter.Ipv6Gateway?.ToString() ?? "-"}");
        }
    }

    private static void Doctor(BottleStore store)
    {
        Console.WriteLine($"OS:        {Environment.OSVersion}");
        Console.WriteLine($"Config:    {store.DirectoryPath}");
        Console.WriteLine($"Platform:  {(OperatingSystem.IsWindows() || OperatingSystem.IsMacOS() ? "поддерживается" : "не поддерживается")}");
        Console.WriteLine($"Admin:     {(PrivilegeManager.IsElevated() ? "да" : "нет/не требуется постоянно")}");
        Console.WriteLine($"Network:   {NetworkInterface.GetIsNetworkAvailable()}");
        Console.WriteLine($"Log:       {DiagnosticLog.FilePath}");
    }

    private static async Task LogsAsync(Arguments arguments, CancellationToken cancellationToken)
    {
        if (arguments.Positionals.Count == 0)
        {
            Console.WriteLine($"Текущий журнал: {DiagnosticLog.FilePath}");
            Console.WriteLine($"Каталог журналов: {DiagnosticLog.DirectoryPath}");
            Console.WriteLine("Для архива: bottle logs export [путь.zip]");
            return;
        }

        if (!arguments.Positionals[0].Equals("export", StringComparison.OrdinalIgnoreCase))
        {
            throw new ArgumentException("Используйте bottle logs или bottle logs export [путь.zip].");
        }

        var path = await SupportBundle.CreateAsync(arguments.Positionals.ElementAtOrDefault(1), cancellationToken);
        Console.WriteLine($"Диагностический архив создан: {path}");
    }

    private static async Task RoutesAsync(Arguments arguments, CancellationToken cancellationToken)
    {
        var subcommand = arguments.Positionals.ElementAtOrDefault(0) ?? "status";
        switch (subcommand)
        {
            case "cleanup":
            {
                var removed = await PrivilegeManager.CleanupOrphanedRoutesAsync(cancellationToken);
                Console.WriteLine(removed
                    ? "Осиротевшие маршруты удалены службой маршрутов."
                    : "Служба маршрутов недоступна или маршрутов не осталось.");
                break;
            }
            default:
                throw new ArgumentException("Используйте bottle routes cleanup.");
        }
    }

    private static string RequireId(Arguments arguments) => arguments.Positionals.Count > 0
        ? BottleStore.MakeId(arguments.Positionals[0])
        : throw new ArgumentException("Не указан идентификатор бутылки.");

    private static async Task DoctorAsync(BottleStore store, CancellationToken cancellationToken)
    {
        var helperAvailable = await RouteHelperClient.IsAvailableAsync(cancellationToken);
        Console.WriteLine($"OS:        {Environment.OSVersion}");
        Console.WriteLine($"Config:    {store.DirectoryPath}");
        Console.WriteLine($"Platform:  {(OperatingSystem.IsWindows() || OperatingSystem.IsMacOS() ? "поддерживается" : "не поддерживается")}");
        Console.WriteLine($"Admin:     {(PrivilegeManager.IsElevated() ? "да" : "нет")}");
        Console.WriteLine($"Служба маршрутов: {(helperAvailable ? "доступна (права администратора не нужны)" : "недоступна")}");
        Console.WriteLine($"Network:   {NetworkInterface.GetIsNetworkAvailable()}");
        Console.WriteLine($"Log:       {DiagnosticLog.FilePath}");
        if (!helperAvailable)
        {
            Console.WriteLine("Подсказка: установите приложение через установщик, чтобы служба маршрутов");
            Console.WriteLine("была зарегистрирована, и запросы прав администратора исчезнут.");
        }
    }

    // Явно указанный интерфейс проверяем сразу; пустое значение означает автовыбор при каждом запуске.
    private static string? ResolveBaseInterfaceAtCreate(string? value) =>
        string.IsNullOrWhiteSpace(value)
            ? null
            : new NetworkInspector().FindBaseAdapter(value).Name;

    private static void PrintHelp()
    {
        Console.WriteLine(
            """
            BottleForApps — лёгкая изоляция VPN/RDP на Windows и macOS.

            Команды:
              bottle gui
              bottle create <id> --vpn <exe> --vpn-endpoint <host> --rdp <host> [options]
              bottle list
              bottle show <id>
              bottle delete <id>
              bottle interfaces
              bottle routes cleanup
              bottle doctor
              bottle logs
              bottle logs export [путь.zip]
              bottle start <id>

            Параметры create:
              --name <text>             Отображаемое название
              --vpn <path>              Путь к VPN-клиенту
              --vpn-args <text>         Аргументы запуска VPN-клиента
              --vpn-endpoint <host>     Публичный VPN endpoint; можно повторять
              --vpn-interface <name>    Имя интерфейса VPN; без него автоопределение
              --base-interface <name>   Физический интерфейс; без него автоопределение
              --rdp <host>              Внутренний IPv4/hostname RDP-сервера
              --rdp-port <port>         По умолчанию 3389
              --relay-port <port>       0 — выбрать свободный локальный порт

            Пример Windows:
              bottle create office-a --name "Офис A" ^
                --vpn "C:\Program Files\Vendor VPN\vpn.exe" ^
                --vpn-endpoint vpn.example.com ^
                --vpn-interface "Vendor VPN" ^
                --rdp 10.20.30.15

            Пример macOS:
              ./bottle create office-a --name "Офис A" \
                --vpn "/Applications/Vendor VPN.app" \
                --vpn-endpoint vpn.example.com \
                --vpn-interface utun7 \
                --rdp 10.20.30.15

            Без аргументов открывается графическая панель управления.
            Маршруты изменяет служба маршрутов BottleForApps: после установки запросы
            прав администратора не требуются. Без службы используется прежний способ
            (на Windows — терминал администратора, на macOS — системный запрос).
            bottle routes cleanup удаляет маршруты, оставшиеся после аварийного завершения.
            Повторяйте --vpn-endpoint для всех адресов пула VPN-провайдера.
            При ошибке выполните bottle logs export и отправьте полученный ZIP для диагностики.
            """);
    }
}
