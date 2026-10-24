package main

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"fmt"
	"io"
	"strings"
)

// The mock BMC learns what a BFB installs (the firmware versions it then reports, which DPF checks
// against the BFB object) from the software inventory NVIDIA's BFBs carry. Red Hat's RHCOS BFB
// carries none, so the mock rejects it. The fetch proxy then appends an inventory to the BFB, as one
// more mlx-mkbfb image, with the versions dpusimaas was told the BFB installs (--bfb-versions;
// openshift-dpf sets the same ones on its BFB object).

const (
	bfbMagic     = 0x13026642 // "Bf\x02\x13", little endian
	bfbHeaderLen = 24
	inventoryTag = "This JSON represents"
)

// BFBVersions is what a BFB without an inventory is taken to install.
type BFBVersions struct{ ATF, BSP, UEFI, DOCA string }

func parseBFBVersions(s string) (BFBVersions, error) {
	var v BFBVersions
	for _, kv := range strings.Split(s, ",") {
		k, val, ok := strings.Cut(strings.TrimSpace(kv), "=")
		if !ok {
			return v, fmt.Errorf("%q is not key=value", kv)
		}
		switch strings.ToLower(k) {
		case "atf":
			v.ATF = val
		case "bsp":
			v.BSP = val
		case "uefi":
			v.UEFI = val
		case "doca":
			v.DOCA = val
		default:
			return v, fmt.Errorf("unknown version %q (want atf, bsp, uefi, doca)", k)
		}
	}
	return v, nil
}

// inventoryImage is an mlx-mkbfb image whose payload is a software inventory in the layout the BFB
// tools (and the mock) read: printable lines from "This JSON represents" to "Members@odata.count".
func inventoryImage(v BFBVersions) []byte {
	lines := []string{
		`"@odata.description": "This JSON represents the software inventory of the BFB (added by dpusimaas)",`,
		`"Members": [`,
		fmt.Sprintf(`{"Name": "BF3_ATF", "Version": %q},`, v.ATF),
		fmt.Sprintf(`{"Name": "BF3_BSP", "Version": %q},`, v.BSP),
		fmt.Sprintf(`{"Name": "BF3_UEFI", "Version": %q},`, v.UEFI),
		fmt.Sprintf(`{"Name": "DOCA", "Version": %q}`, v.DOCA),
		`],`,
		`"Members@odata.count": 4`,
	}
	// Indented like NVIDIA's: the readers drop printable runs under 4 characters, such as a bare "],".
	payload := []byte("\n    " + strings.Join(lines, "\n    ") + "\n")
	img := make([]byte, bfbHeaderLen, bfbHeaderLen+len(payload)+8)
	binary.LittleEndian.PutUint32(img[0:4], bfbMagic)
	binary.LittleEndian.PutUint32(img[8:12], uint32(len(payload)))
	img = append(img, payload...)
	for len(img)%8 != 0 {
		img = append(img, 0)
	}
	return img
}

// withInventory passes a BFB (+ bf.cfg) stream through, adding inv after the last image when the
// BFB has no inventory of its own. Anything that isn't a BFB passes through unchanged.
func withInventory(src io.ReadCloser, inv []byte) io.ReadCloser {
	pr, pw := io.Pipe()
	go func() {
		err := copyWithInventory(pw, src, inv)
		_ = src.Close()
		pw.CloseWithError(err)
	}()
	return pr
}

func copyWithInventory(w io.Writer, r io.Reader, inv []byte) error {
	br := bufio.NewReaderSize(r, 1<<20)
	images, found := 0, false
	hdr := make([]byte, bfbHeaderLen)
	for {
		n, err := io.ReadFull(br, hdr)
		if n < bfbHeaderLen || binary.LittleEndian.Uint32(hdr[0:4]) != bfbMagic {
			// The BFB is over: what follows (if anything) is the bf.cfg.
			if images > 0 && !found {
				if _, werr := w.Write(inv); werr != nil {
					return werr
				}
			}
			if _, werr := w.Write(hdr[:n]); werr != nil {
				return werr
			}
			if err == nil {
				_, err = io.Copy(w, br)
			}
			if err == io.EOF || err == io.ErrUnexpectedEOF {
				return nil
			}
			return err
		}
		images++
		if _, err := w.Write(hdr); err != nil {
			return err
		}
		size := int64(binary.LittleEndian.Uint32(hdr[8:12]))
		if rem := size % 8; rem != 0 {
			size += 8 - rem
		}
		s := &tagScanner{w: w, found: found}
		if _, err := io.CopyN(s, br, size); err != nil {
			return fmt.Errorf("BFB image %d: %w", images, err)
		}
		found = s.found
	}
}

// tagScanner writes through and notes whether the inventory tag went by (also across writes).
type tagScanner struct {
	w     io.Writer
	found bool
	tail  []byte
}

func (s *tagScanner) Write(p []byte) (int, error) {
	if !s.found {
		buf := append(s.tail, p...)
		s.found = bytes.Contains(buf, []byte(inventoryTag))
		if len(buf) > len(inventoryTag) {
			buf = buf[len(buf)-len(inventoryTag):]
		}
		s.tail = append(s.tail[:0], buf...)
	}
	return s.w.Write(p)
}
