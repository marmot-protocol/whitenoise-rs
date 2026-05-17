#!/usr/bin/env bash
#
# toxiproxy-helpers.sh — Reusable lifecycle for the docker-compose `latency`
# profile, plus its HTTP control API.
#
# Source this from any benchmark script that wants to inject WAN latency
# between the bench binary and the local relays. Validated by
# experiments/04-toxiproxy-validation.
#
# Provides:
#   toxiproxy_up                 — bring up the `latency` profile, wait for control API
#   toxiproxy_apply_latency N    — add an N-ms latency toxic (both directions, both proxies)
#   toxiproxy_clear              — remove any toxics added by toxiproxy_apply_latency
#   toxiproxy_down               — clear toxics and stop the toxiproxy container
#   toxiproxy_relay_args         — echo `--relays ws://... --relays ws://...` for the proxy ports

TOXIPROXY_API="${TOXIPROXY_API:-http://localhost:8474}"
TOXIPROXY_RELAYS=(ws://localhost:18080 ws://localhost:17777)

toxiproxy_up() {
    docker compose --profile latency up -d toxiproxy >/dev/null
    for _ in $(seq 1 30); do
        if curl -sf "${TOXIPROXY_API}/version" >/dev/null 2>&1; then return 0; fi
        sleep 0.5
    done
    echo "toxiproxy did not come up within 15s" >&2
    return 1
}

toxiproxy_clear() {
    for proxy in nostr-rs-slow strfry-slow; do
        for direction in upstream downstream; do
            curl -s -X DELETE "${TOXIPROXY_API}/proxies/${proxy}/toxics/wan-${direction}" >/dev/null 2>&1 || true
        done
    done
}

toxiproxy_apply_latency() {
    local latency_ms="$1"
    toxiproxy_clear
    for proxy in nostr-rs-slow strfry-slow; do
        for direction in upstream downstream; do
            curl -sf -X POST "${TOXIPROXY_API}/proxies/${proxy}/toxics" \
                -H 'Content-Type: application/json' \
                -d "{\"name\":\"wan-${direction}\",\"type\":\"latency\",\"stream\":\"${direction}\",\"attributes\":{\"latency\":${latency_ms}}}" >/dev/null
        done
    done
}

toxiproxy_down() {
    toxiproxy_clear
    docker compose --profile latency stop toxiproxy >/dev/null 2>&1 || true
}

toxiproxy_relay_args() {
    printf -- '--relays %s --relays %s' "${TOXIPROXY_RELAYS[0]}" "${TOXIPROXY_RELAYS[1]}"
}
