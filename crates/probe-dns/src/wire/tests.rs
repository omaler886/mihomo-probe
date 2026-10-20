use super::*;

const ID: u16 = 0x1234;
const NAME: &str = "a.example";

/// Build a response for `name`/`qtype` with explicit sections and low RCODE.
fn response(
    name: &str,
    qtype: u16,
    answers: &[Vec<u8>],
    authority: &[Vec<u8>],
    additional: &[Vec<u8>],
    rcode: u16,
) -> Vec<u8> {
    let mut out = Vec::new();
    for value in [
        ID,
        0x8180 | rcode,
        1,
        answers.len() as u16,
        authority.len() as u16,
        additional.len() as u16,
    ] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend(encode_name(name).unwrap());
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    for section in [answers, authority, additional] {
        for record in section {
            out.extend_from_slice(record);
        }
    }
    out
}

/// Build a record from an already encoded `owner` and its fixed fields/RDATA.
fn raw_record(owner: &[u8], rtype: u16, class: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    let mut out = owner.to_vec();
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
    out
}

/// Build one IN record for textual `owner`, with the requested type, TTL, and RDATA.
fn record(owner: &str, rtype: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    raw_record(&encode_name(owner).unwrap(), rtype, CLASS_IN, ttl, rdata)
}

/// Build a valid SOA for `zone`, with independent RR TTL and MINIMUM.
fn soa(zone: &str, ttl: u32, minimum: u32) -> Vec<u8> {
    let mut rdata = encode_name("ns.example").unwrap();
    rdata.extend(encode_name("hostmaster.example").unwrap());
    for value in [1u32, 3600, 600, 86400, minimum] {
        rdata.extend_from_slice(&value.to_be_bytes());
    }
    record(zone, TYPE_SOA, ttl, &rdata)
}

/// Parse the default fixture question, returning the public validated answer.
fn parse(packet: &[u8]) -> Result<DnsAnswer, WireError> {
    parse_response(packet, ID, NAME, TYPE_A)
}

/// Return a positive response with two addresses and distinct TTLs.
fn positive_packet() -> Vec<u8> {
    response(
        NAME,
        TYPE_A,
        &[
            record(NAME, TYPE_A, 300, &[1, 2, 3, 4]),
            record(NAME, TYPE_A, 120, &[9, 8, 7, 6]),
        ],
        &[],
        &[],
        0,
    )
}

/// Check public output invariants for every accepted deterministic mutation.
fn assert_answer_invariants(answer: DnsAnswer) {
    assert_eq!(answer.negative, answer.ips.is_empty());
    assert!(answer.ips.len() <= MAX_RECORDS);
    assert!(answer.cname_chain.len() <= MAX_CNAME_CHAIN);
    assert!(answer.ttl_secs <= 0x7fff_ffff);
    for ip in answer.ips {
        assert!(ip.parse::<Ipv4Addr>().is_ok());
    }
}

/// Parse arbitrary `packet` bytes and check invariants whenever parsing succeeds.
fn assert_mutation_is_safe(packet: &[u8]) {
    if let Ok(answer) = parse(packet) {
        assert_answer_invariants(answer);
    }
}

/// Query encoding preserves the caller ID, RD, IN question, and exact ECS bytes.
#[test]
fn query_carries_rd_question_and_masked_ecs() {
    let packet = build_query("example.com.", TYPE_A, Some("192.0.2.255/24"), 25, ID).unwrap();
    assert_eq!(&packet[..12], &[0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 1]);
    let name = encode_name("example.com").unwrap();
    assert_eq!(&packet[12..12 + name.len()], name);
    let question_end = 12 + name.len() + 4;
    assert_eq!(&packet[question_end - 4..question_end], &[0, 1, 0, 1]);
    assert_eq!(
        &packet[question_end..],
        &[0, 0, 41, 16, 0, 0, 0, 0, 0, 0, 12, 0, 8, 0, 8, 0, 1, 25, 0, 192, 0, 2, 128]
    );
}

/// Omitting ECS produces no additional records and does not validate unused prefix.
#[test]
fn no_ecs_has_no_opt_record() {
    let packet = build_query(NAME, TYPE_AAAA, None, 255, ID).unwrap();
    assert_eq!(&packet[10..12], &[0, 0]);
    assert_eq!(packet.len(), 12 + encode_name(NAME).unwrap().len() + 4);
}

