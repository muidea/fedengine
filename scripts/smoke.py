#!/usr/bin/env python3
"""Start the real binary on an ephemeral port, negotiate PG SSLRequest, then drain on SIGTERM."""
import argparse
import os
import json
import urllib.request
from pathlib import Path
import re
import select
import signal
import socket
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/fedengine")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    output = []
    with tempfile.TemporaryDirectory(prefix="fedengine-smoke-") as folder:
        configuration = Path(folder) / "application.toml"
        text = Path("config/application.toml").read_text()
        text = text.replace('listen = "127.0.0.1:15432"', 'listen = "127.0.0.1:0"')
        text = text.replace('listen = "127.0.0.1:15433"', 'listen = "127.0.0.1:0"')
        text = text.replace('database_path = "./data/fedengine.duckdb"', f'database_path = "{folder}/management.duckdb"')
        text = text.replace('temp_directory = "./data/tmp"', f'temp_directory = "{folder}/tmp"')
        configuration.write_text(text)
        environment = dict(os.environ, FEDENGINE_PASSWORD="disposable-smoke-password", RUST_LOG="fedengine=info", NO_COLOR="1")
        # Match cargo run's library search path when exercising a download-lib build.
        libraries = list((Path(binary).parent.parent / "duckdb-download").glob("*/v2.0.0/libduckdb.so"))
        if libraries:
            environment["LD_LIBRARY_PATH"] = str(libraries[0].parent) + os.pathsep + environment.get("LD_LIBRARY_PATH", "")
        process = subprocess.Popen([binary, "--config", str(configuration)], env=environment,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 20
            buffered = b""
            address = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError("binary exited before Ready")
                if select.select([process.stdout], [], [], 0.2)[0]:
                    chunk = os.read(process.stdout.fileno(), 65536)
                    output.append(chunk)
                    buffered += chunk
                    if b"FedEngine Ready" in buffered:
                        found = re.search(rb"127\.0\.0\.1:(\d+)", buffered)
                        if found:
                            address = ("127.0.0.1", int(found[1]))
                            break
            if address is None:
                raise RuntimeError("Ready timeout")
            with socket.create_connection(address, timeout=3) as client:
                client.sendall(bytes([0, 0, 0, 8, 4, 210, 22, 47]))
                assert client.recv(1) == b"N", "unexpected SSLRequest response"
            management = re.search(rb"management_address=Some\(127\.0\.0\.1:(\d+)\)", buffered)
            if management is None:
                raise RuntimeError("management Ready address missing")
            with urllib.request.urlopen(f"http://127.0.0.1:{int(management[1])}/api/v1/sources", timeout=3) as response:
                assert response.status == 200
                assert json.load(response) == []
            process.send_signal(signal.SIGTERM)
            remaining, _ = process.communicate(timeout=15)
            output.append(remaining)
            assert process.returncode == 0, "binary shutdown failed"
            assert b"GracefullyStopped" in b"".join(output), "checked drain receipt missing"
            print(b"".join(output).decode(errors="replace"), end="")
            print("binary startup, PG negotiation, SIGTERM and checked drain: Passed")
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.communicate(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate()


if __name__ == "__main__":
    main()
