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
- `twemoji.woff2`, `twemoji.LICENSE`: the emoji in `emoji.txt` from
  Mozilla's [Twemoji COLR font](https://github.com/mozilla/twemoji-colr)
  v0.7.0 (build Apache-2.0), whose graphics are
  [Twemoji](https://github.com/twitter/twemoji), copyright Twitter, Inc and
  other contributors, licensed under CC-BY 4.0. Changed from upstream:
  subset to those code points and converted to WOFF2, by
  `mise run emoji-font`.
- `emoji.txt`: the emoji the dashboard's markup uses. Adding one to the
  markup means adding it here and to `style.css`'s `unicode-range`, then
  running `mise run emoji-font`.
