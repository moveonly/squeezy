namespace Squeezy.CSharp.SemanticCases;

public partial class Runner
{
    public static Runner Create(string prefix)
    {
        return new Runner(prefix);
    }

    public async Task<string> RunAsync(string input)
    {
        await Task.Yield();
        return Run(input);
    }

    public void Remember(object value)
    {
        var text = value switch
        {
            string literal => literal,
            Runner { Prefix: var knownPrefix } => knownPrefix,
            _ => string.Empty,
        };
        _history.Add(text);
    }
}
