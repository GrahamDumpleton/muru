from flask import Flask, request

app = Flask(__name__)


@app.get("/")
def index():
    if "muru.request_id" in request.environ:
        server = "Muru"
    else:
        server = request.environ["SERVER_SOFTWARE"]

    return f"Hello from {server}\n"
