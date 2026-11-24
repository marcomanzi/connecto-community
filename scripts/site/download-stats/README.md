# Download count for the site

The landing page shows "**N** downloads · +N this week" in the hero. The number
comes from `downloads/stats.json` on the site, a static file that a timer on the
server rewrites every hour. Nothing runs in the app, and the visitor's browser
calls no API: it only fetches that file from the same site. If the file is
missing or holds no usable number, the page shows nothing.

## How it is counted

The release files are served by Caddy from `connecto.studio/downloads/` (a
Caddy container on the site's server; there are no GitHub Releases). Caddy writes one JSON line per request for a `.dmg` or `.zip` in that
folder into `/config/logs/connecto-downloads.log`, without the IP address, port,
cookies or response headers (`caddy-snippet.caddy`). Every other request is not
logged.

`count-downloads.py` reads that log and its rolled copies and counts a download
for each GET answered 200 with (nearly) the whole file sent, from a user agent
that does not call itself a bot. Range (206) and cached (304) answers, HEAD
requests and aborted transfers do not count. Downloads by Connecto's own updater
(user agent `Connecto-Updater/<version>`) are existing installs updating
themselves: they are counted apart, in `updates`, and never in `total` or
`last7days`. It writes:

```json
{"total": 128, "last7days": 23, "updates": 40, "updated": "2026-11-14T10:00:00Z",
 "since": "2026-11-14", "by_version": {"0.1.7": 101, "0.1.8": 27}}
```

The count starts when the log is turned on: downloads before that were not
logged anywhere and cannot be recovered.

## Install (on the server, as root)

The steps below use these values; set them for your own server first:

| Variable | Meaning | Example |
|---|---|---|
| `SERVER` | SSH target of the server that runs Caddy | `root@site.example.com` |
| `SSH_KEY` | Private key for that login | `~/.ssh/site_deploy` |
| `CADDY_CONTAINER` | Name of the Caddy container | `caddy` |
| `CADDY_LOG_DIR` | Host folder where the container's `/config/logs` lands (the `_data/logs` folder of the Docker volume mounted at `/config`) | `/var/lib/docker/volumes/caddy_config/_data/logs` |
| `SITE_DOWNLOADS_DIR` | Host folder served as `connecto.studio/downloads/` | `/var/www/connecto/downloads` |

`CADDY_LOG_DIR` and `SITE_DOWNLOADS_DIR` are read by the systemd service from
`/etc/default/connecto-download-stats` (one `NAME=value` per line); the
service's own `Environment=` lines are only defaults.

Follow the server's Caddy rules: back up, validate, then reload gracefully.
Never restart or recreate containers you do not own.

1. Copy the files:

   ```sh
   scp -i "$SSH_KEY" -o IdentitiesOnly=yes \
     scripts/site/download-stats/count-downloads.py \
     scripts/site/download-stats/connecto-download-stats.service \
     scripts/site/download-stats/connecto-download-stats.timer \
     scripts/site/download-stats/caddy-snippet.caddy \
     "$SERVER":/root/
   ```

2. Install the script and the timer:

   ```sh
   install -d /usr/local/lib/connecto
   install -m 755 /root/count-downloads.py /usr/local/lib/connecto/count-downloads.py
   install -m 644 /root/connecto-download-stats.service /root/connecto-download-stats.timer /etc/systemd/system/
   printf 'CADDY_LOG_DIR=%s\nSITE_DOWNLOADS_DIR=%s\n' "$CADDY_LOG_DIR" "$SITE_DOWNLOADS_DIR" \
     > /etc/default/connecto-download-stats
   systemctl daemon-reload
   ```

3. Turn the log on. Add the lines of `caddy-snippet.caddy` (without the comment
   header) inside the `connecto.studio { ... }` block, before `file_server`. Do
   this in the Caddyfile the container loads (`docker inspect "$CADDY_CONTAINER"`
   shows which host file is mounted), and also in whatever source that file is
   deployed from, or the next deploy drops it. Then:

   ```sh
   docker exec "$CADDY_CONTAINER" mkdir -p /config/logs
   docker exec "$CADDY_CONTAINER" caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
   docker exec "$CADDY_CONTAINER" caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile
   ```

4. Start the timer and run it once:

   ```sh
   systemctl enable --now connecto-download-stats.timer
   systemctl start connecto-download-stats.service
   journalctl -u connecto-download-stats.service -n 5
   curl -fsS https://connecto.studio/downloads/stats.json
   ```

`stats.json` sits in `downloads/`, which the site redeploy already protects
(`--filter 'P /downloads/**'`), so a redeploy keeps it. Until a first download
is counted, `total` is 0 and the page shows nothing.

## Check it locally

```sh
python3 scripts/site/download-stats/count-downloads.py \
  --logs <folder with connecto-downloads*.log> --downloads <folder with the files> --out /tmp/stats.json
```

To see the counter on a local preview, put a `stats.json` like the one above in
`site/downloads/` and run `python3 -m http.server 8080 --directory site`.
