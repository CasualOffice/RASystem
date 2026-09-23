#!/usr/bin/env bash
# Casual Annotate — stand up a local docker-jitsi-meet for the live e2e tests.
#
# Turns the manual copy-paste runbook this file used to be (see git history / README.md §1) into a
# script, so "run the live proof" is a command, not a set of instructions someone re-types by hand
# each time and inevitably drifts from.
#
# Idempotent: safe to re-run against an already-up stack (it just re-applies the config patch and
# waits again).

set -euo pipefail

WORKDIR="${CASUAL_ANNOTATE_DOCKER_DIR:-/tmp/casual-annotate-jitsi}"
PORT="${CASUAL_ANNOTATE_HTTP_PORT:-8000}"

if [ ! -d "$WORKDIR/.git" ]; then
    echo "Cloning docker-jitsi-meet into $WORKDIR"
    git clone --depth 1 https://github.com/jitsi/docker-jitsi-meet.git "$WORKDIR"
fi

cd "$WORKDIR"

if [ ! -f .env ]; then
    cp env.example .env
    ./gen-passwords.sh
fi

# Force the settings the live proof needs, whether .env pre-existed or was just generated.
set_env() {
    local key="$1" val="$2"
    if grep -q "^${key}=" .env; then
        sed -i "s|^${key}=.*|${key}=${val}|" .env
    else
        echo "${key}=${val}" >> .env
    fi
}
set_env PUBLIC_URL "http://localhost:${PORT}"
set_env HTTP_PORT "${PORT}"
set_env DISABLE_HTTPS 1
set_env ENABLE_AUTH 0
set_env ENABLE_GUESTS 1
set_env ENABLE_LOBBY 0
set_env ENABLE_PREJOIN_PAGE 0
set_env JVB_ADVERTISE_IPS 127.0.0.1
# A second, independent gotcha found live (docker-jitsi-meet `unstable`, 2026-09-22): nginx's
# websocket/BOSH proxy_pass target (`$XMPP_BOSH_URL_BASE`) is not set by any default in this image —
# it comes through empty, which makes `proxy_pass {{ empty }}/xmpp-websocket...` an invalid upstream
# and every XMPP connection attempt fail with a 502, symptomatically identical to the ws://-vs-wss://
# gotcha below (a bare "You have been disconnected", no useful client-side error). Set it explicitly
# rather than trust the image's default.
set_env XMPP_BOSH_URL_BASE "http://xmpp.meet.jitsi:5280"

# The exact set of bind-mount sources docker-compose.yml expects (`grep CONFIG} docker-compose.yml`).
# Creating them ourselves, up front, matters: if `docker compose up` has to create a missing one, it
# does so as ROOT — and prosody then refuses to start ("directory '/var/lib/prosody' is not writable
# by the container user (uid 1000)"), a third gotcha found live in the same run. `-p` makes this safe
# to re-run once the real directories exist.
mkdir -p ~/.jitsi-meet-cfg/{web,storage/web,storage/transcripts,storage/prosody,tmp/web-load-test,prosody/config,prosody/prosody-plugins-custom,jicofo,jvb}
chmod -R 777 ~/.jitsi-meet-cfg

echo "docker compose up -d"
docker compose up -d

echo "Waiting for the web container..."
for _ in $(seq 1 60); do
    if docker compose exec -T web sh -c 'test -f /run/web/config/config.js' 2>/dev/null; then
        break
    fi
    sleep 2
done

# The one documented gotcha (test/live/README.md §1): the config template hardcodes `wss://` after
# stripping `https://` from PUBLIC_URL, so an http:// deployment produces `wss://http://...` and the
# websocket never opens — the client just says "You have been disconnected" with no useful clue.
echo "Patching generated config.js for ws:// (docker-jitsi-meet assumes TLS by default)"
docker compose exec -T web sh -c \
    "sed -i 's|wss://http://localhost:${PORT}/|ws://localhost:${PORT}/|g; \
             s|https://http://localhost:${PORT}/|http://localhost:${PORT}/|g' /run/web/config/config.js"

echo "Waiting for http://localhost:${PORT}/config.js to actually serve..."
for _ in $(seq 1 60); do
    if curl -sf "http://localhost:${PORT}/config.js" | grep -q "ws://localhost:${PORT}"; then
        break
    fi
    sleep 2
done

# `config.js` SAYING ws:// is not the same as the XMPP route actually working — the
# XMPP_BOSH_URL_BASE gotcha above produces a config.js that looks completely correct while every
# connection attempt still 502s. `/http-bind` (BOSH) exercises the exact same nginx proxy_pass target
# a real websocket upgrade does, so a 200 here is real evidence the route is live, not just configured.
echo "Verifying the XMPP proxy route (not just config.js) actually works..."
for _ in $(seq 1 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:${PORT}/http-bind")
    if [ "$code" = "200" ]; then
        echo "Ready: http://localhost:${PORT}"
        exit 0
    fi
    sleep 2
done

echo "Timed out: config.js looks right but the BOSH/websocket proxy route (/http-bind) never" >&2
echo "returned 200 (last status: ${code:-none}). Check XMPP_BOSH_URL_BASE in .env and" >&2
echo "'docker compose logs web prosody'." >&2
exit 1
