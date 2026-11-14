# Connecto site

A static site: the landing page (`index.html`) and the docs (`docs/`). No build step.

Preview it:

```sh
python3 -m http.server 8080 --directory site
```

- **Publish a build**: put the file in `downloads/` and add an entry to `assets/releases.js`. The download cards on the landing page come from that list.
- **Add a docs page**: copy a page in `docs/`, set `<body data-page="...">`, and add it to `NAV` in `assets/docs.js`.
- **Support amounts**: the PayPal links are in the `#donate` section of `index.html` ("Sustain open-source development").
- **Download count**: the hero reads `downloads/stats.json`, written hourly on the server by `scripts/site/download-stats/` (see its README). Without that file the count is simply not shown.
