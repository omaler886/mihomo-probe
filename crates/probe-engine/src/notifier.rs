//! 轮末告警核（workstreams/34 R1；`mihomo_test/notifier.py` 的 Rust 镜像）。
//!
//! 两条可选通道：Telegram bot 与通用 webhook。每条告警按 key 走冷却——
//! 冷却**只在至少一条通道送达后**才记入：先记冷却会把一次投递故障
//! （Telegram 宕机、webhook 超时、token 打错）变成 `cooldown_minutes`
//! （默认 240 分钟）的沉默，把瞬时故障转成对真实状况的静音。
//!
//! 空 results = 所有通道都未配置，不是错误（Python 同口径）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use probe_config::AlertSection;
use serde_json::{json, Value};

/// Telegram 生产 API base；测试用 `with_api_base` 指向 127.0.0.1。
pub const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// 告警级别（Python LEVELS；越界值回落 warn）。
pub const LEVELS: [&str; 3] = ["info", "warn", "error"];

/// 冷却状态文件的文件名（Python `config.DATA / "alert-state.json"`）。
pub const STATE_FILE_NAME: &str = "alert-state.json";

/// 一次发送的逐通道结果（`telegram: 已发送` / `webhook: <原因>`）。
pub type ChannelResults = Vec<String>;

/// 告警发送器。冷却状态在调用方给的文件里（`<root>/data/alert-state.json`），
/// 与 Python 同位：重启不丢冷却窗。
#[derive(Debug, Clone)]
pub struct Notifier {
    alert: AlertSection,
    state_path: PathBuf,
    api_base: String,
    http: reqwest::Client,
}

impl Notifier {
    /// 生产构造：冷却状态文件在 `<root>/data/alert-state.json`。
    pub fn from_config(alert: &AlertSection, root: &Path) -> Self {
        Self {
            alert: alert.clone(),
            state_path: root.join("data").join(STATE_FILE_NAME),
            api_base: TELEGRAM_API_BASE.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("static client config"),
        }
    }

    /// 测试构造：Telegram API base 可指到本地 stub。
    pub fn with_api_base(mut self, base: &str) -> Self {
        self.api_base = base.to_string();
        self
    }

    /// 测试辅助：替换告警配置（alive_floor 等）。
    pub fn with_alert_cfg(mut self, alert: AlertSection) -> Self {
        self.alert = alert;
        self
    }

    /// 读冷却状态（坏文件/缺失 = 空 map，与 Python `_state` 同口径）。
    fn read_state(&self) -> BTreeMap<String, f64> {
        match std::fs::read_to_string(&self.state_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        }
    }

    /// 冷却检查：同 key 在窗口内 = 跳过。
    fn cooldown_ok(state: &BTreeMap<String, f64>, key: &str, minutes: i64, now: f64) -> bool {
        match state.get(key) {
            Some(last) => now - last >= (minutes.max(1) as f64) * 60.0,
            None => true,
        }
    }

