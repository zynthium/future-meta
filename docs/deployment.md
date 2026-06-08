# future-meta Deployment

First version uses GitHub Actions for scheduled daemon execution and Cloudflare Pages free tier for static distribution.

## Current Cloudflare Deployment

- Project name: `future-meta`
- Production URL: `https://future-meta.pages.dev`
- Manifest URL: `https://future-meta.pages.dev/manifest.json`
- First manual deployment completed with Wrangler direct upload.

## Required GitHub Secrets

- `CLOUDFLARE_API_TOKEN`: token allowed to deploy Cloudflare Pages.
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account id.

## Cloudflare Pages Project

Project name: `future-meta`

The workflow deploys:

- `public/manifest.json`
- `public/latest.fmeta.zst`
- `public/artifacts/*.fmeta.zst`

## Update Cadence

The hourly schedule runs the daemon with source-state probe skipping enabled. This avoids re-parsing and re-writing unchanged sources.

The daily schedule runs with `--force-full`. This is required for the current free-tier implementation because the first probe hashes stable source URLs, not remote CSV body content. The daily full run prevents silently missing a same-URL CSV content update for longer than one day.

## Manual Run

Use GitHub Actions `workflow_dispatch`.

Set `force_full=true` to bypass source probe skip logic and refresh every CSV source.

For local direct upload after generating `public/`:

```bash
wrangler pages deploy public --project-name=future-meta --branch=main --commit-dirty=true
```

## Client URL

Default manifest URL:

`https://future-meta.pages.dev/manifest.json`
