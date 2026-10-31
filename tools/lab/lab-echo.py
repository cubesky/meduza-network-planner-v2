#!/usr/bin/env python3
"""Lab "internet" endpoint: answers every TCP connection and UDP datagram with the peer's address.

    lab-echo.py ADDRESS PORT

The answer tells which machine really opened the flow: the address of a proxy server, a node that went direct, or
the LAN client itself. Each flow is also logged (journal), one line per flow.
"""
import socket
import sys
import threading


def tcp(addr, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port))
    s.listen(64)
    while True:
        c, peer = s.accept()
        print(f"tcp {peer[0]}:{peer[1]}", flush=True)
        try:
            c.sendall(f"{peer[0]}\n".encode())
        finally:
            c.close()


def udp(addr, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((addr, port))
    while True:
        _, peer = s.recvfrom(2048)
        print(f"udp {peer[0]}:{peer[1]}", flush=True)
        s.sendto(f"{peer[0]}\n".encode(), peer)


if __name__ == "__main__":
    a, p = sys.argv[1], int(sys.argv[2])
    threading.Thread(target=udp, args=(a, p), daemon=True).start()
    tcp(a, p)
