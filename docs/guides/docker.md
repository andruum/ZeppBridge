# Docker

Two images, for two different jobs:

- **`packaging/docker/Dockerfile`** — the headless runtime: `zeppbridge-cli` and
  `zeppbridge-mcp`. This is what you run on a NAS or a home server to keep the
  library synced.
- **`packaging/docker/Dockerfile.build`** — the pinned Linux toolchain. Builds
  nothing by itself; it makes `deb`/`rpm`/`AppImage` builds reproducible off CI.

[简体中文](docker.zh-CN.md)

## What the runtime image is not

It does not contain the desktop app. The GUI needs a WebView, a display and a
sign-in window. If you already have the Zepp App Token, user ID and region host,
headless sync can use those directly; no desktop app or `auth.json` is required.
See the [connection guide](connection.md) if you need help finding the account
metadata.

## Legacy credentials from desktop app

Two things have to reach the container: `auth.json` (non-secret metadata — user
ID and region host) and the App Token (secret).

```bash
mkdir -p ./data
# From a Linux desktop install; see the Linux guide for other platforms' paths.
cp ~/.local/share/zeppbridge/data/auth.json ./data/
```

The token is in your desktop machine's credential store, so copy it out of there
(Settings shows it masked; the store itself holds the value). Then pick one of
these:

### Environment (recommended)

```bash
docker run --rm \
  --user "$(id -u):$(id -g)" \
  -v "$PWD/data:/data" \
  -e ZEPPBRIDGE_CREDENTIAL_STORE=env \
  -e ZEPPBRIDGE_APP_TOKEN \
  -e TZ="$(cat /etc/timezone)" \
  zeppbridge:local \
  zeppbridge-cli sync --mode incremental --json
```

Read-only: nothing is written to disk and the process cannot change it. Note
`-e ZEPPBRIDGE_APP_TOKEN` with no value — that passes the variable through from
your shell instead of putting the token in the command, which would otherwise
land in your shell history.

Anyone who can reach the Docker daemon can read a container's environment with
`docker inspect`. On a machine where that matters, use a Docker/Swarm secret and
read the file in your own wrapper, or use the file store below.

### File

```bash
docker run --rm ... -e ZEPPBRIDGE_CREDENTIAL_STORE=file zeppbridge:local ...
```

Writes the token to `/data/credentials.json`, mode 0600, in a directory
tightened to 0700. This is an explicit downgrade from a keyring: file
permissions are the only thing protecting it, and it sits in the same volume as
your backups. It exists because on a headless machine the alternative is not a
safer store, it is nothing at all.

Once `credentials.json` is there, later runs pick the file store up on their own
— you do not have to keep passing the variable. Set it once, on the run that
writes the token.

## Headless credentials via environment

To avoid copying `auth.json` from a desktop install, set all three values in the
runtime environment (Coolify protected variables are suitable):

- `ZEPPBRIDGE_APP_TOKEN` — secret App Token
- `ZEPPBRIDGE_USER_ID` — Zepp user ID
- `ZEPPBRIDGE_REGION_HOST` — API origin such as `https://api-mifit.zepp.com`
- `ZEPPBRIDGE_CREDENTIAL_STORE=env`

This mode uses the App Token in memory and does not save it into the data volume.
The values are validated before sync. Do not put actual secrets in Compose files
or commit a `.env` file.

## Build and run

```bash
docker build -f packaging/docker/Dockerfile -t zeppbridge:local .
```

About 95 MB. It contains the two binaries, `ca-certificates` (sync is HTTPS),
`libdbus-1-3` (the Secret Service backend is linked in even though a container
never uses it) and `tzdata`.

```bash
# What is in the library. This is also the default command.
docker run --rm -v "$PWD/data:/data" --user "$(id -u):$(id -g)" \
  zeppbridge:local

# Export a month. Note --json goes to stdout alone, so redirection is clean.
docker run --rm -v "$PWD/data:/data" --user "$(id -u):$(id -g)" \
  zeppbridge:local \
  zeppbridge-cli export --from 2026-01-01 --to 2026-01-31 --format csv \
    --out /data/exports/january.csv
```

