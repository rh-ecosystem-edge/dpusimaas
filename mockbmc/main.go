// mockbmc is a Zero Trust lease's DPU BMC: upstream DPF's mock BlueField Redfish server
// (test/mock/dpuagent), as is, on 443 of the lease's address (in the lease's network namespace).
// dpusimaas runs one per lease and owns everything else about the lease:
//   - when DPF installs a BFB or resets the DPU's Arm through it, mockbmc tells dpusimaas
//     (POST /internal/leases/{id}/{boot,reboot} on dpusimaas's internal socket), which drives the
//     DPU VM;
//   - when the DPU's power changes otherwise (the host's BMC, a boot dpusimaas drives), dpusimaas
//     tells mockbmc (POST /state/{power-on,power-off,os-starting,apply-activated-bundle} on its
//     control socket), so the mock reports it.
package main

import (
	"bytes"
	"context"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"

	provisioningv1 "github.com/nvidia/doca-platform/api/provisioning/v1alpha1"
	"github.com/nvidia/doca-platform/test/mock/dpuagent/pkg/config"
	"github.com/nvidia/doca-platform/test/mock/dpuagent/pkg/redfish"
)

func main() {
	lease := flag.String("lease", "", "the lease this BMC is for")
	netnsName := flag.String("netns", "", "network namespace to listen in (empty: the current one)")
	listen := flag.String("listen", "0.0.0.0:443", "Redfish address, in the namespace")
	serial := flag.String("serial", "", "BlueField serial number")
	control := flag.String("control", "", "unix socket dpusimaas changes the BMC's state through")
	dpusimaasSocket := flag.String("dpusimaas-socket", "/run/dpusimaas.sock", "dpusimaas's internal socket")
	bfbVersions := flag.String("bfb-versions", "", "firmware versions a BFB without a software inventory installs: atf=,bsp=,uefi=,doca=")
	installed := flag.Bool("installed", false, "DPF already installed an OS: report its BFB versions and the DPU powered on")
	check := flag.Bool("check", false, "only check -bfb-versions, then exit")
	flag.Parse()

	versions, err := parseBFBVersions(*bfbVersions)
	if err != nil {
		fail("-bfb-versions", err)
	}
	if *check {
		return
	}
	if *lease == "" || *serial == "" || *control == "" {
		fail("flags", fmt.Errorf("-lease, -serial and -control are required"))
	}
	if err := startFetchProxy(versions); err != nil {
		fail("starting the fetch proxy", err)
	}

	state := newState(*serial)
	if *installed {
		// The mock keeps no state: a restarted BMC reports the OS DPF had installed again.
		state.SetInstalledBFBVersions(&provisioningv1.BFBVersions{ATF: versions.ATF, BSP: versions.BSP, UEFI: versions.UEFI, DOCA: versions.DOCA})
		state.SetPowerOn()
	}
	srv, err := redfish.NewServer(state, &dpusimaas{lease: *lease, client: unixClient(*dpusimaasSocket)})
	if err != nil {
		fail("redfish server", err)
	}
	if err := inNetns(*netnsName, func() error { _, err := srv.Listen(*listen); return err }); err != nil {
		fail("redfish", err)
	}
	// Listening on the control socket last tells dpusimaas the BMC is up.
	_ = os.Remove(*control)
	controlListener, err := net.Listen("unix", *control)
	if err != nil {
		fail("control socket", err)
	}
	go func() { _ = http.Serve(controlListener, controlHandler(state)) }()

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	slog.Info("mock BMC up", "lease", *lease, "netns", *netnsName, "listen", *listen)
	if err := srv.Serve(ctx); err != nil {
		fail("redfish server", err)
	}
}

func fail(what string, err error) {
	slog.Error(what, "err", err)
	os.Exit(1)
}

func newState(serial string) *redfish.State {
	state := redfish.NewState(redfish.Options{
		Personality:  redfish.ForType(config.DPUTypeBF3),
		SerialNumber: serial,
		PSID:         config.DefaultPSID,
		PF0MAC:       config.PF0MAC(serial),
		Firmware: config.Firmware{
			BMC:  config.BMCMinSupportedVersion,
			ERoT: config.PlaceholderVersion,
			UEFI: config.PlaceholderVersion,
			NIC:  config.PlaceholderVersion,
		},
	})
	// The one manual step on a real DPU BMC before Zero Trust: rshim enabled.
	state.SetBMCRShimEnabled(true)
	return state
}

// controlHandler is dpusimaas telling the mock how the DPU's power changed.
func controlHandler(state *redfish.State) http.Handler {
	changes := map[string]func(){
		"power-on":    state.SetPowerOn,
		"power-off":   state.SetPowerOff,
		"os-starting": state.SetOSStarting,
		// Firmware DPF activated takes effect on a host reboot.
		"apply-activated-bundle": func() { state.ApplyActivatedBundle() },
	}
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) { _, _ = io.WriteString(w, "ok\n") })
	mux.HandleFunc("POST /state/{change}", func(w http.ResponseWriter, r *http.Request) {
		change, ok := changes[r.PathValue("change")]
		if !ok {
			http.Error(w, fmt.Sprintf("unknown state change %q", r.PathValue("change")), http.StatusNotFound)
			return
		}
		change()
		w.WriteHeader(http.StatusNoContent)
	})
	return mux
}

// dpusimaas implements redfish.Supervisor: DPF booting or resetting the DPU through its BMC, which
// dpusimaas applies to the DPU VM. The mock calls these on goroutines of their own.
type dpusimaas struct {
	lease  string
	client *http.Client
}

// Boot is DPF having installed a BFB: dpusimaas boots the DPU VM from its bf.cfg.
func (d *dpusimaas) Boot(ctx context.Context, bfcfg []byte) { d.post(ctx, "boot", bfcfg) }

// Reboot is DPF resetting the DPU's Arm through its BMC.
func (d *dpusimaas) Reboot(ctx context.Context) { d.post(ctx, "reboot", nil) }

func (d *dpusimaas) post(ctx context.Context, event string, body []byte) {
	url := fmt.Sprintf("http://dpusimaas/internal/leases/%s/%s", d.lease, event)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		slog.Error("telling dpusimaas", "event", event, "err", err)
		return
	}
	resp, err := d.client.Do(req)
	if err != nil {
		slog.Error("telling dpusimaas", "event", event, "err", err)
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		out, _ := io.ReadAll(resp.Body)
		slog.Error("dpusimaas failed the "+event, "status", resp.Status, "body", string(out))
	}
}

func unixClient(socket string) *http.Client {
	return &http.Client{Transport: &http.Transport{
		DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
			return (&net.Dialer{}).DialContext(ctx, "unix", socket)
		},
	}}
}
