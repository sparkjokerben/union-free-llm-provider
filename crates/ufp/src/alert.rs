//! 邮件告警。
//!
//! 只发「需要人管」的事件：key 被上游拒绝、整层/整池不可用、搜索后端全挂、
//! 写库开始丢数据。同类事件按 `alerts.minIntervalSecs` 限频，避免一次故障刷满邮箱。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::store::settings::AlertSettings;

pub struct Alerter {
    /// 上次发送时刻（按事件类型限频）。
    last: Mutex<HashMap<String, Instant>>,
}

impl Default for Alerter {
    fn default() -> Self {
        Self::new()
    }
}

impl Alerter {
    pub fn new() -> Self {
        Self {
            last: Mutex::new(HashMap::new()),
        }
    }

    /// 发一封告警邮件（异步、限频；配置不全时静默跳过）。
    pub fn notify(&self, settings: &AlertSettings, kind: &str, subject: &str, body: &str) {
        if !settings.smtp.enabled || settings.smtp.to.is_empty() || settings.smtp.host.is_empty() {
            return;
        }
        let interval = Duration::from_secs(settings.min_interval_secs.max(60));
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if let Some(prev) = last.get(kind) {
                if now.duration_since(*prev) < interval {
                    tracing::debug!(kind, "同类告警在限频窗口内，跳过");
                    return;
                }
            }
            last.insert(kind.to_string(), now);
        }
        let smtp = settings.smtp.clone();
        let subject = subject.to_string();
        let body = body.to_string();
        let kind = kind.to_string();
        // 告警路径绝不能 panic：没有异步运行时（例如单元测试）时只记一条日志。
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                tracing::debug!(kind, "当前没有异步运行时，跳过告警发送");
                return;
            }
        };
        handle.spawn(async move {
            if let Err(e) = send(&smtp, &subject, &body).await {
                tracing::warn!(kind, error = %e, "告警邮件发送失败");
            } else {
                tracing::info!(kind, "已发送告警邮件");
            }
        });
    }
}

async fn send(
    smtp: &crate::store::settings::SmtpSettings,
    subject: &str,
    body: &str,
) -> Result<(), String> {
    let from: Mailbox = smtp
        .from
        .parse()
        .map_err(|e| format!("发件人地址不合法（{}）：{e}", smtp.from))?;
    let mut builder = Message::builder().from(from.clone()).subject(subject);
    for to in &smtp.to {
        let mailbox: Mailbox = to
            .parse()
            .map_err(|e| format!("收件人地址不合法（{to}）：{e}"))?;
        builder = builder.to(mailbox);
    }
    let email = builder
        .body(format!("{body}\n\n—— ufp 网关"))
        .map_err(|e| format!("构造邮件失败：{e}"))?;

    let mut transport = match smtp.security.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host)
            .map_err(|e| format!("连接 SMTP 失败：{e}"))?,
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host),
        // 默认 STARTTLS
        _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)
            .map_err(|e| format!("连接 SMTP 失败：{e}"))?,
    }
    .port(smtp.port);
    if !smtp.username.is_empty() {
        transport = transport.credentials(Credentials::new(
            smtp.username.clone(),
            smtp.password.clone(),
        ));
    }
    transport
        .build()
        .send(email)
        .await
        .map_err(|e| format!("SMTP 发送失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::settings::SmtpSettings;

    #[test]
    fn 未配置时静默跳过() {
        let alerter = Alerter::new();
        let settings = AlertSettings::default();
        // 不会 panic，也不会尝试发送
        alerter.notify(&settings, "test", "标题", "正文");
    }

    #[test]
    fn 同类告警被限频() {
        let alerter = Alerter::new();
        let settings = AlertSettings {
            smtp: SmtpSettings {
                enabled: true,
                host: "127.0.0.1".into(),
                to: vec!["a@example.com".into()],
                from: "ufp@example.com".into(),
                ..Default::default()
            },
            min_interval_secs: 3600,
        };
        alerter.notify(&settings, "key_disabled", "标题", "正文");
        // 第二次应被限频挡住：last 里有记录，且间隔未到
        alerter.notify(&settings, "key_disabled", "标题", "正文");
        let last = alerter.last.lock().unwrap();
        assert!(last.contains_key("key_disabled"));
    }
}
