# ── Single-stage: MyClaw daemon (CI-built release) + nginx serving webui ──
FROM ubuntu:24.04

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates libssl3 nginx && \
    rm -rf /var/lib/apt/lists/*

# MyClaw binary (CI release build, downloaded from GitHub Actions artifact)
COPY deploy/myclaw /usr/local/bin/myclaw
RUN chmod +x /usr/local/bin/myclaw

# Web UI static files (pre-built via `npm run build` in clients/web)
COPY clients/web/dist /var/www/myclaw-ui

# Nginx config: serve webui + reverse-proxy WS to myclaw daemon
COPY deploy/nginx.conf /etc/nginx/sites-available/myclaw
RUN ln -sf /etc/nginx/sites-available/myclaw /etc/nginx/sites-enabled/myclaw && \
    rm -f /etc/nginx/sites-enabled/default

# Entrypoint script: start myclaw daemon + nginx
COPY deploy/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Workspace & config volume
VOLUME ["/root/.myclaw"]
WORKDIR /root/.myclaw

EXPOSE 80

ENTRYPOINT ["/entrypoint.sh"]
