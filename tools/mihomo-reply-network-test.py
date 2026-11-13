"""Real TCP/UDP reply routing in throwaway Linux namespaces, using native generated Mihomo tables.

cargo run --locked -q -p meduza-np-mihomo --example reply-routing-fixture > /tmp/reply-fixtures.json
sudo python3 tools/mihomo-reply-network-test.py /tmp/reply-fixtures.json
Requires iproute2, nftables, nsenter, unshare and kernel TPROXY support. No host routes/rules are changed.
"""
import json
import os
from pathlib import Path
import subprocess
import sys


def check_network(fixtures):
    if os.stat('/proc/self/ns/net').st_ino == os.stat('/proc/1/ns/net').st_ino:
        raise RuntimeError('refusing initial network namespace')

    def run(*args, **kwargs):
        return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout

    children = []
    try:
        # This namespace is the side router; every peer gets another isolated namespace.
        run('ip', 'link', 'set', 'lo', 'up')
        run('ip', 'link', 'add', 'lan', 'type', 'bridge')
        run('ip', 'addr', 'add', '192.0.2.2/24', 'dev', 'lan')
        run('ip', 'link', 'set', 'lan', 'up')

        def host(name, router_if, addresses, bridge=False):
            process = subprocess.Popen(['unshare', '--net', sys.executable, '-u', '-c',
                                        'import time; print("ready",flush=True); time.sleep(300)'],
                                       stdout=subprocess.PIPE, text=True)
            children.append(process)
            assert process.stdout.readline().strip() == 'ready'
            enter = ('nsenter', '-t', str(process.pid), '--net')
            run('ip', 'link', 'add', router_if, 'type', 'veth', 'peer', 'name', name)
            run('ip', 'link', 'set', name, 'netns', str(process.pid))
            if bridge:
                run('ip', 'link', 'set', router_if, 'master', 'lan')
            run('ip', 'link', 'set', router_if, 'up')
            run(*enter, 'ip', 'link', 'set', 'lo', 'up')
            run(*enter, 'ip', 'link', 'set', name, 'up')
            for address in addresses:
                run(*enter, 'ip', 'addr', 'add', address, 'dev', name)
            return enter

        server = host('server', 'lan-server', ['192.0.2.4/24'], True)
        wan = host('gateway', 'lan-gateway', ['192.0.2.1/24', '198.51.100.10/32'], True)
        vpn = host('client', 'tun-access', ['10.88.0.2/24', '203.0.113.2/24'])
        for address in ('10.88.0.1/24', '203.0.113.1/24'):
            run('ip', 'addr', 'add', address, 'dev', 'tun-access')
        run('ip', 'route', 'add', 'default', 'via', '192.0.2.1')
        run(*server, 'ip', 'route', 'add', 'default', 'via', '192.0.2.2')
        run(*vpn, 'ip', 'route', 'add', 'default', 'via', '10.88.0.1')
        run(*wan, 'ip', 'route', 'add', '192.0.2.4/32', 'via', '192.0.2.2')
        Path('/proc/sys/net/ipv4/ip_forward').write_text('1')
        for node in [(), server, wan, vpn]:
            run(*node, 'sh', '-c', 'for f in /proc/sys/net/ipv4/conf/*/rp_filter; do echo 0 > "$f"; done')
        run('ip', 'rule', 'add', 'priority', '9626', 'fwmark', '0x4d4e', 'table', '4626')
        run('ip', 'route', 'add', 'local', '0.0.0.0/0', 'dev', 'lo', 'table', '4626')
        run('ip', 'rule', 'add', 'priority', '9627', 'fwmark', '0x4d50', 'table', '4627')
        run('ip', 'route', 'add', 'default', 'via', '192.0.2.1', 'table', '4627')

        listener = subprocess.Popen([*server, sys.executable, '-u', '-c', '''
import socket,threading,time
def tcp():
 s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind(('0.0.0.0',5900));s.listen()
 while True:
  c,_=s.accept();c.sendall(b'RFB 003.008\\n');c.close()
def udp():
 s=socket.socket(type=socket.SOCK_DGRAM);s.bind(('0.0.0.0',5900))
 while True:
  data,peer=s.recvfrom(100);s.sendto(data,peer)
threading.Thread(target=tcp,daemon=True).start();threading.Thread(target=udp,daemon=True).start()
time.sleep(.1);print('ready',flush=True);time.sleep(300)
'''], stdout=subprocess.PIPE, text=True)
        children.append(listener)
        assert listener.stdout.readline().strip() == 'ready'

        def probe(node, address, udp=False):
            code = ("import socket; s=socket.socket(type=" + ('socket.SOCK_DGRAM' if udp else 'socket.SOCK_STREAM') +
                    "); s.settimeout(.6); s.bind((" + repr(address) + ",0)); s.connect(('192.0.2.4',5900)); " +
                    ("s.send(b'reply-ok'); assert s.recv(100)==b'reply-ok'" if udp else "assert s.recv(100)==b'RFB 003.008\\n'"))
            return subprocess.run([*node, sys.executable, '-c', code], capture_output=True).returncode == 0

        for fixture in fixtures:
            fixed = fixture['table']
            # Reproduce the previous behavior with the same generated table, removing only its reply guards.
            before, after = fixed.split('\tchain not_proxied {\n', 1)
            reply, tail = after.split('\t}\n', 1)
            mark = next(line for line in reply.splitlines() if 'meta mark set' in line)
            broken = before + '\tchain not_proxied {\n' + mark + '\n\t}\n' + tail
            for label, table, expected in [('before', broken, False), ('fixed', fixed, True)]:
                subprocess.run(['nft', 'delete', 'table', 'inet', 'meduza_np_mihomo'], capture_output=True)
                run('nft', '-f', '-', input=table)
                for udp in (False, True):
                    for address in ('10.88.0.2', '203.0.113.2'):
                        actual = probe(vpn, address, udp)
                        assert actual == expected, (fixture['conntrack'], label, address, udp, actual)
                    assert probe(wan, '198.51.100.10', udp), 'ordinary WAN replies must still reach the main router'
                print(f"PASS conntrack={fixture['conntrack']} {label}: private/public VPN TCP+UDP={expected}, WAN replies=True", flush=True)
    finally:
        for child in reversed(children):
            child.terminate()
            child.wait(timeout=3)


if __name__ == '__main__':
    path = str(Path(sys.argv[1]).resolve())
    if '--inside' in sys.argv:
        check_network(json.loads(Path(path).read_text()))
    else:
        subprocess.run(['unshare', '--net', sys.executable, str(Path(__file__).resolve()), path, '--inside'], check=True)
