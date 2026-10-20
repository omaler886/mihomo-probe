# DNS Worker Contract

You are an implementation worker for the active complete-delivery goal. Work
directly in this dirty shared worktree; other agents own other modules. Preserve
all existing edits. Do not create chats/subagents, commit, push, deploy, modify
global configuration, or inspect credential/deployment data. Use apply_patch for
manual source edits. Follow existing Rust style and document function contracts.

## Ownership

Only modify `crates/probe-dns/**` and `workstreams/05_DNS_ECS.md`. Do not modify
root Cargo.toml/Cargo.lock, other crates, or Python. Keep existing dependency
versions. The current reqwest URL parser can provide IDNA conversion if useful;
avoid adding new dependencies because root lockfile updates belong to the parent.

Read the full specification's DNS/security sections and the current DNS code.
Replace obsolete assertions that accepted malformed DNS with explicit, justified
hardening tests; do not weaken useful checks to keep an old test green.

## Required Implementation

1. Correct ECS encoding: validate v4/v6 prefix length and mask unused subnet bits.
   Validate label and full-name length, support IDNA and absolute trailing-dot names.
2. Parse compressed names with real bounds/cycle protection. Validate response
   QR/opcode/TC/ID, question name/type/class, class/record lengths and record limits.
   Reject malformed packets without panic or partial misleading success.
3. Return CNAME chain and a conservative positive TTL (including aliases). Support
   negative NXDOMAIN/NODATA with SOA-derived TTL; other RCODEs remain failures.
4. Export this public contract from `probe_dns::wire`:
   `DnsAnswer { pub ips: Vec<String>, pub cname_chain: Vec<String>,
                pub ttl_secs: u32, pub negative: bool }`
   and
   `parse_response(data: &[u8], expected_id: u16, expected_name: &str,
                   expected_qtype: u16) -> Result<DnsAnswer, WireError>`.
5. Add `DohResolver::query_answer` with the same arguments as existing `query`,
   returning `Result<DnsAnswer, String>`. Use response-ID/question matching,
   bounded body reads (DNS message maximum), no redirects, sanitized errors and
   the supplied timeout. Preserve `query` and `resolve_views` as wrappers for
   existing callers, delegating to the stronger implementation.
6. Add local mock DoH tests for success, mismatch, malformed/oversized body, wrong
   MIME type/status and redirects. Keep tests offline; do not rely on `.invalid`
   requests to a real network for success. Add deterministic parser mutation or
   property tests (bounded) covering truncation, lengths, loops and Unicode.

## Verification And Handoff

Run crate-level formatting/checks/tests. Do not run workspace-wide formatting
while another agent edits other crates. Tests should use `--locked` if possible;
report dependency/lockfile issues instead of editing the root lockfile.
Update the owned workstream with facts and exact test commands. Final response:
files changed, APIs delivered, actual results, intentional compatibility changes
and any unfinished requirement. The parent will inspect and integrate your work.
