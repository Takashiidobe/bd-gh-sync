#!/usr/bin/env python3
"""A tiny in-memory stand-in for the GitHub REST issues API.

It implements just what `bd github push/pull` and gh-to-beads.sh call:

  GET    /repos/{o}/{r}/issues[?state=&since=&page=]
  GET    /repos/{o}/{r}/issues/{n}
  POST   /repos/{o}/{r}/issues
  PATCH  /repos/{o}/{r}/issues/{n}
  POST   /repos/{o}/{r}/issues/{n}/labels
  DELETE /repos/{o}/{r}/issues/{n}/labels/{name}

plus test hooks:

  GET    /_stats      {"writes": N}  number of mutating calls so far
  GET    /_issues     every issue

Usage: fake_github.py PORT_FILE   (binds a free port and writes it to PORT_FILE)
"""

import json
import re
import sys
import threading
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

LOCK = threading.Lock()
ISSUES = {}
STATE = {"writes": 0, "next": 1}


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def label_objs(names):
    return [{"name": n} for n in dict.fromkeys(names)]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def route(self):
        url = urlparse(self.path)
        m = re.fullmatch(r"/repos/([^/]+)/([^/]+)/issues(?:/(\d+))?(/labels(?:/(.+))?)?", url.path)
        return url, m

    def do_GET(self):
        url, m = self.route()
        with LOCK:
            if url.path == "/_stats":
                return self.send(200, {"writes": STATE["writes"]})
            if url.path == "/_issues":
                return self.send(200, list(ISSUES.values()))
            if not m:
                return self.send(404, {"message": "Not Found"})
            if m.group(3):
                issue = ISSUES.get(int(m.group(3)))
                return self.send(200, issue) if issue else self.send(404, {"message": "Not Found"})
            q = parse_qs(url.query)
            if q.get("page", ["1"])[0] != "1":
                return self.send(200, [])
            state = q.get("state", ["open"])[0]
            since = q.get("since", [""])[0]
            out = [
                i for i in ISSUES.values()
                if (state == "all" or i["state"] == state) and (not since or i["updated_at"] >= since)
            ]
            return self.send(200, out)

    def do_POST(self):
        url, m = self.route()
        if not m:
            return self.send(404, {"message": "Not Found"})
        body = self.body()
        with LOCK:
            STATE["writes"] += 1
            if m.group(4):  # add labels
                issue = ISSUES[int(m.group(3))]
                names = [l["name"] for l in issue["labels"]] + body.get("labels", [])
                issue["labels"] = label_objs(names)
                issue["updated_at"] = now()
                return self.send(200, issue["labels"])
            n = STATE["next"]
            STATE["next"] += 1
            owner, repo = m.group(1), m.group(2)
            ts = now()
            issue = {
                "id": 1000 + n,
                "node_id": f"I_{n}",
                "number": n,
                "title": body.get("title", ""),
                "body": body.get("body", ""),
                "state": "open",
                "labels": label_objs(body.get("labels", [])),
                "assignee": None,
                "assignees": [],
                "html_url": f"https://github.com/{owner}/{repo}/issues/{n}",
                "url": f"https://api.github.com/repos/{owner}/{repo}/issues/{n}",
                "created_at": ts,
                "updated_at": ts,
                "closed_at": None,
                "user": {"login": "tester", "id": 1},
            }
            ISSUES[n] = issue
            return self.send(201, issue)

    def do_PATCH(self):
        url, m = self.route()
        if not m or not m.group(3):
            return self.send(404, {"message": "Not Found"})
        body = self.body()
        with LOCK:
            issue = ISSUES.get(int(m.group(3)))
            if not issue:
                return self.send(404, {"message": "Not Found"})
            STATE["writes"] += 1
            for k in ("title", "body", "state"):
                if k in body:
                    issue[k] = body[k]
            if "labels" in body:
                issue["labels"] = label_objs(body["labels"])
            issue["closed_at"] = now() if issue["state"] == "closed" else None
            issue["updated_at"] = now()
            return self.send(200, issue)

    def do_DELETE(self):
        url, m = self.route()
        if not m or not m.group(5):
            return self.send(404, {"message": "Not Found"})
        with LOCK:
            STATE["writes"] += 1
            issue = ISSUES[int(m.group(3))]
            name = unquote(m.group(5))
            issue["labels"] = [l for l in issue["labels"] if l["name"] != name]
            issue["updated_at"] = now()
            return self.send(200, issue["labels"])


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(sys.argv[1], "w") as f:
        f.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
