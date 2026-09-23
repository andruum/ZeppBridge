#!/bin/sh
# Turn the two failures that actually happen into sentences somebody can act
# on, then get out of the way.
#
# Everything else is passed straight through, so `docker run ... zeppbridge-cli
# sync --json` behaves exactly like the same command on a host. In particular
# exit codes are the container's exit codes: the CLI's contract (4 means "busy,
# retry later", not "failed") only survives if nothing here rewrites them.
set -eu

data_dir="${ZEPPBRIDGE_DATA_DIR:-/data}"

# 1. The volume is not writable by this user. This is the uid mismatch on a
#    bind mount, and the raw error ("Permission denied") points at the
#    database rather than at the mount.
if ! mkdir -p "$data_dir" 2>/dev/null || [ ! -w "$data_dir" ]; then
  cat >&2 <<MSG
zeppbridge: $data_dir is not writable by uid $(id -u).

This is almost always a bind mount owned by a different user. Either run the
container as the owner of the directory:

  docker run --user "\$(id -u):\$(id -g)" ...

or hand it a named volume instead of a host path. See docs/guides/docker.md.
MSG
  exit 6
fi

# 2. No account credentials are available (neither auth.json nor the complete
#    environment set). The CLI also exits 3, but this message lists both
#    headless and legacy ways to supply them.
if [ ! -f "$data_dir/auth.json" ] && {
  [ -z "${ZEPPBRIDGE_APP_TOKEN:-}" ] ||
  [ -z "${ZEPPBRIDGE_USER_ID:-}" ] ||
  [ -z "${ZEPPBRIDGE_REGION_HOST:-}" ]
}; then
  case "${1:-}" in
    # status and the read-only commands are legitimate on an empty library;
    # only warn for the ones that need the cloud.
    zeppbridge-cli)
      case "${2:-}" in
        sync)
          cat >&2 <<MSG
zeppbridge: no account connected ($data_dir/auth.json is missing).

The container needs either the three Zepp environment variables
(ZEPPBRIDGE_APP_TOKEN, ZEPPBRIDGE_USER_ID, ZEPPBRIDGE_REGION_HOST) or the
legacy auth.json plus App Token setup. See docs/guides/docker.md.
MSG
          ;;
      esac
      ;;
  esac
fi

exec "$@"
