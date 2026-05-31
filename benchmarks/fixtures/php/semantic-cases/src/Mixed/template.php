<?php

declare(strict_types=1);

namespace Mixed;

$title = 'Hello, world';
?>
<!DOCTYPE html>
<html>
<head><title><?= $title ?></title></head>
<body>
  <h1><?= $title ?></h1>
  <?php
  class TemplateRenderer
  {
      public function render(string $body): string
      {
          return "<p>{$body}</p>";
      }
  }
  ?>
</body>
</html>
