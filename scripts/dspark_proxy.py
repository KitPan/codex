#!/usr/bin/env python3
"""DSpark temperature-0 injection proxy for the DeepSeek V4 Flash box.

Kit 的 DSpark 服务层要求每个采样请求显式携带 "temperature": 0（默认置信度阈值
0.7）。codex 在 Responses wire 上不保证发送该字段，因此 rdos-dsflash provider
指向本代理而非直连箱子。代理同时把每个请求的 method/path/temperature 情况打到
stdout——顺带提供请求体级观测。

Run:
    python3 scripts/dspark_proxy.py     # 监听 127.0.0.1:18300
Upstream: http://studio.local:8000 (=192.168.3.3)
"""
import http.client
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM_HOST, UPSTREAM_PORT = "studio.local", 8000
LISTEN_ADDR = ("127.0.0.1", 18300)
INJECT_PATHS = ("/v1/responses", "/v1/chat/completions", "/v1/completions")
HOP_HEADERS = {"connection", "keep-alive", "transfer-encoding", "te", "trailer",
               "proxy-authorization", "proxy-authenticate", "upgrade",
               "content-length", "host"}


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _forward(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        note = ""
        if body and self.command == "POST" and self.path.startswith(INJECT_PATHS):
            try:
                payload = json.loads(body)
                had = payload.get("temperature", "<absent>")
                payload["temperature"] = 0
                body = json.dumps(payload).encode()
                note = f" | temperature: {had} -> 0"
            except (ValueError, UnicodeDecodeError):
                note = " | body not JSON, passthrough"
        conn = http.client.HTTPConnection(UPSTREAM_HOST, UPSTREAM_PORT, timeout=600)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_HEADERS}
        if body is not None:
            headers["Content-Length"] = str(len(body))
        try:
            conn.request(self.command, self.path, body=body, headers=headers)
            resp = conn.getresponse()
        except Exception as exc:
            log(f"{self.command} {self.path} -> upstream error: {exc}")
            self.send_error(502, f"upstream error: {exc}")
            return
        log(f"{self.command} {self.path} -> {resp.status}{note}")
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() not in HOP_HEADERS:
                self.send_header(k, v)
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            while True:
                chunk = resp.read(8192)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            log(f"client disconnected mid-stream ({self.path})")
        finally:
            conn.close()

    do_GET = do_POST = do_DELETE = do_PUT = _forward

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    log(f"DSpark temp0 proxy on http://{LISTEN_ADDR[0]}:{LISTEN_ADDR[1]} -> "
        f"http://{UPSTREAM_HOST}:{UPSTREAM_PORT} (inject on {', '.join(INJECT_PATHS)})")
    try:
        ThreadingHTTPServer(LISTEN_ADDR, Handler).serve_forever()
    except KeyboardInterrupt:
        sys.exit(0)
