//! Servidor falso do ArrangeMe para os testes de integração.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct FakeSong {
    pub id: String,
    pub title: String,
    pub credits: String,
    pub artist: String,
    pub date: String, // "Nov 7, 2026"
}

pub fn song(n: u32, credits: &str) -> FakeSong {
    FakeSong {
        id: format!("SC{n:09}"),
        title: format!("Song {n}"),
        credits: credits.to_string(),
        artist: String::new(),
        // datas decrescentes com o número: n maior = mais novo
        date: format!("Jan {}, 2026", (n % 28) + 1),
    }
}

#[derive(Default)]
pub struct FakeState {
    /// termo (minúsculo) -> músicas, do mais novo ao mais velho
    pub data: HashMap<String, Vec<FakeSong>>,
    /// (termo, página) que devem falhar uma vez com HTTP 500
    pub fail_once: Vec<(String, usize)>,
    /// páginas pedidas, em ordem: (termo, página)
    pub requests: Vec<(String, usize)>,
}

pub type Shared = Arc<Mutex<FakeState>>;

async fn search(State(st): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let term = q.get("q").cloned().unwrap_or_default().to_lowercase();
    let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let mut st = st.lock().unwrap();
    st.requests.push((term.clone(), page));
    if let Some(i) = st
        .fail_once
        .iter()
        .position(|(t, p)| *t == term && *p == page)
    {
        st.fail_once.remove(i);
        return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    }
    let all = st.data.get(&term).cloned().unwrap_or_default();
    let rows: String = all
        .iter()
        .skip((page - 1) * 50)
        .take(50)
        .map(|s| {
            format!(
                r#"<tr data-arrange-url="/sell/copyrighted/upload/{id}" data-arrangements="0" data-copyright="c" data-song-catalog-id="{id}" data-medley-approved="Y">
                <td data-label="TITLE"><strong>{title}</strong></td><td data-label="SHOW">&nbsp;</td>
                <td data-label="ARTIST">{artist}</td><td data-label="SONG CREDITS">By {credits}</td>
                <td data-label="DATE ADDED">{date}</td><td data-label="UPLOADS">0</td></tr>"#,
                id = s.id, title = s.title, artist = s.artist, credits = s.credits, date = s.date
            )
        })
        .collect();
    format!(
        "<html><body><strong>\nTotal Results:\n{}\n</strong><table><tbody>{rows}</tbody></table></body></html>",
        all.len()
    )
    .into_response()
}

/// Sobe o servidor falso numa porta livre e devolve (url_base, estado).
pub async fn spawn() -> (String, Shared) {
    let state: Shared = Arc::new(Mutex::new(FakeState::default()));
    let app = Router::new()
        .route("/search", get(search))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, state)
}
