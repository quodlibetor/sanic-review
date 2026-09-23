# GitHub's GraphQL schema

`github.graphql` is GitHub's public GraphQL schema (SDL), vendored so the
tests can check every query we send against it offline; see
`src/schema_check.rs`.

Downloaded on 2026-09-23 from GitHub's published copy:
<https://docs.github.com/public/fpt/schema.docs.graphql>

To refresh it, from the repo root:

```sh
curl -fsSL -o crates/github/schema/github.graphql https://docs.github.com/public/fpt/schema.docs.graphql
```

The file is over jj's default 1 MiB limit for new files, so snapshot a
refresh with `jj --config snapshot.max-new-file-size=4MiB st` if jj refuses
it.
