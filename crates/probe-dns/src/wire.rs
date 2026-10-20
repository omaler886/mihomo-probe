//! Bounded DNS wire encoding and response validation for A/AAAA DoH queries.
//!
//! A response is validated in full before any addresses are returned. Only
//! records at the end of the question's CNAME chain contribute addresses.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use thiserror::Error;

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
/// The two-byte DNS-over-TCP length field bounds a DoH DNS message too.
pub const MAX_DNS_MESSAGE_SIZE: usize = 65_535;

const TYPE_NS: u16 = 2;
const TYPE_CNAME: u16 = 5;
const TYPE_SOA: u16 = 6;
const OPT_TYPE: u16 = 41;
const ECS_OPTION: u16 = 8;
const CLASS_IN: u16 = 1;
const HEADER_LEN: usize = 12;
const MAX_NAME_LEN: usize = 255;
const MAX_NAME_STEPS: usize = 256;
const MAX_RECORDS: usize = 1024;
const MAX_CNAME_CHAIN: usize = 128;
const FLAG_RD: u16 = 0x0100;

/// A validated address answer; a zero TTL means it must not be cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsAnswer {
    /// Stable, deduplicated addresses of the requested family and canonical name.
    pub ips: Vec<String>,
    /// Ordered CNAME targets, excluding the original question; no trailing dots.
    pub cname_chain: Vec<String>,
    /// Minimum address/alias TTL, or SOA-derived negative TTL including aliases.
    pub ttl_secs: u32,
    /// NXDOMAIN or NODATA. A negative answer without an applicable SOA has TTL 0.
    pub negative: bool,
}

