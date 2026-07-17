# Ritz documentation site

This folder is the GitHub Pages site (plain static HTML/CSS, no build step).

**To publish:** repo *Settings → Pages → Build and deployment → Source: Deploy from a
branch*, then select branch `master` and folder `/docs`. The site goes live at
`https://ritze03.github.io/ritz/`.

Pages: `index.html` (home + quickstart), `usage.html` (usage guide), `extensions.html`
(module/extension reference). Styling in `style.css` mirrors the app's "Graphite" theme
(`crates/ritz-app/src/theme.rs`); `.nojekyll` disables Jekyll so the raw HTML is served.
