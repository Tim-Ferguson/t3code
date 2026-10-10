#!/usr/bin/env python3
"""The `glab` of the browser fixture: GitLab merge requests for `acme/lab` on gitlab.t3code-test.invalid.

Never the real CLI, never the network. It answers `glab api ...` and `glab mr ...` calls from a fixed
set of recorded-looking merge requests (an open one with a failed pipeline, review threads, comments
with awards, several commits and files; a draft; a merged one; a closed one). Writes (comment, reply,
resolve, reaction, edit, approve, reviewers, merge, close, reopen, draft, rebase) are applied to a state file
next to the call log ($T3_FIXTURE_CALL_LOG, glab-state.json), since every call is a new process; deleting the
file, or a fresh scratch directory, starts again from the recorded picture.

Any call it has no answer for exits 97 and names the arguments on stderr, so a gap shows up in the UI
as an error rather than as an empty panel. Every call is appended to $T3_FIXTURE_CALL_LOG.
"""
import json
import os
import re
import sys
import tempfile
import time
from urllib.parse import parse_qs, unquote, urlsplit

HOST = "gitlab.t3code-test.invalid"
REPO = "acme/lab"
BRANCH = "feature/mr-panel-demo"
ME = "carol"
argv = sys.argv[1:]
stdin_text = ""


def log(entry):
    path = os.environ.get("T3_FIXTURE_CALL_LOG")
    if path:
        with open(path, "a") as handle:
            handle.write(json.dumps(dict(entry, at=time.time(), fake="glab", pid=os.getpid(), args=argv)) + "\n")


def refuse(code, message):
    log({"refused": message})
    sys.stderr.write("fake glab (browser fixture): %s\nargs: %s\n" % (message, " ".join(argv)))
    sys.exit(code)


def user(identifier, username, name=None):
    return {"id": identifier, "username": username, "name": name or username.capitalize(), "avatar_url": None}


ALICE, BOB, CAROL, DANA = user(1, "alice"), user(2, "bob"), user(3, ME), user(4, "dana")
WEB = "https://%s/%s" % (HOST, REPO)

MERGE_REQUESTS = {
    7: {"title": "Retry failed calls with exponential backoff", "state": "opened", "draft": False, "author": ALICE, "reviewers": [BOB, DANA], "labels": ["bug", "enhancement"],
        "created_at": "2026-09-20T09:00:00.000Z", "updated_at": "2026-09-27T14:30:00.000Z", "pipeline": "failed", "changes": 4, "behind": 2, "source": BRANCH,
        "description": "Retries failed calls with exponential backoff.\n\n- adds `retry()`\n- raises the default timeout\n- renames the usage doc"},
    8: {"title": "Draft: rework the config loader", "state": "opened", "draft": True, "author": BOB, "reviewers": [], "labels": ["refactor"],
        "created_at": "2026-09-22T10:00:00.000Z", "updated_at": "2026-09-26T08:00:00.000Z", "pipeline": "running", "changes": 2, "behind": 0, "source": "feature/config-loader",
        "description": "Work in progress."},
    9: {"title": "Fix the README typo", "state": "merged", "draft": False, "author": DANA, "reviewers": [ALICE], "labels": [],
        "created_at": "2026-09-10T10:00:00.000Z", "updated_at": "2026-09-12T08:00:00.000Z", "merged_at": "2026-09-12T08:00:00.000Z", "pipeline": "success", "changes": 1, "behind": 0, "source": "fix/readme-typo",
        "description": "Typo."},
    10: {"title": "Try a different cache key", "state": "closed", "draft": False, "author": ALICE, "reviewers": [], "labels": ["wontfix"],
         "created_at": "2026-09-05T10:00:00.000Z", "updated_at": "2026-09-06T08:00:00.000Z", "closed_at": "2026-09-06T08:00:00.000Z", "pipeline": "canceled", "changes": 3, "behind": 5, "source": "try/cache-key",
         "description": "Superseded."},
}


def merge_request(number):
    m = MERGE_REQUESTS[number]
    return {
        "iid": number, "title": m["title"], "web_url": "%s/-/merge_requests/%d" % (WEB, number), "description": m["description"], "author": m["author"],
        "source_branch": m["source"], "target_branch": "main", "state": m["state"], "draft": m["draft"],
        "merge_status": "can_be_merged" if m["state"] == "opened" else "cannot_be_merged", "has_conflicts": False,
        "created_at": m["created_at"], "updated_at": m["updated_at"], "merged_at": m.get("merged_at"), "closed_at": m.get("closed_at"),
        "reviewers": m["reviewers"], "labels": m["labels"], "changes_count": str(m["changes"]),
        "head_pipeline": {"status": m["pipeline"], "web_url": "%s/-/pipelines/%d" % (WEB, 100 + number), "source": "merge_request_event"},
        "user": {"can_merge": True}, "merge_when_pipeline_succeeds": bool(m.get("auto")), "squash_on_merge": False, "diverged_commits_count": m["behind"],
        "diff_refs": {"base_sha": "b0b0b0b0", "head_sha": "a1a1a1a1", "start_sha": "c2c2c2c2"},
    }


