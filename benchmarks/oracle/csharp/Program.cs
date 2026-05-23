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
            var visitor = new DeclarationVisitor(relative, rows);
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
        unparseableFiles.Sort(StringComparer.Ordinal);

        var payload = new
        {
            rows,
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
    private readonly Stack<string> _typeStack = new();

    public DeclarationVisitor(string file, List<string[]> rows)
    {
        _file = file;
        _rows = rows;
    }

    public override void VisitNamespaceDeclaration(NamespaceDeclarationSyntax node)
    {
        EmitNamespace(node.Name.ToString());
        base.VisitNamespaceDeclaration(node);
    }

    public override void VisitFileScopedNamespaceDeclaration(FileScopedNamespaceDeclarationSyntax node)
    {
        EmitNamespace(node.Name.ToString());
        base.VisitFileScopedNamespaceDeclaration(node);
    }

    public override void VisitClassDeclaration(ClassDeclarationSyntax node)
    {
        Emit("Class", node.Identifier.ValueText);
        _typeStack.Push("Class");
        base.VisitClassDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitInterfaceDeclaration(InterfaceDeclarationSyntax node)
    {
        // Squeezy stores interfaces as `SymbolKind::Trait`; the normalized
        // comparison kind is "Trait".
        Emit("Trait", node.Identifier.ValueText);
        _typeStack.Push("Trait");
        base.VisitInterfaceDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitStructDeclaration(StructDeclarationSyntax node)
    {
        Emit("Struct", node.Identifier.ValueText);
        _typeStack.Push("Struct");
        base.VisitStructDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitRecordDeclaration(RecordDeclarationSyntax node)
    {
        // record / record struct: squeezy classifies them as Struct.
        Emit("Struct", node.Identifier.ValueText);
        _typeStack.Push("Struct");
        base.VisitRecordDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitEnumDeclaration(EnumDeclarationSyntax node)
    {
        Emit("Enum", node.Identifier.ValueText);
        _typeStack.Push("Enum");
        base.VisitEnumDeclaration(node);
        _typeStack.Pop();
    }

    public override void VisitDelegateDeclaration(DelegateDeclarationSyntax node)
    {
        Emit("TypeAlias", node.Identifier.ValueText);
        base.VisitDelegateDeclaration(node);
    }

    public override void VisitMethodDeclaration(MethodDeclarationSyntax node)
    {
        var kind = _typeStack.Count > 0 ? "Method" : "Function";
        Emit(kind, node.Identifier.ValueText);
        base.VisitMethodDeclaration(node);
    }

    public override void VisitConstructorDeclaration(ConstructorDeclarationSyntax node)
    {
        Emit("Method", node.Identifier.ValueText);
        base.VisitConstructorDeclaration(node);
    }

    public override void VisitDestructorDeclaration(DestructorDeclarationSyntax node)
    {
        Emit("Method", node.Identifier.ValueText);
        base.VisitDestructorDeclaration(node);
    }

    public override void VisitOperatorDeclaration(OperatorDeclarationSyntax node)
    {
        Emit("Method", "operator" + node.OperatorToken.ValueText);
        base.VisitOperatorDeclaration(node);
    }

    public override void VisitConversionOperatorDeclaration(ConversionOperatorDeclarationSyntax node)
    {
        Emit("Method", "operator" + node.Type.ToString());
        base.VisitConversionOperatorDeclaration(node);
    }

    public override void VisitLocalFunctionStatement(LocalFunctionStatementSyntax node)
    {
        var kind = _typeStack.Count > 0 ? "Method" : "Function";
        Emit(kind, node.Identifier.ValueText);
        base.VisitLocalFunctionStatement(node);
    }

    private void EmitNamespace(string name)
    {
        _rows.Add(new[] { _file, "Module", name });
    }

    private void Emit(string kind, string name)
    {
        if (string.IsNullOrEmpty(name))
        {
            return;
        }
        _rows.Add(new[] { _file, kind, name });
    }
}
