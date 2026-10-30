import time
from urllib.parse import parse_qs


def fetch_rows(months):
    time.sleep(0.1 * months)  # as if waiting on a slow database query
    return range(8_000_000 * months)


def summarise(rows):
    total = 0

    for row in rows:
        total += row * row % 7

    return total


def build_report(months):
    rows = fetch_rows(months)
    return f"{months} months: {summarise(rows)}\n"


def application(environ, start_response):
    query = parse_qs(environ["QUERY_STRING"])
    months = int(query.get("months", ["1"])[0])

    if environ["PATH_INFO"] == "/report":
        body = build_report(months)
    else:
        body = "ok\n"

    start_response("200 OK", [("Content-Type", "text/plain")])
    return [body.encode()]
