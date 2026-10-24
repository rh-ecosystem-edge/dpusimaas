package main

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"log/slog"
	"math/big"
	"net"
	"net/http"
	"net/http/httputil"
	"os"
	"strings"
	"sync"
	"time"
)

// The mock BMC downloads the BFB over HTTPS whatever DPF asks for: older DPF (v26.4) sends
// TransferProtocol HTTP and a bfb-registry that only speaks HTTP. The mock's download goes through
// HTTPS_PROXY and doesn't verify certificates (like a real BMC), so mockbmc points its HTTPS_PROXY
// at this proxy: it accepts the mock's TLS itself and fetches from the registry in whatever the
// registry speaks.

func startFetchProxy(versions BFBVersions) error {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return err
	}
	cert, err := selfSignedCert()
	if err != nil {
		return err
	}
	p := &fetchProxy{tlsConfig: &tls.Config{Certificates: []tls.Certificate{cert}}, schemes: map[string]string{}, inventory: inventoryImage(versions)}
	go func() { _ = http.Serve(ln, p) }()
	// Read once, at the mock's first download (net/http's ProxyFromEnvironment).
	return os.Setenv("HTTPS_PROXY", "http://"+ln.Addr().String())
}

type fetchProxy struct {
	tlsConfig *tls.Config
	inventory []byte // added to BFBs without one
	mu        sync.Mutex
	schemes   map[string]string // origin host:port -> http or https
}

func (p *fetchProxy) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodConnect {
		http.Error(w, "only CONNECT", http.StatusMethodNotAllowed)
		return
	}
	origin := r.Host
	conn, _, err := w.(http.Hijacker).Hijack()
	if err != nil {
		return
	}
	_, _ = conn.Write([]byte("HTTP/1.1 200 Connection Established\r\n\r\n"))
	tlsConn := tls.Server(conn, p.tlsConfig)
	scheme := p.scheme(origin)
	slog.Info("fetching for a mock BMC", "origin", origin, "scheme", scheme)
	rp := &httputil.ReverseProxy{
		Rewrite: func(pr *httputil.ProxyRequest) {
			pr.Out.URL.Scheme = scheme
			pr.Out.URL.Host = origin
			pr.Out.Host = origin
		},
		Transport: &http.Transport{TLSClientConfig: &tls.Config{InsecureSkipVerify: true}, ResponseHeaderTimeout: 2 * time.Minute}, //nolint:gosec // the registry is self-signed
		ModifyResponse: func(resp *http.Response) error {
			if resp.StatusCode == http.StatusOK && strings.Contains(resp.Request.URL.Path, "/bfb/") {
				// The length changes when an inventory is added; the mock doesn't need it.
				resp.Body = withInventory(resp.Body, p.inventory)
				resp.ContentLength = -1
				resp.Header.Del("Content-Length")
			}
			return nil
		},
	}
	srv := &http.Server{Handler: rp, ReadHeaderTimeout: time.Minute}
	_ = srv.Serve(&oneConn{conn: tlsConn, done: make(chan struct{})})
}

// scheme finds out (once per origin) whether it speaks TLS.
func (p *fetchProxy) scheme(origin string) string {
	p.mu.Lock()
	defer p.mu.Unlock()
	if s, ok := p.schemes[origin]; ok {
		return s
	}
	s := "http"
	if c, err := tls.DialWithDialer(&net.Dialer{Timeout: 10 * time.Second}, "tcp", origin, &tls.Config{InsecureSkipVerify: true}); err == nil { //nolint:gosec
		_ = c.Close()
		s = "https"
	}
	p.schemes[origin] = s
	return s
}

// oneConn is a listener that hands out one connection, then waits for it to close.
type oneConn struct {
	conn net.Conn
	once sync.Once
	done chan struct{}
}

func (l *oneConn) Accept() (net.Conn, error) {
	var c net.Conn
	l.once.Do(func() { c = &closeNotify{Conn: l.conn, done: l.done} })
	if c != nil {
		return c, nil
	}
	<-l.done
	return nil, net.ErrClosed
}

func (l *oneConn) Close() error   { return nil }
func (l *oneConn) Addr() net.Addr { return l.conn.LocalAddr() }

type closeNotify struct {
	net.Conn
	done chan struct{}
	once sync.Once
}

func (c *closeNotify) Close() error {
	c.once.Do(func() { close(c.done) })
	return c.Conn.Close()
}

func selfSignedCert() (tls.Certificate, error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return tls.Certificate{}, err
	}
	tmpl := &x509.Certificate{
		SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "dpusimaas fetch proxy"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(10 * 365 * 24 * time.Hour),
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		return tls.Certificate{}, err
	}
	return tls.Certificate{Certificate: [][]byte{der}, PrivateKey: key}, nil
}