### The `--user` flag

The image runs as uid 1000, which is the first uid most desktop Linux users get,
so a bind mount usually just works. When it does not, the container tells you
which uid it is and that `--user` is the fix, rather than failing with a
permission error pointing at the database. Named volumes avoid the question
entirely.

### Timezone

Set `TZ`. The database stores **local** days, so a container left on UTC files a
reading taken at 00:30 under the previous day, and the mistake is invisible
until you compare a chart against the phone.

## Scheduling

The CLI syncs and exits — it is not a daemon. Schedule it from the host rather
than running cron inside the container: a second init system in there means its
own log destination and a container that looks healthy while doing nothing.

A systemd timer, running as your own user:

```ini
# ~/.config/systemd/user/zeppbridge-sync.service
[Unit]
Description=ZeppBridge incremental sync
After=network-online.target

[Service]
Type=oneshot
Environment=ZEPPBRIDGE_APP_TOKEN=
EnvironmentFile=%h/.config/zeppbridge/token.env
ExecStart=/usr/bin/docker run --rm \
  --user %U:%U \
  -v %h/zeppbridge/data:/data \
  -e ZEPPBRIDGE_CREDENTIAL_STORE=env \
  -e ZEPPBRIDGE_APP_TOKEN \
  -e TZ=Europe/Athens \
  zeppbridge:local \
  zeppbridge-cli sync --mode incremental --json
# 4 means "the desktop app is syncing right now" — retry later, not a failure.
SuccessExitStatus=4
```

```ini
# ~/.config/systemd/user/zeppbridge-sync.timer
[Unit]
Description=Sync ZeppBridge daily

[Timer]
OnCalendar=daily
Persistent=true
RandomizedDelaySec=30m

[Install]
WantedBy=timers.target
```

```bash
chmod 600 ~/.config/zeppbridge/token.env
systemctl --user enable --now zeppbridge-sync.timer
```

`SuccessExitStatus=4` is the part worth copying. Exit code 4 means another
process holds the write lock; treating it as a failure gives you a red timer
every time the desktop app happens to be open. The
[full exit-code table](../reference/cli-and-mcp.md#exit-codes) is a contract —
existing codes never change meaning.

For `cron` instead, the same command works; remember cron's environment is
nearly empty, so pass `TZ` and use absolute paths.

## docker compose

`packaging/docker/docker-compose.yml` starts the authenticated `zepp-mcp-http`
service, private `zepp-sync-worker`, and scheduled-task `zepp-sync-runner` by
default. It keeps a named persistent Compose volume for the synced library
(default volume name: `zeppbridge_data`). The MCP service has no published host
port; Hermes reaches it on `zeppbridge-mcp-net`, while the worker is reachable
only on a separate internal control network.

Set `ZEPPBRIDGE_MCP_AUTH_TOKEN` in Coolify's protected environment variables,
then deploy. For local Compose, export a strong random value before running:

```bash
export ZEPPBRIDGE_MCP_AUTH_TOKEN="$(openssl rand -hex 32)"
docker compose -f packaging/docker/docker-compose.yml up -d --build
```

Set `ZEPPBRIDGE_APP_TOKEN`, `ZEPPBRIDGE_USER_ID` and
`ZEPPBRIDGE_REGION_HOST` in the deployment environment for sync. The HTTP MCP
container is deliberately not given these Zepp credentials.

For a local one-shot run, use:

```bash
docker compose -f packaging/docker/docker-compose.yml --profile sync run --rm sync
```

Coolify's Scheduled Tasks run a command *inside an already-running container*;
they do not launch `docker compose run`. The Compose file therefore includes a
separate `zepp-sync-runner` container with Zepp credentials and the same data
volume. The HTTP MCP container remains credential-free.

In Coolify, after deploying:

1. Open the resource's **Scheduled Tasks** and create a task.
2. Set the command to `zeppbridge-cli sync --mode incremental --json`.
3. Select the `zepp-sync-runner` container.
4. Choose a schedule and timeout. Coolify uses the deployment server's timezone.
   Use **Execute Now** for the first sync, review its output, then leave the
   schedule enabled for subsequent runs.

To let a separate Hermes Compose project reach it, attach only the Hermes
service to this pre-existing network in Hermes' Compose file (preserve any
networks it already uses):

