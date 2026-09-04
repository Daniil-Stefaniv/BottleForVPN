using System.Net.NetworkInformation;
using System.Security.Principal;
using BottleForApps.Domain;
using BottleForApps.Networking;
using BottleForApps.Persistence;
using BottleForApps.Runtime;

namespace BottleForApps.Cli;

internal static class Application
{
    public static async Task<int> RunAsync(string[] rawArguments)
    {
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
                    EnsureAdministrator();
                    await StartAsync(arguments, store, cancellation.Token);
                    break;
                case "interfaces":
                    PrintInterfaces();
                    break;
                case "doctor":
                    Doctor(store);
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
            Console.Error.WriteLine("Операция отменена.");
            return 130;
        }
        catch (Exception exception)
        {
            Console.Error.WriteLine($"Ошибка: {exception.Message}");
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
            BaseInterface = arguments.Get("base-interface"),
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
        }
    }

    private static void Doctor(BottleStore store)
    {
        Console.WriteLine($"OS:        {Environment.OSVersion}");
        Console.WriteLine($"Config:    {store.DirectoryPath}");
        Console.WriteLine($"Windows:   {(OperatingSystem.IsWindows() ? "да" : "нет (запуск бутылок недоступен)")}");
        Console.WriteLine($"Admin:     {(IsAdministrator() ? "да" : "нет")}");
        Console.WriteLine($"Network:   {NetworkInterface.GetIsNetworkAvailable()}");
    }

    private static string RequireId(Arguments arguments) => arguments.Positionals.Count > 0
        ? BottleStore.MakeId(arguments.Positionals[0])
        : throw new ArgumentException("Не указан идентификатор бутылки.");

    private static void EnsureAdministrator()
    {
        if (!OperatingSystem.IsWindows())
        {
            throw new PlatformNotSupportedException("Запуск поддерживается только на Windows 10/11.");
        }
        if (!IsAdministrator())
        {
            throw new UnauthorizedAccessException("Команду start нужно запустить в терминале от имени администратора.");
        }
    }

    private static bool IsAdministrator()
    {
        if (!OperatingSystem.IsWindows())
        {
            return false;
        }
        using var identity = WindowsIdentity.GetCurrent();
        return new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator);
    }

    private static void PrintHelp()
    {
        Console.WriteLine(
            """
            BottleForApps — лёгкая изоляция VPN/RDP через маршруты и TCP-релей.

            Команды:
              bottle create <id> --vpn <exe> --vpn-endpoint <host> --rdp <host> [options]
              bottle list
              bottle show <id>
              bottle delete <id>
              bottle interfaces
              bottle doctor
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

            Пример:
              bottle create office-a --name "Офис A" ^
                --vpn "C:\Program Files\Vendor VPN\vpn.exe" ^
                --vpn-endpoint vpn.example.com ^
                --vpn-interface "Vendor VPN" ^
                --rdp 10.20.30.15

            Команда start требует терминал от имени администратора.
            Повторяйте --vpn-endpoint для всех адресов пула VPN-провайдера.
            """);
    }
}
