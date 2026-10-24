"""wsgi.file_wrapper: files, offsets, Content-Length and the fallback."""

from __future__ import annotations

from support.server import Server

FILE_CONTENT = b"file_wrapper test: SUCCESS\n" * 100

AREA = "/file-wrapper"


def test_whole_file(server: Server) -> None:
    response = server.get(f"{AREA}/basic")
    assert response.status == 200
    assert response.body == FILE_CONTENT


def test_with_content_length(server: Server) -> None:
    response = server.get(f"{AREA}/with-content-length")
    assert response.header("Content-Length") == str(len(FILE_CONTENT))
    assert response.body == FILE_CONTENT


def test_starts_at_the_file_position(server: Server) -> None:
    response = server.get(f"{AREA}/partial")
    assert response.body == FILE_CONTENT[len(b"file_wrapper test: SUCCESS\n") :]


def test_content_length_bounds_the_range(server: Server) -> None:
    response = server.get(f"{AREA}/range")
    assert response.body == FILE_CONTENT[100:150]


def test_non_file_objects_are_iterated(server: Server) -> None:
    response = server.get(f"{AREA}/iterable-fallback")
    assert response.body == b"file_wrapper iterable fallback: SUCCESS"


def test_block_size_is_honoured_by_iteration(server: Server) -> None:
    assert server.get(f"{AREA}/small-blocks").body == b"0123456789" * 10


def test_wrapper_is_an_ordinary_iterable(server: Server) -> None:
    assert server.get(f"{AREA}/wrapped-in-list").body == FILE_CONTENT


def test_file_is_closed_after_sending(server: Server) -> None:
    server.mark_log()
    assert server.get(f"{AREA}/closes-file").body == b"closed after sending"
    assert server.wait_for_log("MARKER_FILE_CLOSED")
