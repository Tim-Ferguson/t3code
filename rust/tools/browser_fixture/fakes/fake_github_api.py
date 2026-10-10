#!/usr/bin/env python3
"""The GitHub API of the browser fixture, for the host github.t3code-test.invalid. Loopback only.

The server reads GitHub over HTTPS (REST and GraphQL), not through `gh`. A server started with `--fixture` and
T3CODE_FIXTURE_GITHUB_API_ORIGIN pointing here sends it all there (README.md, "Server hooks"); this process answers as
the redirect used by the test suite does: REST under /rest/<host>/..., GraphQL at /graphql/<host>.

GraphQL documents are recognised by the query text itself: the constants in
crates/server/src/pull_request_queries.rs are read at start, so an answer is chosen by the document the server
really sends and not by a guess about it. The answers are built from the shapes the test suite's own GitHub
(crates/server/tests/pull_request_flow.rs) is known to decode.

The data is a picture of acme/widgets: #42 open on feature/pr-panel-demo (checks in every state, two
review threads, comments with reactions, labels, reviewers, four commits, four changed files), #41 a draft, #40
merged, #39 closed. The writes the server makes are applied to this process's memory, so the panel can be driven
through merge, close, reopen, draft, auto-merge, update branch, comments, replies, resolutions, reactions, edits,
reviews, reviewer requests, labels and viewed marks; a restart starts again from the picture above.
Anything not handled is answered 501 naming the request, and every request is appended to $T3_FIXTURE_CALL_LOG.
"""
import json
import os
import re
import itertools
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", "..", ".."))
QUERIES_FILE = os.environ.get("T3_FIXTURE_QUERIES") or os.path.join(ROOT, "crates", "server", "src", "pull_request_queries.rs")
REPO = "acme/widgets"
ME = "carol"
FEATURE = "feature/pr-panel-demo"


def load_queries():
    source = open(QUERIES_FILE).read()
    found = {}
    for name, text in re.findall(r'pub const (\w+): &str = r#"(.*?)"#;', source, re.S):
        found[name] = text
    return found


QUERIES = load_queries()


def known(query):
    """The constant a document is: the same text, up to the rate-limit selection the server appends before the last brace."""
    for name, text in QUERIES.items():
        if query.startswith(text[: text.rfind("}")]):
            return name
    return None


def log(entry):
    path = os.environ.get("T3_FIXTURE_CALL_LOG")
    if path:
        with open(path, "a") as handle:
            handle.write(json.dumps(dict(entry, at=time.time(), fake="github-api", pid=os.getpid())) + "\n")


def actor(login, name=None):
    return {"__typename": "User", "login": login, "avatarUrl": None, "name": name or login.capitalize(), "id": "ID_" + login}


PULLS = {
    42: {"title": "Retry failed calls with exponential backoff", "state": "OPEN", "draft": False, "author": "alice", "head": FEATURE, "decision": "CHANGES_REQUESTED", "additions": 17, "deletions": 11, "files": 4,
         "created": "2026-09-20T09:00:00Z", "updated": "2026-09-27T14:30:00Z", "rollup": "FAILURE", "labels": [("bug", "d73a4a"), ("enhancement", "a2eeef")], "reviewers": ["bob", "dana"]},
    41: {"title": "Rework the config loader", "state": "OPEN", "draft": True, "author": "bob", "head": "feature/config-loader", "decision": "REVIEW_REQUIRED", "additions": 40, "deletions": 3, "files": 2,
         "created": "2026-09-22T10:00:00Z", "updated": "2026-09-26T08:00:00Z", "rollup": "PENDING", "labels": [("refactor", "0e8a16")], "reviewers": []},
    40: {"title": "Fix the README typo", "state": "MERGED", "draft": False, "author": "dana", "head": "fix/readme-typo", "decision": "APPROVED", "additions": 1, "deletions": 1, "files": 1,
         "created": "2026-09-10T10:00:00Z", "updated": "2026-09-12T08:00:00Z", "merged": "2026-09-12T08:00:00Z", "rollup": "SUCCESS", "labels": [], "reviewers": ["alice"]},
    39: {"title": "Try a different cache key", "state": "CLOSED", "draft": False, "author": "alice", "head": "try/cache-key", "decision": None, "additions": 9, "deletions": 4, "files": 3,
         "created": "2026-09-05T10:00:00Z", "updated": "2026-09-06T08:00:00Z", "closed": "2026-09-06T08:00:00Z", "rollup": None, "labels": [("wontfix", "ffffff")], "reviewers": []},
}