/// Every valid v4/v6 prefix retains exactly its subnet bits, including /0.
#[test]
fn ecs_masks_every_prefix_for_both_families() {
    for (address, max_prefix, family) in [
        ("255.255.255.255", 32u8, 1u16),
        ("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", 128, 2),
    ] {
        for prefix in 0..=max_prefix {
            let option = ecs_option(address, prefix).unwrap();
            let payload = &option[4..];
            assert_eq!(&payload[..2], &family.to_be_bytes());
            assert_eq!(&payload[2..4], &[prefix, 0]);
            assert_eq!(payload.len(), 4 + usize::from(prefix).div_ceil(8));
            validate_ecs(payload).unwrap();
            if !prefix.is_multiple_of(8) {
                assert_eq!(*payload.last().unwrap(), 0xff << (8 - prefix % 8));
            }
        }
    }
}

/// Impossible prefixes and malformed CIDR suffixes must not silently truncate.
#[test]
fn ecs_rejects_invalid_addresses_and_prefixes() {
    for (address, prefix) in [
        ("192.0.2.1", 33),
        ("2001:db8::1", 129),
        ("192.0.2.1/33", 24),
        ("2001:db8::1/129", 64),
        ("192.0.2.1/no", 24),
        ("192.0.2.1/", 24),
        ("192.0.2.1/24/1", 24),
        ("not-an-ip", 0),
        ("", 0),
    ] {
        assert_eq!(ecs_option(address, prefix), Err(WireError::InvalidEcs));
    }
}

/// IDNA, case folding, Unicode separators, and a single absolute dot are equivalent.
#[test]
fn names_support_idna_absolute_forms_and_numeric_labels() {
    for (unicode, ascii) in [
        ("\u{4f8b}\u{3048}.jp.", "xn--r8jz45g.jp"),
        (
            "B\u{fc}CHER\u{3002}Example\u{ff0e}",
            "xn--bcher-kva.example",
        ),
        ("bu\u{308}cher.example", "xn--bcher-kva.example"),
        ("EXAMPLE.COM.", "example.com"),
        ("127.1", "127.1"),
        ("_service.example", "_service.example"),
    ] {
        assert_eq!(encode_name(unicode).unwrap(), encode_name(ascii).unwrap());
    }
    assert_eq!(encode_name(".").unwrap(), vec![0]);
}

/// Empty labels, URL syntax, controls, and repeated trailing dots are rejected.
#[test]
fn invalid_names_do_not_become_urls_or_partial_hostnames() {
    for name in [
        "",
        "..",
        ".example",
        "a..example",
        "a.example..",
        "a/b",
        "a\\b",
        "a@b",
        "a?b",
        "a#b",
        "a:b",
        "a%2eb",
        "a b",
        "a\0b",
        "a\nb",
        "\u{200d}.example",
    ] {
        assert!(encode_name(name).is_err(), "{name:?}");
    }
}

/// DNS label and expanded wire-name limits are checked after IDNA conversion.
#[test]
fn name_lengths_obey_63_byte_labels_and_255_byte_names() {
    assert!(encode_name(&"x".repeat(63)).is_ok());
    assert!(encode_name(&"x".repeat(64)).is_err());
    let longest = [63, 63, 63, 61].map(|length| "x".repeat(length)).join(".");
    assert_eq!(encode_name(&longest).unwrap().len(), 255);
    assert_eq!(
        encode_name(&format!("{longest}x")),
        Err(WireError::NameTooLong)
    );
    assert!(encode_name(&format!("{}.example", "\u{fc}".repeat(64))).is_err());
}

/// The decoder accepts 127 short labels but rejects an expanded 256-byte name.
#[test]
fn decoded_name_limits_are_independent_of_pointer_count() {
    let longest = vec!["x"; 127].join(".");
    let mut bytes = vec![0; HEADER_LEN];
    bytes.extend(encode_name(&longest).unwrap());
    assert_eq!(
        read_name(&bytes, HEADER_LEN, bytes.len(), &mut HashSet::new())
            .unwrap()
            .0
            .to_text(),
        longest
    );
    bytes.splice(HEADER_LEN..HEADER_LEN, [1, b'y']);
    assert_eq!(
        read_name(&bytes, HEADER_LEN, bytes.len(), &mut HashSet::new()),
        Err(WireError::NameTooLong)
    );
}

