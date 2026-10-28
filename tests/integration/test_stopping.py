"""Stopping a development run (design specification section 13.1).

Ctrl-C in a terminal sends SIGINT to the whole foreground process group.
muructl alone acts on it: the edge stops accepting and finishes the
requests it holds, then the host is told to stop and stops its workers,
so no request is written to a worker that is going and nothing in the
window is answered as a crash.
"""

from __future__ import annotations

import http.client
import os
import re
import signal
import threading
import time
from collections import Counter

from support import server as support


def test_ctrl_c_answers_no_request_as_a_crash() -> None:
    server = support.start("plain_wsgi:application", workers=2, threads=2, new_session=True)
    outcomes: Counter[str] = Counter()
    failures: list[bytes] = []

    def hammer() -> None:
        while server.process.poll() is None:
            try:
                connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=2)
                connection.request("GET", "/")
                response = connection.getresponse()
                body = response.read()
                connection.close()
                outcomes[str(response.status)] += 1

                if response.status != 200:
                    failures.append(body)
            except (OSError, http.client.HTTPException) as error:
                outcomes[type(error).__name__] += 1

            time.sleep(0.002)

    thread = threading.Thread(target=hammer, daemon=True)

    try:
        assert server.get("/").status == 200
        thread.start()
        time.sleep(0.2)

        # The whole process group, as the terminal does.

        os.killpg(server.process.pid, signal.SIGINT)
        server.process.wait(timeout=30)
    finally:
        server.stop()

    thread.join(timeout=10)
    assert outcomes["200"] > 0, outcomes

    # Once the edge has stopped accepting, a client gets a refused or
    # reset connection, never an answer that blames the worker.

    assert not failures, (outcomes, failures)
    assert len(re.findall(r"received SIG\w+, stopping", server.log())) == 1, server.log()
