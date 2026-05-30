// Squeezy semantic oracle for Kotlin.
//
// Walks every `.kt` / `.kts` file under the supplied source root, parses each
// with the JetBrains `kotlin-compiler-embeddable` PSI front-end, and emits a
// declaration symbol list on stdout. The shape mirrors the Java JDK-compiler-
// tree oracle in `benchmarks/oracle/...` — `{"rows": [["<rel>", "<kind>",
// "<name>"], ...]}` — so the Rust harness can plug either oracle into the
// same `SymbolScan` comparison code (`oracles/kotlin_oracle.rs`).
//
// Exclusions kept symmetric with the squeezy Kotlin extractor:
//   - locals (`KtDeclaration.isLocal`)
//   - lambdas and lambda parameters
//   - implicit `it` parameters / anonymous objects
//   - synthetic / generated members (data-class `componentN`, `copy`,
//     `equals`, `hashCode`, `toString`, synthesized accessors)
//   - function parameters and receiver parameters
//
// Build: `bash build.sh` (requires `kotlinc` ≥ 1.9 and JDK 17).
// Run:   `java -jar kotlin-oracle.jar <source-root>` — exits non-zero on
//        oracle errors so the Rust harness can degrade to scan-only mode.

import java.io.File
import org.jetbrains.kotlin.cli.common.CLIConfigurationKeys
import org.jetbrains.kotlin.cli.common.messages.MessageCollector
import org.jetbrains.kotlin.cli.common.messages.MessageRenderer
import org.jetbrains.kotlin.cli.common.messages.PrintingMessageCollector
import org.jetbrains.kotlin.cli.jvm.compiler.EnvironmentConfigFiles
import org.jetbrains.kotlin.cli.jvm.compiler.KotlinCoreEnvironment
import org.jetbrains.kotlin.com.intellij.openapi.Disposable
import org.jetbrains.kotlin.com.intellij.openapi.util.Disposer
import org.jetbrains.kotlin.config.CompilerConfiguration
import org.jetbrains.kotlin.psi.KtClass
import org.jetbrains.kotlin.psi.KtClassOrObject
import org.jetbrains.kotlin.psi.KtFile
import org.jetbrains.kotlin.psi.KtFunction
import org.jetbrains.kotlin.psi.KtNamedFunction
import org.jetbrains.kotlin.psi.KtObjectDeclaration
import org.jetbrains.kotlin.psi.KtParameter
import org.jetbrains.kotlin.psi.KtProperty
import org.jetbrains.kotlin.psi.KtSecondaryConstructor
import org.jetbrains.kotlin.psi.KtTypeAlias
import org.jetbrains.kotlin.psi.KtEnumEntry

data class Row(val file: String, val kind: String, val name: String)

fun main(args: Array<String>) {
    if (args.isEmpty()) {
        System.err.println("usage: java -jar kotlin-oracle.jar <source-root>")
        System.exit(2)
    }
    val root = File(args[0]).absoluteFile.canonicalFile
    if (!root.isDirectory) {
        System.err.println("source root is not a directory: $root")
        System.exit(2)
    }

    val disposable = Disposer.newDisposable()
    try {
        val configuration = CompilerConfiguration().apply {
            put(
                CLIConfigurationKeys.MESSAGE_COLLECTOR_KEY,
                PrintingMessageCollector(System.err, MessageRenderer.PLAIN_RELATIVE_PATHS, false),
            )
        }
        val env = KotlinCoreEnvironment.createForProduction(
            disposable as Disposable,
            configuration,
            EnvironmentConfigFiles.JVM_CONFIG_FILES,
        )

        val rows = mutableListOf<Row>()
        val files = root.walkTopDown()
            .filter { it.isFile && (it.extension == "kt" || it.extension == "kts") }
            .sortedBy { it.absolutePath }
            .toList()

        val psiFactory = env.project
        // KotlinCoreEnvironment exposes a PsiFileFactory we can use to parse
        // single .kt files without a build system. For each file we create
        // a virtual file and walk its top-level declarations recursively.
        val psiManager = org.jetbrains.kotlin.com.intellij.psi.PsiManager.getInstance(psiFactory)
        val virtualFileManager =
            org.jetbrains.kotlin.com.intellij.openapi.vfs.VirtualFileManager.getInstance()
        val localFs = virtualFileManager
            .getFileSystem(org.jetbrains.kotlin.com.intellij.openapi.vfs.StandardFileSystems.FILE_PROTOCOL)
            ?: throw IllegalStateException("local virtual filesystem not available")

        for (file in files) {
            val rel = root.toPath().relativize(file.toPath()).toString().replace('\\', '/')
            val virtualFile = localFs.findFileByPath(file.absolutePath)
                ?: continue
            val ktFile = psiManager.findFile(virtualFile) as? KtFile ?: continue
            collect(ktFile, rel, rows)
        }

        rows.sortWith(compareBy({ it.file }, { it.kind }, { it.name }))
        val sb = StringBuilder()
        sb.append("{\"rows\":[")
        for ((i, row) in rows.withIndex()) {
            if (i > 0) sb.append(',')
            sb.append('[')
                .append('"').append(escape(row.file)).append('"').append(',')
                .append('"').append(escape(row.kind)).append('"').append(',')
                .append('"').append(escape(row.name)).append('"')
                .append(']')
        }
        sb.append("]}")
        println(sb.toString())
    } finally {
        Disposer.dispose(disposable)
    }
}

