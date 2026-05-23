namespace Squeezy.CSharp.SemanticCases;

public static class RunnerExtensions
{
    public static string Describe(this IRunner runner)
    {
        return runner.Run("extension");
    }
}

public delegate string RunnerFormatter(Runner runner);

public record RunnerSnapshot(string Prefix)
{
    public RunnerSnapshot WithPrefix(string prefix)
    {
        return this with { Prefix = prefix };
    }
}

public class GenericBox<T> where T : IRunner
{
    public T Value { get; }

    public GenericBox(T value)
    {
        Value = value;
    }
}

public unsafe partial struct NativeHandle
{
    public nint Pointer;

    [DllImport("kernel32")]
    public static extern int GetTickCount();
}
