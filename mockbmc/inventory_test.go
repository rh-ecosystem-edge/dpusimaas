package main

import (
	"bytes"
	"encoding/binary"
	"io"
	"testing"

	"github.com/nvidia/doca-platform/test/mock/dpuagent/pkg/artifact"
)

func testImage(payload string) []byte {
	img := make([]byte, bfbHeaderLen)
	binary.LittleEndian.PutUint32(img[0:4], bfbMagic)
	binary.LittleEndian.PutUint32(img[8:12], uint32(len(payload)))
	img = append(img, payload...)
	for len(img)%8 != 0 {
		img = append(img, 0)
	}
	return img
}

// split runs a stream through the mock BMC's own splitter, as it sees a download.
func split(t *testing.T, stream []byte) *artifact.BFBSplitter {
	t.Helper()
	s := artifact.NewBFBSplitter()
	if _, err := s.Write(stream); err != nil {
		t.Fatal(err)
	}
	if err := s.Finish(); err != nil {
		t.Fatal(err)
	}
	return s
}

func inject(t *testing.T, stream []byte) []byte {
	t.Helper()
	versions := BFBVersions{ATF: "4.15.0-4-g419fbf393", BSP: "4.15.0.13998", UEFI: "4.15.0-19-g37c6f5adb2", DOCA: "3.4.1"}
	out, err := io.ReadAll(withInventory(io.NopCloser(bytes.NewReader(stream)), inventoryImage(versions)))
	if err != nil {
		t.Fatal(err)
	}
	return out
}

func TestBFBWithoutInventoryGetsOne(t *testing.T) {
	bfcfg := []byte(`{"ignition": {}}`)
	stream := append(append(testImage("kernel"), testImage("initramfs-with-odd-length")...), bfcfg...)
	s := split(t, inject(t, stream))
	v, err := s.Inventory().Versions()
	if err != nil {
		t.Fatalf("the mock finds no inventory: %v", err)
	}
	if v.BSP != "4.15.0.13998" || v.UEFI != "4.15.0-19-g37c6f5adb2" || v.ATF != "4.15.0-4-g419fbf393" || v.DOCA != "3.4.1" {
		t.Errorf("versions %+v", v)
	}
	if !bytes.Equal(s.BFCFG(), bfcfg) {
		t.Errorf("bf.cfg %q, want %q", s.BFCFG(), bfcfg)
	}
}

func TestBFBWithInventoryIsUntouched(t *testing.T) {
	own := "\n\"@odata.description\": \"This JSON represents the BF software inventory\",\n\"Members\": [{\"Name\": \"BF3_BSP\", \"Version\": \"9.9\"}],\n\"Members@odata.count\": 1\n"
	stream := append(append(testImage("kernel"), testImage(own)...), []byte("bfcfg")...)
	if out := inject(t, stream); !bytes.Equal(out, stream) {
		t.Errorf("a BFB with its own inventory was changed (%d -> %d bytes)", len(stream), len(out))
	}
}

func TestNotABFBIsUntouched(t *testing.T) {
	stream := []byte("just a bf.cfg, no images at all")
	if out := inject(t, stream); !bytes.Equal(out, stream) {
		t.Errorf("got %q", out)
	}
}
