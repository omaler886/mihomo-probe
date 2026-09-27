"""DNS-over-HTTPS with EDNS Client Subnet, stdlib only.

Geo-DNS hands different addresses to CN and overseas resolvers, and round-robin
may return several per query. Probing only whatever the kernel happens to
resolve would sample one address per domain: a node behind five addresses where
one is dead flips between alive and dead for no reason, and the addresses CN
clients actually get are never tested at all.

ECS needs the wire format -- the JSON DoH APIs do not accept a client subnet --
so this builds and parses DNS packets by hand.
"""
import base64
import ipaddress
import os
import struct
import urllib.parse
import urllib.request

TYPE_A = 1
TYPE_AAAA = 28
OPT_TYPE = 41
ECS_OPTION = 8


def _encode_name(name):
    out = bytearray()
    for label in str(name).rstrip(".").split("."):
        raw = label.encode("idna") if not label.isascii() else label.encode()
        if not 0 < len(raw) < 64:
            raise ValueError(f"bad label {label!r}")
        out.append(len(raw))
        out += raw
    out.append(0)
    return bytes(out)


def ecs_option(address, source_prefix):
    """Build the EDNS Client Subnet option for one address."""
    net = ipaddress.ip_address(address.split("/")[0])
    family = 1 if net.version == 4 else 2
    nbytes = (source_prefix + 7) // 8
    payload = struct.pack("!HBB", family, source_prefix, 0) + net.packed[:nbytes]
    return struct.pack("!HH", ECS_OPTION, len(payload)) + payload


def build_query(name, qtype, ecs_address=None, ecs_prefix=24):
    """One question plus an OPT record carrying the client subnet."""
    qid = int.from_bytes(os.urandom(2), "big")
    has_ecs = ecs_address is not None
    header = struct.pack("!HHHHHH", qid, 0x0100, 1, 0, 0, 1 if has_ecs else 0)
    question = _encode_name(name) + struct.pack("!HH", qtype, 1)
    extra = b""
    if has_ecs:
        option = ecs_option(ecs_address, ecs_prefix)
        extra = (struct.pack("!B", 0)                       # root name
                 + struct.pack("!HHIH", OPT_TYPE, 4096, 0, len(option))
                 + option)
    return header + question + extra


def _skip_name(data, offset):
    while True:
        if offset >= len(data):
            return offset
        length = data[offset]
        if length == 0:
            return offset + 1
        if length & 0xC0 == 0xC0:      # compression pointer
            return offset + 2
        offset += 1 + length


def parse_ips(data):
    """Return the A/AAAA addresses in the answer section, in order."""
    ips = []
    if len(data) < 12:
        return ips
    questions = struct.unpack("!H", data[4:6])[0]
    answers = struct.unpack("!H", data[6:8])[0]
    offset = 12
    for _ in range(questions):
        offset = _skip_name(data, offset)
        offset += 4
    for _ in range(answers):
        offset = _skip_name(data, offset)
        if offset + 10 > len(data):
            break
        rtype, _class, _ttl, rdlength = struct.unpack("!HHIH", data[offset:offset + 10])
        offset += 10
        rdata = data[offset:offset + rdlength]
        offset += rdlength
        try:
            if rtype == TYPE_A and rdlength == 4:
                ips.append(str(ipaddress.IPv4Address(rdata)))
            elif rtype == TYPE_AAAA and rdlength == 16:
                ips.append(str(ipaddress.IPv6Address(rdata)))
        except ValueError:
            continue
    return ips


def query(name, qtype, resolver, ecs_address=None, ecs_prefix=24, timeout=8):
    """Resolve through one DoH endpoint; return the answer addresses."""
    packet = build_query(name, qtype, ecs_address, ecs_prefix)
    token = base64.urlsafe_b64encode(packet).rstrip(b"=").decode("ascii")
    separator = "&" if "?" in resolver else "?"
    url = f"{resolver}{separator}dns={token}"
    req = urllib.request.Request(url)
    req.add_header("Accept", "application/dns-message")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return parse_ips(resp.read())


def resolve_views(name, views, timeout=8):
    """Resolve through every configured vantage; return {view: [ip, ...]}.

    A view that fails yields an empty list -- the round still tests the other
    vantage, and if both fail the caller falls back to letting the kernel
    resolve the domain itself.
    """
    out = {}
    for label, view in (views or {}).items():
        resolver = (view or {}).get("resolver")
        if not resolver:
            continue
        ecs = (view or {}).get("ecs") or None
        prefix = int((view or {}).get("ecs_prefix", 24))
        found = []
        for qtype in (TYPE_A, TYPE_AAAA):
            try:
                found += query(name, qtype, resolver, ecs, prefix, timeout)
            except Exception:
                continue
        # both views may return the same address; dedupe but keep order
        seen, ordered = set(), []
        for ip in found:
            if ip not in seen:
                seen.add(ip)
                ordered.append(ip)
        out[label] = ordered
    return out