/// Responses return only the requested owner's family and a conservative TTL.
#[test]
fn positive_answers_filter_owners_families_and_dedupe_stably() {
    let packet = response(
        NAME,
        TYPE_A,
        &[
            record("other.example", TYPE_A, 1, &[8, 8, 8, 8]),
            record(NAME, TYPE_AAAA, 2, &[0; 16]),
            record(NAME, TYPE_A, 300, &[1, 2, 3, 4]),
            record(NAME, TYPE_A, 90, &[9, 9, 9, 9]),
            record(NAME, TYPE_A, 60, &[1, 2, 3, 4]),
        ],
        &[],
        &[],
        0,
    );
    assert_eq!(
        parse(&packet).unwrap(),
        DnsAnswer {
            ips: vec!["1.2.3.4".into(), "9.9.9.9".into()],
            cname_chain: vec![],
            ttl_secs: 60,
            negative: false,
        }
    );
    assert_eq!(parse_ips(&packet).unwrap(), vec!["1.2.3.4", "9.9.9.9"]);
}

/// IPv6 responses retain canonical address formatting and TTL metadata.
#[test]
fn aaaa_answers_use_the_aaaa_question() {
    let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
    let packet = response(
        NAME,
        TYPE_AAAA,
        &[record(NAME, TYPE_AAAA, 90, &ip.octets())],
        &[],
        &[],
        0,
    );
    let answer = parse_response(&packet, ID, NAME, TYPE_AAAA).unwrap();
    assert_eq!(answer.ips, ["2001:db8::1"]);
    assert_eq!(answer.ttl_secs, 90);
}

/// Matching is case-insensitive and compares IDNA equivalents, not raw user bytes.
#[test]
fn question_matching_normalizes_idna_and_absolute_names() {
    let name = "xn--r8jz45g.jp";
    let mut packet = response(
        name,
        TYPE_A,
        &[record(name, TYPE_A, 5, &[1, 1, 1, 1])],
        &[],
        &[],
        0,
    );
    packet[13..24].make_ascii_uppercase();
    let answer = parse_response(&packet, ID, "\u{4f8b}\u{3048}.jp.", TYPE_A).unwrap();
    assert_eq!(answer.ips, ["1.1.1.1"]);
}

/// QR, opcode, TC, reserved flags, ID, question count, name, type, and class are bound.
#[test]
fn mismatched_response_envelopes_are_rejected() {
    let packet = positive_packet();
    for (offset, bytes) in [
        (2, [0x01, 0x80]),
        (2, [0x89, 0x80]),
        (2, [0x83, 0x80]),
        (2, [0x81, 0xc0]),
        (4, [0, 0]),
        (4, [0, 2]),
    ] {
        let mut malformed = packet.clone();
        malformed[offset..offset + 2].copy_from_slice(&bytes);
        assert!(parse(&malformed).is_err());
    }
    assert_eq!(
        parse_response(&packet, ID + 1, NAME, TYPE_A),
        Err(WireError::IdMismatch)
    );
    assert_eq!(
        parse_response(&packet, ID, "b.example", TYPE_A),
        Err(WireError::QuestionMismatch)
    );
    assert_eq!(
        parse_response(&packet, ID, NAME, TYPE_AAAA),
        Err(WireError::QuestionMismatch)
    );
    let mut wrong_class = packet;
    let class_offset = HEADER_LEN + encode_name(NAME).unwrap().len() + 2;
    wrong_class[class_offset..class_offset + 2].copy_from_slice(&3u16.to_be_bytes());
    assert_eq!(parse(&wrong_class), Err(WireError::QuestionMismatch));
}

/// Reserved extended label tags are errors even when sufficient bytes follow.
#[test]
fn reserved_label_tags_are_not_lengths() {
    for tag in [0x40, 0x41, 0x80, 0xbf] {
        let mut packet = positive_packet();
        packet[12] = tag;
        packet.extend_from_slice(&[0; 256]);
        assert!(matches!(parse(&packet), Err(WireError::BadLabel(_))));
    }
}

/// Self, forward, header, and out-of-packet pointers are never accepted.
#[test]
fn invalid_compression_targets_fail_explicitly() {
    for pointer in [[0xc0, 12], [0xc0, 14], [0xc0, 0], [0xff, 0xff]] {
        let mut packet = response(NAME, TYPE_A, &[], &[], &[], 0);
        packet.splice(12..23, pointer);
        assert_eq!(parse(&packet), Err(WireError::InvalidPointer));
    }
}

/// A zero byte in QTYPE is not a root-name occurrence usable for compression.
#[test]
fn reject_hidden_pointer_into_a_non_name_field() {
    let qtype_offset = HEADER_LEN + encode_name(NAME).unwrap().len();
    let bad_owner = (0xc000 | qtype_offset as u16).to_be_bytes();
    let packet = response(
        NAME,
        TYPE_A,
        &[record(NAME, TYPE_A, 60, &[1, 2, 3, 4])],
        &[],
        &[raw_record(&bad_owner, TYPE_A, CLASS_IN, 60, &[5, 6, 7, 8])],
        0,
    );
    assert_eq!(parse(&packet), Err(WireError::InvalidPointer));
}

