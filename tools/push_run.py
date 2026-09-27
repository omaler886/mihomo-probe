"""把本地脚本 base64 推到 vps:/tmp，然后 nohup 后台跑，日志写 /tmp/<name>.log。"""
import base64
import subprocess
import sys
from pathlib import Path

SSH = ["ssh", "-o", "ConnectTimeout=20", "-o", "BatchMode=yes", "vps"]

local = sys.argv[1]
name = Path(local).stem
remote = f"/tmp/{name}.py"
logf = f"/tmp/{name}.log"

raw = Path(local).read_bytes()
b64 = base64.b64encode(raw)
# payload goes in over stdin: the command string stays quote-free and short,
# so neither the remote shell parser nor an argv length limit can mangle it.
r = subprocess.run(SSH + [f"base64 -d > {remote} && wc -c {remote}"],
                   input=b64 + b"\n", capture_output=True, timeout=180)
print(r.stdout.decode().strip())
if r.returncode:
    print("UPLOAD FAILED:", r.stderr.decode()[:600])
    raise SystemExit(1)

run = (f"cd /tmp && rm -f {logf} && nohup python3 -u {remote} > {logf} 2>&1 & "
       f"sleep 2; echo LAUNCHED; head -5 {logf}")
r2 = subprocess.run(SSH + [run], capture_output=True, timeout=180)
print(r2.stdout.decode())
print(r2.stderr.decode()[:600])
