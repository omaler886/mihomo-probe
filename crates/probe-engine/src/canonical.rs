//! Canonical 唯一订阅产物（G-1，指导 §8 去重保证 / workstreams/24）。
//!
//! 多来源 → **一份**产物：按连接参数指纹去重（首个来源为主，其余记
//! `source_aliases`）、重命名只动 `name`、固定排序（country → protocol →
//! normalized original name → fingerprint）、连接字段 digest（改名不移动）。
//! 与 Python 侧 `mihomo_test/canonical_export.py` 的行为对齐，两条硬保证：
//!
//! * **digest 跨实现一致**：`meta.digest` 用 `probe_source::identity` 的
//!   Python 兼容序列化（`json.dumps(sort_keys=True, ensure_ascii=False)`
//!   字节级等价）对「除 name 与 `_` 前缀字段外的全部连接字段」取
//!   sha256 前 32 hex —— 与 Python `_proxy_digest` 同字节串。
//! * **产物字节确定性**：渲染不含任何每轮易变字段；同输入 N 轮 =
//!   字节相同、digest 不变（幂等发布的零 PATCH 依赖这一点）。
//!
//! 渲染复用 `publish::render_clash_meta`（JSON-YAML、键序归一、拓扑闭合
//! 校验）。**内容字节与 Python 的 `yaml.dump` 不必相同**——消费方以 URL 拉
//! 各自实现的产物，跨实现只需 digest 与节点集合等价（24 号 workstream 裁决）。
//!
//! 空结果返回 `None`：与 Python 一致——上一稳定产物站住，绝不写空列表
//! 去清空所有订阅方（护栏语义，19 号验收 §11.2-4）。
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::publish::{self, PublishResult, RenderedExport};
use probe_source::identity;

/// 与 Python `canonical_export.DEFAULT` 对齐的重命名模板。
pub const DEFAULT_TEMPLATE: &str = "{country} | {display_name}";
/// 冲突后缀（`{n}` 从 2 递增）。
pub const DEFAULT_COLLISION_SUFFIX: &str = " #{n}";
/// 无出口国别信息时的排序/模板取值。
pub const UNSET_COUNTRY: &str = "UN";

/// 单节点测量元数据（重命名模板与排序的国别来源）。meta 表按显示名或
/// 指纹都能命中——调用方任选其一装表，与 Python `meta_map` 语义一致。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeMeta {
    pub egress_country: Option<String>,
    pub delay: Option<i64>,
    pub best_probe: Option<String>,
}

/// 重命名配置（默认启用、`{country} | {display_name}`、冲突 ` #{n}`）。
#[derive(Debug, Clone)]
pub struct RenameConfig {
    pub enabled: bool,
    pub template: String,
    pub collision_suffix: String,
}

impl Default for RenameConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            template: DEFAULT_TEMPLATE.to_string(),
            collision_suffix: DEFAULT_COLLISION_SUFFIX.to_string(),
        }
    }
}

/// 一个来源的入选节点（`publish::select_source_exports` 的下游产物）。
///
/// `meta` 键 = 显示名或指纹；`groups` 是该来源的链式拓扑（合并时剔除产物
/// 里不存在的成员，重名组只保留首个）。
#[derive(Debug, Clone)]
pub struct SourceInput<'a> {
    pub key: String,
    pub proxies: &'a [Value],
    pub groups: &'a [Value],
    pub meta: &'a BTreeMap<String, NodeMeta>,
}

/// 一条重命名记录（审计用；凭据字段永不进入）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenameRecord {
    pub fingerprint: String,
    pub original_name: String,
    pub final_name: String,
    pub renamed: bool,
}

/// 产物元数据（对应 Python 的 `mihomo-alive.meta.json`）。
#[derive(Debug, Clone, Serialize)]
pub struct CanonicalMeta {
    pub count: usize,
    /// 连接字段 digest（32 hex，改名不移动；跨实现与 Python 同字节串）。
    pub digest: String,
    /// 渲染产物全文字节 sha256。
    pub content_sha256: String,
    pub renames: Vec<RenameRecord>,
    /// 指纹 → 主来源之外的来源 key（同节点多来源）。
    pub source_aliases: BTreeMap<String, Vec<String>>,
}

/// 一次 canonical 构建的结果。
///
/// `RenderedExport` 有意不实现 `Debug`（含凭据的验证产物），因此本结构同样
/// 不派生 `Debug`——审计走 `CanonicalMeta`（无凭据字段）。
#[derive(Clone)]
pub struct CanonicalArtifact {
    pub rendered: RenderedExport,
    pub meta: CanonicalMeta,
}

struct Entry {
    fingerprint: String,
    proxy: Value,
    meta: NodeMeta,
    source: String,
}

