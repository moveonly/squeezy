using System.Text.Json;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;

namespace Squeezy.Oracle.CSharp;

// Squeezy semantic oracle for C#.
//
// Walks every `*.cs` and `*.csx` file under the supplied root, parses each with
// Roslyn's `CSharpSyntaxTree`, and emits one row per declaration that the
// squeezy graph also considers comparable. The output JSON shape matches the
// Python AST oracle so the Rust benchmark can reuse the same comparison code:
//
//     { "rows": [["src/Foo.cs", "Class", "Runner"], ...],
//       "unparseable_files": ["src/Broken.cs"] }
//
// The oracle is intentionally syntactic: it does not perform binding, so it
// never inflates the symbol count with members coming from referenced assemblies.
internal static class Program
{
    private static readonly HashSet<string> SkipDirs = new(StringComparer.OrdinalIgnoreCase)
    {
        "bin",
        "obj",
        "node_modules",
        "packages",
        ".git",
        ".vs",
        ".idea",
        "target",
    };

    public static int Main(string[] args)
    {
        if (args.Length < 1)
        {
            Console.Error.WriteLine("usage: csharp-oracle <root>");
            return 64;
        }

        var rootArg = args[0];
        var root = Path.GetFullPath(rootArg);
        if (!Directory.Exists(root))
        {
            Console.Error.WriteLine($"root does not exist: {root}");
            return 66;
        }

        var rows = new List<string[]>();
        var edges = new List<string[]>();
        var unparseableFiles = new List<string>();

        foreach (var path in EnumerateSourceFiles(root))
        {
            var relative = ToPosixRelative(root, path);
            string source;
            try
            {
                source = File.ReadAllText(path);
            }
            catch (Exception)
            {
                unparseableFiles.Add(relative);
                continue;
            }

            var tree = CSharpSyntaxTree.ParseText(
                source,
                new CSharpParseOptions(LanguageVersion.Preview, DocumentationMode.None));
            if (tree.GetDiagnostics().Any(d => d.Severity == DiagnosticSeverity.Error))
            {
                unparseableFiles.Add(relative);
                continue;
            }

            var root_ = tree.GetCompilationUnitRoot();
            var visitor = new DeclarationVisitor(relative, rows, edges);
            visitor.Visit(root_);
        }

        rows.Sort((a, b) =>
        {
            var byFile = string.CompareOrdinal(a[0], b[0]);
            if (byFile != 0)
            {
                return byFile;
            }
            var byKind = string.CompareOrdinal(a[1], b[1]);
            if (byKind != 0)
            {
                return byKind;
            }
            return string.CompareOrdinal(a[2], b[2]);
        });
        edges.Sort((a, b) =>
        {
            var byFile = string.CompareOrdinal(a[0], b[0]);
            if (byFile != 0)
            {
                return byFile;
            }
            var byKind = string.CompareOrdinal(a[1], b[1]);
            if (byKind != 0)
            {
                return byKind;
            }
            return string.CompareOrdinal(a[2], b[2]);
        });
        unparseableFiles.Sort(StringComparer.Ordinal);

        var payload = new
        {
            rows,
            edges,
            unparseable_files = unparseableFiles,
        };
        Console.Out.Write(JsonSerializer.Serialize(payload));
        return 0;
    }

    private static IEnumerable<string> EnumerateSourceFiles(string root)
    {
        var stack = new Stack<string>();
        stack.Push(root);
        while (stack.Count > 0)
        {
            var current = stack.Pop();
            string[] directories;
            string[] files;
            try
            {
                directories = Directory.GetDirectories(current);
                files = Directory.GetFiles(current);
            }
            catch (UnauthorizedAccessException)
            {
                continue;
            }
            catch (DirectoryNotFoundException)
            {
                continue;
            }

            Array.Sort(directories, StringComparer.Ordinal);
            Array.Sort(files, StringComparer.Ordinal);

            for (var i = directories.Length - 1; i >= 0; i--)
            {
                var name = Path.GetFileName(directories[i]);
                if (SkipDirs.Contains(name))
                {
                    continue;
                }
                stack.Push(directories[i]);
            }

            foreach (var file in files)
            {
                var ext = Path.GetExtension(file);
                if (string.Equals(ext, ".cs", StringComparison.OrdinalIgnoreCase)
                    || string.Equals(ext, ".csx", StringComparison.OrdinalIgnoreCase))
                {
                    yield return file;
                }
            }
        }
    }

    private static string ToPosixRelative(string root, string path)
    {
        var relative = Path.GetRelativePath(root, path);
        return relative.Replace('\\', '/');
    }
}