/// Walk `ktFile`'s declarations and append a Row per emitted symbol.
fun collect(ktFile: KtFile, rel: String, rows: MutableList<Row>) {
    for (decl in ktFile.declarations) {
        visitDeclaration(decl, rel, rows)
    }
}

fun visitDeclaration(
    decl: org.jetbrains.kotlin.psi.KtDeclaration,
    rel: String,
    rows: MutableList<Row>,
) {
    if (decl.isLocal()) return
    if (decl is KtParameter) return
    when (decl) {
        is KtClassOrObject -> {
            val kind = classKind(decl)
            val name = decl.name ?: return
            rows.add(Row(rel, kind, name))
            // Body members.
            for (child in decl.declarations) {
                if (isSynthesized(child)) continue
                visitDeclaration(child, rel, rows)
            }
            // Primary-constructor `val`/`var` parameters become field-like
            // symbols on the host class (matching squeezy `kotlin:ctor_property`
            // promotion). Skip plain positional parameters.
            decl.primaryConstructor?.let { primary ->
                for (param in primary.valueParameters) {
                    if (param.hasValOrVar() && param.name != null) {
                        rows.add(Row(rel, "Field", param.name!!))
                    }
                }
            }
        }
        is KtEnumEntry -> {
            val name = decl.name ?: return
            rows.add(Row(rel, "Variant", name))
        }
        is KtNamedFunction -> {
            val name = decl.name ?: return
            val kind = if (decl.parent is org.jetbrains.kotlin.psi.KtClassBody) "Method" else "Function"
            rows.add(Row(rel, kind, name))
        }
        is KtSecondaryConstructor -> {
            // Use the host class name (mirrors the Java oracle's `<init>` →
            // enclosing-class-name rewrite).
            val host = decl.getContainingClassOrObject()
            host.name?.let { rows.add(Row(rel, "Method", it)) }
        }
        is KtProperty -> {
            val name = decl.name ?: return
            val kind = if (decl.parent is org.jetbrains.kotlin.psi.KtClassBody) {
                "Field"
            } else if (decl.isVar) {
                "Static"
            } else {
                "Const"
            }
            rows.add(Row(rel, kind, name))
        }
        is KtTypeAlias -> {
            val name = decl.name ?: return
            rows.add(Row(rel, "TypeAlias", name))
        }
        else -> {}
    }
}

fun classKind(klass: KtClassOrObject): String = when {
    klass is KtClass && klass.isInterface() -> "Trait"
    klass is KtClass && klass.isEnum() -> "Enum"
    klass is KtObjectDeclaration -> "Class"
    else -> "Class"
}

/// Heuristic for declarations the squeezy extractor also skips (see
/// `kotlin.rs` §4e exclusion of generated data-class members). The PSI
/// front-end does not expose a `DescriptorUtils.isSynthesized` without
/// descriptors, so we use a name/position guard that catches the standard
/// names while staying safe for user code that happens to share them
/// (`copy`/`equals`/etc.) — only suppress when the host is a data class
/// or when the function appears in the synthetic-member sentinel set.
fun isSynthesized(decl: org.jetbrains.kotlin.psi.KtDeclaration): Boolean {
    if (decl !is KtNamedFunction) return false
    val host = decl.getContainingClassOrObject() ?: return false
    val isData = (host as? KtClass)?.isData() == true
    if (!isData) return false
    return when (decl.name) {
        "copy", "equals", "hashCode", "toString" -> true
        else -> decl.name?.startsWith("component") == true
    }
}

fun org.jetbrains.kotlin.psi.KtDeclaration.isLocal(): Boolean {
    // A declaration is "local" when its parent chain hits a function body
    // / lambda body before a class body or file. The PSI exposes
    // `KtDeclaration.isLocal` as an extension; mirror it here without
    // pulling in `org.jetbrains.kotlin.psi.psiUtil.*` so the helper jar
    // stays slim.
    var current: org.jetbrains.kotlin.com.intellij.psi.PsiElement? = this.parent
    while (current != null) {
        when (current) {
            is KtFile -> return false
            is org.jetbrains.kotlin.psi.KtClassBody -> return false
            is org.jetbrains.kotlin.psi.KtBlockExpression -> return true
            is org.jetbrains.kotlin.psi.KtFunctionLiteral -> return true
        }
        current = current.parent
    }
    return false
}

fun org.jetbrains.kotlin.psi.KtClassOrObject.getContainingClassOrObject(): KtClassOrObject {
    // Stub for unused builtin (depending on Kotlin compiler version, the
    // psiUtil version may not be available); secondary constructors / etc.
    // call this; falling back to traversing parents.
    var current: org.jetbrains.kotlin.com.intellij.psi.PsiElement? = this.parent
    while (current != null) {
        if (current is KtClassOrObject) return current
        current = current.parent
    }
    return this
}

fun org.jetbrains.kotlin.psi.KtSecondaryConstructor.getContainingClassOrObject(): KtClassOrObject {
    var current: org.jetbrains.kotlin.com.intellij.psi.PsiElement? = this.parent
    while (current != null) {
        if (current is KtClassOrObject) return current
        current = current.parent
    }
    throw IllegalStateException("secondary constructor outside class")
}

fun escape(value: String): String =
    value.replace("\\", "\\\\").replace("\"", "\\\"")
