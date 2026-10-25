"""Offloaded uploads (section 4.11 of the design specification).

On a route with ``upload = 'multipart'`` or ``'raw'`` the edge receives
the request body before the application sees the request and stores each
file. A multipart form arrives with each file field replaced by text
fields: for a field named ``avatar``, ``avatar.name``, ``avatar.content_type``,
``avatar.size``, ``avatar.sha256`` and ``avatar.ref``. A raw upload arrives
with an empty body. The files themselves are reached through this module::

    from muru import uploads

    for upload in uploads.files(environ):
        upload.save(f"/srv/media/{upload.ref}")

    avatar = uploads.get(environ, form["avatar.ref"])

Upload objects belong to their request: the files they describe are closed
when the request ends, and a file that was not saved disappears. Outside
Muru, and on a route that does not offload uploads, ``files()`` returns an
empty list, so the same code runs anywhere.
"""

from __future__ import annotations

import errno
import io
import os
import secrets
import sys
from collections.abc import Mapping
from typing import Any

__all__ = ["Upload", "files", "get", "is_precheck"]

# How much a copy moves at a time.

_BLOCK = 1024 * 1024


def files(request: Mapping[str, Any]) -> list[Upload]:
    """Return every upload of a WSGI environ or ASGI scope, in the order they arrived."""

    return [Upload(item) for item in _described(request)]


def get(request: Mapping[str, Any], ref: str | bytes) -> Upload | None:
    """Find the upload a form's ``FIELD.ref`` value names; None when none does."""

    wanted = ref.decode() if isinstance(ref, bytes) else str(ref)

    for item in _described(request):
        if item["ref"] == wanted:
            return Upload(item)

    return None


def is_precheck(request: Mapping[str, Any]) -> bool:
    """Whether the request is a precheck: the headers alone, sent before the body.

    A route with ``precheck = true`` asks the application first; a 2xx
    answer lets the edge read the body and send the request again with
    the files, and anything else is the client's answer.
    """

    muru = _muru(request)

    if muru is not None:
        return bool(muru.get("precheck", False))

    return bool(request.get("muru.precheck", False))


def _muru(request: Mapping[str, Any]) -> Mapping[str, Any] | None:
    """Return an ASGI scope's ``extensions["muru"]``; None for a WSGI environ."""

    extensions = request.get("extensions")

    if not isinstance(extensions, Mapping):
        return None

    muru = extensions.get("muru")
    return muru if isinstance(muru, Mapping) else {}


def _described(request: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    muru = _muru(request)
    described = muru.get("uploads") if muru is not None else request.get("muru.uploads")
    return list(described) if isinstance(described, list) else []


class Upload:
    """A file the edge received and stored for the current request.

    ``field`` is the form field the file came in, or None for a raw
    upload; ``name`` and ``content_type`` are what the client gave, either
    of which may be empty; ``size`` is the bytes received; ``ref`` is the
    reference the form carries as ``FIELD.ref``; and ``digests`` holds the
    hex digests computed while the file arrived, by algorithm.
    """

    def __init__(self, described: Mapping[str, Any]) -> None:
        self._fd: int = described["fd"]
        self._spool_path: str | None = described.get("path")
        self.field: str | None = described.get("field")
        self.name: str = described.get("name", "")
        self.content_type: str = described.get("content_type", "")
        self.size: int = described.get("size", 0)
        self.ref: str = described["ref"]
        self.digests: dict[str, str] = dict(described.get("digests", {}))

    def __repr__(self) -> str:
        return f"<Upload ref={self.ref!r} field={self.field!r} name={self.name!r} size={self.size}>"

    @property
    def sha256(self) -> str | None:
        """The SHA-256 digest in hex, or None when the route computes none."""

        return self.digests.get("sha256")

    @property
    def path(self) -> str:
        """A path that opens the file, for libraries that need one.

        It names the request's descriptor, so it is valid only while the
        request lasts and only in this process.
        """

        if sys.platform.startswith("linux"):
            return f"/proc/self/fd/{self._fd}"

        return f"/dev/fd/{self._fd}"

    def open(self) -> io.BufferedReader:
        """Open a readable, seekable binary file over the upload, from its start.

        Each call gives an independent file with its own position; close
        it, or use it in a ``with`` statement, when done.
        """

        return io.BufferedReader(_Reader(self._fd, self.name))

    def save(self, dest: str | os.PathLike[str], overwrite: bool = False) -> str:
        """Give the file a permanent name and return it.

        The file is linked into place when the destination is on the
        spool's file system and the worker may link it, which takes no
        time whatever the size; otherwise it is copied. An existing
        destination raises ``FileExistsError`` unless ``overwrite`` is
        true, in which case it is replaced in one step.
        """

        target = os.fspath(dest)

        if not overwrite and os.path.lexists(target):
            raise FileExistsError(errno.EEXIST, os.strerror(errno.EEXIST), target)

        source = self._link_source()

        if source is not None:
            try:
                _link(source, target, overwrite)
                return target
            except FileExistsError:
                raise
            except OSError:
                # Another file system, or a spool file the worker's user
                # may not link: a copy does instead.

                pass

        self._copy(target, overwrite)
        return target

    def _link_source(self) -> str | None:
        if self._spool_path is not None:
            return self._spool_path

        if sys.platform.startswith("linux"):
            return f"/proc/self/fd/{self._fd}"

        return None

    def _copy(self, target: str, overwrite: bool) -> None:
        destination = _temporary_name(target) if overwrite else target
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_CLOEXEC", 0)
        out = os.open(destination, flags, 0o666)

        try:
            offset = 0

            while chunk := os.pread(self._fd, _BLOCK, offset):
                view = memoryview(chunk)

                while view:
                    view = view[os.write(out, view) :]

                offset += len(chunk)
        except BaseException:
            os.close(out)
            os.unlink(destination)
            raise

        os.close(out)

        if overwrite:
            os.replace(destination, target)


def _link(source: str, target: str, overwrite: bool) -> None:
    if not overwrite:
        os.link(source, target)
        return

    temporary = _temporary_name(target)
    os.link(source, temporary)

    try:
        os.replace(temporary, target)
    except BaseException:
        os.unlink(temporary)
        raise


def _temporary_name(target: str) -> str:
    directory, name = os.path.split(target)
    return os.path.join(directory, f".{name}.{os.getpid()}.{secrets.token_hex(4)}.tmp")


class _Reader(io.RawIOBase):
    """Reads an upload's descriptor at a position of its own."""

    def __init__(self, fd: int, name: str) -> None:
        super().__init__()
        self._fd = os.dup(fd)
        self._position = 0
        self.name = name

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return True

    def fileno(self) -> int:
        return self._fd

    def readinto(self, buffer: Any) -> int:
        data = os.pread(self._fd, len(buffer), self._position)
        count = len(data)
        buffer[:count] = data
        self._position += count
        return count

    def seek(self, offset: int, whence: int = os.SEEK_SET) -> int:
        if whence == os.SEEK_SET:
            position = offset
        elif whence == os.SEEK_CUR:
            position = self._position + offset
        elif whence == os.SEEK_END:
            position = os.fstat(self._fd).st_size + offset
        else:
            raise ValueError(f"invalid whence ({whence})")

        if position < 0:
            raise ValueError(f"negative seek position {position}")

        self._position = position
        return position

    def tell(self) -> int:
        return self._position

    def close(self) -> None:
        if not self.closed:
            os.close(self._fd)

        super().close()
