<?php

declare(strict_types=1);

namespace Psr\Log;

/**
 * Vendor stub copied into the fixture so the fallback-quality query has a
 * `vendor/` payload to surface. Squeezy must not include this file in
 * oracle-comparison counts.
 */
interface LoggerInterface
{
    public function emergency(string|\Stringable $message, array $context = []): void;
    public function info(string|\Stringable $message, array $context = []): void;
}
