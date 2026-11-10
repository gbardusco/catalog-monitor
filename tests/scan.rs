mod common;

use catalog_monitor::{
    api,
    app::App,
    db::{self, NewTerm},
    scan,
};
use common::{song, spawn};
use std::sync::Arc;

/// App apontado para o servidor falso, sem pausas entre páginas.
async fn setup() -> (Arc<App>, common::Shared) {
    let (url, state) = spawn().await;
    let app = App::new(db::open_memory().unwrap(), &url, 0);
    (app, state)
}

fn add(app: &Arc<App>, name: &str, terms: &[&str]) -> i64 {
    let terms: Vec<NewTerm> = terms
        .iter()
        .map(|t| NewTerm {
            term: t.to_string(),
            visual: false,
        })
        .collect();
    db::add_composer(&mut app.db.lock().unwrap(), name, &terms).unwrap()
}

/// n músicas, do mais novo (id maior) ao mais velho
fn songs(range: std::ops::RangeInclusive<u32>, credits: &str) -> Vec<common::FakeSong> {
    range.rev().map(|n| song(n, credits)).collect()
}

fn count(app: &Arc<App>, sql: &str) -> i64 {
    app.db
        .lock()
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn carga_inicial_nao_marca_novos() {
    let (app, st) = setup().await;
    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(1..=120, "Dua Lipa"));
    let id = add(&app, "Dua Lipa", &[]);
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits"), 120);
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits WHERE is_new=1"), 0);
    assert_eq!(
        count(
            &app,
            "SELECT COUNT(*) FROM terms WHERE baselined=1 AND resume_page=0"
        ),
        1
    );
    assert_eq!(st.lock().unwrap().requests.len(), 3); // 120 registros = 3 páginas
    assert_eq!(count(&app, "SELECT COUNT(*) FROM runs WHERE ok=1"), 1);
}

#[tokio::test]
async fn retoma_carga_interrompida_na_pagina_com_falha() {
    let (app, st) = setup().await;
    {
        let mut s = st.lock().unwrap();
        s.data.insert("dua lipa".into(), songs(1..=120, "Dua Lipa"));
        s.fail_once.push(("dua lipa".into(), 2)); // página 2 falha (3 tentativas esgotam? só 1 falha)
    }
    let id = add(&app, "Dua Lipa", &[]);
    // a falha é única: a retentativa interna do scraper (espera 3s) a supera
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits"), 120);

    // agora simula falha persistente: página 2 sempre falha -> carga fica incompleta
    let (app2, st2) = setup().await;
    st2.lock()
        .unwrap()
        .data
        .insert("x".into(), songs(1..=120, "X"));
    let id2 = add(&app2, "X", &[]);
    {
        // 4 falhas esgotam as 4 tentativas da página 2
        let mut s = st2.lock().unwrap();
        for _ in 0..4 {
            s.fail_once.push(("x".into(), 2));
        }
    }
    assert!(scan::scan_composer(&app2, id2, false, "manual")
        .await
        .is_err());
    assert_eq!(count(&app2, "SELECT COUNT(*) FROM hits"), 50); // só a página 1 foi salva
    assert_eq!(count(&app2, "SELECT resume_page FROM terms"), 2);
    assert_eq!(count(&app2, "SELECT baselined FROM terms"), 0);

    st2.lock().unwrap().requests.clear();
    scan::scan_composer(&app2, id2, false, "resume")
        .await
        .unwrap();
    assert_eq!(count(&app2, "SELECT COUNT(*) FROM hits"), 120);
    assert_eq!(count(&app2, "SELECT COUNT(*) FROM hits WHERE is_new=1"), 0); // continua sendo carga inicial
    assert_eq!(count(&app2, "SELECT baselined FROM terms"), 1);
    // retomou da página 1 (recua uma) em diante, sem refazer nada além disso
    let pages: Vec<usize> = st2
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|(_, p)| *p)
        .collect();
    assert_eq!(pages, vec![1, 2, 3]);
}

