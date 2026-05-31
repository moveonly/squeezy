// Squeezy benchmark oracle for Dart corpora.
//
// Walks the supplied source root with `package:analyzer`'s
// `AnalysisContextCollection`, resolves each library, and emits one JSON line
// per resolved library to stdout (NDJSON), followed by a trailing
// `{"summary": ...}` line. Each library line carries the file-relative path of
// every member declaration plus the host library's import / export / part
// directives.
//
// The Rust bench wrapper (`benchmarks/squeezy-graph-bench/src/oracles/
// dart_oracle.rs`) parses the stream into the squeezy `SymbolScan`/oracle
// accuracy comparison. The shape mirrors `docs/internal/lang-specs/dart.md`
// §9: one `Library` row per defining unit, `Class`/`Mixin`/`Extension`/
// `ExtensionType`/`Enum`/`Function`/`Method`/`Field`/`Variable`/`Const`/
// `TypeAlias` rows per declaration, with members of `part`-included files
// re-parented onto the host library's defining file.
//
// On unrecoverable failure the script emits `{"error": "..."}` on stderr and
// exits 1, so the Rust side can degrade to scan-only mode.

import 'dart:convert';
import 'dart:io';

// ignore_for_file: deprecated_member_use
import 'package:analyzer/dart/analysis/analysis_context_collection.dart';
import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:path/path.dart' as p;

const _codegenSuffixes = <String>[
  '.g.dart',
  '.freezed.dart',
  '.mocks.dart',
];

bool _isCodegen(String relPath) {
  for (final suffix in _codegenSuffixes) {
    if (relPath.endsWith(suffix)) return true;
  }
  return false;
}

String _relativize(String absPath, String root) {
  final rel = p.relative(absPath, from: root);
  return p.posix.joinAll(p.split(rel));
}

List<String> _classModifierAttributes(ClassElement cls) {
  final attrs = <String>[];
  if (cls.isSealed) attrs.add('dart:sealed');
  if (cls.isBase) attrs.add('dart:base');
  if (cls.isInterface) attrs.add('dart:interface');
  if (cls.isFinal) attrs.add('dart:final');
  if (cls.isMixinClass) attrs.add('dart:mixin-class');
  if (cls.isAbstract) attrs.add('dart:abstract');
  return attrs;
}

Map<String, Object> _row(
  String file,
  String kind,
  String name, {
  List<String> attributes = const [],
}) {
  final row = <String, Object>{
    'file': file,
    'kind': kind,
    'name': name,
  };
  if (attributes.isNotEmpty) {
    row['attributes'] = attributes;
  }
  return row;
}

