import time


def application(environ, start_response):
    time.sleep(0.2)  # as if waiting on a slow database query
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"done\n"]
