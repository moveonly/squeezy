<?php

declare(strict_types=1);

namespace Squeezy\PhpOracle;

use PhpParser\Node;
use PhpParser\Node\Const_;
use PhpParser\Node\Stmt;
use PhpParser\NodeVisitorAbstract;

/**
 * Visits a single compilation unit and pushes declaration rows + class-level
 * edges into the shared output buffers. The collector is shared across files
 * to keep allocation costs low.
 */
final class Collector extends NodeVisitorAbstract
{
    /** @var list<array{0:string,1:string,2:string}> */
    public array $rows;

    /** @var list<array{0:string,1:string,2:string}> */
    public array $edges;

    private string $relPath;

    /** @var array<int,string> stack of namespace dotted names for nested scopes */
    private array $namespaceStack = [];

    /** @var array<int,string> stack of enclosing type leaf names (for nested classes) */
    private array $typeStack = [];

    public function setFile(string $relPath, array &$rows, array &$edges): void
    {
        $this->relPath = $relPath;
        $this->rows = &$rows;
        $this->edges = &$edges;
        $this->namespaceStack = [];
        $this->typeStack = [];
    }

    public function enterNode(Node $node): ?int
    {
        if ($node instanceof Stmt\Namespace_) {
            $name = $node->name !== null ? \implode('.', $node->name->getParts()) : '';
            if ($name !== '') {
                $this->rows[] = [$this->relPath, 'Namespace', $name];
            }
            $this->namespaceStack[] = $name;
            return null;
        }

        if ($node instanceof Stmt\Class_) {
            $name = $node->name?->toString() ?? '';
            if ($name === '') {
                return null;
            }
            $this->rows[] = [$this->relPath, 'Class', $name];
            if ($node->extends !== null) {
                $this->edges[] = [
                    $this->relPath,
                    'Extends',
                    $name . '->' . \implode('.', $node->extends->getParts()),
                ];
            }
            foreach ($node->implements as $iface) {
                $this->edges[] = [
                    $this->relPath,
                    'Implements',
                    $name . '->' . \implode('.', $iface->getParts()),
                ];
            }
            foreach ($node->stmts as $stmt) {
                if ($stmt instanceof Stmt\TraitUse) {
                    foreach ($stmt->traits as $trait) {
                        $this->edges[] = [
                            $this->relPath,
                            'UsesTrait',
                            $name . '->' . \implode('.', $trait->getParts()),
                        ];
                    }
                }
            }
            $this->typeStack[] = $name;
            return null;
        }

        if ($node instanceof Stmt\Interface_) {
            $name = $node->name?->toString() ?? '';
            if ($name === '') {
                return null;
            }
            $this->rows[] = [$this->relPath, 'Interface', $name];
            foreach ($node->extends as $iface) {
                $this->edges[] = [
                    $this->relPath,
                    'Extends',
                    $name . '->' . \implode('.', $iface->getParts()),
                ];
            }
            $this->typeStack[] = $name;
            return null;
        }

        if ($node instanceof Stmt\Trait_) {
            $name = $node->name?->toString() ?? '';
            if ($name === '') {
                return null;
            }
            $this->rows[] = [$this->relPath, 'Trait', $name];
            $this->typeStack[] = $name;
            return null;
        }

        if ($node instanceof Stmt\Enum_) {
            $name = $node->name?->toString() ?? '';
            if ($name === '') {
                return null;
            }
            $this->rows[] = [$this->relPath, 'Enum', $name];
            foreach ($node->implements as $iface) {
                $this->edges[] = [
                    $this->relPath,
                    'Implements',
                    $name . '->' . \implode('.', $iface->getParts()),
                ];
            }
            $this->typeStack[] = $name;
            return null;
        }

        if ($node instanceof Stmt\EnumCase) {
            $this->rows[] = [$this->relPath, 'Variant', $node->name->toString()];
            return null;
        }

        if ($node instanceof Stmt\Function_) {
            $this->rows[] = [$this->relPath, 'Function', $node->name->toString()];
            return null;
        }

        if ($node instanceof Stmt\ClassMethod) {
            $this->rows[] = [$this->relPath, 'Method', $node->name->toString()];
            return null;
        }

        if ($node instanceof Stmt\Property) {
            foreach ($node->props as $prop) {
                $this->rows[] = [$this->relPath, 'Property', $prop->name->toString()];
            }
            return null;
        }

        if ($node instanceof Stmt\ClassConst) {
            foreach ($node->consts as $const) {
                $this->rows[] = [$this->relPath, 'Constant', $const->name->toString()];
            }
            return null;
        }

        if ($node instanceof Stmt\Const_) {
            // Top-level `const X = 1;` is intentionally skipped per the spec
            // (it is rare and noisy in templates).
            return null;
        }

        if ($node instanceof Stmt\InlineHTML) {
            // Inline HTML is fallback content — not a declaration.
            return null;
        }

        return null;
    }

    public function leaveNode(Node $node): ?int
    {
        if (
            $node instanceof Stmt\Class_
            || $node instanceof Stmt\Interface_
            || $node instanceof Stmt\Trait_
            || $node instanceof Stmt\Enum_
        ) {
            \array_pop($this->typeStack);
        }
        if ($node instanceof Stmt\Namespace_) {
            \array_pop($this->namespaceStack);
        }
        return null;
    }
}
