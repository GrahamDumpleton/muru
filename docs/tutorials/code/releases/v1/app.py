import muru

MESSAGE = "Hello"


def application(environ, start_response):
    where = muru.context()
    body = f"{MESSAGE} from {where.app}@{where.release}\n".encode()
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [body]
