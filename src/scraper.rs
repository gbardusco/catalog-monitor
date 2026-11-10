use chrono::NaiveDate;
use scraper::{ElementRef, Html, Selector};
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

pub const DEFAULT_BASE: &str = "https://www.arrangeme.com";

pub const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone)]
pub struct Song {
    pub catalog_id: String,
    pub title: String,
    pub show: String,
    pub artist: String,
    pub credits: String,
    /// ISO (YYYY-MM-DD) quando consegue interpretar; senão o texto original.
    pub date_added: String,
    pub medley: bool,
    pub uploads: i64,
    pub copyright: String,
    pub url: String,
}

pub fn normalize(s: &str) -> String {
    s.nfd()
        .filter(|c| !('\u{0300}'..='\u{036f}').contains(c))
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn text(el: ElementRef) -> String {
    el.text()
        .collect::<String>()
        .replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_page(html: &str) -> (Option<usize>, Vec<Song>) {
    let doc = Html::parse_document(html);
    let row_sel = Selector::parse("tr[data-song-catalog-id]").unwrap();
    let td_sel = Selector::parse("td").unwrap();

    let total = html.find("Total Results:").and_then(|i| {
        let digits: String = html[i + 14..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    });

    let mut songs = Vec::new();
    for row in doc.select(&row_sel) {
        let v = row.value();
        let id = v.attr("data-song-catalog-id").unwrap_or("").to_string();
        if id.is_empty() {
            continue;
        }
        let mut s = Song {
            catalog_id: id,
            title: String::new(),
            show: String::new(),
            artist: String::new(),
            credits: String::new(),
            date_added: String::new(),
            medley: v.attr("data-medley-approved") == Some("Y"),
            uploads: v
                .attr("data-arrangements")
                .and_then(|x| x.parse().ok())
                .unwrap_or(0),
            copyright: v.attr("data-copyright").unwrap_or("").to_string(),
            url: v
                .attr("data-arrange-url")
                .map(|u| format!("https://www.arrangeme.com{u}"))
                .unwrap_or_default(),
        };
        for td in row.select(&td_sel) {
            let label = td.value().attr("data-label").unwrap_or("");
            let t = text(td);
            match label {
                "TITLE" => s.title = t,
                "SHOW" => s.show = t,
                "ARTIST" => s.artist = t,
                "SONG CREDITS" => s.credits = t.strip_prefix("By ").unwrap_or(&t).to_string(),
                "DATE ADDED" => {
                    s.date_added = NaiveDate::parse_from_str(&t, "%b %d, %Y")
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .unwrap_or(t)
                }
                _ => {}
            }
        }
        songs.push(s);
    }
    (total, songs)
}

/// Cliente HTTP + URL base (configurável para testes com servidor falso).
pub struct Fetcher {
    pub client: reqwest::Client,
    pub base: String,
}

impl Fetcher {
    pub fn new(base: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("client");
        Fetcher {
            client,
            base: base.trim_end_matches('/').to_string(),
        }
    }
}

const USER_AGENTS: [&str; 4] = [
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
];

fn nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

/// Espera `base` + até 1/3 de jitter.
fn jittered(base: Duration) -> Duration {
    base + Duration::from_millis(nanos() % (base.as_millis() as u64 / 3 + 1))
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s.min(120)))
}

async fn fetch(f: &Fetcher, term: &str, page: usize) -> Result<String, String> {
    let mut last = String::new();
    let mut wait = Duration::ZERO;
    for attempt in 0..4u32 {
        if attempt > 0 {
            tokio::time::sleep(jittered(wait)).await;
        }
        wait = Duration::from_secs(3 << attempt); // 3s, 6s, 12s...
        let r = f
            .client
            .get(format!("{}/search", f.base))
            .header(
                reqwest::header::USER_AGENT,
                USER_AGENTS[(nanos() as usize) % USER_AGENTS.len()],
            )
            .query(&[
                ("q", term),
                ("sort", "date_desc"),
                ("page", &page.to_string()),
            ])
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                return resp.text().await.map_err(|e| e.to_string())
            }
            Ok(resp) => {
                let st = resp.status();
                last = format!("HTTP {st}");
                if st.as_u16() == 429 || st.as_u16() == 503 {
                    if let Some(ra) = retry_after(&resp) {
                        wait = ra; // respeita o pedido do servidor
                    }
                } else if st.is_client_error() {
                    break; // 4xx (exceto 429) não melhora tentando de novo
                }
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("fetch failed for \"{term}\" (page {page}): {last}"))
}

/// Callback chamado a cada página: (página, total no site, linhas). `Ok(false)` interrompe a busca.
pub type OnPage<'a> = dyn FnMut(usize, usize, &[Song]) -> Result<bool, String> + Send + 'a;

/// Percorre as páginas de resultados de um termo (do mais novo ao mais velho) a partir de
/// `start_page`. A cada página chama `on_page(página, total_no_site, linhas)`; se devolver
/// `Ok(false)` a busca para (usado para parar cedo quando nada de novo aparece).
pub async fn search_pages(
    f: &Fetcher,
    term: &str,
    start_page: usize,
    delay: Duration,
    on_page: &mut OnPage<'_>,
) -> Result<(), String> {
    for page in start_page.max(1)..=MAX_PAGES {
        let html = fetch(f, term, page).await?;
        let (total, songs) = parse_page(&html);
        let total = total.unwrap_or(0);
        if songs.is_empty() {
            if total > 0 && (page - 1) * PAGE_SIZE < total {
                return Err("site layout changed: total > 0 but no rows found".into());
            }
            break;
        }
        if !on_page(page, total, &songs)? {
            break;
        }
        if total > 0 && page * PAGE_SIZE >= total {
            break;
        }
        tokio::time::sleep(jittered(delay)).await;
    }
    Ok(())
}
