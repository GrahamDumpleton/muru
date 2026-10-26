Added `h2c` on a listener, naming the clients that may speak HTTP/2 in cleartext on it; another client opening with HTTP/2 is answered 505, and its HTTP/1.1 requests are served.
