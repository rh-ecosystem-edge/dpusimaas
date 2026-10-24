# The host BMC of a dpusimaas lease: IPMI lanplus on port 623 of the lease's address (it runs in
# the lease's network namespace). Power requests go to dpusimaas over its unix socket, which applies
# them to the DPU VM. Configuration comes from the environment: LEASE, SOCKET, IPMI_USER,
# IPMI_PASSWORD.
import http.client
import json
import os
import socket

from pyghmi.ipmi.bmc import Bmc

LEASE, SOCK = os.environ["LEASE"], os.environ["SOCKET"]


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=600)
        self.unix_path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.unix_path)


def power(action):
    conn = UnixConnection(SOCK)
    conn.request("POST", "/internal/leases/%s/power/%s" % (LEASE, action))
    resp = conn.getresponse()
    body = json.loads(resp.read() or b"{}")
    if resp.status != 200:
        raise RuntimeError(body.get("error", "HTTP %d" % resp.status))
    return body.get("result")


class SimHostBmc(Bmc):
    def get_power_state(self):
        return power("status")

    def power_on(self):
        power("on")

    def power_off(self):
        power("off")

    def power_shutdown(self):
        power("off")

    def power_reset(self):
        power("reset")

    def power_cycle(self):
        power("cycle")

    def cold_reset(self):
        pass  # the BMC itself restarting: nothing to do


SimHostBmc({os.environ["IPMI_USER"]: os.environ["IPMI_PASSWORD"]}, port=623, address="::").listen()
