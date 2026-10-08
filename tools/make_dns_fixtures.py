"""Regenerate the DNS wire-format fixtures under crates/probe-dns/fixtures/.

workstreams/05 (R4) asks for a binary fixture set covering the malformed
shapes the parser has to survive: a truncated packet, a lying rdlength, a
compression-pointer loop, and a bad label. The Rust tests in wire.rs load
these with `include_bytes!`, so the bytes are fixed at build time and a parser
regression shows up as a byte-level diff rather than a re-derived packet.

Run from the repo root:

    python tools/make_dns_fixtures.py

The files are committed; this script only exists so they can be regenerated
(and reviewed) instead of being opaque blobs. It writes the same bytes every
run -- no randomness -- so re-running it is a no-op for git.
"""
from __future__ import annotations

import pathlib
import struct

OUT = pathlib.Path(__file__).resolve().parent.parent / "crates" / "probe-dns" / "fixtures" / "dns-packets"

TYPE_A = 1
TYPE_AAAA = 28
TYPE_CNAME = 5
CLASS_IN = 1


def name(labels: str) -> bytes:
    """Length-prefixed labels plus the zero terminator (`a.example`)."""
    out = bytearray()
    for label in labels.rstrip(".").split("."):
        raw = label.encode()
        out.append(len(raw))
        out += raw
    out.append(0)
    return bytes(out)


def header(*, qid: int = 0x1234, flags: int = 0x8180, qd: int = 1, an: int = 0, ns: int = 0, ar: int = 0) -> bytes:
    return struct.pack("!HHHHHH", qid, flags, qd, an, ns, ar)


def question(labels: str, qtype: int = TYPE_A) -> bytes:
    return name(labels) + struct.pack("!HH", qtype, CLASS_IN)


def answer(owner: bytes, rtype: int, rdata: bytes, ttl: int = 300, rdlength: int | None = None) -> bytes:
    """One answer record. `owner` is raw (a pointer or a label list); `rdlength`
    may be forced to a value that disagrees with `rdata` to fake a bad length."""
    declared = len(rdata) if rdlength is None else rdlength
    return owner + struct.pack("!HHIH", rtype, CLASS_IN, ttl, declared) + rdata


# A compression pointer to offset 12 -- the start of the question name.
PTR_Q = b"\xc0\x0c"

FIXTURES: dict[str, bytes] = {
    # A well-formed answer whose owner name is a pointer into the question.
    "a-answer-compressed.bin": (
        header(an=1) + question("a.example") + answer(PTR_Q, TYPE_A, bytes([1, 2, 3, 4]))
    ),
    # AAAA rdata is 16 bytes; the parser must render it as an address.
    "aaaa-answer.bin": (
        header(an=1)
        + question("v6.example", TYPE_AAAA)
        + answer(PTR_Q, TYPE_AAAA, bytes([0x20, 0x01, 0x0D, 0xB8] + [0] * 11 + [1]))
    ),
    # A CNAME the parser must skip over to reach the A record behind it.
    "cname-then-a.bin": (
        header(an=2)
        + question("c.example")
        + answer(PTR_Q, TYPE_CNAME, name("d.example"))
        + answer(PTR_Q, TYPE_A, bytes([1, 2, 3, 4]))
    ),
    # RCODE 3: Python returned an empty list here, the port returns Rcode(3)
    # (the caller's view is empty either way -- see wire.rs).
    "nxdomain.bin": header(flags=0x8183, an=0) + question("gone.example"),
    # ANCOUNT=1 but the answer's rdlength claims 4 bytes while only 2 follow:
    # the classic "malicious length" shape. Python's `break` returned the
    # partial answer; the port errors (Truncated).
    "truncated-answer.bin": (
        header(an=1) + question("a.example") + PTR_Q + struct.pack("!HHIH", TYPE_A, CLASS_IN, 300, 4) + b"\x01\x02"
    ),
    # The question name is a pointer to itself. The skipper steps over the
    # pointer without following it, so the parse terminates on the (absent)
    # answer section instead of looping.
    "pointer-loop-question.bin": header(an=0) + PTR_Q + struct.pack("!HH", TYPE_A, CLASS_IN),
    # A label length byte of 0x41 (65): not a valid label (max 63) and not a
    # pointer (the 0xC0 mask fails), so the walk treats it as a 65-byte label,
    # runs off the end of the packet and the parse is rejected. This is the
    # "bad label" shape workstreams/05 asks for.
    "bad-label-overrun.bin": header(an=0)
    + bytes([0x41])
    + b"short"
    + struct.pack("!HH", TYPE_A, CLASS_IN),
    # Fewer than 12 bytes: no header at all.
    "short-header.bin": b"\x12\x34\x81\x80\x00\x01",
}


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for filename, blob in FIXTURES.items():
        path = OUT / filename
        path.write_bytes(blob)
        print(f"{path.relative_to(OUT.parent.parent.parent.parent)}  {len(blob)} bytes")


if __name__ == "__main__":
    main()
