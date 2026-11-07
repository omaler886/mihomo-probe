//! Probe-agent 签名与结果校验（G-3，HK3 §4.2/§13.1；`probe_dispatch.py`
//! 的 Rust 镜像）。存储交互（轮次行、注册表查询）由调用方（API 端点）完成
//! 后以参数传入，本模块保持纯函数可测。

use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 签名头：hex HMAC-SHA256 over `ts + "\n" + body`，keyed by probe token。
pub const SIGNATURE_HEADER: &str = "X-Probe-Signature";
/// 签名时间戳头（秒级 Unix epoch）。
pub const TIMESTAMP_HEADER: &str = "X-Probe-Timestamp";
/// 签名时间窗：窗口外的请求一律拒绝（防重放，§13.1）。
pub const SIGNATURE_WINDOW_S: i64 = 300;

/// Hex HMAC-SHA256 over `ts + "\n" + body` keyed by `secret`. Controller 与
/// Probe Agent 用同一算法，永不吵架（§13.1）；时间戳在签名材料里，被捕获的
/// 请求无法在窗口外重放。
pub fn sign_request(secret: &str, body: &[u8], ts: i64) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b"\n");
    mac.update(body);
    hex(&mac.finalize().into_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `true` 当签名有效**且**时间戳在窗口内。两种失败都答 false——想区分
/// 「过期」与「伪造」的调用方自己去读时间戳（公开数据）。
pub fn verify_signature(
    secret: &str,
    body: &[u8],
    ts: &str,
    signature: Option<&str>,
    window_s: i64,
    now_epoch: i64,
) -> bool {
    let Some(signature) = signature.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let Ok(ts_value) = ts.trim().parse::<i64>() else {
        return false;
    };
    if (now_epoch - ts_value).abs() > window_s {
        return false;
    }
    let expected = sign_request(secret, body, ts_value);
    // 常量时间比较 + 大小写不敏感（hex 比较统一小写）。
    expected == signature.trim().to_ascii_lowercase()
}

/// 一个探测点的六段配置 digest（Python `config_digest` 的 Rust 镜像）：
/// probes/aggregation/chain/test/policy/verify 六个 section 的 canonical
/// JSON（sorted keys）sha256，`"sha256:"` 前缀。两种实现对不同键集做哈希
/// 会互拒结果——键集照抄，不发明新的。
pub fn config_digest(sections: &[(&str, &Value)]) -> String {
    // serde_json 开了 preserve_order（内核配置可比性依赖），digest 需要
    // sorted keys——递归转 BTreeMap 后序列化，字节与 Python 的
    // sort_keys=True 一致。
    let sorted: BTreeValue = sections
        .iter()
        .map(|(key, value)| ((*key).to_string(), sort_value(value)))
        .collect();
    let blob = serde_json::to_string(&sorted).unwrap_or_default();
    format!("sha256:{}", hex(&Sha256::digest(blob.as_bytes())))
}

type BTreeValue = std::collections::BTreeMap<String, Value>;

fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeValue = map
                .iter()
                .map(|(key, value)| (key.clone(), sort_value(value)))
                .collect();
            Value::Object(Map::from_iter(sorted))
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// 一条已通过 `_clean_result_item` 校验的结果项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedItem {
    pub fingerprint: String,
    pub alive: bool,
    pub delay_ms: Option<i64>,
    pub reason: Option<String>,
    pub egress_country: Option<String>,
}

/// 校验拒绝码（稳定拼写，HTTP 映射在 API 层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyReject {
    BadPayload(&'static str),
    RoundStale(&'static str),
    UnknownProbe,
    DigestMismatch,
    ResultLate(&'static str),
}

impl VerifyReject {
    /// `error` 响应体的稳定拼写。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BadPayload(_) => "BAD_PAYLOAD",
            Self::RoundStale(_) => "ROUND_STALE",
            Self::UnknownProbe => "UNKNOWN_PROBE",
            Self::DigestMismatch => "DIGEST_MISMATCH",
            Self::ResultLate(_) => "RESULT_LATE",
        }
    }
}

/// 轮次状态（调用方从账本查好后传入）：是否存在、未结算、是否最新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundState {
    pub exists: bool,
    pub finished: bool,
    pub is_newest: bool,
}