/// 构建 canonical 产物；无任何可发布节点时返回 `None`（上一稳定产物站住）。
pub fn build_canonical(
    sources: &[SourceInput],
    cfg: &RenameConfig,
) -> PublishResult<Option<CanonicalArtifact>> {
    // ---- 去重：指纹主键，首来源为主，其余来源记别名
    let mut entries: Vec<Entry> = Vec::new();
    let mut index_by_fp: HashMap<String, usize> = HashMap::new();
    let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in sources {
        for proxy in source.proxies {
            if !proxy.is_object() {
                continue;
            }
            let fingerprint = identity::fingerprint_proxy(proxy);
            if let Some(&i) = index_by_fp.get(&fingerprint) {
                if !source.key.is_empty()
                    && source.key != entries[i].source
                    && !aliases
                        .entry(fingerprint.clone())
                        .or_default()
                        .contains(&source.key)
                {
                    aliases
                        .get_mut(&fingerprint)
                        .unwrap()
                        .push(source.key.clone());
                }
                continue;
            }
            let meta = lookup_meta(source.meta, proxy, &fingerprint);
            index_by_fp.insert(fingerprint.clone(), entries.len());
            entries.push(Entry {
                fingerprint,
                proxy: proxy.clone(),
                meta,
                source: source.key.clone(),
            });
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }

    // ---- 固定排序：country → protocol → normalized original name → fingerprint
    entries.sort_by_key(sort_key);

    // ---- 重命名（只动 name）+ 冲突后缀
    let (proxies, renames) = apply_renames(&entries, cfg);

    // ---- 组拓扑合并 + 渲染
    let published: BTreeSet<String> = proxies
        .iter()
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let groups = merge_groups(sources, &published);
    let rendered = publish::render_clash_meta(&proxies, &groups)?;

    let meta = CanonicalMeta {
        count: rendered.node_count(),
        digest: connection_digest(&proxies),
        content_sha256: rendered.sha256().to_string(),
        renames,
        source_aliases: aliases.into_iter().filter(|(_, v)| !v.is_empty()).collect(),
    };
    Ok(Some(CanonicalArtifact { rendered, meta }))
}

fn lookup_meta(map: &BTreeMap<String, NodeMeta>, proxy: &Value, fingerprint: &str) -> NodeMeta {
    let name = proxy.get("name").and_then(Value::as_str).unwrap_or("");
    map.get(name)
        .cloned()
        .or_else(|| map.get(fingerprint).cloned())
        .unwrap_or_default()
}

fn name_of(proxy: &Value) -> &str {
    proxy.get("name").and_then(Value::as_str).unwrap_or("")
}

/// 出口国别：meta.egress_country → proxy `_country`/meta.country → 名称
/// `[CC-…]` 前缀 → `UN`（Python `_country_of` 同一条回退链）。
fn country_of(proxy: &Value, meta: &NodeMeta) -> String {
    if let Some(c) = &meta.egress_country {
        if !c.is_empty() {
            return c.to_uppercase();
        }
    }
    if let Some(c) = proxy.get("_country").and_then(Value::as_str) {
        return c.to_uppercase();
    }
    if let Some(c) = proxy
        .get("meta")
        .and_then(Value::as_object)
        .and_then(|m| m.get("country"))
        .and_then(Value::as_str)
    {
        return c.to_uppercase();
    }
    let name = name_of(proxy);
    if name.len() > 3 && name.starts_with('[') {
        if let Some(end) = name[1..].find(']') {
            let tag = &name[1..1 + end];
            let mut parts = tag.splitn(2, '-');
            let region = parts.next().unwrap_or_default();
            let suffix_ok = match parts.next() {
                None => true,
                Some(s) => !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()),
            };
            if region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()) && suffix_ok {
                return region.to_string();
            }
        }
    }
    UNSET_COUNTRY.to_string()
}

/// 排序名：剥前导地区 tag/旗帜后小写（不改显示名本身）。
fn normalize_for_sort(name: &str) -> String {
    publish::strip_leading_region_tags(name)
        .trim()
        .to_lowercase()
}

fn sort_key(entry: &Entry) -> (String, String, String, String) {
    (
        country_of(&entry.proxy, &entry.meta),
        entry
            .proxy
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        normalize_for_sort(name_of(&entry.proxy)),
        entry.fingerprint.clone(),
    )
}

fn apply_renames(entries: &[Entry], cfg: &RenameConfig) -> (Vec<Value>, Vec<RenameRecord>) {
    let mut used: HashMap<String, String> = HashMap::new();
    let mut proxies = Vec::with_capacity(entries.len());
    let mut renames = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let mut out = entry.proxy.clone();
        let original = name_of(&entry.proxy).to_string();
        let final_name = if cfg.enabled {
            let stripped = publish::strip_leading_region_tags(&original);
            let display = if stripped.is_empty() {
                &original
            } else {
                stripped
            };
            let country = country_of(&entry.proxy, &entry.meta);
            let probe = entry.meta.best_probe.clone().unwrap_or_default();
            let delay = match entry.meta.delay {
                None => String::new(),
                Some(d) => d.to_string(),
            };
            let protocol = entry
                .proxy
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let fields = |name: &str| match name {
                "display_name" => Some(display.to_string()),
                "country" => Some(country.clone()),
                "probe" => Some(probe.clone()),
                "delay" => Some(delay.clone()),
                "protocol" => Some(protocol.clone()),
                "index" => Some((index + 1).to_string()),
                _ => None,
            };
            // 未知占位符 = Python str.format 的 KeyError → 回退显示名。
            let rendered = render_template(&cfg.template, &fields, display);
            uniquify(&rendered, &entry.fingerprint, &mut used, cfg)
        } else {
            // 关闭重命名仍要防同名不同指纹在客户端撞名（mihomo 按 name 索引）。
            uniquify(&original, &entry.fingerprint, &mut used, cfg)
        };
        out["name"] = Value::String(final_name.clone());
        renames.push(RenameRecord {
            fingerprint: entry.fingerprint.clone(),
            original_name: original,
            renamed: cfg.enabled && final_name != name_of(&entry.proxy),
            final_name,
        });
        proxies.push(out);
    }
    (proxies, renames)
}

