# Appendix D. Environ keys, scope extensions and environment variables

```{admonition} Design specification
:class: note

This page is part of the Muru design specification and describes intended
design, for contributors. The user documentation is authoritative for released
behaviour. See the [specification index](index.md) for status and how the
specification is maintained.
```

## D.1 WSGI environ

| Key | Value |
| --- | --- |
| REQUEST_METHOD, SCRIPT_NAME, PATH_INFO, QUERY_STRING | from the request; SCRIPT_NAME is the trusted prefix plus the mount path |
| CONTENT_TYPE, CONTENT_LENGTH | from headers; CONTENT_LENGTH also set for buffered chunked bodies |
| SERVER_NAME, SERVER_PORT, SERVER_PROTOCOL | matched or canonical host name, resolved port, client protocol (HTTP/1.1, HTTP/2, HTTP/3) |
| REMOTE_ADDR, REMOTE_PORT | resolved client; REMOTE_PORT only when known |
| HTTP_* | request headers, after stripping and normalisation |
| HTTPS | on for https |
| REQUEST_ID | the request id, also in the access log |
| REDIRECT_URL, REDIRECT_QUERY_STRING, REDIRECT_STATUS | on internal redirects and error page handlers |
| wsgi.version, wsgi.url_scheme, wsgi.input, wsgi.errors | as PEP 3333; wsgi.errors writes to the worker's stderr stream |
| wsgi.multithread, wsgi.multiprocess, wsgi.run_once | section 6.5 |
| wsgi.input_terminated | always True |
| wsgi.file_wrapper | the muru._native.FileWrapper class: block iteration, descriptor passing when the wrapper is returned as the iterable (section 6.5) |
| muru.wsgi_to_asgi | the handoff object of section 6.14, present when the worker has an event loop |
| muru.state | the ASGI scope state of a request handed from ASGI to WSGI (section 6.14) |
| muru.request_id, muru.release, muru.group, muru.mount | request and placement |
| muru.proxy_addr | original peer when behind trusted proxies |
| muru.host_captures | named groups from regular expression host names |
| muru.internal, muru.error_reason | internal dispatch flag and the error reason for error handlers |
| muru.precheck | true for upload precheck requests |
| muru.uploads | upload references, read through muru.uploads.files() |
| muru.max_body_size | limit in effect |
| muru.deadline | absolute request deadline (Unix time, seconds) |
| muru.tls | dictionary of TLS details, present over HTTPS only: version (TLSv1.3 or TLSv1.2), cipher (the negotiated suite), server_name (the SNI, empty when the client sent none) alpn (the negotiated application protocol, empty when none) and ja4 (the JA4 fingerprint of the ClientHello, section 4.14, empty when it could not be read); client certificate details when configured |
| muru.client_class | client class assigned by the edge (section 4.14) |
| muru.mode | site mode such as read-only, usually set by an overlay (sections 4.15 and 9.10) |
| muru.overlays | names of overlays active for this request's route |
| muru.auth_request, muru.original_method, muru.original_uri, muru.original_host | set on authentication sub-requests (section 4.16) |

## D.2 ASGI scope

Standard ASGI 3.0 keys (type, asgi, http_version, method, scheme, path, raw_path, query_string, root_path, headers, client, server, state), with root_path set to the trusted prefix plus the mount path. Muru values are under scope['extensions']['muru'] with the same names as the muru.* environ keys, plus asgi_to_wsgi, the handoff object of section 6.14, on http scopes. Supported extensions: http.response.trailers, http.response.pathsend and http.response.zerocopysend (file hints), and tls.

## D.3 Environment variables set by Muru

| Variable | Meaning |
| --- | --- |
| MURU_APP, MURU_RELEASE | application and release |
| MURU_GROUP, MURU_WORKER_ID | worker group and worker id, set after fork |
| MURU_RELEASE_DIR, MURU_RUN_DIR | release source (also the working directory) and run directory |
| MURU_INTERFACE, MURU_PROCESSES, MURU_THREADS | default mount interface and process model |
| MURU_LOG_LEVEL, MURU_LOG_FORMAT | Python logging defaults |
| MURU_DEVELOPMENT | set to 1 in muructl run |

MURU_* variables cannot be overridden by application or release settings.

## D.4 Environment variables read by muructl

| Variable | Meaning |
| --- | --- |
| MURU_CONFIG | daemon configuration file |
| MURU_SOCKET, MURU_SERVER | admin endpoint for client commands |
| MURU_UV | path to uv |
| MURU_LOG | muructl's own log filter (tracing syntax) |
| NO_COLOR | disable coloured output |
| MURU_STATE_DIR, MURU_RUN_DIR, MURU_LOG_DIR, MURU_CACHE_DIR | override file locations (section 13.8) |
| STATE_DIRECTORY, RUNTIME_DIRECTORY, LOGS_DIRECTORY, CACHE_DIRECTORY, CONFIGURATION_DIRECTORY | set by systemd from the unit's directory settings and used when present |
| XDG_CONFIG_HOME, XDG_STATE_HOME, XDG_RUNTIME_DIR, XDG_CACHE_HOME | user-mode defaults on Linux |
