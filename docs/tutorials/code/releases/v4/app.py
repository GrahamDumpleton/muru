import itertools

import muru

MESSAGE = "Howdy"
counter = itertools.count(1)


def application(environ, start_response):
    if next(counter) % 4 == 0:
        raise RuntimeError("one request in four fails")

    where = muru.context()
    body = f"{MESSAGE} from {where.app}@{where.release}\n".encode()
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [body]
