use catalog_monitor::scraper::parse_page;

#[test]
fn le_total_e_linhas_da_pagina_real() {
    let html = include_str!("fixtures/search_page.html");
    let (total, songs) = parse_page(html);
    assert_eq!(total, Some(187));
    assert_eq!(songs.len(), 2);
    let s = &songs[0];
    assert_eq!(s.catalog_id, "SC012493611");
    assert_eq!(s.title, "Pixie Dust");
    assert_eq!(s.date_added, "2026-11-07");
    assert!(s
        .credits
        .starts_with("Henry Russell Walter, Ali Tamposi, Dua Lipa"));
    assert!(s.medley);
    assert_eq!(
        s.url,
        "https://www.arrangeme.com/sell/copyrighted/upload/SC012493611"
    );
}

#[test]
fn pagina_sem_resultados() {
    let (total, songs) = parse_page("<html><body>nada</body></html>");
    assert_eq!(total, None);
    assert!(songs.is_empty());
}
