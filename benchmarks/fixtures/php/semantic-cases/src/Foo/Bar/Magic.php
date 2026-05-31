<?php

declare(strict_types=1);

namespace Foo\Bar;

class Magic
{
    public function __call(string $name, array $args): mixed
    {
        return [$name, $args];
    }

    public function __get(string $name): mixed
    {
        return null;
    }
}

class MagicCaller
{
    public function invoke(Magic $m): void
    {
        // Magic-method call site: spec §4(f) requires the call-edge confidence
        // to drop to Partial because dispatch is implicit.
        $m->undefined('hello');
    }
}
