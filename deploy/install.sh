#!/usr/bin/env bash
# Instala o Catalog Monitor como serviço systemd de usuário (inicia no login e reinicia sozinho).
#
#   ./deploy/install.sh              compila, instala e inicia o serviço
#   ./deploy/install.sh --no-start   só instala os arquivos (não mexe no systemd)
#
# Se já existe um catalog.db na pasta do projeto, ele é COPIADO (não movido) para o diretório de dados,
# desde que o destino ainda não exista. Pare qualquer instância aberta com `cargo run` antes.
set -euo pipefail

cd "$(dirname "$0")/.."
BIN_DIR="${HOME}/.local/bin"
DATA_DIR="${HOME}/.local/share/catalog-monitor"
UNIT_DIR="${HOME}/.config/systemd/user"
NO_START=0
[[ "${1:-}" == "--no-start" ]] && NO_START=1

echo "==> compilando (release)…"
cargo build --release

echo "==> instalando binário em ${BIN_DIR}"
install -Dm755 target/release/catalog-monitor "${BIN_DIR}/catalog-monitor"

mkdir -p "${DATA_DIR}"
if [[ -f catalog.db && ! -f "${DATA_DIR}/catalog.db" ]]; then
  echo "==> copiando catalog.db existente para ${DATA_DIR}"
  if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 catalog.db ".backup '${DATA_DIR}/catalog.db'"
  else
    cp catalog.db catalog.db-wal catalog.db-shm "${DATA_DIR}/" 2>/dev/null || cp catalog.db "${DATA_DIR}/"
  fi
elif [[ -f "${DATA_DIR}/catalog.db" ]]; then
  echo "==> ${DATA_DIR}/catalog.db já existe: mantido como está"
fi

echo "==> instalando unidade systemd em ${UNIT_DIR}"
install -Dm644 deploy/catalog-monitor.service "${UNIT_DIR}/catalog-monitor.service"

if [[ ${NO_START} -eq 1 ]]; then
  echo "==> (--no-start) pronto. Para ativar: systemctl --user enable --now catalog-monitor"
  exit 0
fi

systemctl --user daemon-reload
systemctl --user enable --now catalog-monitor
sleep 1
systemctl --user --no-pager --lines=5 status catalog-monitor || true

cat <<MSG

Pronto! Abra http://localhost:8787

Dicas:
  logs:        journalctl --user -u catalog-monitor -f
  reiniciar:   systemctl --user restart catalog-monitor
  desativar:   systemctl --user disable --now catalog-monitor
  rodar sem precisar de sessão gráfica aberta (opcional):  loginctl enable-linger \$USER
MSG
