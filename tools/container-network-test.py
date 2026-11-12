"""Bootstrap networking tests; run with python3 on Linux (no Podman or network privileges needed).

--real-network additionally checks real routes and a bind-mounted resolv.conf in an isolated network/mount
namespace. Run this variant as root; it never touches the host's interfaces, routes or resolver.
--real-tproxy checks real transparent TCP handshakes and mark/source-validation conflicts in throwaway
network namespaces. Requires root, nftables and kernel TPROXY support; no Internet access is needed.
"""

import ipaddress
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
BASE = REPO / "packages/meduza-np-base-container"
NETWORK = BASE / "rootfs/usr/lib/meduza-np/container/network.sh"
RUN = BASE / "deploy/podman/run.sh"


class BootstrapNetwork(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.resolv = self.root / "resolv.conf"
        self.original = "search lan.example\noptions ndots:1\nnameserver 192.168.50.2\n"
        self.resolv.write_text(self.original)
        self.route_log = self.root / "routes"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("MEDUZA_NP_", "DEFAULT_GW", "DNS_"))}
        self.env.update(MEDUZA_NP_NETWORK_MODE="macvlan", ROUTE_LOG=str(self.route_log))
        self.bin = self.root / "bin"
        self.bin.mkdir()
        real_ip = shutil.which("ip") or "/usr/sbin/ip"
        # Only route writes are replaced; syntax validation goes through the real iproute2 parser.
        self.executable("ip", f"""#!/bin/sh
case "$*" in
  '-4 route replace default via '*) printf '%s\\n' "$*" >>"$ROUTE_LOG"; exit "${{ROUTE_FAILURE:-0}}" ;;
esac
exec {shlex.quote(real_ip)} "$@"
""")
        self.env["PATH"] = str(self.bin) + os.pathsep + self.env["PATH"]

    def executable(self, name, content):
        path = self.bin / name
        path.write_text(content)
        path.chmod(0o755)

    def shell(self, body, **env):
        script = f"""set -eu
log() {{ printf '%s: %s\\n' "$1" "$2" >&2; }}
. {shlex.quote(str(NETWORK))}
NETWORK_RESOLV={shlex.quote(str(self.resolv))}
trap stop_dns_monitor EXIT
{body}
"""
        return subprocess.run(["/bin/sh", "-c", script], env=self.env | env, text=True, capture_output=True, timeout=12)

    def test_unset_keeps_runtime_network(self):
        result = self.shell("start_network")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.resolv.read_text(), self.original)
        self.assertFalse(self.route_log.exists())

    def test_canonical_and_legacy_gateway_dns(self):
        for prefix in ("MEDUZA_NP_", ""):
            with self.subTest(prefix=prefix):
                result = self.shell("start_network; stop_dns_monitor", **{
                    prefix + "DEFAULT_GW": "192.168.50.1", prefix + "DEFAULT_GW_DEV": "eth0",
                    prefix + "DNS_NAMESERVERS": "223.5.5.5,119.29.29.29",
                })
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("-4 route replace default via 192.168.50.1 dev eth0", self.route_log.read_text())
                self.assertEqual(self.resolv.read_text(), "search lan.example\noptions ndots:1\nnameserver 223.5.5.5\nnameserver 119.29.29.29\n")

    def test_canonical_values_override_aliases(self):
        result = self.shell("start_network; stop_dns_monitor", MEDUZA_NP_DEFAULT_GW="192.168.50.1",
                            DEFAULT_GW="192.168.50.2", MEDUZA_NP_DNS_NAMESERVERS="223.5.5.5", DNS_NAMESERVERS="192.168.50.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("192.168.50.2", self.route_log.read_text() + self.resolv.read_text())

    def test_explicit_empty_disables_aliases(self):
        result = self.shell("start_network", MEDUZA_NP_DEFAULT_GW="", DEFAULT_GW="192.168.50.1",
                            MEDUZA_NP_DNS_NAMESERVERS="", DNS_NAMESERVERS="223.5.5.5")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.route_log.exists())
        self.assertEqual(self.resolv.read_text(), self.original)

    def test_ipv6_dns_and_singular_alias(self):
        result = self.shell("start_network; stop_dns_monitor", DNS_NAMESERVER="223.5.5.5,\t2606:4700:4700::1111\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nameserver 2606:4700:4700::1111\n", self.resolv.read_text())

    def test_invalid_dns_stops_before_gateway_change(self):
        for dns in ("999.1.1.1", "127.1", "1", "01.1.1.1", "dns.example.org", "::xyz", "1::2::3", "1.1.1.1;touch /tmp/bad", " , \t"):
            with self.subTest(dns=dns):
                result = self.shell("start_network", MEDUZA_NP_DEFAULT_GW="192.168.50.1", MEDUZA_NP_DNS_NAMESERVERS=dns)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertFalse(self.route_log.exists())
                self.assertEqual(self.resolv.read_text(), self.original)

    def test_invalid_gateway_interface_interval(self):
        for env in ({"DEFAULT_GW": "999.1.1.1"}, {"DEFAULT_GW": "router.example"}, {"DEFAULT_GW": "127.1"},
                    {"DEFAULT_GW_DEV": "eth0"}, {"DEFAULT_GW": "192.168.50.1", "DEFAULT_GW_DEV": "eth0;true"},
                    {"DNS_NAMESERVERS": "223.5.5.5", "DNS_MONITOR_INTERVAL": "0"},
                    {"DNS_NAMESERVERS": "223.5.5.5", "DNS_MONITOR_INTERVAL": "-1"},
                    {"DEFAULT_GW": "192.168.50.1", "DNS_MONITOR_INTERVAL": "0"},
                    {"DEFAULT_GW": "192.168.50.1", "MEDUZA_NP_NETWORK_MONITOR_INTERVAL": ""}):
            with self.subTest(env=env):
                self.assertNotEqual(self.shell("start_network", **env).returncode, 0)
                self.assertFalse(self.route_log.exists())

    def test_network_interval_takes_precedence_over_legacy_dns_interval(self):
        result = self.shell("start_network; stop_dns_monitor", DEFAULT_GW="192.168.50.1",
                            MEDUZA_NP_NETWORK_MONITOR_INTERVAL="1", DNS_MONITOR_INTERVAL="0",
                            MEDUZA_NP_DNS_MONITOR_INTERVAL="0")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_gateway_cannot_change_host_or_bridge(self):
        for mode in ("host", "bridge", ""):
            with self.subTest(mode=mode):
                result = self.shell("start_network", MEDUZA_NP_NETWORK_MODE=mode, DEFAULT_GW="192.168.50.1")
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.route_log.exists())

    def test_gateway_only_monitor_restores_dhcp_route_and_stops_cleanly(self):
        state = self.root / "default-route"
        state.write_text("default via 192.168.50.2 dev eth0\n")
        self.executable("ip", '''#!/bin/sh
case "$*" in
  '-4 route show default') cat "$ROUTE_STATE" ;;
  '-4 route replace default via '*)
    printf '%s\\n' "$*" >>"$ROUTE_LOG"
    printf 'default via %s dev eth0\\n' "$6" >"$ROUTE_STATE" ;;
  *) exit 0 ;;
esac
''')
        result = self.shell('''
start_network
apply_gateway
test "$(wc -l < "$ROUTE_LOG")" -eq 1
printf 'default via 192.168.50.2 dev eth0 proto dhcp\\n' > "$ROUTE_STATE"
sleep 2
grep -q 'via 192.168.50.1' "$ROUTE_STATE"
test "$(wc -l < "$ROUTE_LOG")" -eq 2
stop_dns_monitor
printf 'default via 192.168.50.2 dev eth0\\n' > "$ROUTE_STATE"
sleep 2
test "$(wc -l < "$ROUTE_LOG")" -eq 2
''', DEFAULT_GW="192.168.50.1", DNS_MONITOR_INTERVAL="1", ROUTE_STATE=str(state))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.resolv.read_text(), self.original, "gateway-only override leaves DHCP DNS alone")

    def test_route_failure_stops_bootstrap(self):
        result = self.shell("start_network; echo role-started", DEFAULT_GW="192.168.50.1", ROUTE_FAILURE="2", DNS_NAMESERVERS="223.5.5.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("role-started", result.stdout)
        self.assertEqual(self.resolv.read_text(), self.original)

    def test_resolver_write_failure_stops_bootstrap(self):
        result = self.shell('NETWORK_RESOLV=' + shlex.quote(str(self.root)) + '; start_network; echo role-started',
                            DNS_NAMESERVERS="223.5.5.5")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("role-started", result.stdout)
        self.assertRegex(result.stderr, "cannot (read|write)")

    def test_dns_monitor_repairs_overwrite_and_stops(self):
        inode = self.resolv.stat().st_ino
        result = self.shell('start_network; printf "nameserver 192.168.50.2\\n" >"$NETWORK_RESOLV"; sleep 2; '
                            'stop_dns_monitor; test -z "$dns_monitor_pid"', DNS_NAMESERVERS="223.5.5.5", DNS_MONITOR_INTERVAL="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.resolv.read_text(), "nameserver 223.5.5.5\n")
        self.assertEqual(self.resolv.stat().st_ino, inode, "must write through the runtime's bind mount")

    def test_matching_dns_does_not_rewrite(self):
        self.resolv.write_text("nameserver 223.5.5.5\n")
        before = self.resolv.stat().st_mtime_ns
        result = self.shell("start_network; apply_dns; stop_dns_monitor", DNS_NAMESERVERS="223.5.5.5")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.resolv.stat().st_mtime_ns, before)

    def test_roles_prepare_network_before_bootstrap(self):
        # Run the real role functions with throwaway executables. The role process must see working bootstrap
        # settings even on a blank store (updater) and before dependency preparation (planner).
        entry = NETWORK.with_name("entrypoint.sh").read_text().split('role=${1:-updater}')[0]
        (self.root / "network.sh").write_text(NETWORK.read_text())
        role_binary = self.root / "role"
        role_binary.write_text(f'#!/bin/sh\nset -eu\ntest -s "$ROUTE_LOG"\ngrep -q "nameserver 223.5.5.5" {shlex.quote(str(self.resolv))}\n')
        role_binary.chmod(0o755)
        for role in ("updater", "planner"):
            with self.subTest(role=role):
                store = self.root / role / "store"
                (store / "current/usr/bin").mkdir(parents=True)
                if role == "planner":
                    shutil.copy(role_binary, store / "current/usr/bin/meduza-np-planner")
                script = self.root / "entrypoint.sh"
                script.write_text(entry + f'''
NETWORK_RESOLV={shlex.quote(str(self.resolv))}
IMAGE_UPDATER={shlex.quote(str(role_binary))}
prepare_deps() {{ "$IMAGE_UPDATER"; }}
dbus_loop() {{ :; }}
wait_for_bus() {{ :; }}
dashboard_loop() {{ :; }}
run_{role}
''')
                result = subprocess.run(["/bin/sh", str(script)], text=True, capture_output=True, timeout=12,
                                        env=self.env | {"MEDUZA_NP_DATA_DIR": str(self.root / role),
                                                        "MEDUZA_NP_RUN_DIR": str(self.root / "run"),
                                                        "DEFAULT_GW": "192.168.50.1", "DNS_NAMESERVERS": "223.5.5.5"})
                self.assertEqual(result.returncode, 0, result.stderr)


class Deployment(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.log = self.root / "podman.jsonl"
        self.env_file = self.root / "node.env"
        self.env_file.write_text("MEDUZA_NP_DEFAULT_GW=192.168.50.1\n")
        self.env = dict(os.environ, MEDUZA_NP_DEPLOY_STATE=str(self.root / "state"), PODMAN_LOG=str(self.log),
                        PATH=str(self.root) + os.pathsep + os.environ["PATH"])
        podman = self.root / "podman"
        podman.write_text('''#!/usr/bin/python3
import json, os, sys
with open(os.environ["PODMAN_LOG"], "a") as f: f.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[1:3] == ["network", "inspect"]:
    if "IPAMOptions" in sys.argv[4]: print(os.environ.get("UPDATER_IPAM", "host-local"))
    else: print(os.environ.get("NETWORK_DRIVER", "macvlan"))
''')
        podman.chmod(0o755)
        for name, content in (("id", '#!/bin/sh\necho "${TEST_UID:-0}"\n'), ("systemctl", "#!/bin/sh\nexit 1\n")):
            path = self.root / name
            path.write_text(content)
            path.chmod(0o755)

    def run_deploy(self, *args, **env):
        return subprocess.run(["/bin/sh", str(RUN), "--env-file", str(self.env_file), *args],
                              env=self.env | env, text=True, capture_output=True, timeout=8)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_both_roles_use_macvlan_and_replay(self):
        result = self.run_deploy("--network", "macvlan", "--macvlan-network", "lan", "--ip", "192.168.50.2", "--updater-ip", "192.168.50.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        runs = [a for a in self.calls() if a[0] == "run"]
        self.assertEqual(len(runs), 2)
        for args, ip in zip(runs, ("192.168.50.3", "192.168.50.2")):
            self.assertEqual(args[args.index("--network") + 1], f"lan:ip={ip}")
            self.assertIn("MEDUZA_NP_NETWORK_MODE=macvlan", args)
            self.assertIn("NET_ADMIN", args[args.index("--cap-add") + 1].split(","))
            self.assertNotIn("--publish", args)
        replay = subprocess.run(["/bin/sh", str(RUN), "--again"], env=self.env, text=True, capture_output=True, timeout=8)
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertEqual([a for a in self.calls() if a[0] == "run"][2:], runs)
        self.assertFalse(any(a[:2] == ["network", "create"] for a in self.calls()))

    def test_invalid_macvlan_options_preserve_existing_containers(self):
        for options, env in (
            (["--ip", "192.168.50.2"], {}),
            (["--ip", "192.168.50.2", "--updater-ip", "192.168.50.2"], {}),
            (["--ip", "192.168.50.2", "--updater-ip", "192.168.50.3", "--publish", "53/udp"], {}),
            (["--ip", "192.168.50.2", "--updater-ip", "192.168.50.3"], {"TEST_UID": "1000"}),
            (["--ip", "192.168.50.2", "--updater-ip", "192.168.50.3"], {"NETWORK_DRIVER": "bridge"}),
            (["--ip", "192.168.50.2:ip=1.1.1.1", "--updater-ip", "192.168.50.3"], {}),
            (["--ip", "192.168.50.2", "--updater-ip", "dhcp"], {}),
            (["--ip", "dhcp", "--updater-ip", "192.168.50.3"], {}),
            (["--ip", "192.168.50.2", "--updater-ip", "dhcp", "--updater-macvlan-network", "lan:ip=1.1.1.1"], {}),
        ):
            with self.subTest(options=options, env=env):
                result = self.run_deploy("--network", "macvlan", *options, **env)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(a[0] in ("rm", "run", "volume") for a in self.calls()))

    def test_updater_dhcp_keeps_planner_static_and_replays(self):
        result = self.run_deploy("--network", "macvlan", "--macvlan-network", "lan-static", "--ip", "192.168.50.2",
                                 "--updater-macvlan-network", "lan-dhcp", "--updater-ip", "dhcp", UPDATER_IPAM="dhcp")
        self.assertEqual(result.returncode, 0, result.stderr)
        runs = [a for a in self.calls() if a[0] == "run"]
        self.assertEqual(runs[0][runs[0].index("--network") + 1], "lan-dhcp")
        self.assertEqual(runs[1][runs[1].index("--network") + 1], "lan-static:ip=192.168.50.2")
        self.assertIn("NET_ADMIN", runs[0][runs[0].index("--cap-add") + 1].split(","))
        replay = subprocess.run(["/bin/sh", str(RUN), "--again"], env=self.env | {"UPDATER_IPAM":"dhcp"},
                                text=True, capture_output=True, timeout=8)
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertEqual([a for a in self.calls() if a[0] == "run"][2:], runs)

    def test_bridge_and_host_keep_updater_unprivileged(self):
        for mode in ("bridge", "host"):
            with self.subTest(mode=mode):
                result = self.run_deploy("--network", mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                updater = [a for a in self.calls() if a[0] == "run"][-2]
                self.assertEqual(updater[updater.index("--network") + 1], "meduza-np")
                self.assertNotIn("NET_ADMIN", updater[updater.index("--cap-add") + 1].split(","))

    def test_planner_source_validation_does_not_use_tproxy_mark(self):
        for mode in ("bridge", "macvlan", "host"):
            with self.subTest(mode=mode):
                extra = ("--macvlan-network", "lan", "--ip", "192.168.50.2", "--updater-ip", "192.168.50.3") if mode == "macvlan" else ()
                result = self.run_deploy("--network", mode, *extra)
                self.assertEqual(result.returncode, 0, result.stderr)
                planner = [a for a in self.calls() if a[0] == "run"][-1]
                sysctls = [planner[i + 1] for i, arg in enumerate(planner) if arg == "--sysctl"]
                if mode == "host":
                    self.assertEqual(sysctls, [], "host policy remains administrator-controlled")
                else:
                    self.assertIn("net.ipv4.conf.all.src_valid_mark=0", sysctls)
                    self.assertIn("net.ipv4.conf.default.src_valid_mark=0", sysctls)
                self.assertNotIn("net.ipv4.conf.all.src_valid_mark=1", sysctls)
        units = RUN.parent
        for relative in ("meduza-np-planner.container", "macvlan/meduza-np-planner.container"):
            lines = (units / relative).read_text(encoding="utf-8").splitlines()
            self.assertIn("Sysctl=net.ipv4.conf.all.src_valid_mark=0", lines)
            self.assertIn("Sysctl=net.ipv4.conf.default.src_valid_mark=0", lines)
            self.assertNotIn("Sysctl=net.ipv4.conf.all.src_valid_mark=1", lines)


def real_network():
    # Called ONLY after unshare; all route/interface/mount changes are private to this process tree.
    subprocess.run(["mount", "--make-rprivate", "/"], check=True)
    subprocess.run(["ip", "link", "add", "lan-test", "type", "dummy"], check=True)
    subprocess.run(["ip", "link", "set", "lan-test", "up"], check=True)
    subprocess.run(["ip", "link", "add", "link", "lan-test", "name", "mv-test", "type", "macvlan", "mode", "bridge"], check=True)
    subprocess.run(["ip", "addr", "add", "192.168.50.2/24", "dev", "mv-test"], check=True)
    subprocess.run(["ip", "link", "set", "mv-test", "up"], check=True)
    # A runtime-provided gateway that must be superseded, including a route with a different metric.
    subprocess.run(["ip", "route", "add", "default", "via", "192.168.50.254", "dev", "mv-test", "metric", "100"], check=True)
    with tempfile.TemporaryDirectory() as tmp:
        resolv = Path(tmp) / "resolv.conf"
        resolv.write_text("nameserver 192.168.50.2\n")
        subprocess.run(["mount", "--bind", str(resolv), "/etc/resolv.conf"], check=True)
        env = dict(os.environ, MEDUZA_NP_NETWORK_MODE="macvlan", MEDUZA_NP_DEFAULT_GW="192.168.50.1",
                   MEDUZA_NP_DEFAULT_GW_DEV="mv-test", MEDUZA_NP_DNS_NAMESERVERS="223.5.5.5,119.29.29.29",
                   MEDUZA_NP_DNS_MONITOR_INTERVAL="1")
        subprocess.run(["/bin/sh", "-c", f'set -eu; log() {{ echo "$*"; }}; . {shlex.quote(str(NETWORK))}; '
                        'trap stop_dns_monitor EXIT; start_network; '
                        # Simulate renewal: DHCP changes the IP, supplies the side-router default and DNS again.
                        'ip addr del 192.168.50.2/24 dev mv-test; ip addr add 192.168.50.3/24 dev mv-test; '
                        'ip route replace default via 192.168.50.2 dev mv-test; '
                        'printf "nameserver 192.168.50.2\\n" >/etc/resolv.conf; '
                        'sleep 2; stop_dns_monitor'], env=env, check=True)
        route = json.loads(subprocess.check_output(["ip", "-j", "route", "get", "1.1.1.1"], text=True))[0]
        assert ipaddress.ip_address(route["gateway"]) == ipaddress.ip_address("192.168.50.1"), route
        assert route["dev"] == "mv-test", route
        assert route["prefsrc"] == "192.168.50.3", route
        assert resolv.read_text() == "nameserver 223.5.5.5\nnameserver 119.29.29.29\n"
        subprocess.run(["umount", "/etc/resolv.conf"], check=True)
    print("PASS: real macvlan route/DNS recover after simulated DHCP renewal; leased IP change is preserved")


def real_tproxy_network():
    """Actual TCP interception, including reverse lookup of the one-way TPROXY mark (D91)."""
    import time

    if os.stat("/proc/self/ns/net").st_ino == os.stat("/proc/1/ns/net").st_ino:
        raise RuntimeError("refusing the initial network namespace; use --real-tproxy")

    def run(*args, **kwargs):
        result = subprocess.run(args, text=True, capture_output=True, timeout=8, **kwargs)
        if result.returncode:
            raise RuntimeError(f"{args}: {result.stderr}")
        return result.stdout

    def sysctl(interface, key, value):
        Path(f"/proc/sys/net/ipv4/conf/{interface}/{key}").write_text(str(value))

    child = subprocess.Popen(["unshare", "--net", "sleep", "60"])
    listener = None
    try:
        for _ in range(100):
            if child.poll() is not None:
                raise RuntimeError("client namespace failed to start")
            if os.stat(f"/proc/{child.pid}/ns/net").st_ino != os.stat("/proc/self/ns/net").st_ino:
                break
            time.sleep(0.01)
        else:
            raise RuntimeError("client namespace did not become ready")
        run("ip", "link", "set", "lo", "up")
        run("ip", "link", "add", "router-lan", "type", "veth", "peer", "name", "client-lan")
        run("ip", "link", "set", "client-lan", "netns", str(child.pid))
        run("ip", "address", "add", "192.0.2.1/24", "dev", "router-lan")
        run("ip", "link", "set", "router-lan", "up")
        client = ("nsenter", "-t", str(child.pid), "--net")
        run(*client, "ip", "link", "set", "lo", "up")
        run(*client, "ip", "address", "add", "192.0.2.2/24", "dev", "client-lan")
        run(*client, "ip", "link", "set", "client-lan", "up")
        run(*client, "ip", "route", "add", "default", "via", "192.0.2.1")
        run("ip", "rule", "add", "priority", "9626", "fwmark", "0x4d4e", "table", "4626")
        run("ip", "route", "add", "local", "0.0.0.0/0", "dev", "lo", "table", "4626")
        Path("/proc/sys/net/ipv4/ip_forward").write_text("1")
        for interface in ("all", "default", "router-lan", "lo"):
            sysctl(interface, "rp_filter", 0 if interface == "all" else 2)
            sysctl(interface, "src_valid_mark", 1 if interface == "all" else 0)
            sysctl(interface, "accept_local", 0)
            sysctl(interface, "route_localnet", 0)
        run("nft", "-f", "-", input="""table inet meduza_np_probe {
 chain prerouting {
  type filter hook prerouting priority mangle; policy accept;
  ip saddr 192.0.2.2 tcp dport 443 tproxy ip to 127.0.0.1:7894 meta mark set 0x4d4e accept
 }
}
""")
        listener = socket.socket()
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.setsockopt(socket.SOL_IP, 19, 1)  # Linux IP_TRANSPARENT
        listener.bind(("0.0.0.0", 7894))
        listener.listen()
        listener.settimeout(1.2)
        connect = ("import socket; s=socket.socket(); s.settimeout(0.9); "
                   "s.connect(('198.51.100.10',443)); "
                   "assert s.recv(64)==b'meduza-tproxy-ok'; s.close()")

        def attempt(label, expected):
            with subprocess.Popen((*client, sys.executable, "-c", connect), text=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE) as connection:
                accepted = False
                try:
                    stream, _ = listener.accept()
                    with stream:
                        stream.sendall(b"meduza-tproxy-ok")
                    accepted = True
                except socket.timeout:
                    pass
                _, error = connection.communicate(timeout=3)
                success = accepted and connection.returncode == 0
                assert success == expected, f"{label}: accepted={accepted}, {error}"
                print(f"PASS: {label}: {'connected' if success else 'blocked as expected'}", flush=True)

        attempt("old deployment src_valid_mark=1, ingress rp_filter=2", False)
        sysctl("all", "src_valid_mark", 0)
        attempt("fixed src_valid_mark=0, ingress rp_filter=2", True)
        sysctl("all", "src_valid_mark", 1)
        for interface in ("all", "default", "router-lan", "lo"):
            sysctl(interface, "rp_filter", 0)
        attempt("disabling rp_filter alone does not fix marked source lookup", False)
        sysctl("all", "src_valid_mark", 0)
        sysctl("router-lan", "rp_filter", 2)
        sysctl("router-lan", "src_valid_mark", 1)
        attempt("per-interface src_valid_mark=1 still overrides all=0", False)
        sysctl("router-lan", "src_valid_mark", 0)
        attempt("both all and ingress src_valid_mark=0, rp_filter retained", True)
    finally:
        if listener is not None:
            listener.close()
        child.terminate()
        child.wait(timeout=3)


if __name__ == "__main__":
    if "--real-tproxy" in sys.argv:
        subprocess.run(["unshare", "--net", sys.executable, str(Path(__file__).resolve()), "--inside-tproxy"], check=True)
    elif "--inside-tproxy" in sys.argv:
        real_tproxy_network()
    elif "--real-network" in sys.argv:
        subprocess.run(["unshare", "--net", "--mount", sys.executable, str(Path(__file__).resolve()), "--inside-netns"], check=True)
    elif "--inside-netns" in sys.argv:
        real_network()
    else:
        unittest.main(verbosity=2)
