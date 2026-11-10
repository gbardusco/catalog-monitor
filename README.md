# Catalog Monitor

Monitor local de novos registros de compositores no catálogo do [ArrangeMe](https://www.arrangeme.com).
Você cadastra os nomes que quer acompanhar, o programa busca todos os registros existentes e, depois,
verifica uma vez por dia se apareceu algo novo. Tudo roda na sua máquina e é controlado por uma
página em `http://localhost:8787`.

Escrito em Rust (axum + SQLite embutido), sem dependências externas em tempo de execução.

## Recursos

- **Carga inicial completa** — ao adicionar um compositor, busca todas as páginas de resultados e
  guarda tudo, ordenado do mais novo para o mais antigo (igual ao site).
- **Verificação automática a cada 24h** para todos os compositores. Se o programa ficou fechado,
  verifica assim que abrir; falhas são retentadas depois de 1h.
- **Aliases** — vários nomes para o mesmo compositor (ex.: *Lady Gaga* e *Stefani Germanotta*). Os
  resultados são consolidados sem duplicar músicas. Também dá para **unir** dois compositores já cadastrados.
- **Nomes só visuais** — exibe um nome (ex.: *Tinashe*) sem buscá-lo no site, para evitar resultados de
  homônimos. Esses nomes também não entram na checagem de créditos.
- **Filtro por créditos** — a busca do site também acha o nome em títulos e álbuns; por padrão a tela
  mostra só as músicas em que o compositor consta nos créditos.
- **Tela "Novos"** — histórico de tudo que apareceu depois da carga inicial, com contador no cabeçalho
  que zera ao abrir a aba.
- **Notificações** — aviso nativo do desktop (`notify-send`, Linux) e aviso do navegador (opcional).
- **Barra de progresso** enquanto os registros são carregados.
- **Vários idiomas** — Português, English e Español. A página detecta o idioma do navegador e há um
  seletor no cabeçalho (a escolha fica salva).

## Como rodar

Requer [Rust](https://rustup.rs) e um compilador C (para o SQLite embutido).

```sh
git clone https://github.com/gbardusco/catalog-monitor.git
cd catalog-monitor
cargo run --release
```

Abra <http://localhost:8787>. O primeiro build leva alguns minutos.

## Configuração

| Variável     | Padrão       | Descrição                         |
|--------------|--------------|-----------------------------------|
| `PORT`       | `8787`       | Porta do servidor local           |
| `CATALOG_DB` | `catalog.db` | Caminho do arquivo SQLite         |
| `CATALOG_LANG` | `$LANG`    | Idioma da notificação do desktop (`pt`, `en`, `es`) |

O servidor escuta apenas em `127.0.0.1` (não fica acessível pela rede). Para backup, copie o `catalog.db`.

## Como funciona

1. Cada compositor tem um nome principal e, opcionalmente, aliases pesquisáveis ou só visuais.
2. Cada nome pesquisável é consultado em `arrangeme.com/search?q=…&sort=date_desc&page=N` (50 por
   página, com pausa entre requisições). O HTML é lido e as músicas são guardadas pelo ID de catálogo.
3. Na primeira varredura de um nome, tudo é carga inicial. Nas seguintes, qualquer ID ainda desconhecido
   é registrado como **novo**.
4. Se o site responder com total > 0 mas nenhuma linha reconhecível, a varredura falha com erro
   explícito (indício de mudança de layout) em vez de gravar dados incorretos.

## API (usada pela página)

| Método e rota                         | Função                                  |
|---------------------------------------|-----------------------------------------|
| `GET/POST /api/composers`             | Listar / adicionar compositor           |
| `DELETE/PATCH /api/composers/:id`     | Excluir / renomear                      |
| `POST /api/composers/:id/aliases`     | Adicionar alias (`{alias, visual}`)     |
| `DELETE /api/aliases/:id`             | Remover alias                           |
| `POST /api/composers/:id/merge`       | Unir em outro compositor (`{into}`)     |
| `POST /api/composers/:id/scan`        | Verificar agora                         |
| `GET /api/composers/:id/songs`        | Registros (`credits_only`, `new_only`)  |
| `POST /api/scan`                      | Verificar todos                         |
| `GET /api/new`                        | Histórico de registros novos            |
| `GET /api/log`                        | Log das verificações                    |

## Idiomas

As traduções ficam no objeto `I18N` no início do script de `static/index.html`. Para adicionar um idioma,
copie um bloco (por exemplo `en`), traduza os valores, registre-o em `LOCALES` e inclua uma `<option>` no
seletor `#lang`. O backend devolve apenas códigos (`err.duplicate`, `scan.initial|…`), traduzidos na página.

## Aviso

Projeto pessoal, sem relação com o ArrangeMe. Ele lê páginas públicas de busca em ritmo baixo
(uma verificação por dia por compositor); respeite os termos de uso do site.
