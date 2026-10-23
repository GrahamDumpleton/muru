# Dashboard

Server-rendered askama templates kept live with htmx and its Server-Sent
Events extension, with uPlot for charts (design specification section 10.7
and ADR 0011). Everything here is embedded into the binary by the
`muru-dashboard` crate with rust-embed, so building needs only cargo.

- `templates/`: askama templates.
- `static/`: stylesheets and the dashboard's own small scripts.
- `vendor/`: htmx, the htmx SSE extension and uPlot, pinned by version and
  updated by hand (see `vendor/README.md`).
