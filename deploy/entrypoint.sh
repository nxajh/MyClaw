#!/bin/bash
set -e

# Start nginx in background
nginx -g 'daemon off;' &
NGINX_PID=$!

# Start myclaw daemon (foreground) — reads config from /root/.myclaw/myclaw.toml
exec myclaw run
