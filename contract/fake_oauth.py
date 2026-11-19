"""接口测试用的假 OAuth 服务 + 要登录才能连的 MCP 端点（Streamable HTTP，回 JSON）。"""
import base64
import hashlib
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs


class FakeOAuth:
    def __init__(self):
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        self.reset()
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def reset(self):
        self.registered: list[dict] = []
        self.grants: list[str] = []
        self.valid: set[str] = set()
        self.challenge = ""
        self.verifier_ok = False
        self.expires_in = 3600
        self.next_token = "at-1"
        self.revoked = False
        self.refuse_refresh = False

    def stop(self):
        self.httpd.shutdown()

    def _handler(self):
        f = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def send(self, code, body=None, ctype="application/json", headers=None):
                raw = b"" if body is None else (body if isinstance(body, bytes) else json.dumps(body, ensure_ascii=False).encode())
                self.send_response(code)
                self.send_header("content-type", ctype)
                self.send_header("content-length", str(len(raw)))
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(raw)

            def body(self) -> bytes:
                return self.rfile.read(int(self.headers.get("content-length") or 0))

            def do_GET(self):
                if self.path == "/.well-known/oauth-protected-resource/mcp":
                    return self.send(200, {"resource": f"{f.url}/mcp", "authorization_servers": [f.url], "scopes_supported": ["docs.read"]})
                if self.path == "/.well-known/oauth-authorization-server":
                    return self.send(200, {"issuer": f.url, "authorization_endpoint": f"{f.url}/authorize?extra=1",
                                           "token_endpoint": f"{f.url}/token", "registration_endpoint": f"{f.url}/register",
                                           "scopes_supported": ["docs.read"], "code_challenge_methods_supported": ["S256"],
                                           "response_types_supported": ["code"]})
                if self.path == "/mcp":
                    return self.send(405, {"error": "no stream"})
                self.send(404, {"error": "not found"})

            def do_DELETE(self):
                self.send(200, {})

            def do_POST(self):
                raw = self.body()
                if self.path == "/register":
                    d = json.loads(raw)
                    f.registered.append(d)
                    return self.send(201, {"client_id": "cid-1", "redirect_uris": d["redirect_uris"], "token_endpoint_auth_method": "none",
                                           "grant_types": d.get("grant_types"), "response_types": ["code"], "client_name": d.get("client_name"),
                                           "client_secret": None})
                if self.path == "/token":
                    form = {k: v[0] for k, v in parse_qs(raw.decode()).items()}
                    g = form.get("grant_type")
                    f.grants.append(g)
                    if g == "authorization_code":
                        want = base64.urlsafe_b64encode(hashlib.sha256(form.get("code_verifier", "").encode()).digest()).decode().rstrip("=")
                        f.verifier_ok = form.get("code") == "code-1" and want == f.challenge and form.get("client_id") == "cid-1"
                        if not f.verifier_ok:
                            return self.send(400, {"error": "invalid_grant"})
                        tok = f.next_token
                        f.valid.add(tok)
                        return self.send(200, {"access_token": tok, "token_type": "bearer", "expires_in": f.expires_in,
                                               "refresh_token": "rt-1", "scope": "docs.read"})
                    if g == "refresh_token":
                        if f.refuse_refresh or form.get("refresh_token") != "rt-1":
                            return self.send(400, {"error": "invalid_grant"})
                        tok = f.next_token
                        f.valid.add(tok)
                        return self.send(200, {"access_token": tok, "token_type": "Bearer", "expires_in": f.expires_in})
                    return self.send(400, {"error": "unsupported_grant_type"})
                if self.path == "/mcp":
                    auth = self.headers.get("authorization", "")
                    tok = auth.removeprefix("Bearer ").strip()
                    if f.revoked or tok not in f.valid:
                        return self.send(401, {"error": "invalid_token"}, headers={
                            "WWW-Authenticate": f'Bearer resource_metadata="{f.url}/.well-known/oauth-protected-resource/mcp"'})
                    m = json.loads(raw)
                    if "id" not in m:
                        return self.send(202)
                    meth = m.get("method")
                    if meth == "initialize":
                        res = {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}},
                               "serverInfo": {"name": "fake-oauth", "version": "1"}}
                    elif meth == "tools/list":
                        res = {"tools": [{"name": "list_docs", "description": "列出文档", "inputSchema": {"type": "object", "properties": {}}}]}
                    elif meth == "tools/call":
                        res = {"content": [{"type": "text", "text": "文档：a、b"}]}
                    elif meth == "ping":
                        res = {}
                    else:
                        return self.send(200, {"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "no"}})
                    return self.send(200, {"jsonrpc": "2.0", "id": m["id"], "result": res}, headers={"mcp-session-id": "sess-1"})
                self.send(404, {"error": "not found"})

        return H
