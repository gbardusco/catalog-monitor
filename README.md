# Catalog Monitor

Monitor local de novos registros de compositores no catálogo do [ArrangeMe](https://www.arrangeme.com).
Você cadastra os nomes que quer acompanhar, o programa busca todos os registros existentes e, depois,
verifica uma vez por dia se apareceu algo novo. Tudo roda na sua máquina e é controlado por uma
página em `http://localhost:8787`.

Escrito em Rust (axum + SQLite embutido). Sem dependências externas em tempo de execução.

## Recursos

**Monitoramento**
- **Carga inicial completa** e ordenada do mais novo para o mais antigo, como no site.
- **Verificação automática** (24 h por padrão, ajustável). Leve: para na primeira página sem nada novo.
- **Retomada de carregamentos** — cada página é gravada ao chegar. Se a carga for interrompida (programa
  fechado, queda de rede, erro do site), ela continua de onde parou: sozinha ao abrir o programa, ou pelo
  botão **Continuar carregamento**.
- **Recarregamento geral** — percorre todas as páginas, atualiza todos os campos e detecta registros que
  **saíram do site** (marcados como "removido do site").
- **Auditoria** — aba **Histórico** com cada verificação (origem, páginas lidas, novos, erros).

**Nomes e precisão**
- **Aliases** — vários nomes para o mesmo compositor (*Lady Gaga* e *Stefani Germanotta*), consolidados
  sem duplicar músicas. Também dá para **unir** dois compositores já cadastrados.
- **Nome em destaque** — escolha (⭐) qual nome aparece como principal.
- **Nomes só visuais** — exibe um nome (ex.: *Tinashe*) sem buscá-lo no site, evitando homônimos.
- **Casamento por nome inteiro** — "Amy Lee" **não** casa com "Amy Lee Roberts". Cada registro recebe um
  tipo de match: *créditos*, *artista*, *título* ou *sem match* (a busca do site traz muita coisa que não é
  do compositor; por padrão a tela mostra só *créditos*).

**Uso diário**
- **Popup** para cadastrar e editar compositores e seus nomes.
- **Barra lateral para muitos compositores** — busca por nome **ou alias** (sem distinguir acentos nem
  maiúsculas; tecla `/` foca a busca), ordenação (A–Z, Z–A, novidades recentes, mais registros, verificados
  há mais tempo, adicionados recentemente), **grupos por inicial recolhíveis** (o compositor selecionado
  nunca some), filtros "Novos (7 dias)" e "Com pendência". As preferências ficam salvas no navegador.
- **Tabela** com busca, ordenação, filtros por tipo de match, "só novos" e "esconder removidos".
- **Paginação completa** (tabela de cada compositor e aba **Novos**): primeira/anterior/próxima/última, números
  de página com reticências, "ir para a página" e tamanho da página (25 a 500), no topo e no rodapé.
  A paginação é feita no servidor, então continua rápida com milhares de registros.
- **Conexões entre registros** — nomes nos créditos que são compositores monitorados viram links; painel
  com **músicas em comum** (clique para filtrar) e **co-autores frequentes** com botão "monitorar".
- **Exportação** CSV/JSON (por compositor ou geral) e **feed Atom** dos novos em `/feed.xml`.
- Atualização em tempo real (SSE) sem apagar o que você está digitando. Tema claro/escuro.
- **Português, English e Español**, com detecção automática do idioma do navegador.

**Avisos fora do navegador** (aba **Configurações**, com botão "Enviar teste")
- Aviso do desktop (`notify-send`), **Discord** (webhook), **Telegram** (bot), **ntfy** (push no celular) e
  **webhook genérico** (JSON). Registros novos só são avisados uma vez (`notified`).

## Como rodar

Requer [Rust](https://rustup.rs) e um compilador C (para o SQLite embutido).

```sh
git clone https://github.com/gbardusco/catalog-monitor.git
cd catalog-monitor
cargo run --release
```

Abra <http://localhost:8787>. O primeiro build leva alguns minutos.

### Como serviço automático (Linux, systemd de usuário)

```sh
./deploy/install.sh
```

Compila, instala o binário em `~/.local/bin`, copia um `catalog.db` existente para
`~/.local/share/catalog-monitor/` (sem sobrescrever) e ativa o serviço, que inicia no login e reinicia
sozinho em caso de falha. Pare qualquer instância aberta com `cargo run` antes.

```sh
journalctl --user -u catalog-monitor -f          # logs
systemctl --user restart catalog-monitor         # reiniciar
systemctl --user disable --now catalog-monitor   # desativar
loginctl enable-linger $USER                     # (opcional) rodar sem sessão gráfica aberta
```

`./deploy/install.sh --no-start` só instala os arquivos.

## Configuração

| Variável                | Padrão                      | Descrição                                           |
|-------------------------|-----------------------------|-----------------------------------------------------|
| `PORT`                  | `8787`                      | Porta do servidor local                             |
| `CATALOG_DB`            | `catalog.db`                | Caminho do arquivo SQLite                           |
| `CATALOG_LANG`          | `$LANG`                     | Idioma dos avisos (`pt`, `en`, `es`)                |
| `CATALOG_BASE_URL`      | `https://www.arrangeme.com` | URL base do site (usada nos testes)                 |
| `CATALOG_ALLOWED_HOSTS` | —                           | Hosts extras aceitos no cabeçalho `Host`            |

Intervalo de verificação, pausa entre páginas e canais de aviso são ajustados na aba **Configurações**.

**Segurança.** O servidor escuta só em `127.0.0.1` e recusa requisições cujo `Host`/`Origin` não sejam
locais (bloqueia CSRF e DNS rebinding). URLs de webhook e tokens ficam em texto no SQLite local, nunca
aparecem em logs nem são devolvidos pela API (ela informa apenas "configurado: sim/não").
Para backup, copie o `catalog.db`; uma migração de esquema cria antes `catalog.db.bak-vN` automaticamente.

## Migrando do `arrangeme-monitor` (projeto antigo)

```sh
catalog-monitor import --toml artists.toml
```

Cada `[[artist]]` vira um compositor: `alias` é o nome em destaque e cada item de `writers` um nome
pesquisável (se o alias não estiver nos writers, entra como só visual). É idempotente. A carga inicial
roda quando o servidor estiver ativo. O `monitor.db` antigo não é importado (usa outro identificador de
música); o histórico é refeito pela carga inicial.

## Como funciona

1. Cada compositor tem nomes pesquisáveis e/ou só visuais; um deles é o destaque.
2. Cada nome pesquisável é consultado em `arrangeme.com/search?q=…&sort=date_desc&page=N` (50 por
   página), com pausa e jitter, rotação de User-Agent e respeito a `429`/`Retry-After`.
3. Cada página é gravada num checkpoint (`resume_page`). Ao retomar, volta uma página; duplicatas são ignoradas.
4. Na primeira varredura de um nome tudo é carga inicial; depois, IDs desconhecidos são **novos**.
5. Se o site responder com total > 0 mas nenhuma linha reconhecível, a varredura falha com erro explícito
   (indício de mudança de layout) em vez de gravar dados incorretos.

O site não oferece `ETag`/`Last-Modified`, parâmetro de tamanho de página nem API JSON; por isso a economia
de rede vem da parada antecipada (≈ 1 requisição por nome por verificação).

## Desenvolvimento

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Os testes incluem um **servidor falso do ArrangeMe** (`tests/common`) que cobre carga inicial, retomada após
falha, parada antecipada, detecção de novos, alias novo, removidos, casamento por nome inteiro, conexões,
bloqueio de `Host` e avisos por webhook. A CI roda isso a cada push (`.github/workflows/ci.yml`).

Módulos: `scraper` (rede e parsing), `scan` (varredura e agendador), `db` (migrações versionadas por
`PRAGMA user_version`), `matching`, `notify`, `api`, `import`, `error`.

### Idiomas

As traduções ficam no objeto `I18N` no início do script de `static/index.html`. Para adicionar um idioma,
copie um bloco (por exemplo `en`), traduza os valores, registre-o em `LOCALES` e inclua uma `<option>` em
`#lang`. O backend devolve apenas códigos (`err.duplicate`, `scan.initial|…`), traduzidos na página.

## API (usada pela página)

| Método e rota                                    | Função                                                                 |
|--------------------------------------------------|------------------------------------------------------------------------|
| `GET/POST /api/composers`                        | Listar / criar (`{name, terms:[{term, visual}]}`)                      |
| `DELETE /api/composers/:id`                      | Excluir                                                                |
| `POST /api/composers/:id/aliases`                | Adicionar nome (`{alias, visual}`)                                     |
| `POST /api/composers/:id/featured`               | Definir o nome em destaque (`{term_id}`)                               |
| `DELETE/PATCH /api/aliases/:id`                  | Remover / alternar "só visual" (`{visual}`)                            |
| `POST /api/composers/:id/merge`                  | Unir em outro compositor (`{into}`)                                    |
| `POST /api/composers/:id/scan`                   | Verificar / continuar carga (`?full=1` recarrega tudo)                 |
| `GET /api/composers/:id/songs`                   | Registros: `q, kind, new, hide_removed, with, sort, dir, page, limit`  |
| `GET /api/composers/:id/connections`             | Músicas em comum e co-autores frequentes                               |
| `GET /api/composers/:id/export`                  | `?format=csv` ou `?format=json`                                        |
| `POST /api/scan`                                 | Verificar todos (`?full=1` recarregamento geral)                       |
| `GET /api/new`, `GET /api/runs`                  | Histórico de novos / auditoria das verificações                        |
| `GET /api/export`, `GET /feed.xml`               | Exportação geral / feed Atom                                           |
| `GET/PUT /api/settings`, `POST /api/notify/test` | Configurações e teste de avisos                                        |
| `GET /api/events`                                | Eventos em tempo real (SSE)                                            |

## Aviso

Projeto pessoal, sem relação com o ArrangeMe. Ele lê páginas públicas de busca em ritmo baixo
(uma verificação por dia por compositor); respeite os termos de uso do site.
