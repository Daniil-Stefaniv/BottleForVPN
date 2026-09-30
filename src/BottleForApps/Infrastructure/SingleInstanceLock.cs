namespace BottleForApps.Infrastructure;

/// <summary>
/// Гарантирует один экземпляр GUI: файл-блокировка держится открытым всё время работы.
/// FileShare.None не даёт второму процессу открыть файл, пока первый жив.
/// </summary>
internal sealed class SingleInstanceLock : IDisposable
{
    private static string LockPath => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "BottleForApps",
        "gui.lock");

    private FileStream? _stream;

    public bool TryAcquire()
    {
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(LockPath)!);
            _stream = File.Open(LockPath, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
            return true;
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException)
        {
            return false;
        }
    }

    public void Dispose() => _stream?.Dispose();
}