/// Cyclic unrelated answer CNAMEs must not be hidden by an earlier valid address.
#[test]
fn reject_hidden_cname_cycle_outside_the_question_chain() {
    let packet = response(
        NAME,
        TYPE_A,
        &[
            record(NAME, TYPE_A, 60, &[1, 2, 3, 4]),
            record(
                "b.example",
                TYPE_CNAME,
                60,
                &encode_name("c.example").unwrap(),
            ),
            record(
                "c.example",
                TYPE_CNAME,
                60,
                &encode_name("b.example").unwrap(),
            ),
        ],
        &[],
        &[],
        0,
    );
    assert_eq!(parse(&packet), Err(WireError::CnameLoop));
}

/// CNAME exclusivity applies to TXT/NS/etc. too, except permitted DNSSEC metadata.
#[test]
fn reject_hidden_cname_and_txt_owner_conflict() {
    let packet = response(
        NAME,
        TYPE_A,
        &[
            record(NAME, TYPE_CNAME, 60, &encode_name("b.example").unwrap()),
            record(NAME, 16, 60, &[1, b'x']),
            record("b.example", TYPE_A, 60, &[1, 2, 3, 4]),
        ],
        &[],
        &[],
        0,
    );
    assert!(matches!(parse(&packet), Err(WireError::InvalidRecord(_))));
}

/// A backward pointer may still form a label/pointer cycle and must be detected.
#[test]
fn backward_compression_cycles_and_long_walks_are_bounded() {
    let mut packet = response(NAME, TYPE_A, &[], &[], &[], 0);
    packet.splice(12..23, [1, b'x', 0xc0, 12]);
    assert_eq!(parse(&packet), Err(WireError::CompressionLoop));
    let mut bytes = vec![0; HEADER_LEN + 1];
    let mut previous = HEADER_LEN;
    for _ in 0..MAX_NAME_STEPS {
        let position = bytes.len();
        bytes.extend_from_slice(&(0xc000 | previous as u16).to_be_bytes());
        previous = position;
    }
    let mut boundaries = (HEADER_LEN + 1..bytes.len())
        .step_by(2)
        .collect::<HashSet<_>>();
    boundaries.insert(HEADER_LEN);
    assert_eq!(
        read_name(&bytes, previous, bytes.len(), &mut boundaries),
        Err(WireError::CompressionLoop)
    );
}

/// Compressed CNAME RDATA and owners may point to earlier suffixes or pointers.
#[test]
fn compressed_cname_targets_and_nested_owner_pointers_are_resolved() {
    let mut packet = response(NAME, TYPE_A, &[], &[], &[], 0);
    packet[6..8].copy_from_slice(&3u16.to_be_bytes());
    let target_offset = packet.len() + 12;
    packet.extend(raw_record(
        &[0xc0, 12],
        TYPE_CNAME,
        CLASS_IN,
        45,
        &[1, b'b', 0xc0, 14],
    ));
    let second_owner = packet.len();
    packet.extend(raw_record(
        &(0xc000 | target_offset as u16).to_be_bytes(),
        TYPE_A,
        CLASS_IN,
        120,
        &[1, 2, 3, 4],
    ));
    packet.extend(raw_record(
        &(0xc000 | second_owner as u16).to_be_bytes(),
        TYPE_A,
        CLASS_IN,
        30,
        &[9, 9, 9, 9],
    ));
    let answer = parse(&packet).unwrap();
    assert_eq!(answer.cname_chain, ["b.example"]);
    assert_eq!(answer.ips, ["1.2.3.4", "9.9.9.9"]);
    assert_eq!(answer.ttl_secs, 30);
}

/// CNAME graph order follows links, not the order records happened to arrive in.
#[test]
fn out_of_order_cname_chain_includes_all_alias_ttls() {
    let packet = response(
        NAME,
        TYPE_A,
        &[
            record("c.example", TYPE_A, 300, &[1, 2, 3, 4]),
            record(
                "b.example",
                TYPE_CNAME,
                40,
                &encode_name("c.example").unwrap(),
            ),
            record(NAME, TYPE_CNAME, 80, &encode_name("b.example").unwrap()),
            record(NAME, TYPE_CNAME, 20, &encode_name("b.example").unwrap()),
        ],
        &[],
        &[],
        0,
    );
    let answer = parse(&packet).unwrap();
    assert_eq!(answer.cname_chain, ["b.example", "c.example"]);
    assert_eq!(answer.ttl_secs, 20);
}

