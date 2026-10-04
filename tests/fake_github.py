#!/usr/bin/env python3
"""A tiny in-memory stand-in for the GitHub REST issues API.

It implements just what `bd github push/pull`, gh-to-beads.sh and
bd-gh-watch call:

  GET    /repos/{o}/{r}/issues[?state=&since=&page=]
  GET    /repos/{o}/{r}/issues/{n}
  POST   /repos/{o}/{r}/issues
  PATCH  /repos/{o}/{r}/issues/{n}                      (incl. assignees)
  POST   /repos/{o}/{r}/issues/{n}/labels
  DELETE /repos/{o}/{r}/issues/{n}/labels/{name}
  GET    /repos/{o}/{r}/issues/{n}/comments
  POST   /repos/{o}/{r}/issues/{n}/comments
  GET    /repos/{o}/{r}/issues/{n}/sub_issues
  POST   /repos/{o}/{r}/issues/{n}/sub_issues            {"sub_issue_id": id}
  DELETE /repos/{o}/{r}/issues/{n}/sub_issue             {"sub_issue_id": id}
  GET    /repos/{o}/{r}/issues/{n}/dependencies/blocked_by
  POST   /repos/{o}/{r}/issues/{n}/dependencies/blocked_by {"issue_id": id}
  DELETE /repos/{o}/{r}/issues/{n}/dependencies/blocked_by/{id}
  POST   /graphql   (the relations query: every issue's parent and blockedBy)

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
COMMENTS = {}   # number -> [comment]
PARENT = {}     # sub-issue number -> parent number
BLOCKED_BY = {}  # number -> [blocking numbers]
STATE = {"writes": 0, "next": 1, "next_comment": 1}

ISSUE_PATH = re.compile(r"/repos/([^/]+)/([^/]+)/issues(?:/(\d+)(/.*)?)?")


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def label_objs(names):
    return [{"name": n} for n in dict.fromkeys(names)]


def user(login):
    return {"login": login, "id": abs(hash(login)) % 100000}


def view(issue):
    return dict(issue, comments=len(COMMENTS.get(issue["number"], [])))


def by_id(issue_id):
    return next((i for i in ISSUES.values() if i["id"] == issue_id), None)


def gql_ref(number, repo):
    return {"number": number, "repository": {"nameWithOwner": repo}}


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

    def not_found(self):
        return self.send(404, {"message": "Not Found"})

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def route(self):
        url = urlparse(self.path)
        m = ISSUE_PATH.fullmatch(url.path)
        if not m:
            return url, None, None, None
        issue = ISSUES.get(int(m.group(3))) if m.group(3) else None
        return url, m, issue, m.group(4) or ""

    def do_GET(self):
        url, m, issue, sub = self.route()
        with LOCK:
            if url.path == "/_stats":
                return self.send(200, {"writes": STATE["writes"]})
            if url.path == "/_issues":
                return self.send(200, [view(i) for i in ISSUES.values()])
            if not m:
                return self.not_found()
            if m.group(3):
                if not issue:
                    return self.not_found()
                n = issue["number"]
                if sub == "":
                    return self.send(200, view(issue))
                if sub == "/comments":
                    return self.send(200, COMMENTS.get(n, []))
                if sub == "/sub_issues":
                    return self.send(200, [view(ISSUES[c]) for c, p in PARENT.items() if p == n])
                if sub == "/dependencies/blocked_by":
                    return self.send(200, [view(ISSUES[b]) for b in BLOCKED_BY.get(n, [])])
                return self.not_found()
            q = parse_qs(url.query)
            if q.get("page", ["1"])[0] != "1":
                return self.send(200, [])
            state = q.get("state", ["open"])[0]
            since = q.get("since", [""])[0]
            out = [
                view(i) for i in ISSUES.values()
                if (state == "all" or i["state"] == state) and (not since or i["updated_at"] >= since)
            ]
            return self.send(200, out)

    def do_POST(self):
        if urlparse(self.path).path == "/graphql":
            return self.graphql()
        url, m, issue, sub = self.route()
        if not m:
            return self.not_found()
        body = self.body()
        with LOCK:
            if m.group(3) and not issue:
                return self.not_found()
            STATE["writes"] += 1
            if sub == "/labels":
                names = [l["name"] for l in issue["labels"]] + body.get("labels", [])
                issue["labels"] = label_objs(names)
                issue["updated_at"] = now()
                return self.send(200, issue["labels"])
            if sub == "/comments":
                c = {
                    "id": 5000 + STATE["next_comment"],
                    "body": body["body"],
                    "user": user(body.get("_as", "tester")),
                    "created_at": now(),
                }
                STATE["next_comment"] += 1
                COMMENTS.setdefault(issue["number"], []).append(c)
                issue["updated_at"] = now()
                return self.send(201, c)
            if sub == "/sub_issues":
                child = by_id(body.get("sub_issue_id"))
                if not child:
                    return self.not_found()
                if child["number"] in PARENT:
                    return self.send(422, {"message": "Issue already has a parent"})
                PARENT[child["number"]] = issue["number"]
                return self.send(201, view(child))
            if sub == "/dependencies/blocked_by":
                blocker = by_id(body.get("issue_id"))
                if not blocker:
                    return self.not_found()
                deps = BLOCKED_BY.setdefault(issue["number"], [])
                if blocker["number"] not in deps:
                    deps.append(blocker["number"])
                return self.send(201, view(blocker))
            if sub:
                return self.not_found()
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
            return self.send(201, view(issue))

    def do_PATCH(self):
        url, m, issue, sub = self.route()
        if not m or sub or not issue:
            return self.not_found()
        body = self.body()
        with LOCK:
            STATE["writes"] += 1
            for k in ("title", "body", "state"):
                if k in body:
                    issue[k] = body[k]
            if "labels" in body:
                issue["labels"] = label_objs(body["labels"])
            if "assignees" in body:
                if any(a.startswith("ghost") for a in body["assignees"]):
                    return self.send(422, {"message": "Invalid assignee"})
                issue["assignees"] = [user(a) for a in body["assignees"]]
                issue["assignee"] = issue["assignees"][0] if issue["assignees"] else None
            issue["closed_at"] = now() if issue["state"] == "closed" else None
            issue["updated_at"] = now()
            return self.send(200, view(issue))

    def do_DELETE(self):
        url, m, issue, sub = self.route()
        if not m or not issue:
            return self.not_found()
        with LOCK:
            STATE["writes"] += 1
            n = issue["number"]
            if sub.startswith("/labels/"):
                name = unquote(sub[len("/labels/"):])
                issue["labels"] = [l for l in issue["labels"] if l["name"] != name]
                issue["updated_at"] = now()
                return self.send(200, issue["labels"])
            if sub == "/sub_issue":
                child = by_id(self.body().get("sub_issue_id"))
                if not child or PARENT.get(child["number"]) != n:
                    return self.not_found()
                del PARENT[child["number"]]
                return self.send(200, view(issue))
            dep = re.fullmatch(r"/dependencies/blocked_by/(\d+)", sub)
            if dep:
                blocker = by_id(int(dep.group(1)))
                if not blocker or blocker["number"] not in BLOCKED_BY.get(n, []):
                    return self.not_found()
                BLOCKED_BY[n].remove(blocker["number"])
                return self.send(200, view(blocker))
            return self.not_found()

    def graphql(self):
        variables = self.body().get("variables", {})
        repo = f'{variables.get("owner")}/{variables.get("repo")}'
        with LOCK:
            nodes = [
                {
                    "number": n,
                    "parent": gql_ref(PARENT[n], repo) if n in PARENT else None,
                    "blockedBy": {"nodes": [gql_ref(b, repo) for b in BLOCKED_BY.get(n, [])]},
                }
                for n in sorted(ISSUES)
            ]
        return self.send(200, {"data": {"repository": {"issues": {
            "pageInfo": {"hasNextPage": False, "endCursor": None},
            "nodes": nodes,
        }}}})


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(sys.argv[1], "w") as f:
        f.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
