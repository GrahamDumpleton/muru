def application(environ, start_response):
    body = f"The shop, at {environ['wsgi.url_scheme']}://{environ['SERVER_NAME']}\n"
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [body.encode()]