/// Parse/encoding failures contain fixed reasons, never queried names or URLs.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("invalid DNS label: {0}")]
    BadLabel(String),
    #[error("DNS name exceeds 255 wire bytes")]
    NameTooLong,
    #[error("invalid ECS address or prefix")]
    InvalidEcs,
    #[error("only A and AAAA questions are supported")]
    UnsupportedQuestion,
    #[error("truncated DNS packet")]
    Truncated,
    #[error("DNS message exceeds 65535 bytes")]
    MessageTooLarge,
    #[error("invalid DNS response: {0}")]
    InvalidResponse(&'static str),
    #[error("DNS response ID mismatch")]
    IdMismatch,
    #[error("DNS response question mismatch")]
    QuestionMismatch,
    #[error("invalid DNS compression pointer")]
    InvalidPointer,
    #[error("DNS compression cycle or step limit")]
    CompressionLoop,
    #[error("DNS record count exceeds limit")]
    RecordLimit,
    #[error("invalid DNS record: {0}")]
    InvalidRecord(&'static str),
    #[error("DNS CNAME cycle or chain limit")]
    CnameLoop,
    #[error("dns rcode {0}")]
    Rcode(u16),
}

/// Canonical wire labels retain binary DNS names without ambiguous dot joining.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Name(Vec<Vec<u8>>);

impl Name {
    /// Render these labels as lowercase presentation text, escaping binary bytes.
    fn to_text(&self) -> String {
        if self.0.is_empty() {
            return ".".to_string();
        }
        self.0
            .iter()
            .map(|label| {
                let mut text = String::new();
                for &byte in label {
                    if byte.is_ascii_graphic() && byte != b'.' && byte != b'\\' {
                        text.push(char::from(byte));
                    } else {
                        text.push_str(&format!("\\{byte:03}"));
                    }
                }
                text
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Return whether this name lies within `zone`, comparing whole wire labels.
    fn is_within(&self, zone: &Name) -> bool {
        self.0.ends_with(&zone.0)
    }
}

/// Normalize a user name with URL's IDNA implementation, returning bounded labels.
fn normalize_name(name: &str) -> Result<Name, WireError> {
    if name.len() > MAX_NAME_LEN * 4 {
        return Err(WireError::NameTooLong);
    }
    let mapped = name.replace(['\u{3002}', '\u{ff0e}', '\u{ff61}'], ".");
    if mapped == "." {
        return Ok(Name(Vec::new()));
    }
    let bare = mapped.strip_suffix('.').unwrap_or(&mapped);
    if bare.split('.').any(str::is_empty) {
        return Err(WireError::BadLabel("empty label".to_string()));
    }
    if bare
        .bytes()
        .any(|byte| byte.is_ascii() && !byte.is_ascii_alphanumeric() && !b"-_.".contains(&byte))
    {
        return Err(WireError::BadLabel("invalid name character".to_string()));
    }
    // The suffix prevents WHATWG's IPv4 shorthand rewriting (e.g. "127.1").
    let mut url = reqwest::Url::parse("http://idna.invalid/")
        .map_err(|_| WireError::BadLabel("IDNA setup".to_string()))?;
    url.set_host(Some(&format!("{bare}.invalid")))
        .map_err(|_| WireError::BadLabel("IDNA conversion".to_string()))?;
    let host = url
        .host_str()
        .and_then(|host| host.strip_suffix(".invalid"))
        .ok_or_else(|| WireError::BadLabel("IDNA conversion".to_string()))?;
    let mut labels = Vec::new();
    let mut wire_len = 1;
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(WireError::BadLabel("label length".to_string()));
        }
        wire_len += label.len() + 1;
        labels.push(label.as_bytes().to_ascii_lowercase());
    }
    if wire_len > MAX_NAME_LEN {
        return Err(WireError::NameTooLong);
    }
    Ok(Name(labels))
}

/// Encode `name` as IDNA wire labels; accept one absolute trailing dot or root.
pub fn encode_name(name: &str) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    for label in normalize_name(name)?.0 {
        out.push(label.len() as u8);
        out.extend_from_slice(&label);
    }
    out.push(0);
    Ok(out)
}

/// Return the same canonical ASCII name used in questions, for cache and TLS identity.
pub fn canonical_name(name: &str) -> Result<String, WireError> {
    Ok(normalize_name(name)?.to_text())
}

/// Encode an ECS option for `address`; `source_prefix` overrides a valid CIDR suffix.
fn ecs_option(address: &str, source_prefix: u8) -> Result<Vec<u8>, WireError> {
    let (address, cidr) = match address.split_once('/') {
        Some((address, prefix)) => (
            address,
            Some(prefix.parse::<u8>().map_err(|_| WireError::InvalidEcs)?),
        ),
        None => (address, None),
    };
    let ip: IpAddr = address.parse().map_err(|_| WireError::InvalidEcs)?;
    let (family, mut packed, max_prefix): (u16, Vec<u8>, u8) = match ip {
        IpAddr::V4(ip) => (1, ip.octets().to_vec(), 32),
        IpAddr::V6(ip) => (2, ip.octets().to_vec(), 128),
    };
    if source_prefix > max_prefix || cidr.is_some_and(|prefix| prefix > max_prefix) {
        return Err(WireError::InvalidEcs);
    }
    packed.truncate(usize::from(source_prefix).div_ceil(8));
    if !source_prefix.is_multiple_of(8) {
        if let Some(last) = packed.last_mut() {
            *last &= 0xff << (8 - source_prefix % 8);
        }
    }
    let mut out = ECS_OPTION.to_be_bytes().to_vec();
    out.extend_from_slice(&(4 + packed.len() as u16).to_be_bytes());
    out.extend_from_slice(&family.to_be_bytes());
    out.extend_from_slice(&[source_prefix, 0]);
    out.extend_from_slice(&packed);
    Ok(out)
}

/// Build one IN A/AAAA question with `qid`, RD, and optional validated ECS.
pub fn build_query(
    name: &str,
    qtype: u16,
    ecs_address: Option<&str>,
    ecs_prefix: u8,
    qid: u16,
) -> Result<Vec<u8>, WireError> {
    validate_qtype(qtype)?;
    let mut out = Vec::with_capacity(64);
    for value in [qid, FLAG_RD, 1, 0, 0, u16::from(ecs_address.is_some())] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&encode_name(name)?);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    if let Some(address) = ecs_address {
        let option = ecs_option(address, ecs_prefix)?;
        out.push(0);
        out.extend_from_slice(&OPT_TYPE.to_be_bytes());
        out.extend_from_slice(&4096u16.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(option.len() as u16).to_be_bytes());
        out.extend_from_slice(&option);
    }
    Ok(out)
}

