<?php

declare(strict_types=1);

namespace Foo\Bar;

use Foo\Bar\Service;

class Repository
{
    public function fetch(int $id): void
    {
        $service = new Service();
        $service->run($id);
    }
}