def row(number):
    m = MERGE_REQUESTS[number]
    return {"iid": number, "title": m["title"], "web_url": "%s/-/merge_requests/%d" % (WEB, number), "source_branch": m["source"], "target_branch": "main",
            "state": m["state"], "draft": m["draft"], "updated_at": m["updated_at"], "merged_at": m.get("merged_at"), "closed_at": m.get("closed_at")}


def default_notes():
    return [
        {"id": 11, "body": "Looks fine overall. Can we get a test for the timeout?", "author": BOB, "created_at": "2026-09-21T09:00:00.000Z", "system": False, "type": None},
        {"id": 12, "body": "Rename this to `retryWithBackoff`.", "author": BOB, "created_at": "2026-09-21T09:05:00.000Z", "system": False, "type": "DiffNote",
         "position": {"new_path": "src/retry.ts", "old_path": "src/retry.ts"}},
        {"id": 13, "body": "Good point, done in the next push.", "author": ALICE, "created_at": "2026-09-21T10:00:00.000Z", "system": False, "type": None},
        {"id": 14, "body": "added 4 commits", "author": ALICE, "created_at": "2026-09-21T10:01:00.000Z", "system": True, "type": None},
        {"id": 15, "body": "Why 5 attempts and not 3?", "author": DANA, "created_at": "2026-09-22T11:00:00.000Z", "system": False, "type": "DiffNote",
         "position": {"new_path": "src/config.ts", "old_path": "src/config.ts"}},
    ]


def default_discussions():
    position = lambda path, line: {"position_type": "text", "new_path": path, "old_path": path, "new_line": line, "old_line": None}
    return [
        {"id": "d1", "notes": [{"id": 12, "body": "Rename this to `retryWithBackoff`.", "author": BOB, "created_at": "2026-09-21T09:05:00.000Z", "system": False, "resolvable": True, "resolved": False, "position": position("src/retry.ts", 1)},
                               {"id": 13, "body": "Good point, done in the next push.", "author": ALICE, "created_at": "2026-09-21T10:00:00.000Z", "system": False, "resolvable": True, "resolved": False, "position": position("src/retry.ts", 1)}]},
        {"id": "d2", "notes": [{"id": 15, "body": "Why 5 attempts and not 3?", "author": DANA, "created_at": "2026-09-22T11:00:00.000Z", "system": False, "resolvable": True, "resolved": True, "position": position("src/config.ts", 1)}]},
    ]


STATE_FILE = os.path.join(os.path.dirname(os.environ.get("T3_FIXTURE_CALL_LOG") or os.path.join(tempfile.gettempdir(), "x")), "glab-state.json")
NOTES = {7: default_notes()}
DISCUSSIONS = {7: default_discussions()}
AWARDS = {7: {"mr": [{"id": 1, "name": "tada", "username": "bob"}, {"id": 2, "name": "rocket", "username": "dana"}],
              "12": [{"id": 3, "name": "thumbsup", "username": ME}, {"id": 4, "name": "thumbsup", "username": "bob"}],
              "11": [{"id": 5, "name": "heart", "username": "alice"}]}}
SEQ = [100]
USERS = {u["id"]: u for u in (ALICE, BOB, CAROL, DANA)}


def load_state():
    try:
        with open(STATE_FILE) as handle:
            saved = json.load(handle)
    except (OSError, ValueError):
        return
    for number, fields in saved["mrs"].items():
        MERGE_REQUESTS[int(number)].update(fields)
    for target, key in ((NOTES, "notes"), (DISCUSSIONS, "discussions"), (AWARDS, "awards")):
        target.clear()
        target.update({int(number): value for number, value in saved[key].items()})
    SEQ[0] = saved["seq"]


def save_state():
    with open(STATE_FILE, "w") as handle:
        json.dump({"mrs": {str(n): m for n, m in MERGE_REQUESTS.items()}, "notes": NOTES, "discussions": DISCUSSIONS, "awards": AWARDS, "seq": SEQ[0]}, handle)


def next_id():
    SEQ[0] += 1
    return SEQ[0]


def notes(number):
    return NOTES.get(number, [])


