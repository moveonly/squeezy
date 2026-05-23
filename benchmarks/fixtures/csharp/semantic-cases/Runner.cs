namespace Squeezy.CSharp.SemanticCases;

public interface IRunner
{
    string Run(string input);
}

public partial record Runner(string Prefix) : IRunner
{
    public string Run(string input)
    {
        return Format(input);
    }
}