#[tokio::test]
async fn verificacao_normal_para_cedo_e_detecta_novos() {
    let (app, st) = setup().await;
    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(1..=120, "Dua Lipa"));
    let id = add(&app, "Dua Lipa", &[]);
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();

    // nada novo: só a página 1 é lida
    st.lock().unwrap().requests.clear();
    scan::scan_composer(&app, id, false, "auto").await.unwrap();
    assert_eq!(st.lock().unwrap().requests.len(), 1);
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits WHERE is_new=1"), 0);

    // 2 músicas novas no topo
    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(1..=122, "Dua Lipa"));
    scan::scan_composer(&app, id, false, "auto").await.unwrap();
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits WHERE is_new=1"), 2);
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits"), 122);
    // sem canais remotos e com notify-send ausente/presente, o registro fica marcado como avisado
    assert_eq!(
        count(
            &app,
            "SELECT COUNT(*) FROM hits WHERE is_new=1 AND notified=0"
        ),
        0
    );

    // a lista de compositores expõe quando foi criado e quando apareceu algo novo (ordenação da barra lateral)
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let resp = api::router(app.clone())
        .oneshot(
            Request::builder()
                .uri("/api/composers")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v[0]["created_at"].as_i64().unwrap() > 0);
    assert!(v[0]["last_new_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn alias_novo_faz_carga_propria_sem_marcar_novos() {
    let (app, st) = setup().await;
    {
        let mut s = st.lock().unwrap();
        s.data
            .insert("lady gaga".into(), songs(1..=10, "Lady Gaga"));
        s.data.insert(
            "stefani germanotta".into(),
            songs(100..=105, "Stefani Germanotta"),
        );
    }
    let id = add(&app, "Lady Gaga", &[]);
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();
    {
        let db = app.db.lock().unwrap();
        db.execute(
            "INSERT INTO terms(composer_id, term, norm, fold) VALUES (?, 'Stefani Germanotta', 'stefani germanotta', 'stefani germanotta')",
            [id],
        )
        .unwrap();
    }
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits"), 16); // consolidado em um compositor
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits WHERE is_new=1"), 0);
}

#[tokio::test]
async fn recarregamento_geral_marca_removidos() {
    let (app, st) = setup().await;
    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(1..=60, "Dua Lipa"));
    let id = add(&app, "Dua Lipa", &[]);
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();
    // o site "remove" as 5 mais antigas
    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(6..=60, "Dua Lipa"));
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // last_seen_at < início da nova execução
    scan::scan_composer(&app, id, true, "manual").await.unwrap();
    assert_eq!(
        count(
            &app,
            "SELECT COUNT(*) FROM hits WHERE removed_at IS NOT NULL"
        ),
        5
    );
    // verificação normal não mexe nisso (para cedo)
    scan::scan_composer(&app, id, false, "auto").await.unwrap();
    assert_eq!(
        count(
            &app,
            "SELECT COUNT(*) FROM hits WHERE removed_at IS NOT NULL"
        ),
        5
    );
}

#[tokio::test]
async fn casamento_por_nome_inteiro() {
    let (app, st) = setup().await;
    {
        let mut s = st.lock().unwrap();
        // 1: crédito exato; 2: nome maior que contém o termo; 3: só no título
        s.data.insert(
            "amy lee".into(),
            vec![song(1, "Amy Lee, Ben Moody"), song(2, "Amy Lee Roberts"), {
                let mut x = song(3, "Alguém");
                x.title = "Amy Lee Tribute".into();
                x
            }],
        );
    }
    let id = add(&app, "Amy Lee", &[]);
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();

    let router = api::router(app.clone());
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let resp = router
        .oneshot(
            Request::builder()
                .uri(format!("/api/composers/{id}/songs?limit=10"))
                .header("host", "localhost:8787")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let kind = |sid: &str| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["catalog_id"] == sid)
            .unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(kind("SC000000001"), "credit");
    assert_eq!(kind("SC000000002"), "none"); // "Amy Lee Roberts" não casa com "Amy Lee"
    assert_eq!(kind("SC000000003"), "title");
}

