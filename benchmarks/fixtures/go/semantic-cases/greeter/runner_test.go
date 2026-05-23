package greeter

import "testing"

func TestRunner(t *testing.T) {
	if NewRunner("Ada").Greet("Ada") == "" {
		t.Fatal("empty greeting")
	}
}
