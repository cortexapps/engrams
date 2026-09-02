# site

The public site: the landing page at `/` and the docs at `/docs/`. Astro with Starlight,
built to static files and published to GitHub Pages by `.github/workflows/site-deploy.yml`
on every push to `main` that touches this directory.

```sh
pnpm install
pnpm dev       # http://localhost:4321
pnpm lint      # the leak lint: no internal names, no machine-written prose
pnpm build     # lint + astro check + astro build → dist/
pnpm preview   # serve dist/
```

Two build-time env vars pick the deploy target. Unset, the build is for `http://localhost:4321`
at `/`.

| Variable | Meaning | Access-controlled Pages | Public project page |
|---|---|---|---|
| `SITE_URL` | absolute origin for canonical, sitemap, and Open Graph URLs | `https://<host>.pages.github.io` | `https://<owner>.github.io` |
| `BASE_PATH` | path prefix the site is served under | `/` | `/<repo>` |

A custom domain is `SITE_URL=https://<domain>` plus a `public/CNAME` file.

Docs pages are hand-written Markdown under `src/content/docs/docs/`, one directory per sidebar
section. The site never cites decision records or internal infrastructure, and `pnpm lint`
fails the build if a page does.
