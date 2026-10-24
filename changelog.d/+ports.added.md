Added `--port` to `muructl run`, with `--port auto` picking free ports and
printing them, and `--bind` now takes an address alone. A port already in
use is reported with the process holding it, found through lsof or ss.
