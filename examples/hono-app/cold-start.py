#!/usr/bin/env python3
"""Measure HTTP server cold start: wall time from spawning the runtime to its server
answering the first request — the metric ant reports
(https://github.com/theMackabu/ant#cold-start).

Each run spawns `<cmd...>` with a fresh PORT in the environment, then busy-connects to
127.0.0.1:PORT and sends `GET /` until it gets a byte back, timing the whole thing. The
runtime is killed between runs so every sample is a true cold start (fresh process, module
graph re-evaluated, socket freshly bound).

Usage:
    ./cold-start.py [--runs N] [--warmup] -- <runtime> <script> [args...]

Examples:
    ./cold-start.py --runs 20 -- ../../target/release/lumen-cli cold-start-server.js
    ./cold-start.py --runs 20 -- node cold-start-server.js

Only the standard library is used, so it runs anywhere python3 does.
"""
import argparse
import os
import socket
import statistics
import subprocess
import sys
import time


def measure_once(cmd, port, timeout=10.0):
    env = dict(os.environ, PORT=str(port))
    t0 = time.perf_counter()
    proc = subprocess.Popen(
        cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    try:
        deadline = t0 + timeout
        while time.perf_counter() < deadline:
            if proc.poll() is not None:
                raise RuntimeError(f"runtime exited early (code {proc.returncode})")
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2) as s:
                    s.sendall(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
                    if s.recv(1):  # first byte of the response == server is live
                        return (time.perf_counter() - t0) * 1000.0
            except OSError:
                time.sleep(0.001)  # not listening yet
        raise RuntimeError("timed out waiting for first response")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=20)
    ap.add_argument("--port-base", type=int, default=39000)
    ap.add_argument("--warmup", action="store_true", help="discard one untimed run first")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    args = ap.parse_args()

    cmd = args.cmd
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        ap.error("give a command after --, e.g. -- node cold-start-server.js")

    if args.warmup:
        measure_once(cmd, args.port_base - 1)

    samples = []
    for i in range(args.runs):
        ms = measure_once(cmd, args.port_base + i)
        samples.append(ms)
        print(f"  run {i + 1:>2}/{args.runs}: {ms:7.2f} ms", file=sys.stderr)

    samples.sort()
    print(f"\n{' '.join(cmd)}")
    print(f"  runs   : {len(samples)}")
    print(f"  min    : {samples[0]:.2f} ms")
    print(f"  median : {statistics.median(samples):.2f} ms")
    print(f"  mean   : {statistics.mean(samples):.2f} ms")
    print(f"  max    : {samples[-1]:.2f} ms")


if __name__ == "__main__":
    main()