#[tokio::test]
async fn cruza_compositores_por_creditos_em_comum() {
    let (app, st) = setup().await;
    {
        let mut s = st.lock().unwrap();
        s.data.insert(
            "dua lipa".into(),
            vec![song(1, "Dua Lipa, Ali Tamposi"), song(2, "Dua Lipa")],
        );
        s.data.insert(
            "ali tamposi".into(),
            vec![
                song(1, "Dua Lipa, Ali Tamposi"),
                song(3, "Ali Tamposi, Fulano Beltrano"),
            ],
        );
    }
    let dua = add(&app, "Dua Lipa", &[]);
    let ali = add(&app, "Ali Tamposi", &[]);
    scan::scan_composer(&app, dua, false, "manual")
        .await
        .unwrap();
    scan::scan_composer(&app, ali, false, "manual")
        .await
        .unwrap();

    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let get = |uri: String| {
        let r = api::router(app.clone());
        async move {
            let resp = r
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("host", "localhost")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&b).unwrap()
        }
    };
    let c = get(format!("/api/composers/{dua}/connections")).await;
    assert_eq!(c["shared"][0]["name"], "Ali Tamposi");
    assert_eq!(c["shared"][0]["count"], 1);
    let c2 = get(format!("/api/composers/{ali}/connections")).await;
    assert_eq!(c2["cowriters"][0]["name"], "Fulano Beltrano");

    let s = get(format!("/api/composers/{dua}/songs?with={ali}")).await;
    assert_eq!(s["total"], 1);
    let links = s["items"][0]["credit_links"].as_array().unwrap();
    assert!(links
        .iter()
        .any(|l| l["name"] == "Ali Tamposi" && l["composer_id"] == ali));
    assert_eq!(s["items"][0]["also"][0]["name"], "Ali Tamposi");
}

#[tokio::test]
async fn bloqueia_host_e_origin_nao_locais() {
    let (app, _st) = setup().await;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let status = |host: &'static str, origin: Option<&'static str>| {
        let r = api::router(app.clone());
        async move {
            let mut b = Request::builder()
                .uri("/api/composers")
                .header("host", host);
            if let Some(o) = origin {
                b = b.header("origin", o);
            }
            r.oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    assert_eq!(status("localhost:8787", None).await, 200);
    assert_eq!(
        status("127.0.0.1:8787", Some("http://localhost:8787")).await,
        200
    );
    assert_eq!(status("evil.com", None).await, 403);
    assert_eq!(
        status("localhost:8787", Some("https://evil.com")).await,
        403
    );
}

#[tokio::test]
async fn webhook_generico_recebe_novos_e_segredos_nao_vazam() {
    use axum::{extract::State, routing::post, Json, Router};
    use std::sync::Mutex;
    let got: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
    async fn hook(
        State(g): State<Arc<Mutex<Vec<serde_json::Value>>>>,
        Json(v): Json<serde_json::Value>,
    ) {
        g.lock().unwrap().push(v);
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_url = format!("http://{}/hook", l.local_addr().unwrap());
    let router = Router::new()
        .route("/hook", post(hook))
        .with_state(got.clone());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });

    let (app, st) = setup().await;
    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(1..=10, "Dua Lipa"));
    {
        let db = app.db.lock().unwrap();
        db::set_setting(&db, "notify_webhook", &hook_url).unwrap();
        db::set_setting(&db, "notify_desktop", "0").unwrap();
    }
    let id = add(&app, "Dua Lipa", &[]);
    scan::scan_composer(&app, id, false, "manual")
        .await
        .unwrap();
    assert!(got.lock().unwrap().is_empty()); // carga inicial não avisa

    st.lock()
        .unwrap()
        .data
        .insert("dua lipa".into(), songs(1..=11, "Dua Lipa"));
    scan::scan_composer(&app, id, false, "auto").await.unwrap();
    let msgs = got.lock().unwrap().clone();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["composer"], "Dua Lipa");
    assert_eq!(msgs[0]["count"], 1);
    assert_eq!(msgs[0]["records"][0]["title"], "Song 11");
    assert_eq!(count(&app, "SELECT COUNT(*) FROM hits WHERE notified=0"), 0);

    // a API de configurações nunca devolve a URL
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let resp = api::router(app.clone())
        .oneshot(
            Request::builder()
                .uri("/api/settings")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("\"webhook\":true"));
    assert!(!body.contains(&hook_url));
}
