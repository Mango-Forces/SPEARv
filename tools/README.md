# Host deploy (oci)

Public site: https://mango.forces.quest/

## Pieces

| Path | Role |
| --- | --- |
| `tools/caddy/Caddyfile` | Loopback reverse proxy on `:8090` → static build + engine |
| `tools/deploy-web.sh` | `vite build` with `SPEARV_BASE=/` and rsync to `/var/www/spearv` |
| `tools/systemd/*.service` | engine, index, web (dev), watch, cloudflared |
| `tools/cloudflared/config.example.yml` | Tunnel ingress template (no secrets) |
| `tools/rebuild-engine.sh` | Offline release build + restart engine |

## Secrets (stay on the host only)

- `~/.cloudflared/cert.pem` — origin cert from `cloudflared tunnel login`
- `~/.cloudflared/<tunnel-id>.json` — tunnel credentials
- `~/.cloudflared/config.yml` — real config copied from the example

Do not commit those three.

## Apply after a pull

```sh
sudo cp tools/caddy/Caddyfile /etc/caddy/Caddyfile && sudo systemctl reload caddy
tools/deploy-web.sh
# systemd units only when they change:
# cp tools/systemd/*.service ~/.config/systemd/user/   # or /etc/systemd/system for cloudflared
# systemctl --user daemon-reload && systemctl --user restart spearv-engine
```
