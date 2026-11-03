import json
import socket
import struct
import threading
from pathlib import Path


class UnixServer:
    def __init__(self, path, handler, transcript):
        self.path = Path(path)
        self.handler = handler
        self.transcript = Path(transcript)
        self.requests = []
        self._stop = threading.Event()
        self._thread = None

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


class I3Server(UnixServer):
    def __init__(self, path, replies, transcript):
        self.replies = replies
        super().__init__(path, self._handle, transcript)

    def _handle(self, conn, server):
        header = _read_exact(conn, 14)
        assert header[:6] == b"i3-ipc"
        size, message_type = struct.unpack("<II", header[6:])
        payload = _read_exact(conn, size) if size else b""
        self.record(
            (message_type, payload),
            json.dumps({"message_type": message_type, "payload_hex": payload.hex()}, sort_keys=True) + "\n",
        )
        # Real i3/sway accepts command requests on the same IPC socket.  Some
        # test environments also provide i3-msg, so make that path deterministic.
        reply = self.replies.get(message_type, [{"success": True}] if message_type == 0 else None)
        if reply is None:
            raise AssertionError(f"unexpected i3 IPC message type {message_type}")
        response = json.dumps(reply, separators=(",", ":")).encode()
        conn.sendall(b"i3-ipc" + struct.pack("<II", len(response), message_type) + response)


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
