<?php

declare(strict_types=1);

namespace Foo\Bar;

enum Status: string
{
    case Ok = 'ok';
    case Failed = 'fail';

    public function label(): string
    {
        return match ($this) {
            self::Ok => 'OK',
            self::Failed => 'Failed',
        };
    }
}
