//! Hand-built DNS wire format, port of `mihomo_test/doh.py`.
//!
//! ECS needs the wire format -- the JSON DoH APIs do not accept a client
//! subnet -- so queries are built and answers parsed by hand.
//!
//! Three hardening deltas against the Python original, recorded in
//! workstreams/05:
//!
//! * the name skipper is explicitly bounded by [`MAX_NAME_JUMPS`]. Like the
//!   Python one it never dereferences a pointer: a pointer ends the walk in one
//!   step, so a self-pointing one is not a loop at all -- the bound actually
//!   guards a corrupt *label* chain. Python's `while True` stopped such a chain
//!   on its `offset >= len(data)` check and returned the (out-of-range) offset;
//!   here the stop is a named bound and the result is `None`, which the callers
//!   map to `Truncated`.
//! * a truncated packet is an error ([`WireError::Truncated`]) rather than a
//!   partial answer. Python's `break` on a short record returned whatever it
//!   had already collected; a corrupt response now yields an empty view
//!   instead of a silently short one.
//! * the response RCODE is checked: NXDOMAIN and SERVFAIL return an error
//!   instead of an empty answer list, so callers can distinguish "no records"
//!   from "the lookup failed". The caller's observable behaviour is unchanged
//!   -- `resolve_views` skips a failed query either way, so a dead lookup and
//!   a lookup with no records both leave the view empty.

use thiserror::Error;

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const OPT_TYPE: u16 = 41;
const ECS_OPTION: u16 = 8;

const FLAG_RD: u16 = 0x0100;

/// How many steps the walk will take before treating a name as malformed. A
/// name of N labels needs N steps for the labels plus one for the terminating
/// zero, so this allows 63 labels -- generous on purpose. Pointers are stepped
/// over, not followed, so it bounds a corrupt label chain rather than a
/// pointer chain.
const MAX_NAME_JUMPS: usize = 64;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("bad label {0:?}")]
    BadLabel(String),
    #[error("truncated packet")]
    Truncated,
    #[error("dns rcode {0}")]
    Rcode(u16),
}

/// `_encode_name`: length-prefixed labels terminated by the zero label.
///
/// Python idna-encodes a non-ASCII label here; this port rejects one instead
/// (see [`encode_label`]), so the doc names what this function does rather than
/// what the original does.
pub fn encode_name(name: &str) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    for label in name.trim_end_matches('.').split('.') {
        let raw = encode_label(label)?;
        if raw.is_empty() || raw.len() >= 64 {
            return Err(WireError::BadLabel(label.to_string()));
        }
        out.push(raw.len() as u8);
        out.extend_from_slice(&raw);
    }
    out.push(0);
    Ok(out)
}

/// Python `label.encode("idna")`: ASCII passes through, non-ASCII is
/// punycode-mapped. The `idna` crate is not a dependency here, so a non-ASCII
/// label is rejected instead. That is a real divergence, not an equivalent
/// failure: Python would punycode-encode `\u{4f8b}\u{3048}.jp` and resolve it,
/// whereas this port returns `BadLabel` and the view comes back empty. Names in
/// a subscription are ASCII in practice, so the path is not exercised -- it is
/// recorded as a known limitation in workstreams/05.
fn encode_label(label: &str) -> Result<Vec<u8>, WireError> {
    if label.is_ascii() {
        Ok(label.as_bytes().to_vec())
    } else {
        Err(WireError::BadLabel(label.to_string()))
    }
}

