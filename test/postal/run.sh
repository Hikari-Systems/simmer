#!/usr/bin/env bash
# From nothing: start Postal 3.3.7 + MariaDB + Mailpit, provision headlessly, send a test
# message through Postal SMTP with AUTH, and verify it lands in Mailpit.
# Usage: ./run.sh [--keep]   (default tears everything down at the end)
set -euo pipefail
cd "$(dirname "$0")"
P=postalspike
NET=${P}_default
JAIL=$(hostname)

cleanup() {
  docker network disconnect "$NET" "$JAIL" >/dev/null 2>&1 || true
  if [[ "${1:-}" != "--keep" ]]; then docker compose -p "$P" -f compose.yml down -v >/dev/null 2>&1 || true; fi
}
trap 'cleanup "${KEEP:-}"' EXIT
[[ "${1:-}" == "--keep" ]] && KEEP=--keep

t0=$(date +%s.%N)
docker compose -p "$P" -f compose.yml up -d --wait --wait-timeout 300
t1=$(date +%s.%N)
echo "cold_start_to_ready_s=$(python3 -c "print(round($t1-$t0,1))")"
docker logs "${P}-postal-init-1" 2>&1 | tail -1          # provisioning JSON
docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' $(docker compose -p "$P" ps -q)

# Reach the stack from this shell via container DNS (published ports do not work here).
docker network connect "$NET" "$JAIL" 2>/dev/null || true
POSTAL_HOST=smtp MAILPIT_API=http://mailpit:8025 python3 smtp_probe.py >/dev/null
python3 -c 'import json;r=json.load(open("probe-results.json"));print("delivered=",r["delivered"],"latency_s=",r.get("delivery_latency_s"))'
docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' $(docker compose -p "$P" ps -q)