internal sealed class DeclarationVisitor : CSharpSyntaxWalker
{
    private readonly string _file;
    private readonly List<string[]> _rows;
    private readonly List<string[]> _edges;
    private readonly Stack<string> _namespaceStack = new();
    private readonly Stack<string> _typeStack = new();
    private readonly Stack<string> _callableStack = new();

    public DeclarationVisitor(string file, List<string[]> rows, List<string[]> edges)
    {
        _file = file;
        _rows = rows;
        _edges = edges;
    }

    public override void VisitNamespaceDeclaration(NamespaceDeclarationSyntax node)
    {
        var name = node.Name.ToString();
        EmitNamespace(name);
        _namespaceStack.Push(name);
        base.VisitNamespaceDeclaration(node);
        _namespaceStack.Pop();
    }

    public override void VisitFileScopedNamespaceDeclaration(FileScopedNamespaceDeclarationSyntax node)
    {
        var name = node.Name.ToString();
        EmitNamespace(name);
        _namespaceStack.Push(name);
        base.VisitFileScopedNamespaceDeclaration(node);
        _namespaceStack.Pop();
    }

    public override void VisitClassDeclaration(ClassDeclarationSyntax node)
    {
        EmitType("Class", node.Identifier.ValueText);
        EmitBaseEdges(node.Identifier.ValueText, node.BaseList, interfaceOnly: false);
        _typeStack.Push(node.Identifier.ValueText);
        base.VisitClassDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitInterfaceDeclaration(InterfaceDeclarationSyntax node)
    {
        // Squeezy stores C# interfaces as `SymbolKind::Interface`; the
        // normalized comparison kind is "Interface" (matching Go's mapping
        // for `interface_type`).
        EmitType("Interface", node.Identifier.ValueText);
        EmitBaseEdges(node.Identifier.ValueText, node.BaseList, interfaceOnly: true);
        _typeStack.Push(node.Identifier.ValueText);
        base.VisitInterfaceDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitStructDeclaration(StructDeclarationSyntax node)
    {
        EmitType("Struct", node.Identifier.ValueText);
        EmitBaseEdges(node.Identifier.ValueText, node.BaseList, interfaceOnly: false);
        _typeStack.Push(node.Identifier.ValueText);
        base.VisitStructDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitRecordDeclaration(RecordDeclarationSyntax node)
    {
        // record / record struct: squeezy classifies them as Struct.
        EmitType("Struct", node.Identifier.ValueText);
        EmitBaseEdges(node.Identifier.ValueText, node.BaseList, interfaceOnly: false);
        _typeStack.Push(node.Identifier.ValueText);
        base.VisitRecordDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitEnumDeclaration(EnumDeclarationSyntax node)
    {
        EmitType("Enum", node.Identifier.ValueText);
        _typeStack.Push(node.Identifier.ValueText);
        base.VisitEnumDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitDelegateDeclaration(DelegateDeclarationSyntax node)
    {
        EmitType("TypeAlias", node.Identifier.ValueText);
        base.VisitDelegateDeclaration(node);
    }

    public override void VisitMethodDeclaration(MethodDeclarationSyntax node)
    {
        var kind = _typeStack.Count > 0 ? "Method" : "Function";
        EmitMember(kind, "M", node.Identifier.ValueText);
        _callableStack.Push(node.Identifier.ValueText);
        base.VisitMethodDeclaration(node);
        _callableStack.Pop();
    }

    public override void VisitConstructorDeclaration(ConstructorDeclarationSyntax node)
    {
        var name = node.Modifiers.Any(SyntaxKind.StaticKeyword) ? "#cctor" : "#ctor";
        EmitMember("Method", "M", name);
        _callableStack.Push(node.Identifier.ValueText);
        base.VisitConstructorDeclaration(node);
        _callableStack.Pop();
    }

    public override void VisitDestructorDeclaration(DestructorDeclarationSyntax node)
    {
        EmitMember("Method", "M", "Finalize");
        base.VisitDestructorDeclaration(node);
    }

    public override void VisitOperatorDeclaration(OperatorDeclarationSyntax node)
    {
        EmitMember("Method", "M", OperatorIdentity(node.OperatorToken.ValueText));
        base.VisitOperatorDeclaration(node);
    }

    public override void VisitConversionOperatorDeclaration(ConversionOperatorDeclarationSyntax node)
    {
        var name = node.ImplicitOrExplicitKeyword.IsKind(SyntaxKind.ImplicitKeyword)
            ? "op_Implicit"
            : "op_Explicit";
        EmitMember("Method", "M", name);
        base.VisitConversionOperatorDeclaration(node);
    }

    public override void VisitLocalFunctionStatement(LocalFunctionStatementSyntax node)
    {
        var kind = _typeStack.Count > 0 ? "Method" : "Function";
        EmitMember(kind, "M", node.Identifier.ValueText);
        _callableStack.Push(node.Identifier.ValueText);
        base.VisitLocalFunctionStatement(node);
        _callableStack.Pop();
    }

    public override void VisitPropertyDeclaration(PropertyDeclarationSyntax node)
    {
        EmitMember("Field", "P", node.Identifier.ValueText);
        base.VisitPropertyDeclaration(node);
    }

    public override void VisitIndexerDeclaration(IndexerDeclarationSyntax node)
    {
        EmitMember("Field", "P", "Item");
        base.VisitIndexerDeclaration(node);
    }

    public override void VisitEventDeclaration(EventDeclarationSyntax node)
    {
        EmitMember("Field", "E", node.Identifier.ValueText);
        base.VisitEventDeclaration(node);
    }

    public override void VisitEventFieldDeclaration(EventFieldDeclarationSyntax node)
    {
        foreach (var variable in node.Declaration.Variables)
        {
            EmitMember("Field", "E", variable.Identifier.ValueText);
        }
        base.VisitEventFieldDeclaration(node);
    }

    public override void VisitFieldDeclaration(FieldDeclarationSyntax node)
    {
        foreach (var variable in node.Declaration.Variables)
        {
            EmitMember("Field", "F", variable.Identifier.ValueText);
        }
        base.VisitFieldDeclaration(node);
    }

    public override void VisitEnumMemberDeclaration(EnumMemberDeclarationSyntax node)
    {
        EmitMember("Variant", "F", node.Identifier.ValueText);
        base.VisitEnumMemberDeclaration(node);
    }

    private void EmitNamespace(string name)
    {
        _rows.Add(new[] { _file, "Module", "N:" + name });
    }

    private void EmitType(string kind, string name)
    {
        if (string.IsNullOrEmpty(name))
        {
            return;
        }
        _rows.Add(new[] { _file, kind, "T:" + JoinTypeName(name) });
    }

    private void EmitBaseEdges(string name, BaseListSyntax? baseList, bool interfaceOnly)
    {
        if (baseList is null)
        {
            return;
        }
        var from = "T:" + JoinTypeName(name);
        foreach (var type in baseList.Types)
        {
            var target = BaseTypeName(type.Type.ToString());
            if (string.IsNullOrEmpty(target))
            {
                continue;
            }
            var kind = !interfaceOnly && target.StartsWith("I", StringComparison.Ordinal)
                ? "Implements"
                : "Extends";
            _edges.Add(new[] { _file, kind, $"{from}->{target}" });
        }
    }

    private void EmitMember(string kind, string prefix, string name)
    {
        if (string.IsNullOrEmpty(name))
        {
            return;
        }
        var owner = JoinMemberOwner();
        if (string.IsNullOrEmpty(owner))
        {
            return;
        }
        _rows.Add(new[] { _file, kind, $"{prefix}:{owner}.{name}" });
    }

    private string JoinTypeName(string name)
    {
        var parts = NamespaceParts()
            .Concat(_typeStack.Reverse())
            .Append(name);
        return string.Join(".", parts);
    }

    private string JoinMemberOwner()
    {
        var parts = NamespaceParts()
            .Concat(_typeStack.Reverse())
            .Concat(_callableStack.Reverse());
        return string.Join(".", parts);
    }

    private IEnumerable<string> NamespaceParts()
    {
        return _namespaceStack.Reverse().Where(part => !string.IsNullOrWhiteSpace(part));
    }

    private static string OperatorIdentity(string token)
    {
        return token switch
        {
            "+" => "op_Addition",
            "-" => "op_Subtraction",
            "*" => "op_Multiply",
            "/" => "op_Division",
            "%" => "op_Modulus",
            "==" => "op_Equality",
            "!=" => "op_Inequality",
            "<" => "op_LessThan",
            ">" => "op_GreaterThan",
            "<=" => "op_LessThanOrEqual",
            ">=" => "op_GreaterThanOrEqual",
            "true" => "op_True",
            "false" => "op_False",
            "!" => "op_LogicalNot",
            "~" => "op_OnesComplement",
            "&" => "op_BitwiseAnd",
            "|" => "op_BitwiseOr",
            "^" => "op_ExclusiveOr",
            "<<" => "op_LeftShift",
            ">>" => "op_RightShift",
            "++" => "op_Increment",
            "--" => "op_Decrement",
            _ => "op_" + token,
        };
    }

    private static string BaseTypeName(string raw)
    {
        var text = raw.Trim();
        var generic = text.IndexOf('<');
        if (generic >= 0)
        {
            text = text[..generic];
        }
        return text.Split('.').LastOrDefault()?.Trim() ?? string.Empty;
    }
}