```yaml
networks:
  zeppbridge-mcp-net:
    external: true
    name: zeppbridge-mcp-net

services:
  hermes:
    networks:
      zeppbridge-mcp-net: {}
```

Deploy ZeppBridge first so it creates the network. From Hermes, use
`http://zepp-mcp-http:8080/mcp`; configure the same MCP bearer secret in Hermes
without putting it in the Compose YAML. Do not publish port 8080 or add a public
domain unless you intentionally place it behind trusted TLS and restrict access.
The service responds to `/healthz` for private health checks.

### On-demand sync through the original MCP

`zepp-mcp-http` is the **only MCP endpoint**. Alongside the six existing
read-only tools, its authenticated HTTP transport exposes `sync_zepp` and
`get_sync_status` using the existing `ZEPPBRIDGE_MCP_AUTH_TOKEN`.
The stdio transport remains read-only. Keep the existing `zeppbridge` MCPorter
entry at `http://zepp-mcp-http:8080/mcp`; no second MCP client entry is needed.

The MCP forwards those two fixed operations to `zepp-sync-worker` over a
separate internal `zeppbridge-control-net`. This worker is **not an MCP
endpoint**; it is accessible only to the MCP container, holds the Zepp Cloud
credentials, and has a separate egress network for Zepp Cloud. The MCP
container receives no cloud credential environment variables. Neither
service publishes a host port or a public domain.

Calling `zeppbridge.sync_zepp` starts a fixed **incremental** CLI sync and
returns a job ID right away. Poll `zeppbridge.get_sync_status` to check the
same job. Concurrent requests reuse the running job, and immediate repeats
are coalesced. Do not interpret `complete` as proof of new sleep data: query
`get_sleep_for_date` after completion. A cross-process lock conflict (`busy`)
means retry later; partial stream failures are not success. The endpoint
cannot force the watch/app to upload readings to Zepp Cloud.


## MCP over stdio (local)

`zeppbridge-mcp` without the `--http` option speaks stdio and listens on no port, so it is not a service you
leave running — the MCP client spawns it and talks over the pipe. Point the
client at a `docker run`:

```json
{
  "mcpServers": {
    "zeppbridge": {
      "command": "docker",
      "args": [
        "run", "--rm", "-i",
        "--network", "none",
        "--user", "1000:1000",
        "-v", "/home/you/zeppbridge/data:/data",
        "zeppbridge:local",
        "zeppbridge-mcp"
      ]
    }
  }
}
```

`-i` is required: without stdin the server has nothing to read and the client
sees an immediate EOF. `--network none` is safe and worth setting — the MCP
server is read-only and never touches the network, so this makes that
structural rather than a promise.

## The toolchain image

```bash
docker build -f packaging/docker/Dockerfile.build -t zeppbridge-build:local .
docker run --rm -v "$PWD:/src" -w /src zeppbridge-build:local \
  bash -c 'npm ci && npm run tauri build -- \
    --config src-tauri/tauri.linux.conf.json --bundles deb,rpm,appimage'
```

Debian bookworm on purpose, not a rolling base: the glibc a binary links against
is the oldest glibc it will run on, so building on the newest distribution
produces packages that refuse to start on the LTS releases people actually run.
Node and Rust versions are pinned as build args — bump them deliberately.

## Privacy

The runtime image needs the network for exactly one thing: `zeppbridge-cli sync`
talking to the Zepp cloud. Everything else works with `--network none`, and the
MCP server should always be run that way. No telemetry, and nothing is sent
anywhere except your own Zepp account's API.
