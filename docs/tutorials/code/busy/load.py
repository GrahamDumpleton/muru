import sys
import threading
import time
import urllib.request

clients = int(sys.argv[1])
seconds = float(sys.argv[2])
url = sys.argv[3] if len(sys.argv) > 3 else "http://127.0.0.1:8080/"
served = []
failed = []
end = time.monotonic() + seconds


def client():
    while time.monotonic() < end:
        try:
            with urllib.request.urlopen(url, timeout=30) as response:
                response.read()
            served.append(1)
        except OSError:
            failed.append(1)


threads = [threading.Thread(target=client) for _ in range(clients)]

for thread in threads:
    thread.start()

while time.monotonic() < end:
    time.sleep(5)
    print(f"{len(served)} requests served, {len(failed)} failed", flush=True)

for thread in threads:
    thread.join()
