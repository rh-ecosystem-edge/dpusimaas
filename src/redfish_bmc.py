# The host BMC's Redfish of a host-trusted dpusimaas lease: sushy-tools on HTTPS 443 of the lease's
# address (it runs in the lease's network namespace), serving only the lease's host VM, on the x86
# hypervisor over libvirt. Virtual media is how the cluster installs the host. Configuration comes
# from the environment: LIBVIRT_URI, HOST_VM, HOST_UUID, BMC_USER, BMC_PASSWORD, STATE_DIR (where
# the password file, TLS certificate and sushy's state go).
import os
import ssl
import subprocess

import bcrypt
from sushy_tools.emulator import main

state = os.environ["STATE_DIR"]
os.makedirs(state, exist_ok=True)

auth_file = os.path.join(state, "redfish.htpasswd")
hashed = bcrypt.hashpw(os.environ["BMC_PASSWORD"].encode(), bcrypt.gensalt()).decode()
with open(auth_file, "w") as f:
    f.write("%s:%s\n" % (os.environ["BMC_USER"], hashed))

cert, key = os.path.join(state, "redfish.crt"), os.path.join(state, "redfish.key")
if not os.path.exists(cert):
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
                    "-subj", "/CN=dpusimaas-bmc", "-keyout", key, "-out", cert],
                   check=True, capture_output=True)

main.app.configure(extra_config={
    "SUSHY_EMULATOR_LIBVIRT_URI": os.environ["LIBVIRT_URI"],
    "SUSHY_EMULATOR_ALLOWED_INSTANCES": [os.environ["HOST_VM"], os.environ["HOST_UUID"]],
    "SUSHY_EMULATOR_AUTH_FILE": auth_file,
    "SUSHY_EMULATOR_STATE_DIR": state,
})
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
main.app.run(host="::", port=443, ssl_context=context)