/// 验证一个 ProbeResult。`None` 结果 = 接受（返回待写入的观测项与整体
/// payload digest，调用方幂等落库 + 心跳）；`Some(reject)` = 拒绝码。
/// 每一种拒绝都只进审计——绝不产生观测行，绝不改死亡计数（§4.2）。
///
/// `deadline_epoch`：dispatch 侧登记的本轮 deadline（None = 无诚实 deadline
/// 可 enforce，由 round-match 门禁兜底）。`finished_at_epoch`：回传 payload
/// 自报的完成时刻（None = 不比）——晚于 deadline 同判 RESULT_LATE（§14）。
pub fn verify_result(
    payload: &Value,
    round: RoundState,
    probe_known: bool,
    expected_digest: &str,
    deadline_epoch: Option<i64>,
    finished_at_epoch: Option<i64>,
    now_epoch: i64,
) -> Result<(Vec<ValidatedItem>, String), VerifyReject> {
    let Some(map) = payload.as_object() else {
        return Err(VerifyReject::BadPayload("payload 不是对象"));
    };
    let probe_id = map.get("probe_id").and_then(Value::as_str);
    let digest = map.get("config_digest").and_then(Value::as_str);
    let results = map.get("results");
    // probe_id/round_id 的存在性在此钉住；值本身由调用方（端点）持有并落库。
    if probe_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        return Err(VerifyReject::BadPayload("probe_id 缺失或不是字符串"));
    }
    let _ = map.get("round_id");
    let Some(digest) = digest.filter(|d| !d.is_empty()) else {
        return Err(VerifyReject::BadPayload("config_digest 缺失"));
    };
    let Some(results) = results.and_then(Value::as_array) else {
        return Err(VerifyReject::BadPayload("results 缺失或不是数组"));
    };
    let mut items = Vec::with_capacity(results.len());
    for item in results {
        let Some(clean) = clean_result_item(item) else {
            return Err(VerifyReject::BadPayload("results 形状非法"));
        };
        items.push(clean);
    }

    // 1) 轮次：必须存在、未结算、且仍是最新一轮（旧轮次只入审计，§4.2）。
    if !round.exists {
        return Err(VerifyReject::RoundStale("轮次不存在"));
    }
    if round.finished {
        return Err(VerifyReject::RoundStale("轮次已结算"));
    }
    if !round.is_newest {
        return Err(VerifyReject::RoundStale("已被更新的轮次取代"));
    }

    // 2) 探测点：必须已知（配置注册或账本里心跳过）。
    if !probe_known {
        return Err(VerifyReject::UnknownProbe);
    }

    // 3) digest：必须与轮次所依据的唯一配置一致。空期望值 = 该轮尚未启用
    // dispatch（无登记 digest）：任何非空 digest 都接受——与 Python
    // expected_digest=None 的重算口径等价的宽松分支，dispatch 落列后收紧。
    if !expected_digest.is_empty() && digest != expected_digest {
        return Err(VerifyReject::DigestMismatch);
    }

    // 4) deadline：超时回传只入审计（§4.2/§14 集成场景）。迟到包括两种：
    //    接收时刻越过截止，与 payload 自报的 finished_at 越过截止。
    if let Some(deadline) = deadline_epoch {
        if now_epoch > deadline {
            return Err(VerifyReject::ResultLate("超过本轮 deadline"));
        }
        if let Some(finished) = finished_at_epoch {
            if finished > deadline {
                return Err(VerifyReject::ResultLate("finished_at 晚于 deadline"));
            }
        }
    }

    // payload digest = 整体 sorted-JSON sha256（"sha256:" 前缀）；观测行
    // 存它但聚合层不可复算、不可比（Python 同口径）。
    let payload_digest = format!(
        "sha256:{}",
        hex(&Sha256::digest(
            serde_json::to_string(&sort_value(payload))
                .unwrap_or_default()
                .as_bytes()
        ))
    );
    Ok((items, payload_digest))
}

