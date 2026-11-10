use chrono::NaiveDate;
use scraper::{ElementRef, Html, Selector};
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

const BASE: &str = "https://www.arrangeme.com/search";
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
            uploads: v.attr("data-arrangements").and_then(|x| x.parse().ok()).unwrap_or(0),
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

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) CatalogMonitor/0.1 (uso pessoal)")
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

async fn fetch(client: &reqwest::Client, term: &str, page: usize) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(3 * attempt)).await;
        }
        let r = client
            .get(BASE)
            .query(&[("q", term), ("sort", "date_desc"), ("page", &page.to_string())])
            .send()
            .await;
        match r {
            Ok(resp) if resp.status().is_success() => {
                return resp.text().await.map_err(|e| e.to_string())
            }
            Ok(resp) => last = format!("HTTP {}", resp.status()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("fetch failed for \"{term}\" (page {page}): {last}"))
}

/// Percorre as páginas de resultados de um termo (do mais novo ao mais velho) a partir de
/// `start_page`. A cada página chama `on_page(página, total_no_site, linhas)`; se devolver
/// `Ok(false)` a busca para (usado para parar cedo quando nada de novo aparece).
pub async fn search_pages(
    client: &reqwest::Client,
    term: &str,
    start_page: usize,
    on_page: &mut (dyn FnMut(usize, usize, &[Song]) -> Result<bool, String> + Send),
) -> Result<(), String> {
    for page in start_page.max(1)..=MAX_PAGES {
        let html = fetch(client, term, page).await?;
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
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    Ok(())
}