    /// 送达后才写冷却（临时文件 + rename，与 Python `_save` 同形）。
    fn save_state(&self, state: &BTreeMap<String, f64>) -> std::io::Result<()> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(state).unwrap_or_default())?;
        std::fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }

    async fn telegram_send(&self, text: &str) -> Result<(), String> {
        let telegram = &self.alert.telegram;
        if telegram.token.is_empty() || telegram.chat_id.is_empty() {
            return Err("telegram 未配置 token/chat_id".to_string());
        }
        let url = format!("{}/bot{}/sendMessage", self.api_base, telegram.token);
        let payload = json!({
            "chat_id": telegram.chat_id,
            "text": text,
        });
        let response = self
            .http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|error| format!("telegram 请求失败: {error}"))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            // token 不回显（消息契约：只带状态码）。
            Err(format!("telegram HTTP {status}"))
        }
    }

    async fn webhook_send(&self, payload: &Value) -> Result<(), String> {
        let url = self.alert.webhook.url.trim();
        if url.is_empty() {
            return Err("webhook 未配置 url".to_string());
        }
        let response = self
            .http
            .post(url)
            .json(payload)
            .send()
            .await
            .map_err(|error| format!("webhook 请求失败: {error}"))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("webhook HTTP {status}"))
        }
    }

    /// 送达一条告警，尊重 per-key 冷却。返回逐通道结果字符串（空 = 全部
    /// 未配置）。
    pub async fn send(&self, key: &str, title: &str, body: &str, level: &str) -> ChannelResults {
        if !self.alert.enabled {
            return Vec::new();
        }
        let level = if LEVELS.contains(&level) {
            level
        } else {
            "warn"
        };
        let cooldown = self.alert.cooldown_minutes.max(1);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut state = self.read_state();
        if !Self::cooldown_ok(&state, key, cooldown, now) {
            tracing::info!(key, "告警处于冷却期，跳过发送");
            return Vec::new();
        }

        // Labelled UTC and actually UTC（Python 同因）：容器 TZ 不影响时间戳。
        let stamp = time::OffsetDateTime::from_unix_timestamp(now as i64)
            .map(|dt| {
                let fmt = time::macros::format_description!("[month]-[day] [hour]:[minute]");
                dt.format(&fmt).unwrap_or_default()
            })
            .unwrap_or_default();
        let text = format!(
            "[mihomo-test {}] {title}\n{body}\n{stamp} UTC",
            level.to_uppercase()
        );
        let payload = json!({
            "key": key,
            "level": level,
            "title": title,
            "body": body,
            "source": "mihomo-probe",
            "ts": now as i64,
        });

        let mut results = Vec::new();
        let mut delivered = false;
        if self.alert.telegram.enabled {
            match self.telegram_send(&text).await {
                Ok(()) => {
                    delivered = true;
                    results.push("telegram: 已发送".to_string());
                }
                Err(detail) => results.push(format!("telegram: {detail}")),
            }
        }
        if self.alert.webhook.enabled {
            match self.webhook_send(&payload).await {
                Ok(()) => {
                    delivered = true;
                    results.push("webhook: 已发送".to_string());
                }
                Err(detail) => results.push(format!("webhook: {detail}")),
            }
        }

        if delivered {
            state.insert(key.to_string(), now);
            if let Err(error) = self.save_state(&state) {
                tracing::warn!(
                    %error,
                    "告警冷却状态写入失败：下次同类告警可能重复发送"
                );
            }
        } else if !results.is_empty() {
            // 全部通道失败按 error 记录，而不是 info 的「冷却期跳过」——
            // 坏掉的通道必须看起来像坏掉的，而不是像健康的限流。
            tracing::error!(key, results = ?results, "告警投递全部失败");
        }
        results
    }
}