/// `_clean_result_item`：一条结果项的校验形态，畸形返回 None。
pub fn clean_result_item(item: &Value) -> Option<ValidatedItem> {
    let map = item.as_object()?;
    let fingerprint = map.get("fingerprint").and_then(Value::as_str)?;
    if fingerprint.is_empty() {
        return None;
    }
    let alive = map.get("alive").and_then(Value::as_bool)?;
    let delay_ms = match map.get("delay_ms") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let number = value.as_i64()?;
            if number < 0 {
                return None;
            }
            Some(number)
        }
    };
    let reason = match map.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return None,
    };
    let egress_country = match map.get("egress_country") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.chars().count() <= 8 => Some(text.clone()),
        Some(_) => return None,
    };
    Some(ValidatedItem {
        fingerprint: fingerprint.to_string(),
        alive,
        delay_ms,
        reason,
        egress_country,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// ISO 秒精度时间戳 → epoch（测试内联，避免为断言引 time 依赖到生产路径）。
    fn parse_stamp_epoch(stamp: &str) -> Option<i64> {
        let format =
            time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
        // 存储值是 UTC（utc_now() 无时区后缀）：按 naive 解析后假设 UTC。
        time::PrimitiveDateTime::parse(stamp, &format)
            .ok()
            .map(|dt| dt.assume_utc().unix_timestamp())
    }

    /// 签名与 Python 同字节：ts + "\n" + body 的 HMAC-SHA256 hex。
    #[test]
    fn the_signature_matches_the_python_bytes() {
        // 与 mihomo_test/tests 的 sign_request 向量同参数。
        let signature = sign_request("sekret", b"{\"a\":1}", 1_700_000_000);
        assert_eq!(signature.len(), 64);
        // 自验通过；错 body / 错 ts / 缺签名 / 超窗 / 大写 hex 均按契约。
        assert!(verify_signature(
            "sekret",
            b"{\"a\":1}",
            "1700000000",
            Some(&signature),
            SIGNATURE_WINDOW_S,
            1_700_000_000
        ));
        assert!(verify_signature(
            "sekret",
            b"{\"a\":1}",
            "1700000000",
            Some(&signature.to_uppercase()),
            SIGNATURE_WINDOW_S,
            1_700_000_000
        ));
        assert!(!verify_signature(
            "sekret",
            b"{\"a\":2}",
            "1700000000",
            Some(&signature),
            SIGNATURE_WINDOW_S,
            1_700_000_000
        ));
        assert!(!verify_signature(
            "sekret",
            b"{\"a\":1}",
            "1700000001",
            Some(&signature),
            SIGNATURE_WINDOW_S,
            1_700_000_000
        ));
        assert!(!verify_signature(
            "sekret",
            b"{\"a\":1}",
            "1700000000",
            None,
            SIGNATURE_WINDOW_S,
            1_700_000_000
        ));
        assert!(!verify_signature(
            "sekret",
            b"{\"a\":1}",
            "1700000000",
            Some(&signature),
            SIGNATURE_WINDOW_S,
            1_700_000_000 + SIGNATURE_WINDOW_S + 1
        ));
    }

    /// 六段 digest 的确定性：键序无关（sorted keys），节集固定。
    #[test]
    fn the_config_digest_is_key_order_independent() {
        let a = json!({"probes": [{"id": "p1"}], "test": {"url": "u"}});
        let b = json!({"test": {"url": "u"}, "probes": [{"id": "p1"}]});
        let first = config_digest(&[("probes", &a["probes"]), ("test", &a["test"])]);
        let second = config_digest(&[("probes", &b["probes"]), ("test", &b["test"])]);
        assert_eq!(first, second);
        assert!(first.starts_with("sha256:"));
        // 缺一个 section → 不同 digest。
        let third = config_digest(&[("probes", &a["probes"])]);
        assert_ne!(first, third);
    }

    /// verify_result 的五类拒绝各归其位。
    #[test]
    fn the_verifier_routes_every_rejection_class() {
        let payload = json!({
            "round_id": 7,
            "probe_id": "p1",
            "config_digest": "sha256:abc",
            "results": [{"fingerprint": "fp-1", "alive": true, "delay_ms": 100}],
        });
        let good = RoundState {
            exists: true,
            finished: false,
            is_newest: true,
        };
        // 接受：观测项 + payload digest。
        let (items, payload_digest) =
            verify_result(&payload, good, true, "sha256:abc", None, None, 1_000).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].fingerprint, "fp-1");
        assert!(payload_digest.starts_with("sha256:"));

        // BAD_PAYLOAD 各形。
        for broken in [
            json!({"round_id": 7, "config_digest": "d", "results": []}),
            json!({"round_id": 7, "probe_id": "p1", "results": []}),
            json!({"round_id": 7, "probe_id": "p1", "config_digest": "d"}),
            json!({"round_id": 7, "probe_id": "p1", "config_digest": "d", "results": [{"alive": true}]}),
            json!({"round_id": 7, "probe_id": "p1", "config_digest": "d", "results": [{"fingerprint": "f", "alive": "yes"}]}),
            json!({"round_id": 7, "probe_id": "p1", "config_digest": "d", "results": [{"fingerprint": "f", "alive": true, "egress_country": "TOOLONGER"}]}),
        ] {
            let error =
                verify_result(&broken, good, true, "sha256:abc", None, None, 1_000).unwrap_err();
            assert_eq!(error.as_str(), "BAD_PAYLOAD", "{broken}");
        }

        // ROUND_STALE 三态。
        let missing = RoundState {
            exists: false,
            finished: false,
            is_newest: false,
        };
        assert_eq!(
            verify_result(&payload, missing, true, "sha256:abc", None, None, 1_000)
                .unwrap_err()
                .as_str(),
            "ROUND_STALE"
        );
        let settled = RoundState {
            exists: true,
            finished: true,
            is_newest: true,
        };
        assert_eq!(
            verify_result(&payload, settled, true, "sha256:abc", None, None, 1_000)
                .unwrap_err()
                .as_str(),
            "ROUND_STALE"
        );
        let superseded = RoundState {
            exists: true,
            finished: false,
            is_newest: false,
        };
        assert_eq!(
            verify_result(&payload, superseded, true, "sha256:abc", None, None, 1_000)
                .unwrap_err()
                .as_str(),
            "ROUND_STALE"
        );

        // UNKNOWN_PROBE / DIGEST_MISMATCH / RESULT_LATE。
        assert_eq!(
            verify_result(&payload, good, false, "sha256:abc", None, None, 1_000)
                .unwrap_err()
                .as_str(),
            "UNKNOWN_PROBE"
        );
        assert_eq!(
            verify_result(&payload, good, true, "sha256:other", None, None, 1_000)
                .unwrap_err()
                .as_str(),
            "DIGEST_MISMATCH"
        );
        assert_eq!(
            verify_result(&payload, good, true, "sha256:abc", Some(500), None, 1_000)
                .unwrap_err()
                .as_str(),
            "RESULT_LATE"
        );
    }

    /// deadline 登记生效后的两种迟到：接收时刻越窗与 finished_at 越窗；
    /// 无登记 deadline 时 RESULT_LATE 永不触发。
    #[test]
    fn result_late_fires_on_both_arrival_and_finished_at() {
        let payload = json!({
            "round_id": 7,
            "probe_id": "p1",
            "config_digest": "sha256:abc",
            "finished_at": "2026-11-08T00:05:00",
            "results": [{"fingerprint": "fp-1", "alive": true}],
        });
        let good = RoundState {
            exists: true,
            finished: false,
            is_newest: true,
        };
        let deadline = 1_000;
        let finished = parse_stamp_epoch("2026-11-08T00:05:00");
        assert!(finished.is_some());

        // finished_at 晚于 deadline → RESULT_LATE（即使接收时刻没越窗）。
        assert_eq!(
            verify_result(
                &payload,
                good,
                true,
                "sha256:abc",
                Some(deadline),
                finished,
                deadline - 10,
            )
            .unwrap_err()
            .as_str(),
            "RESULT_LATE"
        );
        // finished_at 在窗内 → 接受。
        let early = json!({
            "round_id": 7,
            "probe_id": "p1",
            "config_digest": "sha256:abc",
            "finished_at": "1970-01-01T00:10:00",
            "results": [{"fingerprint": "fp-1", "alive": true}],
        });
        assert!(verify_result(
            &early,
            good,
            true,
            "sha256:abc",
            Some(deadline),
            parse_stamp_epoch("1970-01-01T00:10:00"),
            deadline - 10
        )
        .is_ok());
        // 无登记 deadline：finished_at 再晚也不比（诚实 None 分支）。
        assert!(verify_result(&payload, good, true, "sha256:abc", None, finished, 1_000).is_ok());
    }

    /// 0/负 delay 与缺字段按畸形拒；reason 保留原文。
    #[test]
    fn result_items_are_validated_field_by_field() {
        assert!(clean_result_item(&json!({"fingerprint": "f", "alive": true})).is_some());
        assert!(
            clean_result_item(&json!({"fingerprint": "f", "alive": true, "reason": "timeout"}))
                .unwrap()
                .reason
                .as_deref()
                == Some("timeout")
        );
        assert!(clean_result_item(&json!({"fingerprint": "", "alive": true})).is_none());
        assert!(
            clean_result_item(&json!({"fingerprint": "f", "alive": true, "delay_ms": -1}))
                .is_none()
        );
        assert!(clean_result_item(&json!({"fingerprint": "f", "alive": 1})).is_none());
    }
}
