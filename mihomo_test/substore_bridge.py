"""Read-only projection of the probe ledger for the official Sub-Store bridge.

`GET /api/probe/nodes` (server.py) and its tests must agree byte-for-byte on
what the endpoint may reveal about a node, so the shape lives here instead of
inline in the route: `PROBE_NODE_FIELDS` is the single source of truth for the
field whitelist, and `probe_nodes_payload` turns `db.list_nodes` rows into
payload dicts that cannot carry anything else.

The whitelist is exactly the measurement metadata the Script Operator
(`substore_bridge/probe_filter.script.js`) consumes -- name/source/status/
country/delay/consec_fail/server/proto/category. Deliberately excluded from
the db row (ARCHITECTURE §3.4):

- `fingerprint` -- a sha256 over the node's connection parameters. Not a
  plaintext credential, but a derived identifier; leaving it out keeps the
  "this endpoint carries no credentials" promise clean.
- `last_reason` -- free text that can embed kernel error detail (same
  information surface as S-17).
- `first_seen/last_seen/last_ok/total_ok/total_fail/ip_alive/ip_total` -- no
  use to the script.
"""

from . import db
from . import policy

# The 9 fields a node may carry onto the public payload, in contract order
# (ARCHITECTURE §3.4). `db.list_nodes` rows use `name` (aliased from
# `display`) and `last_delay_ms`; everything else maps by the same name.
PROBE_NODE_FIELDS = ("name", "source", "status", "country", "delay_ms",
                     "consec_fail", "server", "proto", "category")

# The five ledger statuses (policy.py), for edge rejection of a bad
# `?status=` on the read-only surface.
PROBE_NODE_STATUSES = (policy.ALIVE, policy.PENDING, policy.DEAD,
                       policy.UNKNOWN, policy.EXCLUDED)


def probe_nodes_payload(rows):
    """Project `db.list_nodes` rows onto the whitelisted 9-field payload.

    Pure function: no I/O, no config, no clock -- the caller passes rows and
    gets plain dicts containing exactly `PROBE_NODE_FIELDS`, so no credential
    or ledger bookkeeping can leak through even if the row carries more keys.
    Types per contract: `name/source/status/category` are strings; `country/
    server/proto` and `delay_ms` stay string-or-null / int-or-null exactly as
    the ledger stored them (a node the current round has not finished may not
    have its country or server resolved yet); `consec_fail` is always an int.

    A NULL `category` (the column was added by migration and is only stamped
    during rounds) reads as "direct", the same default every other consumer
    applies -- a missing stamp must not surface as a fourth kind of node.

    Rows that come straight from the nodes table instead of through
    `list_nodes` still work: the `display` key is honoured as a fallback for
    `name`.
    """
    out = []
    for row in rows or []:
        name = row.get("name")
        if name is None:
            name = row.get("display")
        category = row.get("category") or "direct"
        out.append({
            "name": name,
            "source": row.get("source"),
            "status": row.get("status"),
            "country": row.get("country"),
            "delay_ms": row.get("last_delay_ms"),
            "consec_fail": int(row.get("consec_fail") or 0),
            "server": row.get("server"),
            "proto": row.get("proto"),
            "category": category,
        })
    return out


def probe_nodes_snapshot(rows, generated_at=None):
    """Build the full `/api/probe/nodes` response envelope for `rows`.

    The endpoint does not make up its own timestamp: `generated_at` is filled
    here (UTC, second precision, via `db.now()` -- the ledger-wide stamp
    convention) so the response shape has exactly one author. An empty ledger
    is a valid snapshot -- 200 with `count: 0` and `nodes: []` -- never a 404:
    the consumer must be able to tell "path/auth wrong" from "the ledger is
    genuinely empty".
    """
    nodes = probe_nodes_payload(rows)
    return {
        "ok": True,
        "generated_at": generated_at or db.now(),
        "count": len(nodes),
        "nodes": nodes,
    }
