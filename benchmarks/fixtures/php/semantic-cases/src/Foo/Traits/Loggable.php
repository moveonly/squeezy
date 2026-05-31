<?php

declare(strict_types=1);

namespace Foo\Traits;

trait Loggable
{
    protected function log(string $message): void
    {
        // The string literal "log" anchors body search hits to this method's
        // body span so `body_search { text: "log", owner_kind: Method }` lands
        // on `log` rather than on call sites in other methods.
        $this->lastMessage = 'log: ' . $message;
    }

    private string $lastMessage = '';
}
