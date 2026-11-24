# Docker

The image runs `tagmail-server` as a static musl binary on
`gcr.io/distroless/static` as the `nonroot` user (uid 65532), with the web
app next to it. It is published for `linux/amd64` and `linux/arm64`:

- `ghcr.io/micheljung/tagmail:<version>`, e.g. `0.1.0`, for every release
- `ghcr.io/micheljung/tagmail:latest`, the newest release that is not a
  prerelease

The server listens on `0.0.0.0:7878` inside the container and keeps its data
in `/data`. Its arguments go after the image name (or in `command:`); the
data directory and listen address are already set.

## Master key

IMAP and SMTP passwords are encrypted with a master key, which the image
reads from the Docker secret `tagmail_master_key`
(`TAGMAIL_MASTER_KEY_FILE=/run/secrets/tagmail_master_key`). The file holds
64 hex digits, base64 of 32 bytes, or 32 raw bytes:

```sh
openssl rand -hex 32 > master.key
sudo chown 65532:65532 master.key
sudo chmod 400 master.key
```

Compose mounts a file secret with its host owner and mode, so the file must
belong to uid 65532 for the server to read it. With rootless Docker or
Podman, use the host uid that maps to 65532 in the container. Keep the key
out of the data volume and back it up too, but store it apart from the data
backups, e.g. in a password manager ([backup](../../docs/backup.md)):
without it the stored passwords cannot be decrypted, and anyone holding both
can read them.

## Run with Compose

```sh
cp docker-compose.example.yml compose.yml
docker compose up -d
docker compose logs tagmail
```

Set `--public-origin` in `compose.yml` to the `https` origin your reverse
proxy serves. The server refuses to listen on a non-loopback address without
an `https` public origin, because it does not terminate TLS itself. On first
start it logs a one-time setup token; open the public origin and finish setup
with it.

The port is published on `127.0.0.1` only, for a reverse proxy on the same
host (Caddy, nginx, Traefik). Behind Docker's port forwarding the server sees
the proxy's connections coming from the Docker network gateway, so the
example pins the network to `172.30.78.0/24` and trusts `X-Forwarded-For`
from its gateway `172.30.78.1` only. If that subnet clashes with one of
yours, change both. A proxy in another container on the same Compose
network can reach `tagmail:7878` directly instead; trust its address then.

The example runs the container with a read-only root file system (the
server writes only to `/data` and temporary files to `/tmp`), without
capabilities and without privilege escalation.

The example uses `:latest` for brevity; pin a version such as
`ghcr.io/micheljung/tagmail:0.1.0` so that an upgrade happens only when you
change it (see [Upgrade](#upgrade)).

Compose prefixes the volume with the project name, which defaults to the
directory name: in `~/tagmail` the data lives in the volume
`tagmail_tagmail-data` (`docker volume ls`).

## Run without Compose

The default bridge network has no fixed gateway, so create a network with
a fixed subnet and trust its gateway:

```sh
docker network create --subnet 172.30.78.0/24 --gateway 172.30.78.1 tagmail
docker volume create tagmail-data
docker run -d --name tagmail --network tagmail -p 127.0.0.1:7878:7878 \
  --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges \
  -v tagmail-data:/data \
  -v "$PWD/master.key:/run/secrets/tagmail_master_key:ro" \
  ghcr.io/micheljung/tagmail:latest \
  --public-origin https://mail.example.com --trusted-proxy 172.30.78.1
```

## Upgrade

Change the version in `compose.yml`'s `image:`, then:

```sh
docker compose pull
docker compose up -d
```

Database migrations run on start. Back up the volume and the master key
before upgrading; a newer version's databases may not open with an older one.

## Backup

The image has no shell, so a throwaway `busybox` container copies the data
([what to back up](../../docs/backup.md#what-to-back-up)). Replace
`tagmail_tagmail-data` with your volume's name:

```sh
docker compose stop
docker run --rm -v tagmail_tagmail-data:/data:ro -v "$PWD:/backup" busybox \
  sh -c 'cd /data && tar -cf /backup/tagmail-backup.tar config.db uploads outbox $(ls config.db-wal 2>/dev/null)'
docker compose start
```

To restore into a new, empty volume:

```sh
docker compose down
docker volume rm tagmail_tagmail-data
docker compose create
docker run --rm -v tagmail_tagmail-data:/data -v "$PWD:/backup:ro" busybox \
  sh -c 'tar -xf /backup/tagmail-backup.tar -C /data && chown -R 65532:65532 /data'
docker compose start
```

The master key is not in the volume; restore it to `master.key` from where
you keep it.

## Build

From the repository root:

```sh
docker buildx build -f deploy/docker/Dockerfile -t tagmail --load .
docker buildx build -f deploy/docker/Dockerfile \
  --platform linux/amd64,linux/arm64 -t tagmail .
```

Both platforms are cross-compiled on the build machine (cargo-zigbuild); no
emulation is needed. `--target dist --output type=local,dest=dist` exports
just the binary and the web app.
