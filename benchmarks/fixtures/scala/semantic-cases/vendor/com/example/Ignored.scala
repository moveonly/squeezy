package com.example.vendored

// Vendored source — excluded from the workspace symbol scan via the
// `vendor/` rule shared with the Java fixture. Should not appear in the
// fixture's symbol oracle.
class Ignored {
  def hidden(): Unit = ()
}