/// Cyclic, contradictory, and over-budget CNAME graphs cannot yield partial success.
#[test]
fn invalid_cname_graphs_are_rejected() {
    let alias = record(NAME, TYPE_CNAME, 30, &encode_name("b.example").unwrap());
    for other in [
        record("b.example", TYPE_CNAME, 30, &encode_name(NAME).unwrap()),
        record(NAME, TYPE_CNAME, 30, &encode_name("c.example").unwrap()),
        record(NAME, TYPE_A, 30, &[1, 2, 3, 4]),
    ] {
        assert!(parse(&response(
            NAME,
            TYPE_A,
            &[alias.clone(), other],
            &[],
            &[],
            0
        ))
        .is_err());
    }
    let mut records = Vec::new();
    let mut owner = NAME.to_string();
    for index in 0..=MAX_CNAME_CHAIN {
        let target = format!("alias{index}.example");
        records.push(record(
            &owner,
            TYPE_CNAME,
            30,
            &encode_name(&target).unwrap(),
        ));
        owner = target;
    }
    assert_eq!(
        parse(&response(NAME, TYPE_A, &records, &[], &[], 0)),
        Err(WireError::CnameLoop)
    );
}

/// Zero and high-bit TTLs are uncacheable; neither addresses nor aliases inflate them.
#[test]
fn zero_and_high_bit_positive_ttls_remain_zero() {
    for ttl in [0, 0x8000_0000, u32::MAX] {
        let packet = response(
            NAME,
            TYPE_A,
            &[record(NAME, TYPE_A, ttl, &[1, 2, 3, 4])],
            &[],
            &[],
            0,
        );
        assert_eq!(parse(&packet).unwrap().ttl_secs, 0);
        let packet = response(
            NAME,
            TYPE_A,
            &[
                record(NAME, TYPE_CNAME, ttl, &encode_name("b.example").unwrap()),
                record("b.example", TYPE_A, 300, &[1, 2, 3, 4]),
            ],
            &[],
            &[],
            0,
        );
        assert_eq!(parse(&packet).unwrap().ttl_secs, 0);
    }
}

/// NXDOMAIN and NODATA use min(SOA TTL, MINIMUM), preserving zero/high-bit values.
#[test]
fn negative_answers_derive_ttls_from_soa() {
    for rcode in [0, 3] {
        for (ttl, minimum, expected) in
            [(300, 60, 60), (30, 600, 30), (0, 60, 0), (60, u32::MAX, 0)]
        {
            let packet = response(
                NAME,
                TYPE_A,
                &[],
                &[soa("example", ttl, minimum)],
                &[],
                rcode,
            );
            let answer = parse(&packet).unwrap();
            assert!(answer.negative);
            assert!(answer.ips.is_empty());
            assert_eq!(answer.ttl_secs, expected);
        }
    }
}

/// A negative alias answer uses only SOAs enclosing its terminal target and alias TTL.
#[test]
fn negative_aliases_respect_terminal_zone_and_alias_ttl() {
    for rcode in [0, 3] {
        let packet = response(
            NAME,
            TYPE_A,
            &[record(
                NAME,
                TYPE_CNAME,
                25,
                &encode_name("gone.other").unwrap(),
            )],
            &[soa("example", 1, 1), soa("other", 300, 60)],
            &[],
            rcode,
        );
        let answer = parse(&packet).unwrap();
        assert!(answer.negative);
        assert_eq!(answer.cname_chain, ["gone.other"]);
        assert_eq!(answer.ttl_secs, 25);
    }
}

/// Empty negatives lacking an applicable authority SOA must never acquire a cache TTL.
#[test]
fn no_soa_or_unrelated_soa_means_zero_negative_ttl() {
    for rcode in [0, 3] {
        for authority in [vec![], vec![soa("ample", 300, 60)]] {
            let packet = response(NAME, TYPE_A, &[], &authority, &[], rcode);
            assert_eq!(parse(&packet).unwrap().ttl_secs, 0);
        }
    }
}

