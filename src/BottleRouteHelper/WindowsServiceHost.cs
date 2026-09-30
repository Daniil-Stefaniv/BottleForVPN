using System.Runtime.Versioning;
using System.ServiceProcess;

namespace BottleRouteHelper;

[SupportedOSPlatform("windows")]
internal sealed class HelperService : ServiceBase
{
    private CancellationTokenSource? _cancellation;
    private Task? _serverTask;

    public HelperService()
    {
        ServiceName = WindowsServiceHost.ServiceName;
        CanStop = true;
        CanShutdown = true;
    }

    protected override void OnStart(string[] args)
    {
        _cancellation = new CancellationTokenSource();
        _serverTask = HelperServer.RunAsync(_cancellation.Token);
    }

    protected override void OnStop() => StopServer();

    protected override void OnShutdown() => StopServer();

    private void StopServer()
    {
        try
        {
            _cancellation?.Cancel();
            _serverTask?.Wait(TimeSpan.FromSeconds(10));
        }
        catch (Exception)
        {
            // Служба останавливается принудительно; ошибки уже в журнале.
        }
        finally
        {
            _cancellation?.Dispose();
            _cancellation = null;
        }
    }
}

[SupportedOSPlatform("windows")]
internal static class WindowsServiceHost
{
    public const string ServiceName = "BottleForAppsRouteHelper";

    public static int Run()
    {
        ServiceBase.Run(new HelperService());
        return 0;
    }
}
