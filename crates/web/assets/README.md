# Dashboard assets

Embedded in the binary; nothing is fetched at runtime.

- `htmx.min.js`, `htmx.LICENSE`: htmx 2.0.11 (0BSD), `dist/htmx.min.js`
  from the npm package `htmx.org`. To update, download the new tarball
  from `https://registry.npmjs.org/htmx.org/-/htmx.org-<version>.tgz`,
  check it against the `dist.integrity` npm lists for that version, and
  copy `package/dist/htmx.min.js` and `package/LICENSE` here.
- `app.js`: the dashboard's keyboard shortcuts.
- `style.css`: the dashboard's styles.
- `favicon.svg`: the dashboard's icon, drawn for this project.
