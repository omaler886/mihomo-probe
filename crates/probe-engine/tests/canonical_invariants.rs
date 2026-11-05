//! Canonical 产物不变量（G-1，workstreams/24；验收写法沿用 19_ACCEPTANCE_MAP）。
//!
//! 每条用例钉住一条行为：同输入 N 轮字节相同（幂等发布零 PATCH 的前提）、
//! 改名不分裂条目也不移动 digest、digest 只随连接参数变化、只动 name、
//! 排序抗抖动、冲突后缀唯一且稳定、组拓扑成员剔除、空输入不产空产物。
//! digest 的跨实现钉值由 Python `_proxy_digest` 直接算出（identity.rs 同款
//! 做法），保证两条实现同字节串。

use std::collections::BTreeMap;

use probe_engine::canonical::{
    build_canonical, NodeMeta, RenameConfig, SourceInput, DEFAULT_COLLISION_SUFFIX,
    DEFAULT_TEMPLATE,
};
use serde_json::{json, Value};

fn proxy(name: &str, server: &str, port: i64) -> Value {
    json!({"name": name, "type": "vless", "server": server, "port": port,
           "uuid": "u-1", "tls": true})
}

fn meta_map(entries: &[(&str, NodeMeta)]) -> BTreeMap<String, NodeMeta> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn sources<'a>(
    defs: &[(&'a str, &'a [Value], &'a BTreeMap<String, NodeMeta>)],
) -> Vec<SourceInput<'a>> {
    defs.iter()
        .map(|(key, proxies, meta)| SourceInput {
            key: key.to_string(),
            proxies,
            groups: &[],
            meta,
        })
        .collect()
}

fn no_groups<'a>(key: &'a str, proxies: &'a [Value]) -> SourceInput<'a> {
    SourceInput {
        key: key.to_string(),
        proxies,
        groups: &[],
        meta: empty_meta(),
    }
}

static EMPTY_META: std::sync::OnceLock<BTreeMap<String, NodeMeta>> = std::sync::OnceLock::new();

fn empty_meta() -> &'static BTreeMap<String, NodeMeta> {
    EMPTY_META.get_or_init(BTreeMap::new)
}

#[test]
fn the_same_input_builds_byte_identical_artifacts() {
    let proxies = [
        proxy("HK | a", "s1.example.com", 443),
        proxy("US | b", "s2.example.com", 443),
    ];
    let src = [no_groups("air", &proxies)];
    let first = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    let second = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    assert_eq!(first.rendered.content(), second.rendered.content());
    assert_eq!(first.meta.digest, second.meta.digest);
    assert_eq!(first.meta.content_sha256, second.meta.content_sha256);
    assert_eq!(first.meta.count, 2);
}

#[test]
fn a_rename_keeps_one_entry_and_records_the_alias_source() {
    // 同一节点（同指纹）两个显示名、两个来源：一条 canonical 记录 + 别名来源
    // + 一条 renamed 审计记录（§11.1-3/§11.3-1 的 canonical 侧写法）。
    let a = [proxy("HK | via-air", "s.example.com", 443)];
    let b = [proxy("HK | via-eps1", "s.example.com", 443)];
    let src = [no_groups("air", &a), no_groups("eps1", &b)];
    let artifact = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    assert_eq!(artifact.meta.count, 1);
    assert!(artifact.rendered.content().contains("via-air"));
    assert!(!artifact.rendered.content().contains("via-eps1"));
    let aliases = artifact
        .meta
        .source_aliases
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(aliases, vec!["eps1".to_string()]);
    assert!(artifact.meta.renames.iter().any(|r| r.renamed));
}

#[test]
fn the_digest_ignores_renames_and_moves_only_with_connection_changes() {
    let round1 = [proxy("HK | a", "s.example.com", 443)];
    let round2 = [proxy("HK | renamed", "s.example.com", 443)];
    let round3 = [proxy("HK | renamed", "s.example.com", 8443)];
    let build = |p: &[Value]| {
        let src = [no_groups("air", p)];
        build_canonical(&src, &RenameConfig::default())
            .unwrap()
            .unwrap()
    };
    let one = build(&round1);
    let two = build(&round2);
    let three = build(&round3);
    assert_eq!(one.meta.digest, two.meta.digest, "纯改名不动 digest");
    assert_ne!(
        one.meta.digest, three.meta.digest,
        "连接参数变化必须移动 digest"
    );
    // 改名仍反映在产物内容里（订阅方看到新名字）
    assert_ne!(one.rendered.content(), two.rendered.content());
}

