# Integration tests

pytest scenarios driving muructl against the fixture applications in
`tests/fixtures/apps`, using the `muru-testkit` crate. The scenarios are
listed in section 15.2 of the design specification. They are added from
phase 1 onwards.

Run with `just test-integration`. Scenarios that need Docker, network, root or
a long time carry markers and are skipped by default. Helpers for starting
muructl and talking to it are in `tests/support`.