Future<int> _run(List<String> arguments) async {
  if (arguments.isEmpty) {
    stderr.writeln('dart-oracle: missing source root argument');
    return 64;
  }
  final root = p.normalize(p.absolute(arguments.first));
  final rootDir = Directory(root);
  if (!rootDir.existsSync()) {
    stderr.writeln('dart-oracle: source root does not exist: $root');
    return 66;
  }

  stderr.writeln('dart-oracle: scanning $root');
  final collection = AnalysisContextCollection(
    includedPaths: [root],
  );

  var resolvedLibraries = 0;
  var unparseableFiles = 0;
  var emittedSymbolRows = 0;
  final emittedLibraries = <String>{};
  final unparseableSamples = <String>[];

  try {
    for (final context in collection.contexts) {
      final files = context.contextRoot
          .analyzedFiles()
          .where((path) => path.endsWith('.dart'))
          .toList()
        ..sort();
      final session = context.currentSession;
      for (final absPath in files) {
        final rel = _relativize(absPath, root);
        if (_isCodegen(rel)) {
          // §4k: codegen excluded from FP/FN accounting symmetrically.
          continue;
        }
        final result = await session.getResolvedUnit(absPath);
        if (result is! ResolvedUnitResult) {
          unparseableFiles += 1;
          if (unparseableSamples.length < 16) unparseableSamples.add(rel);
          stdout.writeln(jsonEncode({
            'file': rel,
            'unparseable': true,
          }));
          continue;
        }

        final library = result.libraryElement;
        final definingSource = library.definingCompilationUnit.source.fullName;
        // Per spec §9: only emit one `Library` row per host library (the
        // defining unit). Skip part-file units entirely here — their members
        // are emitted later under the host library row, but we keep one
        // empty entry per file so the bench tracks which files were
        // analyzed.
        if (definingSource != absPath) {
          // Part file: emit an explicit marker (no symbols, points at host).
          final hostRel = _relativize(definingSource, root);
          stdout.writeln(jsonEncode({
            'file': rel,
            'part_of': hostRel,
            'symbols': const <Map<String, Object>>[],
            'imports': const <String>[],
            'exports': const <String>[],
            'parts': const <String>[],
          }));
          continue;
        }

        if (emittedLibraries.contains(definingSource)) {
          // Library already emitted from a sibling unit walk; skip.
          continue;
        }
        emittedLibraries.add(definingSource);
        resolvedLibraries += 1;

        final hostRel = rel;
        final symbols = <Map<String, Object>>[];
        final imports = <Map<String, Object>>[];
        final exports = <Map<String, Object>>[];
        final parts = <String>[];

        // Library row — `name` is the explicit `library` directive or the
        // synthetic empty string analyzer assigns when omitted.
        symbols.add(_row(
          hostRel,
          'Library',
          library.name,
        ));

        // Library directives (only from the defining unit; part files cannot
        // redeclare imports).
        for (final import in library.definingCompilationUnit.libraryImports) {
          final uri = _directiveUri(import.uri);
          if (uri == null) continue;
          imports.add({
            'uri': uri,
            'prefix': import.prefix?.element.name ?? '',
          });
        }
        for (final export in library.definingCompilationUnit.libraryExports) {
          final uri = _directiveUri(export.uri);
          if (uri == null) continue;
          exports.add({'uri': uri});
        }
        for (final part in library.definingCompilationUnit.parts) {
          final uri = _directiveUri(part.uri);
          if (uri == null) continue;
          parts.add(uri);
        }

        // Walk every CompilationUnit (defining + parts). The library's
        // defining unit symbols are emitted under `hostRel`; part-file
        // members are emitted under the *part's* relative path so the
        // (file, kind, name) keys match what the squeezy tree-sitter
        // extractor produces today. When the squeezy graph resolver lands
        // part-of re-parenting (`__dart_part_of__` import marker, spec
        // §4a), the oracle helper switches back to the §9 rewrite so both
        // sides keep emitting symbols under the host file's path.
        for (final unit in library.units) {
          final unitSource = unit.source.fullName;
          final unitRel = unitSource == definingSource
              ? hostRel
              : _relativize(unitSource, root);
          _walkUnit(unit, unitRel, symbols);
        }

        emittedSymbolRows += symbols.length;
        stdout.writeln(jsonEncode({
          'file': hostRel,
          'symbols': symbols,
          'imports': imports,
          'exports': exports,
          'parts': parts,
        }));
      }
    }
  } finally {
    await collection.dispose();
  }

  stdout.writeln(jsonEncode({
    'summary': {
      'resolved_libraries': resolvedLibraries,
      'unparseable_files': unparseableFiles,
      'unparseable_samples': unparseableSamples,
      'emitted_symbol_rows': emittedSymbolRows,
      'mode': 'analyzer',
    },
  }));
  return 0;
}

String? _directiveUri(Object uri) {
  // `DirectiveUri` hierarchy carries the parsed URI text on the
  // `DirectiveUriWithRelativeUriString` subtype; fall back to `toString`
  // for sealed `DirectiveUri` subclasses we do not pattern-match.
  try {
    final dynamic dyn = uri;
    final str = dyn.relativeUriString as String?;
    if (str != null && str.isNotEmpty) return str;
  } catch (_) {}
  final asString = uri.toString();
  return asString.isEmpty ? null : asString;
}