BODY = "Retries failed calls with exponential backoff.\n\n- adds `retry()`\n- raises the default timeout\n- renames the usage doc\n\nCloses #12."
for _number, _p in PULLS.items():
    _p["body"] = BODY if _number == 42 else "Created by the browser fixture."
    _p["auto"] = None
    _p["behind"] = 2 if _number == 42 else 0
LOCK = threading.RLock()
SEQ = itertools.count(100)
NOW = lambda: time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
HEAD_OID = "e4d3c2b1a0f9e8d7c6b5a4938271605f4e3d2c1b"
LABELS = [("bug", "d73a4a"), ("enhancement", "a2eeef"), ("refactor", "0e8a16"), ("wontfix", "ffffff"), ("documentation", "0075ca"), ("good first issue", "7057ff")]
USERS = ["alice", "bob", "carol", "dana", "erin"]
VIEWED = {"src/config.ts": True}
# subject id -> content -> people; the viewer is ME
REACTIONS = {
    "PR_42": {"ROCKET": ["dana"], "HOORAY": [ME, "bob"]},
    "IC_1": {"THUMBS_UP": ["alice"]},
    "RC_1": {"THUMBS_UP": [ME, "dana"], "EYES": ["alice"]},
    "RC_3": {"HEART": ["alice"]},
}
ISSUE_COMMENTS = [
    {"id": "IC_1", "body": "This looks good overall. Can we get a test for the timeout?", "createdAt": "2026-09-21T09:00:00Z", "lastEditedAt": None, "author": "bob"},
    {"id": "IC_2", "body": "Added in the next push. Also see **the retry docs**:\n\n```ts\nawait retry(() => fetchItem(id), 5);\n```", "createdAt": "2026-09-21T10:00:00Z", "lastEditedAt": "2026-09-21T10:05:00Z", "author": "alice"},
    {"id": "IC_3", "body": "Why five attempts and not three?", "createdAt": "2026-09-22T11:00:00Z", "lastEditedAt": None, "author": "dana"},
]
REVIEWS = [
    {"id": "R_1", "body": "Needs a test for the timeout path.", "state": "CHANGES_REQUESTED", "submittedAt": "2026-09-21T09:10:00Z", "lastEditedAt": None, "author": "bob"},
    {"id": "R_2", "body": "", "state": "COMMENTED", "submittedAt": "2026-09-22T11:05:00Z", "lastEditedAt": None, "author": "dana"},
]
THREADS = [
    {"id": "T_1", "resolved": False, "outdated": False, "path": "src/retry.ts", "line": 3, "side": "RIGHT", "comments": [
        {"id": "RC_1", "author": "bob", "body": "Rename this to `retryWithBackoff`.", "createdAt": "2026-09-21T09:05:00Z", "lastEditedAt": None},
        {"id": "RC_2", "author": "alice", "body": "Good point, done in the next push.", "createdAt": "2026-09-21T10:00:00Z", "lastEditedAt": None}]},
    {"id": "T_2", "resolved": True, "outdated": False, "path": "src/config.ts", "line": 1, "side": "RIGHT", "comments": [
        {"id": "RC_3", "author": "dana", "body": "Why 5 attempts and not 3?", "createdAt": "2026-09-22T11:00:00Z", "lastEditedAt": None}]},
    {"id": "T_3", "resolved": False, "outdated": True, "path": "docs/usage.md", "line": 1, "side": "RIGHT", "comments": [
        {"id": "RC_4", "author": "bob", "body": "This line moved with the rename.", "createdAt": "2026-09-21T09:20:00Z", "lastEditedAt": None}]},
]


def pull(number):
    return PULLS.get(number) or PULLS[42]


def url(number):
    return "https://github.t3code-test.invalid/%s/pull/%d" % (REPO, number)


