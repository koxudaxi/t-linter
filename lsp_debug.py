#!/usr/bin/env python3
# lsp_debug_fixed.py

import subprocess
import json
import sys
import os

def send_request(proc, request):
    """Send LSP request and read response"""
    content = json.dumps(request)
    header = f"Content-Length: {len(content)}\r\n\r\n"
    message = header + content

    proc.stdin.write(message.encode())
    proc.stdin.flush()

    # Read header
    headers = {}
    while True:
        line = proc.stdout.readline().decode().strip()
        if not line:
            break
        key, value = line.split(": ", 1)
        headers[key] = value

    # Read content
    if "Content-Length" in headers:
        content_length = int(headers["Content-Length"])
        content = proc.stdout.read(content_length).decode()
        return json.loads(content)
    return None

# Start LSP server
proc = subprocess.Popen(
    ["./target/release/t-linter", "lsp", "--stdio"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    env={**dict(os.environ), "RUST_LOG": "trace"}
)

# Minimal initialize request
init_request = {
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
        "processId": None,
        "capabilities": {}
    }
}

print("Sending initialize request...")
print(f"Request: {json.dumps(init_request, indent=2)}")

response = send_request(proc, init_request)
print(f"Initialize response: {json.dumps(response, indent=2)}")

# Check stderr for errors
stderr_output = proc.stderr.read(1024).decode() if proc.stderr else ""
if stderr_output:
    print(f"Stderr: {stderr_output}")

proc.terminate()