# Changelog fragments

Every user-visible change adds a fragment here. towncrier assembles them into
`CHANGELOG.md` when a release is prepared, then removes them.

A fragment is a small Markdown file named `<issue>.<type>.md`, or
`+<slug>.<type>.md` when there is no issue, where `<type>` is one of
`added`, `changed`, `deprecated`, `removed`, `fixed`, `security` or `docs`.
It contains one or two sentences written for users, in the past tense, for
example:

```
Added the `--https` flag to `muructl run`, which serves HTTPS on port 8443
with a locally trusted development certificate.
```

Check the result with `uv run towncrier build --draft`.
