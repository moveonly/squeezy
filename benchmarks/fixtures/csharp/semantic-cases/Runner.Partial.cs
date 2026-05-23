namespace Squeezy.CSharp.SemanticCases;

public partial record Runner
{
    public string Format(string input)
    {
        return $"{Prefix}:{input}";
    }
}
