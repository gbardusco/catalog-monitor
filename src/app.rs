//! Estado compartilhado da aplicação.

use crate::{db, scraper::Fetcher};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::broadcast;

#[derive(Clone, Default)]
pub struct Progress {
    pub term: String,
    pub done: usize,
    pub total: usize,
    pub step: usize,
    pub steps: usize,
}

pub struct App {
    pub db: Mutex<Connection>,
    pub fetch: Fetcher,
    /// cliente para avisos (Discord, Telegram, ntfy, webhook)
    pub out: reqwest::Client,
    /// compositor -> progresso da varredura em andamento
    pub scanning: Mutex<HashMap<i64, Progress>>,
    /// eventos para a página (SSE): "changed"
    pub events: broadcast::Sender<String>,
    pub default_delay_ms: u64,
}

impl App {
    pub fn new(conn: Connection, base_url: &str, default_delay_ms: u64) -> Arc<App> {
        let (events, _) = broadcast::channel(64);
        Arc::new(App {
            db: Mutex::new(conn),
            fetch: Fetcher::new(base_url),
            out: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("catalog-monitor")
                .build()
                .expect("client"),
            scanning: Mutex::new(HashMap::new()),
            events,
            default_delay_ms,
        })
    }

    /// Avisa a página (via SSE) que algo mudou.
    pub fn emit(&self) {
        let _ = self.events.send("changed".into());
    }

    pub fn delay(&self) -> Duration {
        let db = self.db.lock().unwrap();
        Duration::from_millis(
            db::setting_i64(&db, "delay_ms", self.default_delay_ms as i64).max(0) as u64,
        )
    }

    pub fn interval_secs(&self) -> i64 {
        let db = self.db.lock().unwrap();
        db::setting_i64(&db, "interval_hours", 24).clamp(1, 24 * 30) * 3600
    }
}