def discussions(number):
    return DISCUSSIONS.get(number, [])


def commits(number):
    if number != 7:
        return []
    rows = [("e4d3c2b1a0", "Remove the legacy module", 1, 9), ("d3c2b1a0f9", "Rename the usage doc and describe retries", 3, 1), ("c2b1a0f9e8", "Add a retry helper", 12, 0), ("b1a0f9e8d7", "Retry with exponential backoff", 1, 1)]
    return [{"id": oid, "title": title, "committed_date": "2026-09-2%dT09:0%d:00.000Z" % (index, index), "parent_ids": ["0000000"], "author_name": "Alice", "stats": {"additions": add, "deletions": delete}}
            for index, (oid, title, add, delete) in enumerate(rows)]


DIFFS = [
    {"old_path": "src/config.ts", "new_path": "src/config.ts", "diff": "@@ -1 +1 @@\n-export const config = { retries: 2, timeoutMs: 1000 };\n+export const config = { retries: 5, timeoutMs: 2500, backoff: \"exponential\" };\n", "new_file": False, "renamed_file": False, "deleted_file": False},
    {"old_path": "src/retry.ts", "new_path": "src/retry.ts", "diff": "@@ -0,0 +1,11 @@\n+export async function retry<T>(run: () => Promise<T>, attempts: number): Promise<T> {\n+  let last: unknown;\n+  for (let i = 0; i < attempts; i += 1) {\n+    try {\n+      return await run();\n+    } catch (error) {\n+      last = error;\n+    }\n+  }\n+  throw last;\n+}\n", "new_file": True, "renamed_file": False, "deleted_file": False},
    {"old_path": "docs/usage.md", "new_path": "docs/guide.md", "diff": "@@ -1,3 +1,3 @@\n-# Usage\n+# Guide\n \n-Run it.\n+Run it. Failed calls are retried with exponential backoff.\n", "new_file": False, "renamed_file": True, "deleted_file": False},
    {"old_path": "src/legacy.ts", "new_path": "src/legacy.ts", "diff": "@@ -1 +0,0 @@\n-export const legacy = true;\n", "new_file": False, "renamed_file": False, "deleted_file": True},
]


def award(name, username):
    return {"name": name, "user": {"username": username}}


def respond(value):
    sys.stdout.write(value if isinstance(value, str) else json.dumps(value))
    sys.exit(0)


def graphql():
    try:
        body = json.loads(stdin_text)
    except ValueError:
        refuse(97, "graphql without a JSON body")
    query = body.get("query", "")
    if "awardEmoji" in query:
        number = int(body.get("variables", {}).get("iid", "7"))
        held = AWARDS.get(number, {})
        nodes = lambda subject: {"nodes": [award(a["name"], a["username"]) for a in held.get(subject, [])]}
        respond({"data": {"currentUser": {"username": ME}, "project": {"mergeRequest": {
            "awardEmoji": nodes("mr"),
            "notes": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": [
                {"id": "gid://gitlab/Note/%s" % subject, "awardEmoji": nodes(subject)} for subject in held if subject != "mr"]}}}}})
    if "blobs" in query:
        paths = body.get("variables", {}).get("paths", [])
        respond({"data": {"project": {"repository": {"blobs": {"nodes": [{"path": path, "oid": "blob-" + re.sub(r"\W", "", path)} for path in paths]}}}}})
    refuse(97, "no answer for this graphql document")


def body_json():
    try:
        return json.loads(stdin_text) if stdin_text.strip() else {}
    except ValueError:
        refuse(97, "write with a body that is not JSON")


def add_note(number, text, **extra):
    note = {"id": next_id(), "body": text, "author": CAROL, "created_at": now(), "system": False, "type": None}
    note.update(extra)
    NOTES.setdefault(number, []).append(note)
    return note


def now():
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime())


