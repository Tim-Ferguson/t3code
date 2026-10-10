#!/usr/bin/env python3
"""The `codex` of the browser fixture. Never the real CLI, never the network.

Two jobs behind one executable, because the server uses one `binaryPath` for both:

  codex exec ...    the one-shot run the server makes to write a commit message, a pull request
                    title and body, a branch name or a thread title. It reads the prompt on stdin
                    and the answer's schema from --output-schema, and writes a plausible answer to
                    --output-last-message. No model is involved: the answer is built from what the
                    prompt names (staged files, branches, the user's message).
  anything else     handed to the repository's scripted app-server, tests/fixtures/codex-provider.py,
                    which is what runs the seeded conversations ([hold], [approval], [question],
                    [background], [plan], [todo], [usage-limit], ...).

Every call is appended to $T3_FIXTURE_CALL_LOG (one JSON line) so a reader can see that only this
fake ran. An `exec` whose schema it does not know fails loudly, so a gap shows in the UI.
"""
import json
import os
import re
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.normpath(os.path.join(HERE, "..", "..", ".."))
UPSTREAM = os.environ.get("T3_FIXTURE_CODEX_PROVIDER") or os.path.join(ROOT, "crates", "server", "tests", "fixtures", "codex-provider.py")
argv = sys.argv[1:]


def log(entry):
    path = os.environ.get("T3_FIXTURE_CALL_LOG")
    if path:
        with open(path, "a") as handle:
            handle.write(json.dumps(dict(entry, at=time.time(), fake="codex", pid=os.getpid())) + "\n")


def flag(name):
    return argv[argv.index(name) + 1] if name in argv and argv.index(name) + 1 < len(argv) else None


def words(text, limit):
    return " ".join(text.split()[:limit])


def slug(text):
    cleaned = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return "-".join(part for part in cleaned.split("-") if part)[:40].strip("-") or "change"


def section(prompt, label):
    match = re.search(re.escape(label) + r"\n(.*?)(?:\n\n[A-Z][A-Za-z ]+:\n|\Z)", prompt, re.S)
    return match.group(1).strip() if match else ""


def files_of(prompt):
    names = re.findall(r"^diff --git a/(\S+) b/", prompt, re.M)
    if not names:
        names = [line.split()[-1] for line in section(prompt, "Staged files:").splitlines() if line.strip()]
    seen = []
    for name in names:
        if name not in seen:
            seen.append(name)
    return seen


def commit(prompt):
    names = files_of(prompt)
    base = [os.path.basename(name) for name in names]
    if not names:
        return {"subject": "Update the project", "body": ""}
    subject = "Update " + (", ".join(base[:2]) + (" and %d more" % (len(base) - 2) if len(base) > 2 else ""))
    body = "\n".join("- Touch " + name for name in names[:8])
    return {"subject": subject[:72], "body": body}


def change_request(prompt):
    head = (re.search(r"^Head branch: (.+)$", prompt, re.M) or [None, "the branch"])[1].strip()
    base = (re.search(r"^Base branch: (.+)$", prompt, re.M) or [None, "main"])[1].strip()
    names = files_of(prompt)
    title = "Add " + head.split("/")[-1].replace("-", " ")
    summary = "\n".join("- Update " + name for name in names[:6]) or "- Changes on " + head
    body = "## Summary\n%s\n\n## Testing\n- Not run (browser fixture text, written without a model)\n- Merges into %s\n" % (summary, base)
    return {"title": title[:70], "body": body}


def branch(prompt):
    message = prompt.split("User message:", 1)[-1]
    return {"branch": slug(words(message, 5)) or "fixture-work"}


def thread_title(prompt):
    message = prompt.split("User message:\n", 1)[-1] if "User message:\n" in prompt else prompt.split("Thread contents:\n", 1)[-1]
    message = re.sub(r"\[[a-z-]+\]", "", message)
    title = words(message.strip(), 6).strip(" .:") or "Fixture thread"
    return {"title": title[:39], "needsRefinement": False}


def exec_answer(schema, prompt):
    keys = set((schema.get("properties") or {}).keys())
    if keys == {"subject", "body", "branch"}:
        answer = commit(prompt)
        answer["branch"] = "feature/" + slug(answer["subject"])
        return answer
    if keys == {"subject", "body"}:
        return commit(prompt)
    if keys == {"title", "body"}:
        return change_request(prompt)
    if keys == {"branch"}:
        return branch(prompt)
    if keys == {"title", "needsRefinement"}:
        return thread_title(prompt)
    return None


if argv[:1] == ["exec"]:
    prompt = sys.stdin.read()
    schema_path = flag("--output-schema")
    output_path = flag("--output-last-message")
    schema = json.load(open(schema_path)) if schema_path else {}
    answer = exec_answer(schema, prompt)
    log({"mode": "exec", "argv": argv[:6], "answered": answer is not None, "promptBytes": len(prompt)})
    if answer is None or not output_path:
        sys.stderr.write("fake codex: no answer for exec with schema keys %s\n" % sorted((schema.get("properties") or {}).keys()))
        sys.exit(64)
    with open(output_path, "w") as handle:
        handle.write(json.dumps(answer))
    sys.exit(0)

if argv[:1] == ["--version"]:
    log({"mode": "version"})
    print("codex-cli 0.156.1 (browser fixture)")
    sys.exit(0)

log({"mode": "delegate", "argv": argv[:4]})
os.execv(sys.executable, [sys.executable, UPSTREAM] + argv)