CHECKS = [
    {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-09-27T14:00:00Z", "completedAt": "2026-09-27T14:03:00Z", "detailsUrl": "https://ci.t3code-test.invalid/runs/1", "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
    {"__typename": "CheckRun", "name": "unit tests", "status": "COMPLETED", "conclusion": "FAILURE", "startedAt": "2026-09-27T14:00:00Z", "completedAt": "2026-09-27T14:06:00Z", "detailsUrl": "https://ci.t3code-test.invalid/runs/2", "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
    {"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": None, "startedAt": "2026-09-27T14:30:00Z", "completedAt": None, "detailsUrl": None, "checkSuite": None},
    {"__typename": "CheckRun", "name": "e2e", "status": "COMPLETED", "conclusion": "SKIPPED", "startedAt": "2026-09-27T14:00:00Z", "completedAt": "2026-09-27T14:00:05Z", "detailsUrl": "https://ci.t3code-test.invalid/runs/4", "checkSuite": {"workflowRun": {"workflow": {"name": "E2E"}}}},
    {"__typename": "CheckRun", "name": "docs", "status": "COMPLETED", "conclusion": "NEUTRAL", "startedAt": "2026-09-27T14:00:00Z", "completedAt": "2026-09-27T14:01:00Z", "detailsUrl": "https://ci.t3code-test.invalid/runs/5", "checkSuite": {"workflowRun": {"workflow": {"name": "Docs"}}}},
    {"__typename": "CheckRun", "name": "deploy preview", "status": "QUEUED", "conclusion": None, "startedAt": None, "completedAt": None, "detailsUrl": None, "checkSuite": None},
    {"__typename": "CheckRun", "name": "security scan", "status": "COMPLETED", "conclusion": "CANCELLED", "startedAt": "2026-09-27T14:00:00Z", "completedAt": "2026-09-27T14:02:00Z", "detailsUrl": "https://ci.t3code-test.invalid/runs/7", "checkSuite": {"workflowRun": {"workflow": {"name": "Security"}}}},
    {"__typename": "StatusContext", "context": "license/cla", "state": "SUCCESS", "targetUrl": "https://cla.t3code-test.invalid/42", "createdAt": "2026-09-27T14:00:00Z", "description": "All contributors have signed"},
    {"__typename": "StatusContext", "context": "coverage/report", "state": "PENDING", "targetUrl": None, "createdAt": "2026-09-27T14:30:00Z", "description": "Waiting for the test run"},
]


def checks_for(number):
    return CHECKS if number == 42 else []


def reviewer_nodes(number):
    nodes = []
    for login in pull(number)["reviewers"]:
        nodes.append({"requestedReviewer": {"login": login, "name": login.capitalize(), "avatarUrl": None}})
    for slug in pull(number).get("teams", ["platform"] if number == 42 else []):
        nodes.append({"requestedReviewer": {"slug": slug, "name": slug.capitalize() + " team"}})
    return nodes


def core(number):
    p = pull(number)
    return {"data": {"repository": {"mergeCommitAllowed": True, "squashMergeAllowed": True, "rebaseMergeAllowed": True, "viewerPermission": "WRITE", "pullRequest": {
        "number": number, "title": p["title"], "url": url(number), "body": p["body"],
        "state": p["state"], "isDraft": p["draft"], "mergeable": "MERGEABLE" if p["state"] == "OPEN" else "UNKNOWN", "reviewDecision": p["decision"],
        "additions": p["additions"], "deletions": p["deletions"], "changedFiles": p["files"], "createdAt": p["created"], "updatedAt": p["updated"],
        "mergedAt": p.get("merged"), "closedAt": p.get("closed"), "headRefName": p["head"], "baseRefName": "main", "headRefOid": "e4d3c2b1a0f9e8d7c6b5a4938271605f4e3d2c1b",
        "isCrossRepository": False, "headRepositoryOwner": {"login": "acme"}, "author": actor(p["author"]), "autoMergeRequest": ({"mergeMethod": p["auto"]} if p["auto"] else None),
        "viewerCanUpdate": True, "viewerDidAuthor": False, "viewerCanUpdateBranch": True, "baseRef": {"compare": {"behindBy": p["behind"]}},
        "reviewRequests": {"nodes": reviewer_nodes(number)}, "labels": {"nodes": [{"name": n, "color": c} for n, c in p["labels"]]},
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [dict(c, isRequired=c.get("name") in ("build", "unit tests")) for c in checks_for(number)], "pageInfo": {"hasNextPage": False}}} if checks_for(number) else None}}]},
    }}}}


def check_contexts(number):
    return {"data": {"repository": {"pullRequest": {"headRefOid": "e4d3c2b1a0f9e8d7c6b5a4938271605f4e3d2c1b", "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
        "nodes": [dict(c, isRequired=c.get("name") in ("build", "unit tests")) for c in checks_for(number)], "pageInfo": {"hasNextPage": False, "endCursor": None}}}}}]}}}}}


