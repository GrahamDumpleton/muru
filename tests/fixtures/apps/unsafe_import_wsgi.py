"""An application whose import leaves a connected socket open, which fork would share."""

from __future__ import annotations

import socket

from preload_wsgi import application

LEFT_OPEN = socket.socketpair()

__all__ = ["LEFT_OPEN", "application"]
