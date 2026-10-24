Added memory limits for workers: `[tool.muru.memory]` or `--memory
KEY=VALUE` set a soft limit, over which a worker reclaims and is then
recycled, and a hard limit, over which it is replaced at once, measured
as USS, PSS or RSS; each worker's memory and requests are served on
`/metrics`.
