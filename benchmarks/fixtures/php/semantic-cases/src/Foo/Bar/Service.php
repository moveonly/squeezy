<?php

declare(strict_types=1);

namespace Foo\Bar;

use Foo\Traits\Loggable;

class Service implements IRunner
{
    use Loggable;

    public string $prefix = 'svc-';

    public function run(int $id): void
    {
        $this->log($this->prefix . $id);
    }
}
