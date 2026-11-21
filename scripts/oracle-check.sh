#!/bin/sh
# Check sql-review against an Oracle profile: connect, a query, typed values
# through every format, a describe as TABLE and as OWNER.TABLE, a read-only
# transaction and binds. Each step prints PASS or FAIL; the exit status is 0
# only if all pass. Nothing is written to the database.
#
# usage: scripts/oracle-check.sh [--profile NAME] [--env-file PATH] [--table TABLE]
#
# Settings come from the environment and .env exactly as for queries
# (DB_BACKEND=oracle, DB_HOST, DB_PORT, DB_SERVICE, DB_USER, DB_PASSWORD).
# Set SQL_REVIEW to the binary to use if it is not on PATH.
set -eu
case "${1:-}" in
  -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
esac
exec "${SQL_REVIEW:-sql-review}" check-oracle "$@"
