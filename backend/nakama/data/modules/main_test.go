package main

import (
	"os"
	"testing"
)

// TestMain makes background push flushes synchronous for the whole suite.
// With the real goroutine, a flush queued by one test lands in the next
// test's send capture. A test that checks the async path replaces
// eventPushGo itself.
func TestMain(m *testing.M) {
	eventPushGo = func(f func()) { f() }
	os.Exit(m.Run())
}
