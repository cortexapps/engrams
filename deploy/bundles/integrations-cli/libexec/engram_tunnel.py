#!/usr/bin/env python3
"""Credential-free guest client for session-authorized Engrams tunnels."""

import argparse
import json
import select
import socket
import threading

GATEWAY = ("169.254.169.254", 80)
MAX_HEADER = 16 * 1024


def request(method: str, target: str) -> tuple[socket.socket, bytes]:
    upstream = socket.create_connection(GATEWAY, timeout=10)
    upstream.sendall(
        f"{method} {target} HTTP/1.1\r\n"
        "Host: gateway.engrams.internal\r\n"
        "Engram-Gateway: 1\r\n\r\n".encode()
    )
    response = b""
    while b"\r\n\r\n" not in response and len(response) < MAX_HEADER:
        chunk = upstream.recv(1024)
        if not chunk:
            raise RuntimeError("guest gateway closed during setup")
        response += chunk
    header, body = response.split(b"\r\n\r\n", 1)
    status = header.split(b"\r\n", 1)[0]
    if b" 200 " not in status:
        while len(body) < MAX_HEADER:
            chunk = upstream.recv(min(4096, MAX_HEADER - len(body)))
            if not chunk:
                break
            body += chunk
        upstream.close()
        detail = body.decode("utf-8", "replace").strip()
        message = status.decode("ascii", "replace")
        if detail:
            message = f"{message}: {detail}"
        raise RuntimeError(message)
    return upstream, body


def list_tunnels() -> None:
    upstream, body = request("GET", "/_engrams/v1/tunnels")
    try:
        while True:
            chunk = upstream.recv(4096)
            if not chunk:
                break
            body += chunk
    finally:
        upstream.close()
    tunnels = json.loads(body)
    if not tunnels:
        print("No host tunnels are authorized in this session.")
        return
    for tunnel in tunnels:
        print(f"{tunnel['id']}\t{tunnel['connector']}")


def relay(client: socket.socket, tunnel: str) -> None:
    try:
        upstream, buffered = request("CONNECT", f"/_engrams/v1/tunnels/{tunnel}")
        if buffered:
            client.sendall(buffered)
        sockets = [client, upstream]
        while sockets:
            readable, _, _ = select.select(sockets, [], [])
            for source in readable:
                data = source.recv(64 * 1024)
                destination = upstream if source is client else client
                if not data:
                    try:
                        destination.shutdown(socket.SHUT_WR)
                    except OSError:
                        pass
                    sockets.remove(source)
                    continue
                destination.sendall(data)
    except Exception as error:
        print(f"Tunnel failed: {error}", flush=True)
    finally:
        client.close()
        if "upstream" in locals():
            upstream.close()


def open_tunnel(tunnel: str, address: str, port: int) -> None:
    if address not in ("127.0.0.1", "::1", "localhost"):
        raise ValueError("--address must be loopback")
    family = socket.AF_INET6 if address == "::1" else socket.AF_INET
    with socket.create_server((address, port), family=family, reuse_port=False) as listener:
        print(f"Tunnel {tunnel} is available at {address}:{port}", flush=True)
        while True:
            client, _ = listener.accept()
            threading.Thread(target=relay, args=(client, tunnel), daemon=True).start()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="List or expose session-authorized Engrams host tunnels."
    )
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list", help="List tunnel IDs authorized for this session.")
    open_parser = commands.add_parser("open", help="Expose one tunnel on guest loopback.")
    open_parser.add_argument("tunnel", help="Tunnel ID from `engram-tunnel list`.")
    open_parser.add_argument("--address", default="127.0.0.1")
    open_parser.add_argument("--port", type=int, default=5432)
    args = parser.parse_args()
    if args.command == "list":
        list_tunnels()
    else:
        open_tunnel(args.tunnel, args.address, args.port)


if __name__ == "__main__":
    main()
