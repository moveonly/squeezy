#!/usr/bin/env php
<?php

declare(strict_types=1);

// Squeezy PHP oracle. Walks `argv[1]` recursively, parses every `.php` file
// with nikic/PHP-Parser, and prints a JSON object shaped to slot into the
// shared SymbolScan format used by the C# and Java oracles. The Rust side
// (benchmarks/squeezy-graph-bench/src/oracles/php_oracle.rs) parses this
// output and feeds it into compare_symbol_sets.

require __DIR__ . '/vendor/autoload.php';

use PhpParser\NodeTraverser;
use PhpParser\ParserFactory;
use Squeezy\PhpOracle\Collector;

if ($argc < 2) {
    fwrite(STDERR, "usage: oracle.php <workspace-root>\n");
    exit(2);
}

$root = realpath($argv[1]);
if ($root === false || !is_dir($root)) {
    fwrite(STDERR, "workspace root not found: {$argv[1]}\n");
    exit(2);
}

$parser = (new ParserFactory())->createForHostVersion();
$collector = new Collector();
$traverser = new NodeTraverser();
$traverser->addVisitor($collector);

$rows = [];
$edges = [];
$unparseableFiles = [];

$iterator = new RecursiveIteratorIterator(
    new RecursiveDirectoryIterator($root, RecursiveDirectoryIterator::SKIP_DOTS)
);
$files = [];
foreach ($iterator as $entry) {
    if (!$entry->isFile()) {
        continue;
    }
    $path = $entry->getPathname();
    if (substr($path, -4) !== '.php') {
        continue;
    }
    $files[] = $path;
}
sort($files);

foreach ($files as $path) {
    $relative = ltrim(substr($path, strlen($root)), DIRECTORY_SEPARATOR);
    $relative = str_replace(DIRECTORY_SEPARATOR, '/', $relative);
    $source = file_get_contents($path);
    if ($source === false) {
        $unparseableFiles[] = $relative;
        continue;
    }
    try {
        $tree = $parser->parse($source);
    } catch (Throwable $e) {
        $unparseableFiles[] = $relative;
        continue;
    }
    if ($tree === null) {
        $unparseableFiles[] = $relative;
        continue;
    }
    $collector->setFile($relative, $rows, $edges);
    $traverser->traverse($tree);
}

echo json_encode(
    [
        'rows' => $rows,
        'edges' => $edges,
        'unparseable_files' => $unparseableFiles,
    ],
    JSON_UNESCAPED_SLASHES
), "\n";