def write(method, endpoint):
    """The merge request writes the server makes through `glab api`; answers {} and saves."""
    parts = urlsplit(endpoint)
    path, query = unquote(parts.path), parse_qs(parts.query)
    body = body_json()
    base = "projects/" + REPO + "/merge_requests/"
    if not path.startswith(base):
        refuse(97, "no route for %s %s" % (method, endpoint))
    rest = path[len(base):].split("/")
    number = int(rest[0]) if rest[0].isdigit() and int(rest[0]) in MERGE_REQUESTS else None
    if number is None:
        refuse(97, "no merge request in %s" % endpoint)
    m, tail = MERGE_REQUESTS[number], rest[1:]
    if not tail and method == "PUT":
        if "reviewer_ids" in body:
            m["reviewers"] = [USERS[i] for i in body["reviewer_ids"] if i in USERS]
        if "title" in body:
            m["title"] = body["title"]
        if "description" in body:
            m["description"] = body["description"]
    elif tail == ["notes"] and method == "POST":
        add_note(number, body["body"])
    elif tail == ["discussions"] and method == "POST":
        position = body.get("position") or {}
        note = {"id": next_id(), "body": body["body"], "author": CAROL, "created_at": now(), "system": False, "resolvable": True, "resolved": False, "position": position}
        DISCUSSIONS.setdefault(number, []).append({"id": "d%d" % note["id"], "notes": [note]})
    elif len(tail) == 3 and tail[0] == "discussions" and tail[2] == "notes" and method == "POST":
        thread = next((d for d in DISCUSSIONS.get(number, []) if d["id"] == tail[1]), None)
        if thread is None:
            refuse(1, "404 discussion not found")
        first = thread["notes"][0]
        thread["notes"].append({"id": next_id(), "body": body["body"], "author": CAROL, "created_at": now(), "system": False, "resolvable": True, "resolved": first.get("resolved", False), "position": first.get("position")})
    elif len(tail) == 2 and tail[0] == "discussions" and method == "PUT":
        thread = next((d for d in DISCUSSIONS.get(number, []) if d["id"] == tail[1]), None)
        if thread is None:
            refuse(1, "404 discussion not found")
        for note in thread["notes"]:
            note["resolved"] = bool(body.get("resolved"))
    elif len(tail) == 2 and tail[0] == "notes" and method == "PUT":
        target = next((n for n in NOTES.get(number, []) if str(n["id"]) == tail[1]), None) or next((n for d in DISCUSSIONS.get(number, []) for n in d["notes"] if str(n["id"]) == tail[1]), None)
        if target is None:
            refuse(1, "404 note not found")
        target["body"] = body["body"]
        for n in NOTES.get(number, []):
            if str(n["id"]) == tail[1]:
                n["body"] = body["body"]
        for d in DISCUSSIONS.get(number, []):
            for n in d["notes"]:
                if str(n["id"]) == tail[1]:
                    n["body"] = body["body"]
    elif tail == ["approve"] and method == "POST":
        add_note(number, "approved this merge request", system=True)
    elif tail == ["cancel_merge_when_pipeline_succeeds"] and method == "POST":
        m["auto"] = False
    elif tail and tail[-1] == "award_emoji" or (len(tail) >= 2 and tail[-2] == "award_emoji") or (len(tail) >= 3 and tail[-3] == "award_emoji"):
        subject = tail[1] if tail[0] == "notes" else "mr"
        held = AWARDS.setdefault(number, {}).setdefault(subject, [])
        if method == "POST":
            held.append({"id": next_id(), "name": query["name"][0], "username": ME})
        elif method == "DELETE":
            identifier = int(tail[-1])
            held[:] = [a for a in held if a["id"] != identifier]
    else:
        refuse(97, "no write route for %s %s" % (method, endpoint))
    save_state()
    respond({})


