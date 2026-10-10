"""端到端测试的公共部分：测试内起本地 HTTP 服务提供 HLS 样本（分片取自 tests/fixtures/media）。"""

import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

MEDIA = Path(__file__).parent.parent / "fixtures" / "media"


def fixture(name: str) -> bytes:
    """样本目录 tests/fixtures/media 里的文件内容。"""
    return (MEDIA / name).read_bytes()


class Gate:
    """让一个路径的请求停住：请求到达时 `arrived` 置位，`release` 之后才响应。"""

    def __init__(self) -> None:
        """未到达、未放行。"""
        self.arrived = threading.Event()
        self._released = threading.Event()

    def release(self) -> None:
        """放行，让停住的请求继续。"""
        self._released.set()

    def hold(self) -> None:
        """服务线程里调用：标记已到达，等到放行。"""
        self.arrived.set()
        self._released.wait()


class Server:
    """本地 HTTP 服务：按路径返回放进来的内容或状态码，记下各路径被请求的次数与最近一次的请求头。"""

    def __init__(self) -> None:
        """在 127.0.0.1 的随机端口上开始服务。"""
        self._lock = threading.Lock()
        self._bodies: dict[str, bytes] = {}
        self._statuses: dict[str, int] = {}
        self._hits: dict[str, int] = {}
        self._headers: dict[str, list[tuple[str, str]]] = {}
        self._gates: dict[str, Gate] = {}
        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self._thread = threading.Thread(target=self._httpd.serve_forever)
        self._thread.start()

    def url(self, path: str) -> str:
        """`path`（不以 / 开头）的完整地址。"""
        host, port = self._httpd.server_address[:2]
        return f"http://{host!s}:{port}/{path}"

    def put(self, path: str, body: bytes | str) -> None:
        """`path` 返回 200 与 `body`（`str` 按 UTF-8 编码）。"""
        with self._lock:
            self._bodies[path] = body.encode() if isinstance(body, str) else body
            self._statuses.pop(path, None)

    def status(self, path: str, code: int) -> None:
        """`path` 返回状态码 `code`，没有内容。"""
        with self._lock:
            self._statuses[path] = code

    def gate(self, path: str) -> Gate:
        """之后对 `path` 的请求先停住，等返回的闸门放行。"""
        gate = Gate()
        with self._lock:
            self._gates[path] = gate
        return gate

    def hits(self, path: str) -> int:
        """`path` 被请求的次数。"""
        with self._lock:
            return self._hits.get(path, 0)

    def headers(self, path: str) -> list[tuple[str, str]]:
        """最近一次请求 `path` 时带的请求头；没被请求过时为空。"""
        with self._lock:
            return list(self._headers.get(path, []))

    def close(self) -> None:
        """放行所有闸门，停止服务，等服务线程结束。"""
        with self._lock:
            gates = list(self._gates.values())
        for gate in gates:
            gate.release()
        self._httpd.shutdown()
        self._httpd.server_close()
        self._thread.join()

    def respond(self, path: str, headers: list[tuple[str, str]]) -> tuple[int, bytes]:
        """记下这次请求，有闸门时先停住，再返回状态码与内容。

        设了状态码的返回它，放了内容的返回 200，都没有的返回 404。
        """
        with self._lock:
            self._hits[path] = self._hits.get(path, 0) + 1
            self._headers[path] = headers
            gate = self._gates.get(path)
        if gate is not None:
            gate.hold()
        with self._lock:
            if path in self._statuses:
                return self._statuses[path], b""
            if path in self._bodies:
                return 200, self._bodies[path]
            return 404, b""

    def _handler(self) -> type[BaseHTTPRequestHandler]:
        server = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                path = self.path.lstrip("/").split("?", 1)[0]
                status, body = server.respond(path, list(self.headers.items()))
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        return Handler


@pytest.fixture
def server() -> Iterator[Server]:
    """每个测试一个本地 HTTP 服务，测试结束时停止。"""
    instance = Server()
    yield instance
    instance.close()
