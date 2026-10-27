# Benchmarks

Scenarios and scripts for the performance comparisons in section 15.4 of the
design specification. There are three scenarios, each a WSGI application
under `apps/`:

| Scenario | Application | Measures |
| --- | --- | --- |
| hello | `hello.py`, a 14 byte response | per-request overhead |
| large | `large.py`, one 1 MiB response | bulk transfer and copies |
| chunks | `chunks.py`, 1000 yields of 16 bytes | per-chunk cost and coalescing |

`run.py` starts each server in turn, warms it up, measures it with
[oha](https://github.com/hatoo/oha) and stops it. The comparison set is
gunicorn (sync and gthread), uvicorn (WSGI interface), Granian and
mod_wsgi-express in three configurations: embedded mode, daemon mode with
defaults, and daemon mode with reloading disabled and the queue timeout set
to 0. Servers that are not installed are skipped.

Muru is run three ways:

| Server | What it is |
| --- | --- |
| `muru` | `muructl run` with `--access-log off`, as the other servers log nothing per request |
| `muru-logged` | `muructl run` with its default access log to the terminal |
| `muru-baseline` | an earlier build, given with `--baseline-muructl` and `--baseline-python` |

```
just bench --list                                   # what is installed
just bench                                          # everything, 10 s each
just bench --servers muru,gunicorn-gthread --scenarios hello --duration 20s
just vm-bench --baseline a1e7a67 --connections 32 --repeat 3
```

Muru is run from `target/release/muructl` (`cargo build --release -p muructl`,
or set `MURUCTL`), with the runtime library built optimised into the
interpreter running the harness (`cd packages/muru && uv run --project ../..
maturin develop --release`; `just dev` builds it unoptimised, which is not
what a wheel ships). `just vm-bench --baseline REV` builds REV the same way
beside the working tree, in the VM, and runs it as `muru-baseline`, so that
the cost added to a request since then is measured on the same machine in
the same run; a1e7a67 is the phase 0 merge, whose numbers are in ADR 0021. The Python servers come from the `bench`
dependency group (`uv sync --group bench`). mod_wsgi-express needs Apache
httpd with its development files (`brew install httpd` on macOS, `apt install
apache2-dev` on Debian or Ubuntu) and then `uv pip install mod_wsgi`, which
builds against them; it is not in the dependency group for that reason.

Results are stored as JSON under `results/` (ignored by git); `just
vm-bench` leaves them in the VM, under `~/muru/bench/results/`. Run on Linux
for representative numbers; macOS numbers are useful only relative to each
other on the same machine.

Servers without keep-alive (gunicorn sync, and uvicorn's WSGI interface)
open a new connection per request, which exhausts the ephemeral port range
within seconds on macOS and corrupts the run that follows with connection
errors. On macOS keep `--connections` at 32 or below and pass `--pause 15`
so the TIME_WAIT sockets drain between servers; a run reports its success
rate and error counts, and anything below 1.0 is not a valid measurement.
