"""Serves docs/ locally with caching off, so an edited script is never mixed with a stale one."""
import functools
import http.server
import sys


class NoCache(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
handler = functools.partial(NoCache, directory="docs")
http.server.ThreadingHTTPServer(("127.0.0.1", port), handler).serve_forever()