void _walkUnit(
  CompilationUnitElement unit,
  String hostRel,
  List<Map<String, Object>> symbols,
) {
  for (final cls in unit.classes) {
    final attrs = _classModifierAttributes(cls);
    symbols.add(_row(hostRel, 'Class', cls.name, attributes: attrs));
    _walkInterfaceMembers(cls, hostRel, symbols);
  }
  for (final mix in unit.mixins) {
    symbols.add(_row(hostRel, 'Mixin', mix.name));
    _walkInterfaceMembers(mix, hostRel, symbols);
  }
  for (final ext in unit.extensions) {
    final name = ext.name ?? '__ext_${ext.nameOffset}';
    symbols.add(_row(hostRel, 'Extension', name, attributes: const [
      'dart:extension',
    ]));
    _walkExtensionMembers(ext, hostRel, symbols);
  }
  for (final ext in unit.extensionTypes) {
    symbols.add(_row(hostRel, 'ExtensionType', ext.name, attributes: const [
      'dart:extension-type',
    ]));
    _walkInterfaceMembers(ext, hostRel, symbols);
  }
  for (final en in unit.enums) {
    symbols.add(_row(hostRel, 'Enum', en.name));
    for (final field in en.fields) {
      if (field.isSynthetic) continue;
      // Enum constants surface as static, const fields on the EnumElement.
      if (field.isEnumConstant) {
        symbols.add(_row(hostRel, 'Variant', field.name));
      } else {
        symbols.add(_row(hostRel, 'Field', field.name, attributes: const [
          'dart:enum-field',
        ]));
      }
    }
    for (final method in en.methods) {
      if (method.isSynthetic) continue;
      symbols.add(_row(hostRel, 'Method', method.name));
    }
    for (final ctor in en.constructors) {
      if (ctor.isSynthetic) continue;
      symbols.add(_row(hostRel, 'Method', _ctorName(en.name, ctor),
          attributes: const ['dart:constructor']));
    }
  }
  for (final fn in unit.functions) {
    if (fn.isSynthetic) continue;
    symbols.add(_row(hostRel, 'Function', fn.name));
  }
  for (final variable in unit.topLevelVariables) {
    if (variable.isSynthetic) continue;
    final kind = (variable.isConst || variable.isFinal) ? 'Const' : 'Variable';
    symbols.add(_row(hostRel, kind, variable.name));
  }
  for (final accessor in unit.accessors) {
    if (accessor.isSynthetic) continue;
    if (accessor.isGetter) {
      symbols.add(_row(hostRel, 'Method', accessor.name, attributes: const [
        'dart:getter',
      ]));
    } else if (accessor.isSetter) {
      var name = accessor.name;
      if (name.endsWith('=')) name = name.substring(0, name.length - 1);
      symbols.add(_row(hostRel, 'Method', name, attributes: const [
        'dart:setter',
      ]));
    }
  }
  for (final alias in unit.typeAliases) {
    symbols.add(_row(hostRel, 'TypeAlias', alias.name, attributes: const [
      'dart:typedef',
    ]));
  }
}

void _walkInterfaceMembers(
  InterfaceElement element,
  String hostRel,
  List<Map<String, Object>> symbols,
) {
  for (final field in element.fields) {
    if (field.isSynthetic) continue;
    symbols.add(_row(hostRel, 'Field', field.name));
  }
  for (final method in element.methods) {
    if (method.isSynthetic) continue;
    symbols.add(_row(hostRel, 'Method', method.name));
  }
  for (final ctor in element.constructors) {
    if (ctor.isSynthetic) continue;
    final attrs = <String>['dart:constructor'];
    if (ctor.isFactory) attrs.add('dart:factory');
    symbols.add(_row(hostRel, 'Method', _ctorName(element.name, ctor),
        attributes: attrs));
  }
  for (final accessor in element.accessors) {
    if (accessor.isSynthetic) continue;
    if (accessor.isGetter) {
      symbols.add(_row(hostRel, 'Method', accessor.name, attributes: const [
        'dart:getter',
      ]));
    } else if (accessor.isSetter) {
      var name = accessor.name;
      if (name.endsWith('=')) name = name.substring(0, name.length - 1);
      symbols.add(_row(hostRel, 'Method', name, attributes: const [
        'dart:setter',
      ]));
    }
  }
}

void _walkExtensionMembers(
  ExtensionElement element,
  String hostRel,
  List<Map<String, Object>> symbols,
) {
  for (final field in element.fields) {
    if (field.isSynthetic) continue;
    symbols.add(_row(hostRel, 'Field', field.name));
  }
  for (final method in element.methods) {
    if (method.isSynthetic) continue;
    symbols.add(_row(hostRel, 'Method', method.name));
  }
  for (final accessor in element.accessors) {
    if (accessor.isSynthetic) continue;
    if (accessor.isGetter) {
      symbols.add(_row(hostRel, 'Method', accessor.name, attributes: const [
        'dart:getter',
      ]));
    } else if (accessor.isSetter) {
      var name = accessor.name;
      if (name.endsWith('=')) name = name.substring(0, name.length - 1);
      symbols.add(_row(hostRel, 'Method', name, attributes: const [
        'dart:setter',
      ]));
    }
  }
}

String _ctorName(String typeName, ConstructorElement ctor) {
  final ctorName = ctor.name;
  if (ctorName.isEmpty) return typeName;
  return '$typeName.$ctorName';
}

void main(List<String> arguments) async {
  try {
    final code = await _run(arguments);
    exit(code);
  } catch (e, st) {
    stderr.writeln('dart-oracle: fatal: $e');
    stderr.writeln(st);
    exit(1);
  }
}
