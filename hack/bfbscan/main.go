// bfbscan streams a BFB (or the registry's bfb+bf.cfg concat URL) through the mock BMC's splitter,
// and reports where the software inventory is: what the mock sees versus the raw stream.
package main

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"os"

	"github.com/nvidia/doca-platform/test/mock/dpuagent/pkg/artifact"
)

type tee struct {
	split  *artifact.BFBSplitter
	off    int64
	marker []int64
	tail   []byte
}

func (t *tee) Write(p []byte) (int, error) {
	buf := append(t.tail, p...)
	for i := 0; ; {
		j := bytes.Index(buf[i:], []byte("This JSON represents"))
		if j < 0 {
			break
		}
		t.marker = append(t.marker, t.off-int64(len(t.tail))+int64(i+j))
		i += j + 1
	}
	if len(buf) > 32 {
		t.tail = append([]byte{}, buf[len(buf)-32:]...)
	}
	t.off += int64(len(p))
	return t.split.Write(p)
}

func main() {
	resp, err := http.Get(os.Args[1])
	if err != nil {
		panic(err)
	}
	defer resp.Body.Close()
	t := &tee{split: artifact.NewBFBSplitter()}
	n, err := io.Copy(t, resp.Body)
	fmt.Println("bytes", n, "copy err", err, "finish err", t.split.Finish())
	fmt.Println("images", t.split.Images(), "bfb bytes", t.split.BFBSize(), "trailer bytes", len(t.split.BFCFG()))
	fmt.Println("inventory found by the mock:", t.split.Inventory().Found(), "marker offsets in stream:", t.marker)
}
