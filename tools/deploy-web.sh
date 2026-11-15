#!/usr/bin/env sh
# Build the web app for the public /SPEARv path and publish it to Caddy.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
export PATH=$HOME/.nvm/versions/node/v24.22.2/bin:$PATH
export SPEARV_BASE=/SPEARv/
cd "$ROOT/web"
npx vite build --outDir dist
install -d -m 755 /var/www/spearv
rsync -a --delete dist/ /var/www/spearv/
echo "published $ROOT/web/dist -> /var/www/spearv"
