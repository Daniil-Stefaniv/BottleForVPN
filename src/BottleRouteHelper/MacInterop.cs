using System.Runtime.Versioning;
using System.Net.Sockets;
using System.Runtime.InteropServices;

namespace BottleRouteHelper;

[SupportedOSPlatform("macos")]
internal static class MacInterop
{
    private const uint AdminGroupId = 80;
    private const int SolLocal = 0;
    private const int LocalPeerCredentials = 1;
    private const uint XUcredVersion = 0;

    [DllImport("libc", SetLastError = true)]
    private static extern int chown(string path, uint owner, uint group);

    /// <summary>Владелец сокета не меняется, группа назначается admin (GID 80).</summary>
    public static bool SetAdminGroup(string path) =>
        chown(path, 0xFFFFFFFF /* владелец без изменений */, AdminGroupId) == 0;

    /// <summary>
    /// Проверка подключившегося клиента через LOCAL_PEERCRED: uid root или членство
    /// в группе admin. Первичный барьер — права файла сокета (root:admin 0660).
    /// </summary>
    public static bool IsPeerAuthorized(Socket client)
    {
        try
        {
            var credential = (byte[]?)client.GetSocketOption(
                (SocketOptionLevel)SolLocal,
                (SocketOptionName)LocalPeerCredentials,
                92);
            if (credential is not { Length: >= 12 })
            {
                return true;
            }
            if (BitConverter.ToUInt32(credential, 0) != XUcredVersion)
            {
                return true;
            }
            var uid = BitConverter.ToUInt32(credential, 4);
            if (uid == 0)
            {
                return true;
            }
            var groupCount = Math.Clamp((int)BitConverter.ToInt16(credential, 8), 0, 16);
            for (var index = 0; index < groupCount; index++)
            {
                if (BitConverter.ToUInt32(credential, 12 + index * 4) == AdminGroupId)
                {
                    return true;
                }
            }
            HelperLog.Warning($"Клиент uid {uid} не входит в группу admin.");
            return false;
        }
        catch (Exception exception)
        {
            // Учесть данные клиента не удалось; остаётся барьер прав файла сокета.
            HelperLog.Warning("Не удалось прочитать учётные данные клиента: " + exception.Message);
            return true;
        }
    }
}
