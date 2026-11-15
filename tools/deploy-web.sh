#!/usr/bin/env sh
# Build the web app for the public host root and publish it to Caddy.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
export PATH=$HOME/.nvm/versions/node/v24.22.2/bin:${PATH:-}
# Public site is https://mango.forces.quest/ (root). Override with SPEARV_BASE if needed.
export SPEARV_BASE="${SPEARV_BASE:-/}"
cd "$ROOT/web"
npx vite build --outDir dist
install -d -m 755 /var/www/spearv
rsync -a --delete dist/ /var/www/spearv/
echo "published $ROOT/web/dist -> /var/www/spearv (base=$SPEARV_BASE)"
