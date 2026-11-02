#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
# This profile creates two dedicated databases; volumes are removed on exit.
if docker compose version >/dev/null 2>&1; then
  compose() { docker compose -p fedengine-test -f docker/docker-compose.test.yml "$@"; }
else
  compose() { docker-compose -p fedengine-test -f docker/docker-compose.test.yml "$@"; }
fi
python3 scripts/fetch-extensions.py --dest data/extensions
compose up -d
trap 'compose down -v' EXIT INT TERM
for service in postgres mysql; do
  count=0
  while [ "$(docker inspect --format '{{.State.Health.Status}}' "$(compose ps -q "$service")")" != healthy ]; do
    count=$((count+1)); [ "$count" -lt 60 ] || exit 1
    sleep 1
  done
done
export FEDENGINE_POSTGRES_DSN='host=127.0.0.1 port=25432 dbname=crm user=fedengine password=fedengine-test-only'
export FEDENGINE_MYSQL_DSN='host=127.0.0.1 port=23306 database=erp user=root password=fedengine-test-only'
cargo test --locked --test federation -- --ignored --nocapture