def api(args):
    # `glab api [--hostname H] [--method M] [--raw-field k=v]... <endpoint>`
    method, endpoint, index = "GET", None, 0
    while index < len(args):
        arg = args[index]
        if arg in ("--hostname", "--header", "-H"):
            index += 2
            continue
        if arg in ("--method", "-X"):
            method = args[index + 1].upper()
            index += 2
            continue
        if arg in ("--raw-field", "--field", "-f", "-F", "--input"):
            index += 2
            continue
        if endpoint is None and not arg.startswith("-"):
            endpoint = arg
        index += 1
    if endpoint is None:
        refuse(97, "api call without an endpoint")
    if endpoint == "graphql":
        graphql()
    if method != "GET":
        write(method, endpoint)
    parts = urlsplit(endpoint)
    path, query = unquote(parts.path), parse_qs(parts.query)
    first = lambda name, default=None: (query.get(name) or [default])[0]
    if path == "user":
        respond({"username": ME, "id": 3, "name": "Carol"})
    if path in ("projects/:fullpath", "projects/" + REPO) and not parts.query:
        respond({"path_with_namespace": REPO, "web_url": WEB, "http_url_to_repo": WEB + ".git", "ssh_url_to_repo": "git@%s:%s.git" % (HOST, REPO), "default_branch": "main"})
    base = "projects/" + REPO
    if path == base:
        respond({"merge_method": "merge", "squash_option": "default_off", "path_with_namespace": REPO, "default_branch": "main"})
    if path == base + "/users":
        respond([ALICE, BOB, CAROL, DANA])
    if path == base + "/merge_requests":
        state = first("state", "all")
        wanted = [number for number, m in MERGE_REQUESTS.items() if state in ("all", m["state"])]
        search = (first("search") or "").lower()
        if search:
            wanted = [number for number in wanted if search in MERGE_REQUESTS[number]["title"].lower()]
        reviewer = first("reviewer_username")
        if reviewer:
            wanted = [number for number in wanted if any(r["username"] == reviewer for r in MERGE_REQUESTS[number]["reviewers"])]
        wanted.sort(key=lambda number: MERGE_REQUESTS[number]["updated_at"], reverse=True)
        per_page, page = int(first("per_page", "20")), int(first("page", "1"))
        respond([merge_request(number) for number in wanted[(page - 1) * per_page: page * per_page]])
    award_match = re.fullmatch(re.escape(base) + r"/merge_requests/(\d+)(?:/notes/(\d+))?/award_emoji", path)
    if award_match:
        held = AWARDS.get(int(award_match.group(1)), {}).get(award_match.group(2) or "mr", [])
        respond([{"id": a["id"], "name": a["name"], "user": {"username": a["username"]}} for a in held])
    match = re.fullmatch(re.escape(base) + r"/merge_requests/(\d+)(/[a-z_]+)?", path)
    if match and int(match.group(1)) in MERGE_REQUESTS:
        number, sub = int(match.group(1)), match.group(2)
        if sub is None:
            respond(merge_request(number))
        page, per_page = int(first("page", "1")), int(first("per_page", "100"))
        pick = lambda items: items[(page - 1) * per_page: page * per_page]
        if sub == "/notes":
            respond(pick(notes(number)))
        if sub == "/discussions":
            respond(pick(discussions(number)))
        if sub == "/commits":
            respond(commits(number))
        if sub == "/diffs":
            respond(pick(DIFFS) if number == 7 else [])
    refuse(97, "no route for GET %s" % endpoint)


def mr(args):
    sub = args[0] if args else ""
    if sub == "list":
        branch = args[args.index("--source-branch") + 1] if "--source-branch" in args else None
        flags = set(args)
        state = "closed" if "--closed" in flags else "merged" if "--merged" in flags else "all" if "--all" in flags else "opened"
        wanted = [n for n, m in MERGE_REQUESTS.items() if (branch is None or m["source"] == branch) and (state == "all" or m["state"] == state)]
        respond([row(n) for n in sorted(wanted)])
    if sub == "view":
        target = args[1] if len(args) > 1 else ""
        number = int(target) if target.isdigit() else next((n for n, m in MERGE_REQUESTS.items() if m["source"] == target), None)
        if number in MERGE_REQUESTS:
            respond(row(number))
        refuse(1, "404 Not Found: no merge request %s" % target)
    if sub in ("merge", "update", "close", "reopen", "rebase") and len(args) > 1 and args[1].isdigit() and int(args[1]) in MERGE_REQUESTS:
        number = int(args[1])
        m = MERGE_REQUESTS[number]
        if sub == "merge":
            if "--auto-merge=true" in args:
                m["auto"] = True
            else:
                m["state"], m["merged_at"], m["auto"] = "merged", now(), False
        elif sub == "update":
            if "--draft" in args:
                m["draft"] = True
                m["title"] = m["title"] if m["title"].startswith("Draft:") else "Draft: " + m["title"]
            if "--ready" in args:
                m["draft"] = False
                m["title"] = m["title"][len("Draft:"):].lstrip() if m["title"].startswith("Draft:") else m["title"]
        elif sub == "close":
            m["state"], m["closed_at"] = "closed", now()
        elif sub == "reopen":
            m["state"] = "opened"
            m.pop("closed_at", None)
        elif sub == "rebase":
            m["behind"] = 0
        m["updated_at"] = now()
        save_state()
        respond("")
    refuse(97 if sub not in ("merge", "approve", "close", "reopen", "note", "update", "checkout", "create") else 1,
           "this fixture does not run `glab mr %s`" % sub)


def main():
    global stdin_text
    if not sys.stdin.isatty():
        stdin_text = sys.stdin.read()
    log({"called": True})
    load_state()
    if argv == ["--version"] or argv[:1] == ["version"]:
        respond("glab 1.50.0 (browser fixture)\n")
    if argv[:2] == ["auth", "status"]:
        respond("%s\n  ✓ Logged in to %s as %s (token)\n" % (HOST, HOST, ME))
    if argv[:1] == ["api"]:
        api(argv[1:])
    if argv[:1] == ["mr"]:
        mr(argv[1:])
    refuse(97, "no handler for this command")


main()