/// 轮末告警判定（Python `_maybe_alert` 的 Rust 镜像）：suspect → error、
/// alive_floor 命中 → warn。调用方在轮次关闭后调用；best-effort，任何
/// 通道故障只记日志，绝不影响轮次结果。
pub async fn maybe_alert_after_round(
    notifier: &Notifier,
    suspect: bool,
    alive: i64,
) -> ChannelResults {
    let mut results = Vec::new();
    if suspect {
        results.extend(
            notifier
                .send(
                    "suspect_round",
                    "护栏触发：本轮结果可疑，未发布",
                    &format!(
                        "存活 {alive} 低于上轮安全下限，已保留上一轮输出。\
                         先确认本机网络与测试目标是否正常。"
                    ),
                    "error",
                )
                .await,
        );
    }
    if notifier.alert.alive_floor > 0 && alive < notifier.alert.alive_floor {
        results.extend(
            notifier
                .send(
                    "alive_low",
                    &format!(
                        "存活节点低于下限（{alive} < {}）",
                        notifier.alert.alive_floor
                    ),
                    &format!(
                        "本轮存活 {alive}，配置下限 {}。",
                        notifier.alert.alive_floor
                    ),
                    "warn",
                )
                .await,
        );
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use serde_json::Value;

    fn alert_cfg() -> AlertSection {
        AlertSection {
            enabled: true,
            telegram: probe_config::TelegramAlertSection {
                enabled: true,
                token: "stub-telegram-token-33".into(),
                chat_id: "42".into(),
            },
            webhook: probe_config::WebhookAlertSection {
                enabled: true,
                url: String::new(), // 由 with_webhook_url 注入本地 stub
            },
            cooldown_minutes: 240,
            alive_floor: 0,
        }
    }

    /// 本地 stub：Telegram sendMessage + webhook 双端点，记录请求。
    async fn spawn_stub() -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
        #[derive(Clone)]
        struct Sink(Arc<Mutex<Vec<(String, Value)>>>);

        async fn telegram(
            axum::extract::State(sink): axum::extract::State<Sink>,
            axum::extract::Path(token): axum::extract::Path<String>,
            body: String,
        ) -> (axum::http::StatusCode, &'static str) {
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            sink.0
                .lock()
                .unwrap()
                .push(("telegram".into(), json!({"token": token, "body": parsed})));
            (axum::http::StatusCode::OK, "ok")
        }

        async fn webhook(
            axum::extract::State(sink): axum::extract::State<Sink>,
            body: String,
        ) -> (axum::http::StatusCode, &'static str) {
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            sink.0.lock().unwrap().push(("webhook".into(), parsed));
            (axum::http::StatusCode::OK, "ok")
        }

        let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let sink = Sink(Arc::clone(&seen));
        let app = axum::Router::new()
            .route("/bot{token}/sendMessage", post(telegram))
            .route("/hook", post(webhook))
            .with_state(sink);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    use std::sync::{Arc, Mutex};

    fn notifier_with_webhook(base: &str, webhook_url: &str, root: &Path) -> Notifier {
        let mut cfg = alert_cfg();
        cfg.webhook.url = webhook_url.to_string();
        Notifier::from_config(&cfg, root).with_api_base(base)
    }

    use std::path::Path;

    /// 双通道送达 + 冷却状态写入 + payload 形状（key/level/ts/source）。
    #[tokio::test]
    async fn both_channels_deliver_and_the_cooldown_is_recorded() {
        let (base, seen) = spawn_stub().await;
        let root = tempfile::tempdir().unwrap();
        let notifier = notifier_with_webhook(&base, &format!("{base}/hook"), root.path());
        let results = notifier.send("k1", "标题", "正文", "error").await;
        assert_eq!(results, vec!["telegram: 已发送", "webhook: 已发送"]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, "telegram");
        assert_eq!(seen[0].1["token"], "stub-telegram-token-33");
        assert!(seen[0].1["body"]["text"]
            .as_str()
            .unwrap()
            .contains("[mihomo-test ERROR] 标题"));
        assert_eq!(seen[1].0, "webhook");
        assert_eq!(seen[1].1["key"], "k1");
        assert_eq!(seen[1].1["level"], "error");
        assert_eq!(seen[1].1["source"], "mihomo-probe");
        drop(seen);
        // 冷却状态落盘（送达才写）。
        let state: Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("data/alert-state.json")).unwrap(),
        )
        .unwrap();
        assert!(state["k1"].as_f64().unwrap() > 0.0);
    }

    /// 冷却窗口内同 key 跳过；送达失败不消耗冷却窗（投递故障 ≠ 四小时静音）。
    #[tokio::test]
    async fn cooldown_skips_within_the_window_and_failures_do_not_consume_it() {
        let (base, _seen) = spawn_stub().await;
        let root = tempfile::tempdir().unwrap();
        let notifier = notifier_with_webhook(&base, &format!("{base}/hook"), root.path());
        notifier.send("k2", "t", "b", "warn").await;
        // 窗口内第二次：两个通道都跳过（空 results = 冷却跳过）。
        let results = notifier.send("k2", "t", "b", "warn").await;
        assert!(results.is_empty(), "cooldown skips: {results:?}");

        // 送达失败：双通道全指向死端口 → 全失败 → 冷却状态不写 → 下次仍发送。
        let mut dead_cfg = alert_cfg();
        dead_cfg.webhook.url = "http://127.0.0.1:1/hook".into();
        let failing = Notifier::from_config(&dead_cfg, root.path())
            .with_api_base("http://127.0.0.1:1")
            .with_alert_cfg(dead_cfg.clone());
        let results = failing.send("k3", "t", "b", "warn").await;
        assert!(results
            .iter()
            .all(|r| r.contains("HTTP") || r.contains("失败")));
        let state: Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("data/alert-state.json")).unwrap(),
        )
        .unwrap();
        assert!(
            state.get("k3").is_none(),
            "a delivery failure never consumes the window"
        );
    }

    /// 未启用 alert：send 直接空返回（Python enabled=false 同口径）。
    #[tokio::test]
    async fn a_disabled_alert_section_never_sends() {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = alert_cfg();
        cfg.enabled = false;
        let notifier = Notifier::from_config(&cfg, root.path());
        let results = notifier.send("k", "t", "b", "warn").await;
        assert!(results.is_empty());
        assert!(!root.path().join("data/alert-state.json").exists());
    }

    /// 轮末判定：suspect → error 告警；alive_floor 命中 → warn；都未命中
    /// → 无告警。
    #[tokio::test]
    async fn the_round_end_verdict_fires_the_right_levels() {
        let (base, seen) = spawn_stub().await;
        let root = tempfile::tempdir().unwrap();
        let mut cfg = alert_cfg();
        cfg.alive_floor = 10;
        let notifier =
            notifier_with_webhook(&base, &format!("{base}/hook"), root.path()).with_alert_cfg(cfg);

        maybe_alert_after_round(&notifier, true, 5).await;
        let count = |seen: &Mutex<Vec<(String, Value)>>| seen.lock().unwrap().len();
        assert_eq!(count(&seen), 2, "suspect(error) + alive_low(warn)");

        // 都未命中：suspect=false, alive=40 ≥ floor=10。
        maybe_alert_after_round(&notifier, false, 40).await;
        assert_eq!(count(&seen), 2, "no alerts when healthy");
    }
}