/// Accept supported address `qtype`s or return a fixed, non-sensitive error.
fn validate_qtype(qtype: u16) -> Result<(), WireError> {
    match qtype {
        TYPE_A | TYPE_AAAA => Ok(()),
        _ => Err(WireError::UnsupportedQuestion),
    }
}

/// Read `length` bytes at `offset`, rejecting overflow and packet truncation.
fn read_bytes(data: &[u8], offset: usize, length: usize) -> Result<&[u8], WireError> {
    let end = offset.checked_add(length).ok_or(WireError::Truncated)?;
    data.get(offset..end).ok_or(WireError::Truncated)
}

/// Read a big-endian u16 at `offset`, or report a truncated packet.
fn read_u16(data: &[u8], offset: usize) -> Result<u16, WireError> {
    let bytes = read_bytes(data, offset, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Read a big-endian u32 at `offset`, or report a truncated packet.
fn read_u32(data: &[u8], offset: usize) -> Result<u32, WireError> {
    let bytes = read_bytes(data, offset, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Decode a bounded name at `start`; return its labels and encoded-field end.
///
/// `name_offsets` tracks label boundaries across this packet, preventing pointers
/// into header integers, label interiors, and opaque non-name RDATA.
fn read_name(
    data: &[u8],
    start: usize,
    field_end: usize,
    name_offsets: &mut HashSet<usize>,
) -> Result<(Name, usize), WireError> {
    let mut cursor = start;
    let mut boundary = field_end.min(data.len());
    let mut encoded_end = None;
    let mut visited = HashSet::new();
    let mut labels = Vec::new();
    let mut wire_len = 1;
    for _ in 0..MAX_NAME_STEPS {
        if !visited.insert(cursor) {
            return Err(WireError::CompressionLoop);
        }
        let byte = *data
            .get(cursor)
            .filter(|_| cursor < boundary)
            .ok_or(WireError::Truncated)?;
        name_offsets.insert(cursor);
        if byte & 0xc0 == 0xc0 {
            if cursor + 2 > boundary {
                return Err(WireError::Truncated);
            }
            let target = usize::from(read_u16(data, cursor)? & 0x3fff);
            // RFC 1035 compression refers to a prior occurrence, never the header.
            if target < HEADER_LEN || target >= cursor || !name_offsets.contains(&target) {
                return Err(WireError::InvalidPointer);
            }
            encoded_end.get_or_insert(cursor + 2);
            cursor = target;
            boundary = data.len();
            continue;
        }
        if byte & 0xc0 != 0 {
            return Err(WireError::BadLabel("reserved label tag".to_string()));
        }
        if byte == 0 {
            return Ok((Name(labels), encoded_end.unwrap_or(cursor + 1)));
        }
        let end = cursor + 1 + usize::from(byte);
        if end > boundary {
            return Err(WireError::Truncated);
        }
        wire_len += usize::from(byte) + 1;
        if wire_len > MAX_NAME_LEN {
            return Err(WireError::NameTooLong);
        }
        labels.push(read_bytes(data, cursor + 1, usize::from(byte))?.to_ascii_lowercase());
        cursor = end;
    }
    Err(WireError::CompressionLoop)
}

/// Header fields retained after validating the message envelope and record budget.
struct Header {
    id: u16,
    rcode: u16,
    counts: [usize; 3],
}

/// Validate `data`'s size, flags, question count, and aggregate record count.
fn read_header(data: &[u8]) -> Result<Header, WireError> {
    read_bytes(data, 0, HEADER_LEN)?;
    if data.len() > MAX_DNS_MESSAGE_SIZE {
        return Err(WireError::MessageTooLarge);
    }
    let flags = read_u16(data, 2)?;
    if flags & 0x8000 == 0 || flags & 0x7800 != 0 || flags & 0x0040 != 0 {
        return Err(WireError::InvalidResponse("QR/opcode/reserved flags"));
    }
    if flags & 0x0200 != 0 {
        return Err(WireError::InvalidResponse("truncated response flag"));
    }
    if read_u16(data, 4)? != 1 {
        return Err(WireError::QuestionMismatch);
    }
    let counts = [
        usize::from(read_u16(data, 6)?),
        usize::from(read_u16(data, 8)?),
        usize::from(read_u16(data, 10)?),
    ];
    if counts.iter().sum::<usize>() > MAX_RECORDS {
        return Err(WireError::RecordLimit);
    }
    Ok(Header {
        id: read_u16(data, 0)?,
        rcode: flags & 0x000f,
        counts,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Answer,
    Authority,
    Additional,
}

enum RecordData {
    Address(IpAddr),
    Cname(Name),
    Soa(u32),
    Opt(u16),
    Other,
}

struct Record {
    owner: Name,
    rtype: u16,
    ttl: u32,
    data: RecordData,
}

struct Message {
    answers: Vec<Record>,
    authority: Vec<Record>,
    rcode: u16,
}

/// Treat a high-bit TTL as zero, as required by RFC 2181 section 8.
fn cache_ttl(ttl: u32) -> u32 {
    if ttl & 0x8000_0000 != 0 {
        0
    } else {
        ttl
    }
}

/// Decode one name RDATA at `start`, requiring it to consume exactly `end`.
fn read_exact_name(
    data: &[u8],
    start: usize,
    end: usize,
    name_offsets: &mut HashSet<usize>,
) -> Result<Name, WireError> {
    let (name, next) = read_name(data, start, end, name_offsets)?;
    if next != end {
        return Err(WireError::InvalidRecord("name RDATA length"));
    }
    Ok(name)
}

/// Read two adjacent names within `end`, updating `name_offsets` and returning their end.
fn read_name_pair(
    data: &[u8],
    start: usize,
    end: usize,
    name_offsets: &mut HashSet<usize>,
) -> Result<usize, WireError> {
    let (_, next) = read_name(data, start, end, name_offsets)?;
    let (_, next) = read_name(data, next, end, name_offsets)?;
    Ok(next)
}

/// Validate SOA names and five integers, returning min(RR TTL, SOA MINIMUM).
fn read_soa(
    data: &[u8],
    start: usize,
    end: usize,
    ttl: u32,
    name_offsets: &mut HashSet<usize>,
) -> Result<u32, WireError> {
    let next = read_name_pair(data, start, end, name_offsets)?;
    if next + 20 != end {
        return Err(WireError::InvalidRecord("SOA RDATA length"));
    }
    Ok(ttl.min(cache_ttl(read_u32(data, next + 16)?)))
}

/// Validate TXT's sequence of length-prefixed strings, returning no payload.
fn validate_txt(data: &[u8]) -> Result<(), WireError> {
    if data.is_empty() {
        return Err(WireError::InvalidRecord("empty TXT RDATA"));
    }
    let mut offset = 0;
    while offset < data.len() {
        offset += 1 + usize::from(data[offset]);
        if offset > data.len() {
            return Err(WireError::InvalidRecord("TXT string length"));
        }
    }
    Ok(())
}

/// Decode supported RDATA inside `[start, end)` and validate known record lengths.
fn read_record_data(
    data: &[u8],
    rtype: u16,
    start: usize,
    end: usize,
    ttl: u32,
    name_offsets: &mut HashSet<usize>,
) -> Result<RecordData, WireError> {
    let bytes = read_bytes(data, start, end - start)?;
    match rtype {
        TYPE_A => {
            let octets: [u8; 4] = bytes
                .try_into()
                .map_err(|_| WireError::InvalidRecord("A RDATA length"))?;
            Ok(RecordData::Address(Ipv4Addr::from(octets).into()))
        }
        TYPE_AAAA => {
            let octets: [u8; 16] = bytes
                .try_into()
                .map_err(|_| WireError::InvalidRecord("AAAA RDATA length"))?;
            Ok(RecordData::Address(Ipv6Addr::from(octets).into()))
        }
        TYPE_CNAME => Ok(RecordData::Cname(read_exact_name(
            data,
            start,
            end,
            name_offsets,
        )?)),
        TYPE_SOA => Ok(RecordData::Soa(read_soa(
            data,
            start,
            end,
            ttl,
            name_offsets,
        )?)),
        TYPE_NS | 3 | 4 | 7 | 8 | 9 | 12 | 39 => {
            read_exact_name(data, start, end, name_offsets)?;
            Ok(RecordData::Other)
        }
        14 | 17 => {
            if read_name_pair(data, start, end, name_offsets)? != end {
                return Err(WireError::InvalidRecord("paired-name RDATA length"));
            }
            Ok(RecordData::Other)
        }
        15 | 18 | 21 | 33 | 36 => {
            let prefix = if rtype == 33 { 6 } else { 2 };
            read_exact_name(data, start + prefix, end, name_offsets)?;
            Ok(RecordData::Other)
        }
        16 => {
            validate_txt(bytes)?;
            Ok(RecordData::Other)
        }
        0 | 251..=255 => Err(WireError::InvalidRecord("reserved or question-only type")),
        _ => Ok(RecordData::Other),
    }
}

/// Validate an ECS response payload's family, prefixes, length, and unused bits.
fn validate_ecs(data: &[u8]) -> Result<(), WireError> {
    read_bytes(data, 0, 4)?;
    let max_prefix = match read_u16(data, 0)? {
        1 => 32,
        2 => 128,
        _ => return Err(WireError::InvalidEcs),
    };
    let source_prefix = data[2];
    if source_prefix > max_prefix || data[3] > max_prefix {
        return Err(WireError::InvalidEcs);
    }
    if data.len() != 4 + usize::from(source_prefix).div_ceil(8) {
        return Err(WireError::InvalidEcs);
    }
    if !source_prefix.is_multiple_of(8) {
        let unused_mask = (1u8 << (8 - source_prefix % 8)) - 1;
        if data.last().is_some_and(|last| last & unused_mask != 0) {
            return Err(WireError::InvalidEcs);
        }
    }
    Ok(())
}

/// Validate EDNS version/options in `data`, returning the extended RCODE bits.
fn read_opt(data: &[u8], ttl: u32) -> Result<u16, WireError> {
    if ttl & 0x00ff_7fff != 0 {
        return Err(WireError::InvalidRecord("EDNS version or reserved flags"));
    }
    let mut offset = 0;
    let mut has_ecs = false;
    while offset < data.len() {
        let code = read_u16(data, offset)?;
        let length = usize::from(read_u16(data, offset + 2)?);
        let payload = read_bytes(data, offset + 4, length)?;
        if code == ECS_OPTION {
            if has_ecs {
                return Err(WireError::InvalidRecord("duplicate ECS option"));
            }
            validate_ecs(payload)?;
            has_ecs = true;
        }
        offset += 4 + length;
    }
    Ok(((ttl >> 24) as u16) << 4)
}

/// Read one record in `section`, advancing `offset` only after complete validation.
fn read_record(
    data: &[u8],
    offset: &mut usize,
    section: Section,
    name_offsets: &mut HashSet<usize>,
) -> Result<Record, WireError> {
    let (owner, fields) = read_name(data, *offset, data.len(), name_offsets)?;
    read_bytes(data, fields, 10)?;
    let rtype = read_u16(data, fields)?;
    let class = read_u16(data, fields + 2)?;
    let raw_ttl = read_u32(data, fields + 4)?;
    let length = usize::from(read_u16(data, fields + 8)?);
    let start = fields + 10;
    let bytes = read_bytes(data, start, length)?;
    let end = start + length;
    let ttl = cache_ttl(raw_ttl);
    let record_data = if rtype == OPT_TYPE {
        if section != Section::Additional || !owner.0.is_empty() || fields != *offset + 1 {
            return Err(WireError::InvalidRecord("OPT owner or section"));
        }
        RecordData::Opt(read_opt(bytes, raw_ttl)?)
    } else {
        if class != CLASS_IN {
            return Err(WireError::InvalidRecord("non-IN class"));
        }
        read_record_data(data, rtype, start, end, ttl, name_offsets)?
    };
    *offset = end;
    Ok(Record {
        owner,
        rtype,
        ttl,
        data: record_data,
    })
}

/// Validate all sections from `offset`; reject duplicate OPT and trailing bytes.
fn read_message(
    data: &[u8],
    header: &Header,
    mut offset: usize,
    name_offsets: &mut HashSet<usize>,
) -> Result<Message, WireError> {
    let mut message = Message {
        answers: Vec::new(),
        authority: Vec::new(),
        rcode: header.rcode,
    };
    let mut has_opt = false;
    let sections = [Section::Answer, Section::Authority, Section::Additional];
    for (section, count) in sections.into_iter().zip(header.counts) {
        for _ in 0..count {
            let record = read_record(data, &mut offset, section, name_offsets)?;
            if has_opt && matches!(record.data, RecordData::Opt(_)) {
                return Err(WireError::InvalidRecord("duplicate OPT record"));
            }
            if let RecordData::Opt(rcode) = record.data {
                has_opt = true;
                message.rcode |= rcode;
            }
            match section {
                Section::Answer => message.answers.push(record),
                Section::Authority => message.authority.push(record),
                Section::Additional => {}
            }
        }
    }
    if offset != data.len() {
        return Err(WireError::InvalidResponse("trailing bytes"));
    }
    Ok(message)
}

type Aliases = HashMap<Name, (Name, u32)>;

/// Index answer CNAMEs, rejecting conflicting targets and prohibited owner data.
fn collect_aliases(records: &[Record]) -> Result<Aliases, WireError> {
    let mut aliases: Aliases = HashMap::new();
    for record in records {
        if let RecordData::Cname(target) = &record.data {
            let entry = aliases
                .entry(record.owner.clone())
                .or_insert_with(|| (target.clone(), record.ttl));
            if entry.0 != *target {
                return Err(WireError::InvalidRecord("conflicting CNAME targets"));
            }
            entry.1 = entry.1.min(record.ttl);
        }
    }
    if records.iter().any(|record| {
        !matches!(record.rtype, TYPE_CNAME | 24 | 25 | 30 | 46 | 47)
            && aliases.contains_key(&record.owner)
    }) {
        return Err(WireError::InvalidRecord(
            "CNAME and other data share an owner",
        ));
    }
    validate_alias_graph(&aliases)?;
    Ok(aliases)
}

/// Validate every path in `aliases`, memoizing lengths to bound work linearly.
fn validate_alias_graph(aliases: &Aliases) -> Result<(), WireError> {
    let mut lengths = HashMap::new();
    for owner in aliases.keys() {
        let mut current = owner;
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let mut length = 0;
        while let Some((target, _)) = aliases.get(current) {
            if let Some(&known_length) = lengths.get(current) {
                length = known_length;
                break;
            }
            if path.len() >= MAX_CNAME_CHAIN || !seen.insert(current) {
                return Err(WireError::CnameLoop);
            }
            path.push(current);
            current = target;
        }
        for owner in path.into_iter().rev() {
            length += 1;
            if length > MAX_CNAME_CHAIN {
                return Err(WireError::CnameLoop);
            }
            lengths.insert(owner.clone(), length);
        }
    }
    Ok(())
}

/// Follow `question` through `aliases`, returning terminal name, targets, and TTL.
fn follow_aliases(
    question: &Name,
    aliases: &Aliases,
) -> Result<(Name, Vec<String>, Option<u32>), WireError> {
    let mut current = question.clone();
    let mut seen = HashSet::from([current.clone()]);
    let mut chain = Vec::new();
    let mut ttl: Option<u32> = None;
    while let Some((target, alias_ttl)) = aliases.get(&current) {
        if chain.len() >= MAX_CNAME_CHAIN || !seen.insert(target.clone()) {
            return Err(WireError::CnameLoop);
        }
        ttl = Some(ttl.map_or(*alias_ttl, |ttl| ttl.min(*alias_ttl)));
        chain.push(target.to_text());
        current = target.clone();
    }
    Ok((current, chain, ttl))
}

/// Collect terminal `name`'s requested addresses in wire order and their minimum TTL.
fn collect_addresses(records: &[Record], name: &Name, qtype: u16) -> (Vec<String>, Option<u32>) {
    let mut ips = Vec::new();
    let mut seen = HashSet::new();
    let mut ttl: Option<u32> = None;
    for record in records {
        if record.owner != *name || record.rtype != qtype {
            continue;
        }
        if let RecordData::Address(ip) = &record.data {
            ttl = Some(ttl.map_or(record.ttl, |ttl| ttl.min(record.ttl)));
            if seen.insert(*ip) {
                ips.push(ip.to_string());
            }
        }
    }
    (ips, ttl)
}

/// Derive a negative TTL from enclosing authority SOAs; absent proof is uncacheable.
fn negative_ttl(message: &Message, name: &Name) -> Result<u32, WireError> {
    let ttl = message
        .authority
        .iter()
        .filter(|record| name.is_within(&record.owner))
        .filter_map(|record| match record.data {
            RecordData::Soa(ttl) => Some(ttl),
            _ => None,
        })
        .min();
    if ttl.is_none()
        && message.rcode == 0
        && message
            .authority
            .iter()
            .any(|record| record.rtype == TYPE_NS)
    {
        return Err(WireError::InvalidResponse(
            "referral is not a negative answer",
        ));
    }
    Ok(ttl.unwrap_or(0))
}

/// Build an answer for `question`/`qtype` only after complete message validation.
fn answer_message(message: Message, question: &Name, qtype: u16) -> Result<DnsAnswer, WireError> {
    if !matches!(message.rcode, 0 | 3) {
        return Err(WireError::Rcode(message.rcode));
    }
    let aliases = collect_aliases(&message.answers)?;
    let (terminal, cname_chain, alias_ttl) = follow_aliases(question, &aliases)?;
    let (ips, address_ttl) = collect_addresses(&message.answers, &terminal, qtype);
    if message.rcode == 3
        && message
            .answers
            .iter()
            .any(|record| record.owner == terminal && matches!(record.data, RecordData::Address(_)))
    {
        return Err(WireError::InvalidResponse(
            "NXDOMAIN contains terminal addresses",
        ));
    }
    let negative = ips.is_empty();
    let ttl = match address_ttl {
        Some(ttl) => ttl,
        None => negative_ttl(&message, &terminal)?,
    };
    Ok(DnsAnswer {
        ips,
        cname_chain,
        ttl_secs: alias_ttl.map_or(ttl, |alias_ttl| ttl.min(alias_ttl)),
        negative,
    })
}

/// Validate `data` against the sent ID, normalized name, and IN A/AAAA question.
///
/// All advertised records must fit and validate. NXDOMAIN/NODATA are successful
/// negative answers; other RCODEs, referrals, ambiguity, and malformed data fail.
/// TTL zero is preserved, including negative replies without an enclosing SOA.
pub fn parse_response(
    data: &[u8],
    expected_id: u16,
    expected_name: &str,
    expected_qtype: u16,
) -> Result<DnsAnswer, WireError> {
    validate_qtype(expected_qtype)?;
    let header = read_header(data)?;
    if header.id != expected_id {
        return Err(WireError::IdMismatch);
    }
    let expected_name = normalize_name(expected_name)?;
    let mut name_offsets = HashSet::new();
    let (question, next) = read_name(data, HEADER_LEN, data.len(), &mut name_offsets)?;
    if question != expected_name
        || read_u16(data, next)? != expected_qtype
        || read_u16(data, next + 2)? != CLASS_IN
    {
        return Err(WireError::QuestionMismatch);
    }
    let message = read_message(data, &header, next + 4, &mut name_offsets)?;
    answer_message(message, &question, expected_qtype)
}

/// Legacy packet-only wrapper returning IPs after strict structural validation.
///
/// Its expected ID/question are inferred from `data`, so it cannot authenticate
/// a reply to a sent query. Transports must use [`parse_response`] instead.
pub fn parse_ips(data: &[u8]) -> Result<Vec<String>, WireError> {
    let header = read_header(data)?;
    let (name, next) = read_name(data, HEADER_LEN, data.len(), &mut HashSet::new())?;
    let qtype = read_u16(data, next)?;
    Ok(parse_response(data, header.id, &name.to_text(), qtype)?.ips)
}

#[cfg(test)]
mod tests;
