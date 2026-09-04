"""Real HTTP probe whose status follows the deployed payload, not a mock SSH command."""

from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from uuid import UUID


def payload_path(request_path):
    """Resolve only the two fixed routes or one canonical isolated run namespace."""
    parts = request_path.split("/")
    if len(parts) == 2 and parts[0] == "" and parts[1] in {"frontend", "backend"}:
        return Path("/srv/shipforge-acceptance") / parts[1] / "current" / "health.txt"
    if len(parts) != 3 or parts[0] != "" or parts[2] not in {"frontend", "backend"}:
        return None
    namespace = parts[1]
    if not namespace.startswith("management-"):
        return None
    identifier = namespace.removeprefix("management-")
    try:
        if str(UUID(identifier)) != identifier:
            return None
    except ValueError:
        return None
    return Path("/srv/shipforge-acceptance") / namespace / parts[2] / "current" / "health.txt"


class Health(BaseHTTPRequestHandler):
    def do_GET(self):
        payload = payload_path(self.path)
        if payload is None:
            self.send_error(404)
            return
        try:
            healthy = payload.read_text().strip() == "healthy"
        except OSError:
            healthy = False
        self.send_response(200 if healthy else 503)
        self.end_headers()
        self.wfile.write(b"healthy\n" if healthy else b"unhealthy\n")

    def log_message(self, _format, *_args):
        pass


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 8080), Health).serve_forever()
