#!/usr/bin/env python3
"""Credential-free guest relay for an Engrams Cloud SQL tunnel."""

import argparse
import select
import socket
import threading
import urllib.parse

METADATA = ("169.254.169.254", 80)
MAX_HEADER = 16 * 1024


def relay(client: socket.socket, instance: str) -> None:
    try:
        upstream = socket.create_connection(METADATA, timeout=10)
        target = urllib.parse.quote(instance, safe=":-")
        upstream.sendall(
            f"CONNECT /_engrams/v1/cloud-sql/{target} HTTP/1.1\r\n"
            "Host: metadata.google.internal\r\n"
            "Metadata-Flavor: Google\r\n\r\n".encode()
        )
        response = b""
        while b"\r\n\r\n" not in response and len(response) < MAX_HEADER:
            chunk = upstream.recv(1024)
            if not chunk:
                raise RuntimeError("host tunnel closed during setup")
            response += chunk
        status = response.split(b"\r\n", 1)[0]
        if b" 200 " not in status:
            raise RuntimeError(status.decode("ascii", "replace"))
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
        print(f"Cloud SQL tunnel failed: {error}", flush=True)
    finally:
        client.close()
        if "upstream" in locals():
            upstream.close()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Expose an authorized Engrams Cloud SQL instance on loopback."
    )
    parser.add_argument("--instance", required=True, help="project:region:instance")
    parser.add_argument("--address", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=5432)
    args = parser.parse_args()
    if args.address not in ("127.0.0.1", "::1", "localhost"):
        parser.error("--address must be loopback")

    with socket.create_server((args.address, args.port), reuse_port=False) as listener:
        print(
            f"Cloud SQL {args.instance} is available at {args.address}:{args.port}",
            flush=True,
        )
        while True:
            client, _ = listener.accept()
            threading.Thread(target=relay, args=(client, args.instance), daemon=True).start()


if __name__ == "__main__":
    main()
