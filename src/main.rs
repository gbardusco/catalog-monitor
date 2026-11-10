use catalog_monitor::{api, app::App, db, import, scan, scraper};

const HELP: &str = "Catalog Monitor

USO:
  catalog-monitor                       inicia o servidor local (http://localhost:8787)
  catalog-monitor import --toml ARQ     importa artistas de um artists.toml (projeto antigo)
  catalog-monitor --help

VARIÁVEIS:
  PORT                  porta do servidor (8787)
  CATALOG_DB            arquivo do banco SQLite (catalog.db)
  CATALOG_LANG          idioma da notificação do desktop (pt, en, es)
  CATALOG_BASE_URL      URL base do site (https://www.arrangeme.com)
  CATALOG_ALLOWED_HOSTS hosts extras aceitos no cabeçalho Host, separados por vírgula";

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    println!("encerrando… (cargas em andamento continuam de onde pararam na próxima vez)");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let db_path = std::env::var("CATALOG_DB").unwrap_or_else(|_| "catalog.db".into());

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    if args.first().map(String::as_str) == Some("import") {
        let Some(path) = args
            .iter()
            .position(|a| a == "--toml")
            .and_then(|i| args.get(i + 1))
        else {
            eprintln!("uso: catalog-monitor import --toml ARQUIVO.toml");
            std::process::exit(2);
        };
        let mut conn = db::open(&db_path).expect("abrir banco");
        match import::import_toml(&mut conn, path) {
            Ok(s) => {
                println!("importados: {} {:?}", s.created.len(), s.created);
                println!(
                    "ignorados (já existiam): {} {:?}",
                    s.skipped.len(),
                    s.skipped
                );
                println!("A carga inicial roda quando o servidor estiver ativo.");
            }
            Err(e) => {
                eprintln!("erro: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let base = std::env::var("CATALOG_BASE_URL").unwrap_or_else(|_| scraper::DEFAULT_BASE.into());
    let delay: u64 = std::env::var("CATALOG_DELAY_MS")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1200);
    let conn = db::open(&db_path).expect("abrir banco");
    let app = App::new(conn, &base, delay);

    tokio::spawn(scan::scheduler(app.clone()));

    let addr = format!("127.0.0.1:{port}");
    println!("Catalog Monitor rodando em http://{addr}  (banco: {db_path})");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}
