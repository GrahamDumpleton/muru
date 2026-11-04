"""Hooks that close the socket unsafe_import_wsgi leaves open before fork and reopen it after."""

from __future__ import annotations

import socket

import unsafe_import_wsgi

from muru import Context


def pre_fork(context: Context) -> None:
    for end in unsafe_import_wsgi.LEFT_OPEN:
        end.close()


def post_fork(context: Context) -> None:
    unsafe_import_wsgi.LEFT_OPEN = socket.socketpair()
