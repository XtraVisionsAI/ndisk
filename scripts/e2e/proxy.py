#!/usr/bin/env python3
"""最小的 HTTP 正向代理（仅 absolute-form 请求，不支持 CONNECT），把每个请求写到日志，用于验证代理注入。"""
import http.client, http.server, socketserver, sys, urllib.parse

LOG = sys.argv[2] if len(sys.argv) > 2 else "/tmp/proxy.log"

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def _fwd(self):
        u = urllib.parse.urlsplit(self.path)
        with open(LOG, "a") as f:
            f.write(f"{self.command} {self.path}\n")
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else None
        c = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=30)
        path = u.path + ("?" + u.query if u.query else "")
        hdrs = {k: v for k, v in self.headers.items() if k.lower() not in ("proxy-connection", "proxy-authorization")}
        c.request(self.command, path, body=body, headers=hdrs)
        r = c.getresponse()
        data = r.read()
        self.send_response(r.status)
        for k, v in r.getheaders():
            if k.lower() not in ("transfer-encoding", "connection", "content-length"):
                self.send_header(k, v)
        # HEAD 没有响应体，必须原样转发上游的 Content-Length（即对象大小）。
        cl = r.getheader("Content-Length") if self.command == "HEAD" else str(len(data))
        self.send_header("Content-Length", cl or "0")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)
    do_GET = do_HEAD = do_PUT = do_POST = do_DELETE = _fwd
    def log_message(self, *a): pass

class S(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

S(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