/// `str.format` 的受控子集：已知占位符替换、`{{`/`}}` 转义、单 `}` 字面量、
/// 未知占位符或未闭合花括号整体回退（Python 的 KeyError → `_render_name`
/// fallback）。花括号是 ASCII，切片都落在 ASCII 边界上，UTF-8 名称安全。
fn render_template(
    template: &str,
    field: &dyn Fn(&str) -> Option<String>,
    fallback: &str,
) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if template[i..].starts_with("{{") {
                    out.push('{');
                    i += 2;
                    continue;
                }
                let Some(end_rel) = template[i..].find('}') else {
                    return fallback.to_string();
                };
                let name = &template[i + 1..i + end_rel];
                match field(name) {
                    Some(value) => out.push_str(&value),
                    None => return fallback.to_string(),
                }
                i += end_rel + 1;
            }
            b'}' => {
                out.push('}');
                i += if template[i..].starts_with("}}") {
                    2
                } else {
                    1
                };
            }
            _ => {
                // 复制到下一个 ASCII 花括号为止（不切断 UTF-8 序列）。
                match bytes[i..].iter().position(|&c| c == b'{' || c == b'}') {
                    None => {
                        out.push_str(&template[i..]);
                        break;
                    }
                    Some(rel) => {
                        out.push_str(&template[i..i + rel]);
                        i += rel;
                    }
                }
            }
        }
    }
    out
}

/// `collision_suffix`：`{n}`/`{N}` 填入序号，否则直接追加（Python 同款——
/// 后缀模板只管后缀本身，完整候选名 = base + 后缀）。
fn collision(base: &str, suffix_template: &str, n: usize) -> String {
    let suffix = if suffix_template.contains("{n}") {
        suffix_template.replace("{n}", &n.to_string())
    } else if suffix_template.contains("{N}") {
        suffix_template.replace("{N}", &n.to_string())
    } else {
        format!("{suffix_template}{n}")
    };
    format!("{base}{suffix}")
}

/// 同名不同指纹 → 追加 ` #2/#3…` 直到唯一；候选名空闲或属同一指纹即取
/// （Python `_Renamer.apply` 的判等语义）。`used` 记录 最终名 → 指纹。
fn uniquify(
    candidate: &str,
    fingerprint: &str,
    used: &mut HashMap<String, String>,
    cfg: &RenameConfig,
) -> String {
    if let Some(owner) = used.get(candidate) {
        if owner != fingerprint {
            let mut n = 2usize;
            loop {
                let next = collision(candidate, &cfg.collision_suffix, n);
                match used.get(&next) {
                    Some(owner) if owner != fingerprint => n += 1,
                    _ => {
                        used.insert(next.clone(), fingerprint.to_string());
                        return next;
                    }
                }
            }
        }
    }
    used.insert(candidate.to_string(), fingerprint.to_string());
    candidate.to_string()
}

/// 连接字段 digest：剔除 `name` 与 `_` 前缀字段后，用 Python 兼容序列化取
/// sha256 前 32 hex——字节串与 Python `_proxy_digest` 一致，改名不移动。
fn connection_digest(proxies: &[Value]) -> String {
    let material: Vec<Value> = proxies
        .iter()
        .map(|p| {
            let mut out = Map::new();
            if let Some(obj) = p.as_object() {
                for (k, v) in obj {
                    if k != "name" && !k.starts_with('_') {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            Value::Object(out)
        })
        .collect();
    let json = identity::to_python_json_sorted(&Value::Array(material));
    let digest = Sha256::digest(json.as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// 组拓扑合并：按来源顺序 union、重名组只保留首个、成员剔除产物里不存在的
/// 名字（Python `_merge_groups` 同款——内置目标如 DIRECT 同样被剔除，行为
/// 有意保持一致）。
fn merge_groups(sources: &[SourceInput], published: &BTreeSet<String>) -> Vec<Value> {
    let mut merged = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for source in sources {
        for group in source.groups {
            if !group.is_object() {
                continue;
            }
            let name = group.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() || !seen.insert(name.to_string()) {
                continue;
            }
            let mut group = group.clone();
            if let Some(members) = group.get_mut("proxies").and_then(Value::as_array_mut) {
                members.retain(|m| m.as_str().map(|s| published.contains(s)).unwrap_or(false));
            }
            merged.push(group);
        }
    }
    merged
}
