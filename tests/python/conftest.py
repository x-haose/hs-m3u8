"""端到端测试的公共部分：测试内起本地 HTTP(S) 服务提供 HLS 样本（分片取自 tests/fixtures/media）。"""

import base64
import copy
import pickle
import selectors
import socket
import ssl
import textwrap
import threading
from collections.abc import Iterable, Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

FIXTURES = Path(__file__).parent.parent / "fixtures"
MEDIA = FIXTURES / "media"


def fixture(name: str) -> bytes:
    """样本目录 tests/fixtures/media 里的文件内容。"""
    return (MEDIA / name).read_bytes()


def assert_survives_pickle(value: object) -> None:
    """`value` 经 pickle、`copy.copy`、`copy.deepcopy` 重建后类型与值都不变。"""
    for rebuilt in (pickle.loads(pickle.dumps(value)), copy.copy(value), copy.deepcopy(value)):
        assert type(rebuilt) is type(value)
        assert rebuilt == value


# 停住的请求最多等多久，秒；只在测试出错、没有放行时才会等满
HOLD_LIMIT = 30.0


class Gate:
    """让一个路径的请求停住，放行之后才响应。

    请求到达时 `arrived` 置位；停住期间客户端断开连接（如任务被取消）时 `abandoned` 置位、不再响应。
    """

    def __init__(self) -> None:
        """未到达、未放行。"""
        self.arrived = threading.Event()
        self.abandoned = threading.Event()
        # 放行即往里写一个字节，之后一直可读：停住的与之后到达的请求都能用 select 同时等它与客户端的连接
        self._released, self._release = socket.socketpair()

    def release(self) -> None:
        """放行，让停住的与之后到达的请求继续。"""
        self._release.send(b"\0")

    def hold(self, client: socket.socket) -> bool:
        """服务线程里调用：标记已到达，等到放行时返回 True；`client` 先断开时置位 `abandoned`，返回 False。"""
        self.arrived.set()
        with selectors.DefaultSelector() as selector:
            selector.register(self._released, selectors.EVENT_READ)
            selector.register(client, selectors.EVENT_READ)
            while True:
                ready = selector.select(HOLD_LIMIT)
                if not ready:
                    message = "停住的请求一直没有放行"
                    raise TimeoutError(message)
                if any(key.fileobj is self._released for key, _ in ready):
                    return True
                if client.recv(1, socket.MSG_PEEK) == b"":
                    self.abandoned.set()
                    return False
                # 客户端又发来了数据（不会是这次请求的）：只等放行
                selector.unregister(client)

    def close(self) -> None:
        """关闭放行用的一对套接字；服务停止后调用。"""
        self._released.close()
        self._release.close()


class Server:
    """本地 HTTP 服务：按路径返回放进来的内容或状态码，记下各路径被请求的次数与最近一次的请求头。"""

    def __init__(self, tls: ssl.SSLContext | None = None) -> None:
        """在 127.0.0.1 的随机端口上开始服务；给了 `tls` 时为 HTTPS。"""
        self._lock = threading.Lock()
        self._bodies: dict[str, bytes] = {}
        self._statuses: dict[str, tuple[int, list[tuple[str, str]]]] = {}
        self._hits: dict[str, int] = {}
        self._headers: dict[str, list[tuple[str, str]]] = {}
        self._gates: dict[str, Gate] = {}
        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self._scheme = "http"
        if tls is not None:
            self._httpd.socket = tls.wrap_socket(self._httpd.socket, server_side=True)
            self._scheme = "https"
        self._thread = threading.Thread(target=self._httpd.serve_forever)
        self._thread.start()

    def url(self, path: str) -> str:
        """`path`（不以 / 开头）的完整地址。"""
        host, port = self._httpd.server_address[:2]
        return f"{self._scheme}://{host!s}:{port}/{path}"

    def put(self, path: str, body: bytes | str) -> None:
        """`path` 返回 200 与 `body`（`str` 按 UTF-8 编码）。"""
        with self._lock:
            self._bodies[path] = body.encode() if isinstance(body, str) else body
            self._statuses.pop(path, None)

    def status(self, path: str, code: int, headers: Iterable[tuple[str, str]] = ()) -> None:
        """`path` 返回状态码 `code` 与响应头 `headers`，没有内容。"""
        with self._lock:
            self._statuses[path] = (code, list(headers))

    def gate(self, path: str) -> Gate:
        """之后对 `path` 的请求先停住，返回的 `Gate` 放行后才响应。

        只用于 HTTP：停住期间用 MSG_PEEK 查客户端是否断开，TLS 套接字不支持。
        """
        if self._scheme != "http":
            message = "停住请求只用于 HTTP 服务"
            raise NotImplementedError(message)
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
        """放行所有停住的请求，停止服务，等服务线程结束。"""
        with self._lock:
            gates = list(self._gates.values())
        for gate in gates:
            gate.release()
        self._httpd.shutdown()
        self._httpd.server_close()
        self._thread.join()
        for gate in gates:
            gate.close()

    def respond(
        self, path: str, headers: list[tuple[str, str]], client: socket.socket
    ) -> tuple[int, list[tuple[str, str]], bytes] | None:
        """记下这次请求，要停住时先停住，再返回状态码、响应头与内容；停住期间客户端 `client` 断开时返回 None。

        设了状态码的返回它，放了内容的返回 200，都没有的返回 404。
        """
        with self._lock:
            self._hits[path] = self._hits.get(path, 0) + 1
            self._headers[path] = headers
            gate = self._gates.get(path)
        if gate is not None and not gate.hold(client):
            return None
        with self._lock:
            if path in self._statuses:
                code, extra = self._statuses[path]
                return code, extra, b""
            if path in self._bodies:
                return 200, [], self._bodies[path]
            return 404, [], b""

    def _handler(self) -> type[BaseHTTPRequestHandler]:
        server = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                path = self.path.lstrip("/").split("?", 1)[0]
                response = server.respond(path, list(self.headers.items()), self.connection)
                if response is None:
                    self.close_connection = True
                    return
                status, extra, body = response
                self.send_response(status)
                for name, value in extra:
                    self.send_header(name, value)
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


def pem(kind: str, der: bytes) -> str:
    """DER 转为 PEM：Python 的 ssl 只从 PEM 文件加载证书与私钥。"""
    body = textwrap.fill(base64.b64encode(der).decode(), 64)
    return f"-----BEGIN {kind}-----\n{body}\n-----END {kind}-----\n"


@pytest.fixture
def tls_server(tmp_path: Path) -> Iterator[Server]:
    """用 tests/fixtures/tls 的自签名证书（CN 为 localhost，含 IP 127.0.0.1）提供 HTTPS 的本地服务。"""
    cert, key = tmp_path / "cert.pem", tmp_path / "key.pem"
    cert.write_text(pem("CERTIFICATE", (FIXTURES / "tls" / "cert.der").read_bytes()))
    key.write_text(pem("PRIVATE KEY", (FIXTURES / "tls" / "key.der").read_bytes()))
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    instance = Server(context)
    yield instance
    instance.close()
