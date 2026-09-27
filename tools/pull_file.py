"""把 vps 上的文件 base64 拉回本地（小文件用）。"""
import base64
import subprocess
import sys
from pathlib import Path

SSH = ["ssh", "-o", "ConnectTimeout=20", "-o", "BatchMode=yes", "vps"]
remote = sys.argv[1]
local = Path(sys.argv[2] if len(sys.argv) > 2 else Path(remote).name)
r = subprocess.run(SSH + [f"base64 -w0 {remote}"], capture_output=True, timeout=180)
if r.returncode:
    print("FAILED:", r.stderr.decode()[:600])
    raise SystemExit(1)
data = base64.b64decode(r.stdout)
local.write_bytes(data)
print(f"{local} <- {remote}  ({len(data)} bytes)")
