"""把本地改动上传到 vps。

`scp` 传大文件会反复 Connection closed，而把内容塞进 ssh 命令串又会被远端
shell 重新解析（引号一嵌套就 unexpected EOF）。所以统一走 base64 + stdin：
命令串里零引号，payload 永远不经过远端 shell。

用法:
    python tools/deploy_files.py            # 只上传
    python tools/deploy_files.py --rebuild  # 上传后重建应用容器
"""
import base64
import hashlib
import pathlib
import posixpath
import subprocess
import sys

SSH = ["ssh", "-o", "ConnectTimeout=20", "-o", "BatchMode=yes", "vps"]
ROOT = "/srv/mihomo-test"

FILES = [
    "mihomo_test/__main__.py",
    "mihomo_test/config.py",
    "mihomo_test/core.py",
    "mihomo_test/engine.py",
    "mihomo_test/ipmap.py",
    "mihomo_test/server.py",
    "mihomo_test/store.py",
    "mihomo_test/ui.py",
    # 静态前端：代码是 COPY 进镜像的，这几个文件不进镜像面板就没有样式和脚本。
    "mihomo_test/web/index.html",
    "mihomo_test/web/app.css",
    "mihomo_test/web/app.js",
    "mihomo_test/web/theme.js",
    "tests/test_alerts_lanes.py",
    "tests/test_hardening.py",
    "tests/test_ipmap.py",
    "tests/test_logic.py",
]


def push(rel):
    raw = pathlib.Path(rel).read_bytes().replace(b"\r\n", b"\n")
    md5 = hashlib.md5(raw).hexdigest()
    remote = "{}/{}".format(ROOT, rel)
    # `mkdir -p` first: `base64 -d > file` does not create the parent, and
    # `mihomo_test/web/` is new -- without this the first web-asset push fails
    # with "No such file or directory" and only the shell's stderr shows why.
    parent = posixpath.dirname(remote)
    # NOTE: `\\0` in this f-string must stay doubled -- a single `\0` becomes a
    # real NUL in the Python string and subprocess raises ValueError.
    cmd = ("mkdir -p {d} && base64 -d > {f} && md5sum -z {f} | tr '\\0' ' ' && stat -c%s {f}"
           .format(d=parent, f=remote))
    p = subprocess.run(SSH + [cmd], input=base64.b64encode(raw) + b"\n",
                       capture_output=True, timeout=300)
    parts = p.stdout.decode().split()
    if not parts or parts[0] != md5:
        print("  FAIL {}: md5 {} != {}".format(rel, parts[0] if parts else "?", md5))
        print("  stderr:", p.stderr.decode("utf-8", "replace")[:300])
        return False
    if parts[-1] != str(len(raw)):
        print("  FAIL {}: size {} != {}".format(rel, parts[-1], len(raw)))
        return False
    print("  OK   {}  {}B  md5={}".format(rel, len(raw), md5[:12]))
    return True


def main():
    # 不给文件参数就推 FILES 全部；给了就只推这些（改一个文件时省时间）。
    picked = [a for a in sys.argv[1:] if not a.startswith("--")]
    for rel in (picked or FILES):
        if not push(rel):
            print("有文件没传成功，中止")
            return 1
    if "--rebuild" in sys.argv:
        print("rebuild mihomo-test ...")
        p = subprocess.run(
            SSH + ["cd {} && docker compose up -d --build mihomo-test".format(ROOT)],
            capture_output=True, timeout=900)
        print(p.stdout.decode("utf-8", "replace")[-1200:])
        err = p.stderr.decode("utf-8", "replace")
        if err.strip():
            print("stderr:", err[-600:])
    return 0


if __name__ == "__main__":
    sys.exit(main())