/// A bare NS referral and contradictory NXDOMAIN addresses are failures, not negatives.
#[test]
fn referrals_and_positive_nxdomain_are_not_misleading_successes() {
    let referral = response(
        NAME,
        TYPE_A,
        &[],
        &[record(
            "example",
            TYPE_NS,
            30,
            &encode_name("ns.example").unwrap(),
        )],
        &[],
        0,
    );
    assert!(matches!(
        parse(&referral),
        Err(WireError::InvalidResponse(_))
    ));
    let contradictory = response(
        NAME,
        TYPE_A,
        &[record(NAME, TYPE_A, 30, &[1, 2, 3, 4])],
        &[],
        &[],
        3,
    );
    assert!(matches!(
        parse(&contradictory),
        Err(WireError::InvalidResponse(_))
    ));
}

/// Ordinary and EDNS extended failure RCODEs never produce negative answers.
#[test]
fn non_negative_rcodes_remain_errors() {
    for rcode in [1, 2, 4, 5, 15] {
        assert_eq!(
            parse(&response(NAME, TYPE_A, &[], &[], &[], rcode)),
            Err(WireError::Rcode(rcode))
        );
    }
    let opt = raw_record(&[0], OPT_TYPE, 1232, 0x0100_0000, &[]);
    assert_eq!(
        parse(&response(NAME, TYPE_A, &[], &[], &[opt], 0)),
        Err(WireError::Rcode(16))
    );
}

/// Known malformed RDATA is rejected even behind an otherwise valid A answer.
#[test]
fn malformed_known_records_are_not_silently_skipped() {
    for (rtype, rdata) in [
        (TYPE_A, vec![1, 2, 3]),
        (TYPE_A, vec![1; 5]),
        (TYPE_AAAA, vec![0; 15]),
        (TYPE_AAAA, vec![0; 17]),
        (TYPE_CNAME, vec![0x61, 0x62, 0x63]),
        (TYPE_CNAME, vec![0, 0]),
        (TYPE_SOA, vec![0, 0, 1]),
        (TYPE_NS, vec![]),
        (12, vec![1]),
        (15, vec![0; 2]),
        (33, vec![0; 6]),
        (16, vec![]),
        (16, vec![3, b'a']),
    ] {
        let packet = response(
            NAME,
            TYPE_A,
            &[
                record(NAME, TYPE_A, 60, &[1, 2, 3, 4]),
                record("other.example", rtype, 60, &rdata),
            ],
            &[],
            &[],
            0,
        );
        assert!(parse(&packet).is_err(), "type {rtype}, {rdata:?}");
    }
}

/// Authority and additional records receive the same class/length checks as answers.
#[test]
fn every_section_is_validated_before_addresses_are_returned() {
    let good = record(NAME, TYPE_A, 60, &[1, 2, 3, 4]);
    for bad in [
        record("other.example", TYPE_AAAA, 60, &[0; 15]),
        raw_record(&[0], TYPE_A, 3, 60, &[1, 2, 3, 4]),
        record("other.example", TYPE_SOA, 60, &[0]),
    ] {
        assert!(parse(&response(
            NAME,
            TYPE_A,
            std::slice::from_ref(&good),
            std::slice::from_ref(&bad),
            &[],
            0
        ))
        .is_err());
        assert!(parse(&response(
            NAME,
            TYPE_A,
            std::slice::from_ref(&good),
            &[],
            &[bad],
            0
        ))
        .is_err());
    }
}

/// Unknown IN records are opaque but still honor advertised boundaries and EOF.
#[test]
fn unknown_records_and_trailing_bytes_obey_packet_boundaries() {
    let unknown = record("example", 65000, 5, &[0xff; 5]);
    let mut packet = response(
        NAME,
        TYPE_A,
        &[record(NAME, TYPE_A, 60, &[1, 2, 3, 4])],
        &[],
        &[unknown],
        0,
    );
    assert_eq!(parse(&packet).unwrap().ips, ["1.2.3.4"]);
    packet.push(0);
    assert!(matches!(parse(&packet), Err(WireError::InvalidResponse(_))));
    packet.truncate(packet.len() - 2);
    assert_eq!(parse(&packet), Err(WireError::Truncated));
}

/// A CNAME field cannot borrow its terminator or pointer byte from the next record.
#[test]
fn name_rdata_respects_its_own_rdlength() {
    let packet = response(
        NAME,
        TYPE_A,
        &[
            record(NAME, TYPE_CNAME, 30, &[1, b'b']),
            record("b", TYPE_A, 60, &[1, 2, 3, 4]),
        ],
        &[],
        &[],
        0,
    );
    assert!(parse(&packet).is_err());
    let packet = response(
        NAME,
        TYPE_A,
        &[record(NAME, TYPE_CNAME, 30, &[0xc0])],
        &[],
        &[],
        0,
    );
    assert_eq!(parse(&packet), Err(WireError::Truncated));
}

