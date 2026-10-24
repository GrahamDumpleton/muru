"""Application threads: fixed, long-lived, one Python thread state each."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor

from support.server import Server, text


def identity(server: Server) -> tuple[str, str, str]:
    body = text(server.get("/threads/identity?sleep=0.05"))
    fields = dict(part.split("=", 1) for part in body.split(","))
    return fields["pid"], fields["native"], fields["tstate"]


def test_each_native_thread_keeps_one_thread_state(server: Server) -> None:
    # Section 6.5: a thread attaches once, so the native thread and the
    # Python thread state pair up for the life of the worker, and no two
    # native threads ever share a thread state.

    slots = server.workers * server.threads
    with ThreadPoolExecutor(max_workers=slots * 2) as pool:
        seen = list(pool.map(lambda _i: identity(server), range(slots * 8)))

    by_thread: dict[tuple[str, str], set[str]] = {}
    by_state: dict[tuple[str, str], set[str]] = {}
    for pid, native, tstate in seen:
        by_thread.setdefault((pid, native), set()).add(tstate)
        by_state.setdefault((pid, tstate), set()).add(native)
    assert all(len(states) == 1 for states in by_thread.values()), by_thread
    assert all(len(threads) == 1 for threads in by_state.values()), by_state

    # Every worker used every one of its threads at least once.

    per_worker: dict[str, set[str]] = {}
    for pid, native, _tstate in seen:
        per_worker.setdefault(pid, set()).add(native)
    assert len(per_worker) == server.workers
    assert all(len(natives) == server.threads for natives in per_worker.values()), per_worker
