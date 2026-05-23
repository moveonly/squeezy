namespace Squeezy.CSharp.SemanticCases;

public interface IRunner
{
    string Run(string input);
}

public abstract class BaseRunner
{
    public virtual string Format(string input)
    {
        return input;
    }
}

[Serializable]
public partial class Runner(string prefix) : BaseRunner, IRunner
{
    private readonly List<string> _history = [];

    public event EventHandler? Completed;

    public string Prefix { get; set; } = prefix;

    public string this[int index] => _history[index];

    public string Run(string input)
    {
        var formatted = this.Format(input);
        Completed?.Invoke(this, EventArgs.Empty);
        return formatted;
    }

    public override string Format(string input)
    {
        string LocalFormat(string value)
        {
            return value.Trim();
        }

        return $"{Prefix}:{LocalFormat(input)}";
    }

    public string ParentFormat(string input)
    {
        return base.Format(input);
    }

    public static Runner operator +(Runner left, string suffix)
    {
        return new Runner($"{left.Prefix}{suffix}");
    }
}