/// EDNS/ECS framing rejects malformed, duplicate, misplaced, and unsupported data.
#[test]
fn opt_and_ecs_response_validation_is_strict() {
    let good = raw_record(
        &[0],
        OPT_TYPE,
        1232,
        0x8000,
        &ecs_option("192.0.2.255", 25).unwrap(),
    );
    parse(&response(
        NAME,
        TYPE_A,
        &[],
        &[],
        std::slice::from_ref(&good),
        0,
    ))
    .unwrap();
    assert!(parse(&response(
        NAME,
        TYPE_A,
        &[],
        &[],
        &[good.clone(), good.clone()],
        0
    ))
    .is_err());
    assert!(parse(&response(NAME, TYPE_A, &[good], &[], &[], 0)).is_err());
    for (owner, ttl, options) in [
        (vec![1, b'x', 0], 0, vec![]),
        (vec![0], 0x0001_0000, vec![]),
        (vec![0], 1, vec![]),
        (vec![0], 0, vec![0, 8, 0]),
        (vec![0], 0, vec![0, 8, 0, 5, 0, 1, 1, 0, 255]),
        (vec![0], 0, vec![0, 8, 0, 4, 0, 1, 33, 0]),
        (vec![0], 0, vec![0, 8, 0, 4, 0, 3, 0, 0]),
    ] {
        let opt = raw_record(&owner, OPT_TYPE, 1232, ttl, &options);
        assert!(parse(&response(NAME, TYPE_A, &[], &[], &[opt], 0)).is_err());
    }
    let mut duplicate = ecs_option("192.0.2.1", 24).unwrap();
    duplicate.extend_from_slice(&duplicate.clone());
    assert!(read_opt(&duplicate, 0).is_err());
}

/// A SOA may compress both names, but its integer portion must be exactly 20 bytes.
#[test]
fn compressed_soa_names_and_exact_integer_lengths_are_checked() {
    let mut rdata = vec![0xc0, 12, 0xc0, 12];
    for value in [1u32, 2, 3, 4, 90] {
        rdata.extend_from_slice(&value.to_be_bytes());
    }
    let packet = response(
        NAME,
        TYPE_A,
        &[],
        &[record("example", TYPE_SOA, 120, &rdata)],
        &[],
        3,
    );
    assert_eq!(parse(&packet).unwrap().ttl_secs, 90);
    for malformed in [
        &rdata[..rdata.len() - 1],
        &[rdata.as_slice(), &[0]].concat(),
    ] {
        let packet = response(
            NAME,
            TYPE_A,
            &[],
            &[record("example", TYPE_SOA, 120, malformed)],
            &[],
            3,
        );
        assert!(parse(&packet).is_err());
    }
}

/// Record budgets cover the sum of all sections, while the exact limit is allowed.
#[test]
fn record_count_and_message_size_limits_are_enforced() {
    let records = vec![raw_record(&[0], 65000, CLASS_IN, 0, &[]); MAX_RECORDS];
    parse(&response(NAME, TYPE_A, &records, &[], &[], 0)).unwrap();
    let mut over_count = response(NAME, TYPE_A, &[], &[], &[], 0);
    over_count[6..8].copy_from_slice(&(MAX_RECORDS as u16).to_be_bytes());
    over_count[10..12].copy_from_slice(&1u16.to_be_bytes());
    assert_eq!(parse(&over_count), Err(WireError::RecordLimit));
    let base = response(NAME, TYPE_A, &[], &[], &[], 0).len() + 11;
    let padding = raw_record(
        &[0],
        65000,
        CLASS_IN,
        0,
        &vec![0; MAX_DNS_MESSAGE_SIZE - base],
    );
    let mut packet = response(NAME, TYPE_A, &[], &[], &[padding], 0);
    assert_eq!(packet.len(), MAX_DNS_MESSAGE_SIZE);
    parse(&packet).unwrap();
    packet.push(0);
    assert_eq!(parse(&packet), Err(WireError::MessageTooLarge));
}

/// Every strict prefix of a valid positive or negative response must fail completely.
#[test]
fn deterministic_all_byte_truncations_never_return_partial_answers() {
    let negative = response(NAME, TYPE_A, &[], &[soa("example", 300, 60)], &[], 3);
    for packet in [positive_packet(), negative] {
        for length in 0..packet.len() {
            assert!(
                parse(&packet[..length]).is_err(),
                "accepted length {length}"
            );
        }
    }
}

