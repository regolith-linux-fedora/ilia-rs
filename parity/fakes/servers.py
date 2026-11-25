import json
import socket
import struct
import threading
from pathlib import Path


class UnixServer:
    # Whether each connection is served on a thread of its own, so a client
    # that keeps its connection open does not hold up the others.
    concurrent = False

    def __init__(self, path, handler, transcript):
        self.path = Path(path)
        self.handler = handler
        self.transcript = Path(transcript)
        self.requests = []
        self._stop = threading.Event()
        self._thread = None
        self._lock = threading.Lock()

    def start(self):
        self.path.unlink(missing_ok=True)
        self.transcript.parent.mkdir(parents=True, exist_ok=True)
        self.transcript.write_text("")
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(str(self.path))
        self.sock.listen(8)
        self.sock.settimeout(0.1)
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()
        return self

    def record(self, request, transcript_line):
        with self._lock:
            self.requests.append(request)
            with self.transcript.open("a") as stream:
                stream.write(transcript_line)
                stream.flush()

    def _serve(self):
        while not self._stop.is_set():
            try:
                conn, _ = self.sock.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            if self.concurrent:
                threading.Thread(target=self._serve_one, args=(conn,), daemon=True).start()
            else:
                self._serve_one(conn)

    def _serve_one(self, conn):
        with conn:
            self.handler(conn, self)

    def close(self):
        self._stop.set()
        try:
            self.sock.close()
        except OSError:
            pass
        if self._thread:
            self._thread.join(timeout=2)
        self.path.unlink(missing_ok=True)

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.close()


def _read_exact(conn, size):
    chunks = []
    while size:
        data = conn.recv(size)
        if not data:
            raise EOFError("client closed early")
        chunks.append(data)
        size -= len(data)
    return b"".join(chunks)


RUN_COMMAND = 0
GET_WORKSPACES = 1
SUBSCRIBE = 2
GET_CONFIG = 9
GET_BINDING_STATE = 12
EVENT_BINDING = (1 << 31) | 5


def _frame(message_type, reply):
    response = json.dumps(reply, separators=(",", ":")).encode()
    return b"i3-ipc" + struct.pack("<II", len(response), message_type) + response


class I3Server(UnixServer):
    """An i3/sway IPC socket: any number of requests per connection, as the
    window managers serve them, and connections served side by side. Like
    them it answers command requests, the configuration (empty), the binding
    state (the default mode) and event subscriptions unless `replies` says
    otherwise; `send_event` delivers an event on every subscribed
    connection."""

    concurrent = True

    def __init__(self, path, replies, transcript):
        self.replies = replies
        self.subscribers = []
        self.subscribed = threading.Event()
        super().__init__(path, self._handle, transcript)

    def _handle(self, conn, server):
        while True:
            try:
                header = _read_exact(conn, 14)
            except (EOFError, OSError):
                return
            assert header[:6] == b"i3-ipc"
            size, message_type = struct.unpack("<II", header[6:])
            payload = _read_exact(conn, size) if size else b""
            self.record(
                (message_type, payload),
                json.dumps({"message_type": message_type, "payload_hex": payload.hex()}, sort_keys=True) + "\n",
            )
            # Real i3/sway accepts command requests on the same IPC socket.  Some
            # test environments also provide i3-msg, so make that path deterministic.
            defaults = {
                RUN_COMMAND: [{"success": True}],
                SUBSCRIBE: {"success": True},
                GET_CONFIG: {"config": "", "included_configs": []},
                GET_BINDING_STATE: {"name": "default"},
            }
            reply = self.replies.get(message_type, defaults.get(message_type))
            if reply is None:
                raise AssertionError(f"unexpected i3 IPC message type {message_type}")
            conn.sendall(_frame(message_type, reply))
            if message_type == SUBSCRIBE and reply.get("success"):
                with self._lock:
                    self.subscribers.append(conn)
                self.subscribed.set()

    def send_event(self, event_type, event):
        with self._lock:
            subscribers = list(self.subscribers)
        for conn in subscribers:
            conn.sendall(_frame(event_type, event))
        return len(subscribers)


class RoficationServer(UnixServer):
    def __init__(self, path, notifications, transcript):
        self.notifications = notifications
        super().__init__(path, self._handle, transcript)

    def _handle(self, conn, server):
        request = b""
        while not request.endswith(b"\n"):
            data = conn.recv(4096)
            if not data:
                break
            request += data
        text = request.decode()
        self.record(text, json.dumps({"request": text}, sort_keys=True) + "\n")
        if text == "list\n":
            conn.sendall(json.dumps(self.notifications).encode())