#[test]
fn the_canonical_digest_matches_the_python_implementation() {
    // Python: json.dumps([{连接字段}...], sort_keys=True, ensure_ascii=False)
    //         → sha256 hexdigest()[:32] == fd73535266f40185b452a0c01485361f
    //         （2026-11-06 由 mihomo_test/canonical_export._proxy_digest 生成）
    let proxies = [json!({"name": "a", "type": "vless",
                           "server": "s.example.com", "port": 443, "uuid": "u-1"})];
    let src = [no_groups("air", &proxies)];
    let artifact = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    assert_eq!(artifact.meta.digest, "fd73535266f40185b452a0c01485361f");
}

#[test]
fn renaming_touches_only_the_name_field() {
    let input = json!({"name": "[HK] raw", "type": "vless", "server": "s",
                       "port": 443, "uuid": "u-1", "tls": true, "ws-opts": {"path": "/w"}});
    let proxies = [input.clone()];
    let src = [no_groups("air", &proxies)];
    let artifact = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    let content = artifact.rendered.content();
    let doc: Value = serde_json::from_str(content).unwrap();
    let out = &doc["proxies"][0];
    let strip_name = |v: &Value| {
        let mut o = v.as_object().unwrap().clone();
        o.remove("name");
        o
    };
    assert_eq!(strip_name(out), strip_name(&input), "除 name 外逐字段不变");
    assert_ne!(out["name"], input["name"]);
}

#[test]
fn sort_order_is_stable_under_shuffled_input() {
    let plain = RenameConfig::default();
    let p1 = [proxy("b", "s2", 443), proxy("a", "s1", 443)];
    let p2 = [proxy("a", "s1", 443), proxy("b", "s2", 443)];
    let one = [no_groups("air", &p1)];
    let two = [no_groups("air", &p2)];
    let a = build_canonical(&one, &plain).unwrap().unwrap();
    let b = build_canonical(&two, &plain).unwrap().unwrap();
    assert_eq!(a.rendered.content(), b.rendered.content());
}

#[test]
fn collision_suffixes_keep_names_unique_and_stable() {
    // 两个不同指纹、剥 tag 后同名：第二个追加默认后缀 " #2"，重跑稳定
    let p = [proxy("[HK] same", "s1", 443), proxy("[HK] same", "s2", 443)];
    let src = [no_groups("air", &p)];
    let one = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    let two = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    assert_eq!(one.rendered.content(), two.rendered.content());
    assert!(one.rendered.content().contains(" #2"));
    // 三个同名：#2 与 #3 各一次
    let p3 = [
        proxy("[HK] same", "s1", 1),
        proxy("[HK] same", "s2", 2),
        proxy("[HK] same", "s3", 3),
    ];
    let src3 = [no_groups("air", &p3)];
    let three = build_canonical(&src3, &RenameConfig::default())
        .unwrap()
        .unwrap();
    assert!(three.rendered.content().contains(" #3"));
}

#[test]
fn an_unset_country_falls_back_to_the_name_tag_then_the_default() {
    // meta 未给国别 → 名称 [US] tag 生效；连 tag 都没有 → UN
    let meta = meta_map(&[(
        "[US] t1",
        NodeMeta {
            egress_country: None,
            ..Default::default()
        },
    )]);
    let p = [
        json!({"name": "[US] t1", "type": "vless", "server": "s", "port": 1, "uuid": "u"}),
        json!({"name": "plain", "type": "vless", "server": "s2", "port": 2, "uuid": "u"}),
    ];
    let src = sources(&[("air", &p, &meta)]);
    let artifact = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    let content = artifact.rendered.content();
    assert!(content.contains("US | t1"), "名称 tag 回退：{content}");
    assert!(content.contains("UN | plain"), "无信息回退 UN：{content}");
}

#[test]
fn a_measured_egress_country_wins_over_the_name_tag() {
    let meta = meta_map(&[(
        "[SG] t1",
        NodeMeta {
            egress_country: Some("hk".into()),
            ..Default::default()
        },
    )]);
    let p = [json!({"name": "[SG] t1", "type": "vless", "server": "s",
                    "port": 1, "uuid": "u"})];
    let src = sources(&[("air", &p, &meta)]);
    let artifact = build_canonical(&src, &RenameConfig::default())
        .unwrap()
        .unwrap();
    assert!(artifact.rendered.content().contains("HK | t1"));
}