def summary_pull(number):
    p = pull(number)
    return {"pullRequest": {"number": number, "title": p["title"], "url": url(number), "state": p["state"], "isDraft": p["draft"], "mergeable": "MERGEABLE", "reviewDecision": p["decision"],
            "additions": p["additions"], "deletions": p["deletions"], "changedFiles": p["files"], "updatedAt": p["updated"], "mergedAt": p.get("merged"), "closedAt": p.get("closed"),
            "headRefName": p["head"], "baseRefName": "main", "author": actor(p["author"]), "latestReviews": {"nodes": [{"state": "CHANGES_REQUESTED", "author": {"login": "bob"}}] if number == 42 else []},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": p["rollup"]} if p["rollup"] else None}}]}, "stack": None, "stackEntry": None}}


def preview(number):
    p = pull(number)
    return {"data": {"repository": {"pullRequest": {"number": number, "title": p["title"], "url": url(number), "state": p["state"], "isDraft": p["draft"], "createdAt": p["created"], "author": actor(p["author"])}}}}


COMMITS = [("e4d3c2b1a0", "Remove the legacy module", 1, 9), ("d3c2b1a0f9", "Rename the usage doc and describe retries", 3, 1), ("c2b1a0f9e8", "Add a retry helper", 12, 0), ("b1a0f9e8d7", "Retry with exponential backoff", 1, 1)]


def activity():
    comments = [dict(c, url=url(42) + "#issuecomment-" + c["id"], author=actor(c["author"])) for c in ISSUE_COMMENTS]
    reviews = [dict(r, url=url(42) + "#pullrequestreview-" + r["id"], author=actor(r["author"])) for r in REVIEWS]
    return {"data": {"repository": {"pullRequest": {"author": actor("alice"),
        "commits": {"nodes": [{"commit": {"oid": oid, "messageHeadline": title, "committedDate": "2026-09-2%dT09:0%d:00Z" % (index, index), "authors": {"nodes": [{"name": "Alice", "email": "alice@example.invalid", "user": {"login": "alice"}}]}}} for index, (oid, title, _a, _d) in enumerate(reversed(COMMITS))]},
        "comments": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": comments},
        "reviews": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": reviews}}}}}


def reactions(subject):
    held = REACTIONS.get(subject, {})
    return [{"content": content, "viewerHasReacted": ME in people, "reactors": {"totalCount": len(people), "nodes": [{"login": login} for login in people]}} for content, people in held.items() if people]


def threads():
    def thread(t):
        comments = [{"id": c["id"], "author": actor(c["author"]), "body": c["body"], "createdAt": c["createdAt"], "lastEditedAt": c["lastEditedAt"], "url": url(42) + "#discussion_" + c["id"], "reactionGroups": reactions(c["id"])} for c in t["comments"]]
        return {"id": t["id"], "isResolved": t["resolved"], "isOutdated": t["outdated"], "path": t["path"], "line": t["line"], "diffSide": t["side"],
                "comments": {"totalCount": len(comments), "pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": comments}}
    nodes = [thread(t) for t in THREADS]
    latest = {}
    for r in REVIEWS:
        latest[r["author"]] = r
    return {"data": {"viewer": {"login": ME}, "repository": {"pullRequest": {
        "reviewThreads": {"totalCount": len(nodes), "pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": nodes},
        "viewerCanUpdate": True, "viewerDidAuthor": False, "author": actor("alice"),
        "reactionGroups": reactions("PR_42"),
        "comments": {"nodes": [{"id": c["id"], "lastEditedAt": c["lastEditedAt"], "author": actor(c["author"]), "reactionGroups": reactions(c["id"])} for c in ISSUE_COMMENTS]},
        "reviews": {"nodes": [{"id": r["id"], "author": actor(r["author"]), "reactionGroups": reactions(r["id"])} for r in REVIEWS]},
        "reviewRequests": {"nodes": reviewer_nodes(42)}, "latestReviews": {"nodes": [{"id": r["id"], "state": r["state"], "author": actor(r["author"])} for r in latest.values() if r["state"] != "COMMENTED"]},
        "reviewDismissals": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": []},
        "commits": {"nodes": [{"commit": {"oid": oid, "messageHeadline": title, "committedDate": "2026-09-2%dT09:0%d:00Z" % (index, index), "additions": add, "deletions": delete, "parents": {"totalCount": 1},
                                          "authors": {"nodes": [{"name": "Alice", "avatarUrl": None, "user": {"login": "alice"}}]}}}
                             for index, (oid, title, add, delete) in enumerate(reversed(COMMITS))]},
    }}}}


def search_row(number, repository=REPO):
    p = pull(number)
    return {"number": number, "title": p["title"], "url": url(number), "author": actor(p["author"]), "headRefName": p["head"], "baseRefName": "main", "state": p["state"], "isDraft": p["draft"],
            "mergeable": "MERGEABLE", "reviewDecision": p["decision"], "latestReviews": {"nodes": []}, "createdAt": p["created"], "updatedAt": p["updated"], "mergedAt": p.get("merged"),
            "repository": {"nameWithOwner": repository}, "reviewRequests": {"nodes": [{"requestedReviewer": {"login": login}} for login in p["reviewers"]]},
            "labels": {"nodes": [{"name": n, "color": c} for n, c in p["labels"]]},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": p["rollup"]} if p["rollup"] else None}}]}}


def search(variables):
    q = variables.get("q", "")
    wanted = [n for n, p in PULLS.items() if ("is:open" not in q or p["state"] == "OPEN") and ("is:merged" not in q or p["state"] == "MERGED") and ("is:closed" not in q or p["state"] in ("CLOSED", "MERGED"))]
    wanted.sort(key=lambda n: PULLS[n]["updated"], reverse=True)
    return {"data": {"search": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": [search_row(n) for n in wanted]}}}


FILES = [
    {"filename": "src/config.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-export const config = { retries: 2, timeoutMs: 1000 };\n+export const config = { retries: 5, timeoutMs: 2500, backoff: \"exponential\" };", "additions": 1, "deletions": 1},
    {"filename": "src/retry.ts", "status": "added", "patch": "@@ -0,0 +1,3 @@\n+export async function retry<T>(run: () => Promise<T>, attempts: number): Promise<T> {\n+  throw new Error(\"stub\");\n+}", "additions": 3, "deletions": 0},
    {"filename": "docs/guide.md", "previous_filename": "docs/usage.md", "status": "renamed", "patch": "@@ -1,3 +1,3 @@\n-# Usage\n+# Guide\n \n-Run it.\n+Run it. Failed calls are retried with exponential backoff.", "additions": 2, "deletions": 2},
    {"filename": "src/legacy.ts", "status": "removed", "patch": "@@ -1 +0,0 @@\n-export const legacy = true;", "additions": 0, "deletions": 1},
]
DIFF = "".join("diff --git a/%s b/%s\n--- a/%s\n+++ b/%s\n%s\n" % (f.get("previous_filename", f["filename"]), f["filename"], f.get("previous_filename", f["filename"]), f["filename"], f["patch"]) for f in FILES)



def find(items, identifier):
    return next((item for item in items if item["id"] == identifier), None)


def all_comments():
    for thread in THREADS:
        for comment in thread["comments"]:
            yield comment


def mutate(query, variables):
    """Applies one mutation the server makes; returns the `data` of the answer, or an error string."""
    inner = variables.get("input") or {}
    node = variables.get("pullRequestId") or inner.get("pullRequestId") or "PR_42"
    number = int(node[3:]) if node.startswith("PR_") and node[3:].isdigit() and int(node[3:]) in PULLS else 42
    p = PULLS[number]
    ok = {"clientMutationId": None}
    if "addComment(" in query:
        ISSUE_COMMENTS.append({"id": "IC_%d" % next(SEQ), "body": variables["body"], "createdAt": NOW(), "lastEditedAt": None, "author": ME})
    elif "addPullRequestReviewThreadReply(" in query:
        thread = find(THREADS, variables["threadId"])
        if thread is None:
            return "no review thread " + variables["threadId"]
        thread["comments"].append({"id": "RC_%d" % next(SEQ), "author": ME, "body": variables["body"], "createdAt": NOW(), "lastEditedAt": None})
    elif "addReaction(" in query or "removeReaction(" in query:
        held = REACTIONS.setdefault(variables["subjectId"], {}).setdefault(variables["content"], [])
        if "addReaction(" in query and ME not in held:
            held.append(ME)
        if "removeReaction(" in query and ME in held:
            held.remove(ME)
    elif "resolveReviewThread(" in query:
        thread = find(THREADS, variables["threadId"])
        if thread is None:
            return "no review thread " + variables["threadId"]
        thread["resolved"] = "unresolveReviewThread(" not in query
    elif "updatePullRequest(" in query:
        if variables.get("title") is not None:
            p["title"] = variables["title"]
        if variables.get("body") is not None:
            p["body"] = variables["body"]
    elif "updateIssueComment(" in query or "updatePullRequestReviewComment(" in query:
        target = find(ISSUE_COMMENTS, variables["commentId"]) or find(list(all_comments()), variables["commentId"])
        if target is None:
            return "no comment " + variables["commentId"]
        target["body"], target["lastEditedAt"] = variables["body"], NOW()
    elif "mergePullRequest(" in query:
        p["state"], p["merged"], p["auto"] = "MERGED", NOW(), None
    elif "enablePullRequestAutoMerge(" in query:
        p["auto"] = inner.get("mergeMethod", "MERGE")
    elif "disablePullRequestAutoMerge(" in query:
        p["auto"] = None
    elif "updatePullRequestBranch(" in query:
        p["behind"] = 0
    elif "markPullRequestReadyForReview(" in query:
        p["draft"] = False
    elif "convertPullRequestToDraft(" in query:
        p["draft"] = True
    elif "closePullRequest(" in query:
        p["state"], p["closed"], p["auto"] = "CLOSED", NOW(), None
    elif "reopenPullRequest(" in query:
        p["state"] = "OPEN"
        p.pop("closed", None)
    elif "markFileAsViewed(" in query or "unmarkFileAsViewed(" in query:
        for index, name in enumerate(re.findall(r"f(\d+): (markFileAsViewed|unmarkFileAsViewed)", query)):
            VIEWED[variables["path" + name[0]]] = name[1] == "markFileAsViewed"
    else:
        return "no mutation for " + query.strip().splitlines()[0][:100]
    p["updated"] = NOW()
    return {"x": ok}


def rest_write(method, rest, body):
    """The REST writes: reviews, reviewer requests and labels. Returns (status, json)."""
    m = re.match(r"^repos/[^/]+/[^/]+/pulls/(\d+)/reviews$", rest)
    if m and method == "POST":
        event = body.get("event")
        state = {"COMMENT": "COMMENTED", "APPROVE": "APPROVED", "REQUEST_CHANGES": "CHANGES_REQUESTED"}.get(event)
        if state is None:
            return 422, {"message": "bad review event %r" % event}
        identifier = "R_%d" % next(SEQ)
        REVIEWS.append({"id": identifier, "body": body.get("body") or "", "state": state, "submittedAt": NOW(), "lastEditedAt": None, "author": ME})
        for comment in body.get("comments") or []:
            THREADS.append({"id": "T_%d" % next(SEQ), "resolved": False, "outdated": False, "path": comment["path"], "line": comment["line"], "side": comment.get("side", "RIGHT"),
                            "comments": [{"id": "RC_%d" % next(SEQ), "author": ME, "body": comment["body"], "createdAt": NOW(), "lastEditedAt": None}]})
        if state != "COMMENTED":
            PULLS[42]["decision"] = "APPROVED" if state == "APPROVED" else "CHANGES_REQUESTED"
        return 200, {"id": 9000, "state": state}
    m = re.match(r"^repos/[^/]+/[^/]+/pulls/(\d+)/requested_reviewers$", rest)
    if m and method in ("POST", "DELETE"):
        p = PULLS[int(m.group(1))] if int(m.group(1)) in PULLS else PULLS[42]
        teams = p.setdefault("teams", ["platform"] if int(m.group(1)) == 42 else [])
        for login in body.get("reviewers") or []:
            if method == "POST" and login not in p["reviewers"]:
                p["reviewers"].append(login)
            if method == "DELETE" and login in p["reviewers"]:
                p["reviewers"].remove(login)
        for slug in body.get("team_reviewers") or []:
            if method == "POST" and slug not in teams:
                teams.append(slug)
            if method == "DELETE" and slug in teams:
                teams.remove(slug)
        return 201 if method == "POST" else 200, {}
    m = re.match(r"^repos/[^/]+/[^/]+/issues/(\d+)/labels(?:/(.*))?$", rest)
    if m:
        p = PULLS[int(m.group(1))] if int(m.group(1)) in PULLS else PULLS[42]
        if method == "POST":
            for name in body.get("labels") or []:
                if name not in [n for n, _c in p["labels"]]:
                    p["labels"].append((name, dict(LABELS).get(name, "cccccc")))
        elif method == "DELETE" and m.group(2):
            name = unquote(m.group(2))
            p["labels"] = [(n, c) for n, c in p["labels"] if n != name]
        return 200, []
    m = re.match(r"^repos/[^/]+/[^/]+/actions/runs/(\d+)/approve$", rest)
    if m:
        return 201, {}
    return None


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body, content_type="application/json"):
        data = body if isinstance(body, bytes) else (body if isinstance(body, str) else json.dumps(body)).encode()
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def unhandled(self, what):
        log({"unhandled": what, "method": self.command, "path": self.path})
        self.reply(501, {"message": "browser fixture: no answer for %s %s (%s)" % (self.command, self.path, what)})

    def body(self):
        length = int(self.headers.get("content-length") or 0)
        return self.rfile.read(length).decode() if length else ""

    def do_POST(self):
        parts = urlsplit(self.path)
        raw = self.body()
        log({"method": "POST", "path": self.path})
        rest_match = re.match(r"^/rest/[^/]+/(.*)$", parts.path)
        if rest_match:
            try:
                body = json.loads(raw) if raw.strip() else {}
            except ValueError:
                return self.unhandled("REST body is not JSON")
            with LOCK:
                done = rest_write("POST", rest_match.group(1), body)
            return self.reply(*done) if done else self.unhandled("POST " + rest_match.group(1))
        if not parts.path.startswith("/graphql/"):
            return self.reply(403, {"message": "browser fixture does not handle POST %s" % parts.path})
        try:
            request = json.loads(raw)
        except ValueError:
            return self.unhandled("graphql body is not JSON")
        query, variables = request.get("query", ""), request.get("variables") or {}
        log({"method": "POST", "graphql": known(query) or query.strip().splitlines()[0][:70]})
        number = variables.get("number") or (int(m.group(1)) if (m := re.search(r"pullRequest\(number: (\d+)\)", query)) else 42)
        if query.lstrip().startswith("mutation"):
            with LOCK:
                outcome = mutate(query, variables)
            if isinstance(outcome, str):
                log({"unhandled": outcome})
                return self.reply(200, {"data": None, "errors": [{"type": "UNPROCESSABLE", "message": "browser fixture: " + outcome}]})
            return self.reply(200, {"data": outcome})
        if query.lstrip().startswith("query PullRequestActionState"):
            p = pull(number)
            return self.reply(200, {"data": {"repository": {"pullRequest": {"id": "PR_%d" % number, "headRefOid": HEAD_OID, "isMergeQueueEnabled": False,
                "mergeStateStatus": "BLOCKED" if p["rollup"] == "FAILURE" else "CLEAN", "baseRef": {"compare": {"behindBy": p["behind"]}}}}}})
        if known(query) == "PULL_REQUEST_NODE_ID_GRAPHQL_QUERY":
            return self.reply(200, {"data": {"repository": {"pullRequest": {"id": "PR_%d" % number}}}})
        if known(query) == "REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY":
            subject = variables.get("subjectId", "")
            return self.reply(200, {"data": {"repository": {"pullRequest": {"id": "PR_%d" % number}}, "node": {"id": subject, "pullRequest": {"id": "PR_%d" % number}} if subject else None}})
        if known(query) == "REVIEWER_CANDIDATES_GRAPHQL_QUERY":
            return self.reply(200, {"data": {"repository": {"assignableUsers": {"pageInfo": {"hasNextPage": False}, "nodes": [{"login": u, "name": u.capitalize(), "avatarUrl": None} for u in USERS]},
                "pullRequest": {"author": {"login": pull(number)["author"]}, "reviewRequests": {"nodes": reviewer_nodes(number)}}}}})
        if known(query) == "LABEL_CANDIDATES_GRAPHQL_QUERY":
            return self.reply(200, {"data": {"repository": {"labels": {"pageInfo": {"hasNextPage": False}, "nodes": [{"name": n, "color": c, "description": None} for n, c in LABELS]},
                "pullRequest": {"labels": {"nodes": [{"name": n} for n, _c in pull(number)["labels"]]}}}}})
        name = known(query)
        if name in ("CORE_GITHUB_COM", "CORE_OTHER"):
            return self.reply(200, core(number))
        if name in ("CHECK_CONTEXTS_GITHUB_COM", "CHECK_CONTEXTS_OTHER"):
            return self.reply(200, check_contexts(number))
        if name == "PULL_REQUEST_PREVIEW_GRAPHQL_QUERY":
            return self.reply(200, preview(number))
        if name == "PULL_REQUEST_ACTIVITY_GRAPHQL_QUERY":
            return self.reply(200, activity())
        if name == "REVIEW_THREADS_GRAPHQL_QUERY":
            return self.reply(200, threads())
        if name == "VIEWER_PERMISSIONS_GRAPHQL_QUERY":
            return self.reply(200, {"data": {"repository": {"mergeCommitAllowed": True, "squashMergeAllowed": True, "rebaseMergeAllowed": True, "viewerPermission": "WRITE", "pullRequest": {"viewerCanUpdate": True, "viewerDidAuthor": False}}}})
        if name == "PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY":
            return self.reply(200, {"data": {"repository": {"pullRequest": {"files": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": [{"path": f["filename"], "viewerViewedState": "VIEWED" if VIEWED.get(f["filename"]) else "UNVIEWED"} for i, f in enumerate(FILES)]}}}}})
        if name == "REVIEW_DISMISSALS_GRAPHQL_QUERY":
            return self.reply(200, {"data": {"repository": {"pullRequest": {"reviewDismissals": {"pageInfo": {"hasNextPage": False, "endCursor": None}, "nodes": []}}}}})
        if name in ("SUMMARY_SINGLE", "SUMMARY_SINGLE_WITH_STACKS"):
            return self.reply(200, {"data": {"s0": summary_pull(number)}})
        if query.startswith("query PullRequestSummaries"):
            numbers = [int(n) for n in re.findall(r"pullRequest\(number: (\d+)\)", query)]
            return self.reply(200, {"data": {"s%d" % i: summary_pull(n) for i, n in enumerate(numbers)}})
        if query.startswith("query($q: String!, $after: String)"):
            return self.reply(200, search(variables))
        if query.startswith("query {\n  s0: repository"):
            numbers = [int(n) for n in re.findall(r"pullRequest\(number: (\d+)\)", query)]
            return self.reply(200, {"data": {"s%d" % i: {"pullRequest": {"additions": pull(n)["additions"], "deletions": pull(n)["deletions"]}} for i, n in enumerate(numbers)}})
        if query.startswith("query PullRequestsByHead"):
            head = variables.get("h0", "")
            match = [n for n, p in PULLS.items() if p["head"] == head]
            nodes = [{"number": n, "title": pull(n)["title"], "url": url(n), "baseRefName": "main", "headRefName": head, "headRefOid": "e4d3c2b1a0", "state": pull(n)["state"], "isDraft": pull(n)["draft"],
                      "mergedAt": pull(n).get("merged"), "closedAt": pull(n).get("closed"), "updatedAt": pull(n)["updated"], "isCrossRepository": False,
                      "headRepository": {"name": "widgets", "nameWithOwner": REPO}, "headRepositoryOwner": {"login": "acme"}} for n in match]
            return self.reply(200, {"data": {"repository": {"h0": {"nodes": nodes}}}})
        return self.unhandled("graphql document %s: %s" % (name, query.strip().splitlines()[0][:80] if query.strip() else ""))

    def do_GET(self):
        parts = urlsplit(self.path)
        path = parts.path
        log({"method": "GET", "path": self.path})
        match = re.match(r"^/rest/([^/]+)/(.*)$", path)
        if not match:
            return self.unhandled("not under /rest/<host>/")
        rest = match.group(2)
        if rest == "user":
            return self.reply(200, {"id": 4242, "login": ME})
        pulls = re.match(r"^repos/[^/]+/[^/]+/pulls/(\d+)(/files)?$", rest)
        if pulls:
            number = int(pulls.group(1))
            if pulls.group(2):
                return self.reply(200, FILES if number == 42 else [])
            if "diff" in (self.headers.get("accept") or ""):
                return self.reply(200, DIFF if number == 42 else "", "text/plain")
            return self.reply(200, {"base": {"sha": "1111111"}, "head": {"sha": "e4d3c2b1a0"}})
        if "/contents/" in rest:
            return self.reply(200, "contents at %s\n" % (parts.query or "HEAD"), "text/plain")
        if "/stacks" in rest:
            return self.reply(404, {})
        return self.unhandled("GET " + rest)

    def do_PUT(self):
        raw = self.body()
        log({"method": self.command, "path": self.path})
        rest_match = re.match(r"^/rest/[^/]+/(.*)$", urlsplit(self.path).path)
        done = None
        if rest_match:
            try:
                body = json.loads(raw) if raw.strip() else {}
            except ValueError:
                body = {}
            with LOCK:
                done = rest_write(self.command, rest_match.group(1), body)
        self.reply(*done) if done else self.unhandled("%s %s" % (self.command, self.path))

    do_PATCH = do_DELETE = do_PUT


if __name__ == "__main__":
    port = int(sys.argv[sys.argv.index("--port") + 1])
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
