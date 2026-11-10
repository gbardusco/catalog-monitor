//! Avisos fora do navegador: desktop (notify-send), Discord, Telegram, ntfy e webhook genérico.
//!
//! Segredos (URLs de webhook, token do Telegram) ficam só na tabela `settings` e nunca
//! aparecem em logs nem em respostas da API; erros de rede têm a URL removida.

use crate::db;
use rusqlite::Connection;
use serde_json::json;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Record {
    pub title: String,
    pub url: String,
    pub credits: String,
    pub date_added: String,
}

#[derive(Clone, Default)]
pub struct Channels {
    pub desktop: bool,
    pub discord: Option<String>,
    pub telegram: Option<(String, String)>,
    pub ntfy: Option<String>,
    pub webhook: Option<String>,
}

impl Channels {
    pub fn load(c: &Connection) -> Channels {
        let g = |k: &str| db::get_setting(c, k).filter(|v| !v.is_empty());
        Channels {
            desktop: g("notify_desktop").map(|v| v != "0").unwrap_or(true),
            discord: g("notify_discord"),
            telegram: g("notify_telegram_token").zip(g("notify_telegram_chat")),
            ntfy: g("notify_ntfy"),
            webhook: g("notify_webhook"),
        }
    }

    pub fn remote_count(&self) -> usize {
        [
            self.discord.is_some(),
            self.telegram.is_some(),
            self.ntfy.is_some(),
            self.webhook.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub sent: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Outcome {
    /// Deve marcar os registros como avisados? Sim se algo foi enviado ou se nenhum canal
    /// *remoto* falhou. O aviso de desktop é um extra local (pode nem ter `notify-send`):
    /// falhar sozinho não deve fazer o mesmo registro ser reenviado a cada verificação.
    pub fn delivered(&self) -> bool {
        !self.sent.is_empty() || self.failed.iter().all(|(channel, _)| channel == "desktop")
    }
}

fn lang() -> &'static str {
    let l = std::env::var("CATALOG_LANG")
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_lowercase();
    if l.starts_with("pt") {
        "pt"
    } else if l.starts_with("es") {
        "es"
    } else {
        "en"
    }
}

fn headline(composer: &str, n: usize) -> String {
    let what = match lang() {
        "pt" => "novo(s) registro(s)",
        "es" => "nuevo(s) registro(s)",
        _ => "new record(s)",
    };
    format!("{composer}: {n} {what}")
}

fn more(n: usize) -> String {
    match lang() {
        "pt" => format!("… e mais {n}"),
        "es" => format!("… y {n} más"),
        _ => format!("… and {n} more"),
    }
}

/// Corpo de texto simples com até `max` registros.
fn body(records: &[Record], max: usize, with_url: bool) -> String {
    let mut lines: Vec<String> = records
        .iter()
        .take(max)
        .map(|r| {
            let mut l = format!("• {}", r.title);
            if !r.credits.is_empty() {
                l.push_str(&format!(" — {}", r.credits));
            }
            if with_url && !r.url.is_empty() {
                l.push_str(&format!("\n  {}", r.url));
            }
            l
        })
        .collect();
    if records.len() > max {
        lines.push(more(records.len() - max));
    }
    lines.join("\n")
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

fn desktop(composer: &str, records: &[Record]) -> Result<(), String> {
    std::process::Command::new("notify-send")
        .args([
            "-a",
            "Catalog Monitor",
            "-i",
            "audio-x-generic",
            &headline(composer, records.len()),
            &body(records, 5, false),
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn post_json(
    c: &reqwest::Client,
    url: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let resp = c
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

async fn with_retry<F, Fut>(mut f: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut last = String::new();
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
        }
        match f().await {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Envia o aviso por todos os canais configurados.
pub async fn dispatch(
    client: &reqwest::Client,
    ch: &Channels,
    composer: &str,
    records: &[Record],
) -> Outcome {
    let mut out = Outcome::default();
    let mut record = |name: &str, r: Result<(), String>| match r {
        Ok(()) => out.sent.push(name.into()),
        Err(e) => out.failed.push((name.into(), e)),
    };
    let head = headline(composer, records.len());

    if ch.desktop {
        record("desktop", desktop(composer, records));
    }
    if let Some(url) = &ch.discord {
        let content = clip(format!("**{head}**\n{}", body(records, 10, true)), 1900);
        record(
            "discord",
            with_retry(|| post_json(client, url, json!({"content": content}))).await,
        );
    }
    if let Some((token, chat)) = &ch.telegram {
        let base = std::env::var("CATALOG_TELEGRAM_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".into());
        let url = format!("{base}/bot{token}/sendMessage");
        let text = clip(format!("{head}\n{}", body(records, 10, true)), 4000);
        let payload = json!({"chat_id": chat, "text": text, "disable_web_page_preview": true});
        record(
            "telegram",
            with_retry(|| post_json(client, &url, payload.clone())).await,
        );
    }
    if let Some(url) = &ch.ntfy {
        let text = clip(body(records, 10, true), 3500);
        let r = with_retry(|| async {
            let resp = client
                .post(url)
                .header("Title", head.as_str())
                .header("Tags", "musical_note")
                .body(text.clone())
                .send()
                .await
                .map_err(|e| e.without_url().to_string())?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(format!("HTTP {}", resp.status()))
            }
        })
        .await;
        record("ntfy", r);
    }
    if let Some(url) = &ch.webhook {
        let payload = json!({
            "composer": composer,
            "count": records.len(),
            "records": records.iter().map(|r| json!({
                "title": r.title, "url": r.url, "credits": r.credits, "date_added": r.date_added
            })).collect::<Vec<_>>(),
        });
        record(
            "webhook",
            with_retry(|| post_json(client, url, payload.clone())).await,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(sent: &[&str], failed: &[&str]) -> Outcome {
        Outcome {
            sent: sent.iter().map(|s| s.to_string()).collect(),
            failed: failed.iter().map(|s| (s.to_string(), "x".into())).collect(),
        }
    }

    #[test]
    fn entrega_considera_so_canais_remotos() {
        assert!(out(&[], &[]).delivered()); // nenhum canal
        assert!(out(&["discord"], &["telegram"]).delivered()); // algum enviou
        assert!(out(&[], &["desktop"]).delivered()); // só o desktop falhou (sem notify-send)
        assert!(!out(&[], &["discord"]).delivered()); // remoto falhou: tenta de novo depois
        assert!(!out(&[], &["desktop", "ntfy"]).delivered());
    }
}
