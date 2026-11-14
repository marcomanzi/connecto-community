# Download count for the site

The landing page shows "**N** downloads · +N this week" in the hero. The number
comes from `downloads/stats.json` on the site, a static file that a timer on the
server rewrites every hour. Nothing runs in the app, and the visitor's browser
calls no API: it only fetches that file from the same site. If the file is
missing or holds no usable number, the page shows nothing.

## How it is counted

The release files are served by Caddy from the site's `downloads/` folder. Caddy
writes one JSON line per request for a `.dmg` or `.zip` in that
folder into `connecto-downloads.log` in its log folder, without the IP address, port,
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

## Install (on your web server, as root)

These steps assume Caddy serves the site; adapt the paths to your server.

1. Copy `count-downloads.py`, the two systemd units and `caddy-snippet.caddy`
   to the server, then install the script and the timer:

   ```sh
   install -d /usr/local/lib/connecto
   install -m 755 count-downloads.py /usr/local/lib/connecto/count-downloads.py
   install -m 644 connecto-download-stats.service connecto-download-stats.timer /etc/systemd/system/
   systemctl daemon-reload
   ```

   In `connecto-download-stats.service`, set `--logs` to the folder Caddy writes
   the download log to, and `--downloads` to the site's downloads folder.

2. Turn the log on: add the lines of `caddy-snippet.caddy` (without the comment
   header) inside the site block that serves `/downloads/`, before
   `file_server`, with the log path you chose. Back up the Caddyfile first, then:

   ```sh
   caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
   caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile
   ```

3. Start the timer and run it once:

   ```sh
   systemctl enable --now connecto-download-stats.timer
   systemctl start connecto-download-stats.service
   curl -fsS https://<your site>/downloads/stats.json
   ```

Keep `stats.json` when you redeploy the site (it sits in `downloads/`).

## Check it locally

```sh
python3 scripts/site/download-stats/count-downloads.py \
  --logs <folder with connecto-downloads*.log> --downloads <folder with the files> --out /tmp/stats.json
```

To see the counter on a local preview, put a `stats.json` like the one above in
`site/downloads/` and run `python3 -m http.server 8080 --directory site`.