/// Fixed-seed byte mutations cover header counts, lengths, labels, pointers, and RDATA.
#[test]
fn deterministic_parser_mutations_never_panic_or_break_answer_invariants() {
    let alias = response(
        NAME,
        TYPE_A,
        &[
            record(NAME, TYPE_CNAME, 30, &encode_name("b.example").unwrap()),
            record("b.example", TYPE_A, 60, &[1, 2, 3, 4]),
        ],
        &[soa("example", 120, 60)],
        &[],
        0,
    );
    for original in [positive_packet(), alias] {
        for index in 0..original.len() {
            for byte in [0, 1, 63, 64, 127, 128, 191, 192, 255] {
                let mut mutated = original.clone();
                mutated[index] = byte;
                assert_mutation_is_safe(&mutated);
            }
        }
    }
    let mut state = 0x6a09_e667u32;
    for length in 0..512 {
        let mut packet = Vec::with_capacity(length);
        for _ in 0..length {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            packet.push((state >> 24) as u8);
        }
        assert_mutation_is_safe(&packet);
    }
}

/// Bounded Unicode inputs either reject cleanly or round-trip through query matching.
#[test]
fn deterministic_unicode_names_round_trip_without_panics() {
    for scalar in (0..=0x10_ffff).step_by(7919).filter_map(char::from_u32) {
        for name in [format!("{scalar}.example"), format!("ex{scalar}ample.")] {
            if let Ok(mut packet) = build_query(&name, TYPE_A, None, 0, ID) {
                packet[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
                let answer = parse_response(&packet, ID, &name, TYPE_A).unwrap();
                assert!(answer.negative);
                assert_eq!(answer.ttl_secs, 0);
            }
        }
    }
}

/// Existing binary fixtures retain their bytes; obsolete acceptance assertions change.
mod fixtures {
    use super::*;

    const A: &[u8] = include_bytes!("../../fixtures/dns-packets/a-answer-compressed.bin");
    const AAAA: &[u8] = include_bytes!("../../fixtures/dns-packets/aaaa-answer.bin");
    const BAD_CNAME_OWNER: &[u8] = include_bytes!("../../fixtures/dns-packets/cname-then-a.bin");
    const NXDOMAIN: &[u8] = include_bytes!("../../fixtures/dns-packets/nxdomain.bin");
    const TRUNCATED: &[u8] = include_bytes!("../../fixtures/dns-packets/truncated-answer.bin");
    const POINTER_LOOP: &[u8] =
        include_bytes!("../../fixtures/dns-packets/pointer-loop-question.bin");
    const BAD_LABEL: &[u8] = include_bytes!("../../fixtures/dns-packets/bad-label-overrun.bin");
    const SHORT_HEADER: &[u8] = include_bytes!("../../fixtures/dns-packets/short-header.bin");

    /// Known-good compressed A and AAAA fixtures still resolve without modification.
    #[test]
    fn valid_binary_answers_remain_compatible() {
        assert_eq!(parse_ips(A).unwrap(), ["1.2.3.4"]);
        assert_eq!(parse_ips(AAAA).unwrap(), ["2001:db8::1"]);
    }

    /// The historical CNAME fixture puts A at the alias owner, violating exclusivity.
    #[test]
    fn historical_misowned_cname_fixture_is_rejected() {
        assert_eq!(
            parse_ips(BAD_CNAME_OWNER),
            Err(WireError::InvalidRecord(
                "CNAME and other data share an owner"
            ))
        );
    }

    /// NXDOMAIN without SOA is an explicit uncacheable negative, not SERVFAIL.
    #[test]
    fn historical_nxdomain_fixture_is_an_uncacheable_negative() {
        let answer = parse_response(NXDOMAIN, ID, "gone.example", TYPE_A).unwrap();
        assert!(answer.negative);
        assert_eq!(answer.ttl_secs, 0);
        assert!(parse_ips(NXDOMAIN).unwrap().is_empty());
    }

    /// The old self-pointer success assertion is replaced with explicit rejection.
    #[test]
    fn historical_loop_and_malformed_fixtures_fail() {
        assert_eq!(parse_ips(POINTER_LOOP), Err(WireError::InvalidPointer));
        assert_eq!(parse_ips(TRUNCATED), Err(WireError::Truncated));
        assert!(matches!(parse_ips(BAD_LABEL), Err(WireError::BadLabel(_))));
        assert_eq!(parse_ips(SHORT_HEADER), Err(WireError::Truncated));
    }
}
