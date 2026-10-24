package main

import (
	"runtime"

	"github.com/vishvananda/netns"
)

// inNetns runs fn with the calling goroutine's thread switched into the named namespace (none:
// where it is). Sockets fn opens stay in that namespace afterwards.
func inNetns(name string, fn func() error) error {
	if name == "" {
		return fn()
	}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	orig, err := netns.Get()
	if err != nil {
		return err
	}
	defer orig.Close()
	target, err := netns.GetFromName(name)
	if err != nil {
		return err
	}
	defer target.Close()
	if err := netns.Set(target); err != nil {
		return err
	}
	defer netns.Set(orig) //nolint:errcheck
	return fn()
}