/// `ecs_option`: the EDNS Client Subnet option for one address
/// (`family, source_prefix, scope(0) + the address' leading bytes`).
fn ecs_option(address: &str, source_prefix: u8) -> Result<Vec<u8>, WireError> {
    let net = address.split('/').next().unwrap_or(address);
    let ip: std::net::IpAddr = net
        .parse()
        .map_err(|_| WireError::BadLabel(address.to_string()))?;
    let family: u16 = if ip.is_ipv4() { 1 } else { 2 };
    let packed = match ip {
        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
        std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    let nbytes = (source_prefix as usize).div_ceil(8);
    let mut payload = Vec::with_capacity(4 + nbytes);
    payload.extend_from_slice(&family.to_be_bytes());
    payload.push(source_prefix);
    payload.push(0);
    payload.extend_from_slice(&packed[..nbytes.min(packed.len())]);
    let mut out = (ECS_OPTION.to_be_bytes()).to_vec();
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// `build_query`: one question plus an OPT record carrying the client subnet.
pub fn build_query(
    name: &str,
    qtype: u16,
    ecs_address: Option<&str>,
    ecs_prefix: u8,
    qid: u16,
) -> Result<Vec<u8>, WireError> {
    let has_ecs = ecs_address.is_some();
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&qid.to_be_bytes());
    out.extend_from_slice(&FLAG_RD.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&(u16::from(has_ecs)).to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&encode_name(name)?);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // IN
    if let Some(address) = ecs_address {
        let option = ecs_option(address, ecs_prefix)?;
        out.push(0); // root name
        out.extend_from_slice(&OPT_TYPE.to_be_bytes());
        out.extend_from_slice(&4096u16.to_be_bytes()); // UDP payload size
        out.extend_from_slice(&0u32.to_be_bytes()); // extended rcode, no EDNS version
        out.extend_from_slice(&(option.len() as u16).to_be_bytes());
        out.extend_from_slice(&option);
    }
    Ok(out)
}

/// Skip over one (possibly pointer-terminated) name, returning the offset just
/// past it. Used for the question names and the answer owner names alike: a
/// pointer is stepped over, never followed -- its target holds a name this
/// parser does not need, because only the fixed-size fields after it
/// (type/class/ttl/rdlen and then the A/AAAA rdata) are read. Python has the
/// same single helper; keeping one here avoids two copies that could drift.
fn skip_name(data: &[u8], mut offset: usize) -> Option<usize> {
    for _ in 0..MAX_NAME_JUMPS {
        let byte = *data.get(offset)?;
        if byte == 0 {
            return Some(offset + 1);
        }
        if byte & 0xC0 == 0xC0 {
            // Compression pointer: two bytes, then the record's fixed fields
            // continue right after the pointer.
            return data.get(offset + 1).map(|_| offset + 2);
        }
        offset += 1 + byte as usize;
    }
    None
}

/// `parse_ips`: the A/AAAA addresses in the answer section, in order.
///
/// Hardened per workstreams/05: a nonzero RCODE is an error and a packet that
/// runs short is an error, where Python returned a partial (or empty) list
/// instead. The remaining Python behaviour is kept: records of any other type
/// are skipped, and an A/AAAA record whose rdlength is not 4/16 is skipped
/// rather than failing the whole answer. A name is stepped over, never
/// dereferenced, so a pointer loop just ends the walk -- see [`skip_name`].
pub fn parse_ips(data: &[u8]) -> Result<Vec<String>, WireError> {
    if data.len() < 12 {
        return Err(WireError::Truncated);
    }
    let rcode = u16::from_be_bytes([data[2], data[3]]) & 0x000F;
    if rcode != 0 {
        return Err(WireError::Rcode(rcode));
    }
    let questions = u16::from_be_bytes([data[4], data[5]]);
    let answers = u16::from_be_bytes([data[6], data[7]]);
    let mut offset = 12usize;
    for _ in 0..questions {
        offset = skip_name(data, offset).ok_or(WireError::Truncated)?;
        offset += 4;
    }
    let mut ips = Vec::new();
    for _ in 0..answers {
        offset = skip_name(data, offset).ok_or(WireError::Truncated)?;
        if offset + 10 > data.len() {
            return Err(WireError::Truncated);
        }
        let rtype = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let rdlength = u16::from_be_bytes([data[offset + 8], data[offset + 9]]) as usize;
        offset += 10;
        let end = offset + rdlength;
        if end > data.len() {
            return Err(WireError::Truncated);
        }
        let rdata = &data[offset..end];
        offset = end;
        match rtype {
            TYPE_A if rdlength == 4 => ips.push(format!(
                "{}.{}.{}.{}",
                rdata[0], rdata[1], rdata[2], rdata[3]
            )),
            TYPE_AAAA if rdlength == 16 => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(rdata);
                let v6 = std::net::Ipv6Addr::from(octets);
                ips.push(v6.to_string());
            }
            _ => {}
        }
    }
    Ok(ips)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DNS response header: rcode 0, one question, `answers` answers.
    fn header(answers: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x1234u16.to_be_bytes());
        out.extend_from_slice(&0x8180u16.to_be_bytes()); // QR|RD|RA, rcode 0
        out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        out.extend_from_slice(&answers.to_be_bytes()); // ANCOUNT
        out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
        out
    }

    /// The question section for one name.
    fn question(name: &str) -> Vec<u8> {
        let mut out = encode_name(name).unwrap();
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // IN
        out
    }

    /// One answer record: owner name (label form), type IN, ttl 0, rdata.
    fn answer(name: &str, rtype: u16, rdata: &[u8]) -> Vec<u8> {
        let mut out = encode_name(name).unwrap();
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // IN
        out.extend_from_slice(&0u32.to_be_bytes()); // TTL
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
        out
    }

    #[test]
    fn the_query_carries_rd_a_question_and_the_ecs_option() {
        let packet =
            build_query("example.com", TYPE_A, Some("114.114.114.0/24"), 24, 0xBEEF).unwrap();
        assert_eq!(&packet[0..2], &[0xBE, 0xEF], "the caller's id");
        assert_eq!(&packet[2..4], &[0x01, 0x00], "RD set");
        assert_eq!(&packet[4..6], &[0, 1], "one question");
        assert_eq!(&packet[10..12], &[0, 1], "one additional (the OPT)");
        let name = encode_name("example.com").unwrap();
        assert_eq!(&packet[12..12 + name.len()], &name[..]);
        let qtype_at = 12 + name.len();
        assert_eq!(&packet[qtype_at..qtype_at + 2], &TYPE_A.to_be_bytes());
        let opt = &packet[qtype_at + 4..];
        // OPT record: NAME(root) TYPE(41) CLASS=udpsize TTL RDLEN, then the
        // EDNS options -- the ECS option is code 8, its payload family,
        // source prefix, scope(0), then the address' leading bytes.
        assert_eq!(opt[0], 0, "root name");
        assert_eq!(&opt[1..3], &OPT_TYPE.to_be_bytes());
        assert_eq!(&opt[3..5], &4096u16.to_be_bytes(), "UDP payload size");
        let rdlen = u16::from_be_bytes([opt[9], opt[10]]) as usize;
        assert_eq!(
            rdlen, 11,
            "option header 4 + family 2 + prefix 1 + scope 1 + 3 address bytes"
        );
        assert_eq!(
            &opt[11..13],
            &ECS_OPTION.to_be_bytes(),
            "the ECS option code"
        );
        assert_eq!(
            &opt[13..15],
            &7u16.to_be_bytes(),
            "the option payload length"
        );
        assert_eq!(&opt[15..17], &[0, 1], "family 1 (IPv4)");
        assert_eq!(opt[17], 24, "source prefix");
        assert_eq!(opt[18], 0, "scope 0 in a query");
        assert_eq!(
            &opt[19..22],
            &[114, 114, 114],
            "the first 3 bytes of the address"
        );
    }

    #[test]
    fn a_query_without_ecs_has_no_additional_section() {
        let packet = build_query("example.com", TYPE_A, None, 24, 1).unwrap();
        assert_eq!(&packet[10..12], &[0, 0]);
    }

    #[test]
    fn a_non_ascii_label_is_rejected() {
        // A real divergence, not an equivalent failure: Python idna-maps
        // `例え.jp` to punycode and resolves it; this port returns BadLabel,
        // so the view comes back empty where Python's would not.
        // Subscriptions are ASCII in practice (workstreams/05, known
        // limitation).
        assert!(encode_name("\u{4f8b}\u{3048}.jp").is_err());
        assert!(encode_name("example.com").is_ok());
    }

    #[test]
    fn parse_ips_reads_a_records_in_order() {
        let mut packet = header(2);
        packet.extend(question("a.example"));
        packet.extend(answer("a.example", TYPE_A, &[1, 2, 3, 4]));
        packet.extend(answer("b.example", TYPE_A, &[9, 9, 9, 9]));
        assert_eq!(parse_ips(&packet).unwrap(), vec!["1.2.3.4", "9.9.9.9"]);
    }

    #[test]
    fn parse_ips_reads_aaaa_records() {
        let rdata = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ];
        let mut packet = header(1);
        packet.extend(question("v6.example"));
        packet.extend(answer("v6.example", TYPE_AAAA, &rdata));
        assert_eq!(parse_ips(&packet).unwrap(), vec!["2001:db8::1"]);
    }

    #[test]
    fn other_record_types_are_skipped() {
        let mut packet = header(2);
        packet.extend(question("c.example"));
        packet.extend(answer("c.example", 5, &[0x61, 0x62, 0x63])); // CNAME
        packet.extend(answer("c.example", TYPE_A, &[1, 2, 3, 4]));
        assert_eq!(parse_ips(&packet).unwrap(), vec!["1.2.3.4"]);
    }

    #[test]
    fn a_nonzero_rcode_is_an_error_not_an_empty_answer() {
        let mut packet = header(0);
        packet[3] = 0x83; // RCODE 3 = NXDOMAIN
        packet.extend(question("gone.example"));
        assert_eq!(parse_ips(&packet), Err(WireError::Rcode(3)));
    }

    #[test]
    fn truncated_packets_are_errors() {
        assert_eq!(parse_ips(&[]), Err(WireError::Truncated));
        assert_eq!(parse_ips(&[0u8; 11]), Err(WireError::Truncated));
    }

    #[test]
    fn a_self_pointing_pointer_terminates_rather_than_hanging() {
        // The question name is a pointer to itself. The skipper never
        // dereferences pointers -- it steps past the two bytes, and the loop is
        // bounded by MAX_NAME_JUMPS -- so a malicious self-loop costs a single
        // step and the walk then ends on the packet's own bounds. Python's
        // unbounded loop reached the same place via its buffer check.
        let mut packet = vec![0u8; 16];
        packet[4..6].copy_from_slice(&1u16.to_be_bytes());
        packet[12] = 0xC0;
        packet[13] = 0x0C; // pointer to offset 12 -- itself
        assert_eq!(parse_ips(&packet), Ok(vec![]));
    }

    #[test]
    fn an_answer_owner_name_may_be_a_pointer_into_the_question() {
        let question_name = encode_name("x.example").unwrap();
        let mut packet = header(1);
        packet.extend(&question_name);
        packet.extend_from_slice(&TYPE_A.to_be_bytes());
        packet.extend_from_slice(&1u16.to_be_bytes());
        // Answer: owner name is a pointer to offset 12 (the question name).
        packet.push(0xC0);
        packet.push(12);
        packet.extend_from_slice(&TYPE_A.to_be_bytes());
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&4u16.to_be_bytes());
        packet.extend_from_slice(&[7, 7, 7, 7]);
        assert_eq!(parse_ips(&packet).unwrap(), vec!["7.7.7.7"]);
    }

    /// The committed binary fixtures (regenerate with
    /// `python tools/make_dns_fixtures.py`). They pin the byte-level shapes
    /// workstreams/05 asks for -- a well-formed answer behind a compression
    /// pointer, an AAAA answer, a CNAME to skip, NXDOMAIN, a lying rdlength,
    /// a self-pointing question name, a bad label length, and a packet with no
    /// header -- so a parser change that shifts an offset fails here rather
    /// than in production. The lying rdlength and the bad label are the two
    /// shapes the inline tests above do not cover.
    mod fixtures {
        use super::*;

        const A_ANSWER: &[u8] = include_bytes!("../fixtures/dns-packets/a-answer-compressed.bin");
        const AAAA_ANSWER: &[u8] = include_bytes!("../fixtures/dns-packets/aaaa-answer.bin");
        const CNAME_THEN_A: &[u8] = include_bytes!("../fixtures/dns-packets/cname-then-a.bin");
        const NXDOMAIN: &[u8] = include_bytes!("../fixtures/dns-packets/nxdomain.bin");
        const TRUNCATED_ANSWER: &[u8] =
            include_bytes!("../fixtures/dns-packets/truncated-answer.bin");
        const POINTER_LOOP: &[u8] =
            include_bytes!("../fixtures/dns-packets/pointer-loop-question.bin");
        const BAD_LABEL: &[u8] = include_bytes!("../fixtures/dns-packets/bad-label-overrun.bin");
        const SHORT_HEADER: &[u8] = include_bytes!("../fixtures/dns-packets/short-header.bin");

        #[test]
        fn a_well_formed_answer_parses_through_its_pointer() {
            assert_eq!(parse_ips(A_ANSWER).unwrap(), vec!["1.2.3.4"]);
        }

        #[test]
        fn an_aaaa_answer_parses() {
            assert_eq!(parse_ips(AAAA_ANSWER).unwrap(), vec!["2001:db8::1"]);
        }

        #[test]
        fn a_cname_is_skipped_to_reach_the_a_record_behind_it() {
            assert_eq!(parse_ips(CNAME_THEN_A).unwrap(), vec!["1.2.3.4"]);
        }

        #[test]
        fn nxdomain_is_an_error() {
            assert_eq!(parse_ips(NXDOMAIN), Err(WireError::Rcode(3)));
        }

        #[test]
        fn a_lying_rdlength_is_an_error_not_a_partial_answer() {
            assert_eq!(parse_ips(TRUNCATED_ANSWER), Err(WireError::Truncated));
        }

        #[test]
        fn a_self_pointing_question_name_terminates() {
            assert_eq!(parse_ips(POINTER_LOOP), Ok(vec![]));
        }

        #[test]
        fn a_bad_label_length_runs_off_the_end_and_is_rejected() {
            // 0x41 is neither a label (max 63) nor a pointer (0xC0 mask), so
            // the walk claims a 65-byte label and leaves the packet.
            assert_eq!(parse_ips(BAD_LABEL), Err(WireError::Truncated));
        }

        #[test]
        fn a_packet_shorter_than_the_header_is_an_error() {
            assert_eq!(parse_ips(SHORT_HEADER), Err(WireError::Truncated));
        }
    }
}
