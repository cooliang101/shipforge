"""Real HTTP probe whose status follows the deployed payload, not a mock SSH command."""

from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


class Health(BaseHTTPRequestHandler):
    def do_GET(self):
        component = self.path.removeprefix("/")
        if component not in {"frontend", "backend"}:
            self.send_error(404)
            return
        payload = Path("/srv/shipforge-acceptance") / component / "current" / "health.txt"
        try:
            healthy = payload.read_text().strip() == "healthy"
        except OSError:
            healthy = False
        self.send_response(200 if healthy else 503)
        self.end_headers()
        self.wfile.write(b"healthy\n" if healthy else b"unhealthy\n")

    def log_message(self, _format, *_args):
        pass


HTTPServer(("127.0.0.1", 8080), Health).serve_forever()