#[test]
fn disabled_rename_still_dedups_colliding_names() {
    let cfg = RenameConfig {
        enabled: false,
        ..RenameConfig::default()
    };
    let p = [proxy("same", "s1", 1), proxy("same", "s2", 2)];
    let src = [no_groups("air", &p)];
    let artifact = build_canonical(&src, &cfg).unwrap().unwrap();
    assert!(artifact.rendered.content().contains(" #2"));
    // 关闭时模板不生效：原名保留
    assert!(artifact.rendered.content().contains("\"same\""));
}

#[test]
fn unknown_template_placeholders_fall_back_to_the_display_name() {
    let cfg = RenameConfig {
        template: "{bogus} node".into(),
        ..RenameConfig::default()
    };
    let p = [proxy("raw-name", "s1", 1)];
    let src = [no_groups("air", &p)];
    let artifact = build_canonical(&src, &cfg).unwrap().unwrap();
    assert!(artifact.rendered.content().contains("raw-name"));
    assert!(!artifact.rendered.content().contains("bogus"));
}

#[test]
fn template_escapes_and_index_are_supported() {
    let cfg = RenameConfig {
        template: "{{{index}}} {display_name}".into(),
        ..RenameConfig::default()
    };
    let p = [proxy("zzz-b", "s2", 2), proxy("aaa-a", "s1", 1)];
    let src = [no_groups("air", &p)];
    let artifact = build_canonical(&src, &cfg).unwrap().unwrap();
    // 排序后 aaa-a 是第 1 个 → {index}=1，{{{ 转义出一个字面 {
    assert!(artifact.rendered.content().contains("{1} aaa-a"));
}

#[test]
fn empty_input_builds_nothing_so_the_previous_artifact_stands() {
    let src = [no_groups("air", &[])];
    let artifact = build_canonical(&src, &RenameConfig::default()).unwrap();
    assert!(artifact.is_none(), "绝不产出空产物清空订阅方（护栏语义）");
}

#[test]
fn group_members_missing_from_the_body_are_dropped_and_groups_dedup() {
    // 组成员必须引用最终显示名（与 Python 流程一致：组拓扑在重命名之后装配）。
    // 关闭重命名：原名即最终名，ghost/DIRECT 被剔除。
    let cfg_off = RenameConfig {
        enabled: false,
        ..RenameConfig::default()
    };
    let p = [proxy("alive", "s1", 1), proxy("ghost", "s2", 2)];
    let groups = [
        json!({"name": "池", "type": "select",
                          "proxies": ["alive", "absent", "DIRECT"]}),
        json!({"name": "池", "type": "select", "proxies": ["alive"]}),
    ];
    let src = vec![SourceInput {
        key: "air".into(),
        proxies: &p,
        groups: &groups,
        meta: empty_meta(),
    }];
    let artifact = build_canonical(&src, &cfg_off).unwrap().unwrap();
    let doc: Value = serde_json::from_str(artifact.rendered.content()).unwrap();
    let gs = doc["proxy-groups"].as_array().unwrap();
    assert_eq!(gs.len(), 1, "重名组只保留首个");
    assert_eq!(gs[0]["proxies"], json!(["alive"]), "ghost/DIRECT 被剔除");
    // 启用重命名：成员引用重命名后的最终名 "UN | alive" 才不会被剔除。
    let groups_on = [json!({"name": "池", "type": "select",
                            "proxies": ["UN | alive", "alive"]})];
    let src_on = vec![SourceInput {
        key: "air".into(),
        proxies: &p,
        groups: &groups_on,
        meta: empty_meta(),
    }];
    let artifact_on = build_canonical(&src_on, &RenameConfig::default())
        .unwrap()
        .unwrap();
    let doc_on: Value = serde_json::from_str(artifact_on.rendered.content()).unwrap();
    assert_eq!(doc_on["proxy-groups"][0]["proxies"], json!(["UN | alive"]));
}

#[test]
fn defaults_match_the_python_module_constants() {
    assert_eq!(DEFAULT_TEMPLATE, "{country} | {display_name}");
    assert_eq!(DEFAULT_COLLISION_SUFFIX, " #{n}");
}
