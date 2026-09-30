using System.Runtime.Versioning;
using System.IO.Pipes;
using System.Security.AccessControl;
using System.Security.Principal;
using BottleIpc;

namespace BottleRouteHelper;

[SupportedOSPlatform("windows")]
internal static class WindowsPipeServer
{
    /// <summary>
    /// Канал с ACL: полный доступ SYSTEM и администраторам, чтение/запись —
    /// интерактивным пользователям. Сетевой доступ к именованным каналам отключён.
    /// </summary>
    public static NamedPipeServerStream Create()
    {
        var security = new PipeSecurity();
        security.AddAccessRule(new PipeAccessRule(
            new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null),
            PipeAccessRights.FullControl,
            AccessControlType.Allow));
        security.AddAccessRule(new PipeAccessRule(
            new SecurityIdentifier(WellKnownSidType.BuiltinAdministratorsSid, null),
            PipeAccessRights.ReadWrite,
            AccessControlType.Allow));
        security.AddAccessRule(new PipeAccessRule(
            new SecurityIdentifier(WellKnownSidType.InteractiveSid, null),
            PipeAccessRights.ReadWrite,
            AccessControlType.Allow));
        return NamedPipeServerStreamAcl.Create(
            RouteHelperProtocol.WindowsPipeName,
            PipeDirection.InOut,
            NamedPipeServerStream.MaxAllowedServerInstances,
            PipeTransmissionMode.Byte,
            PipeOptions.Asynchronous,
            inBufferSize: 4096,
            outBufferSize: 4096,
            pipeSecurity: security);
    }
}
