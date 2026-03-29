#!/usr/bin/env bash
# deploy.sh — deploy gate-arb to Hetzner via SSH.
#
# Usage:
#   ./scripts/deploy.sh [--profile production]
#
# Prerequisites:
#   - SSH access to Hetzner (source ~/.lain-secrets/hetzner.env first)
#   - Docker + docker compose v2 on remote
#   - .env file on remote at /home/user3/gate-arb/.env
#
# What it does:
#   1. rsync repo (excluding target/) to Hetzner
#   2. docker compose build + up -d on remote
#   3. Tail health endpoint to confirm startup

set -euo pipefail

source ~/.lain-secrets/hetzner.env 2>/dev/null || true

REMOTE_HOST="${HETZNER_HOST:-}"
REMOTE_USER="${HETZNER_USER:-user3}"
REMOTE_DIR="/home/${REMOTE_USER}/gate-arb"
PROFILE="${1:-}"

if [[ -z "$REMOTE_HOST" ]]; then
  echo "ERROR: HETZNER_HOST not set. Source ~/.lain-secrets/hetzner.env or set HETZNER_HOST." >&2
  exit 1
fi

echo "==> Syncing to ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_DIR}"
rsync -avz --exclude='.git' --exclude='target/' --exclude='.env' \
  ./ "${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_DIR}/"

echo "==> Building and starting on remote"
# shellcheck disable=SC2029
ssh "${REMOTE_USER}@${REMOTE_HOST}" bash -s <<ENDSSH
set -euo pipefail
cd "${REMOTE_DIR}"

# Build image
docker compose build gate-arb

# Rotate: stop old, clean old images
docker compose down --remove-orphans || true
docker image prune -f

# Start (with optional production profile for nginx)
if [[ -n "${PROFILE}" ]]; then
  docker compose --profile production up -d
else
  docker compose up -d gate-arb
fi

echo "Waiting for health check..."
for i in \$(seq 1 20); do
  if curl -sf http://localhost:8081/health > /dev/null 2>&1; then
    echo "Health check passed after \${i}s"
    break
  fi
  sleep 1
done

curl -s http://localhost:8081/health | python3 -m json.tool || true
ENDSSH

echo "==> Deploy complete: https://gate-arb.111miniapp.com/"
