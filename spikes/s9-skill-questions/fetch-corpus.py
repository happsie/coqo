"""Fetch a corpus of real Claude review comments from public GitHub PRs (S9, #340).

Usage: python3 -I corpus.py <out.json> [per_query]

For each PR: the comments by claude[bot] (the review text as written), the
repository's file list at the PR head, and the line count of every file a
comment could name (basename match), so the prototype can tell a real
`file:line` from a false one offline. Nothing here is committed: the
comments belong to their authors.
"""
import json
import re
import subprocess
import sys

QUERIES = [
    # The official code-review plugin's terminal/comment format.
    'commenter:claude[bot] is:pr is:public "Found" "issues" "Code review"',
    # claude-code-action reviews (free-form review prompts and skills).
    'commenter:claude[bot] is:pr is:public "Claude finished" review',
    'commenter:claude[bot] is:pr is:public "Critical" "Recommendations" review',
]


def gh(path, **params):
    args = ["gh", "api", "-X", "GET", path]
    for k, v in params.items():
        args += ["-f", f"{k}={v}"]
    out = subprocess.run(args, capture_output=True, text=True)
    if out.returncode != 0:
        return None
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return None


def main():
    out_path = sys.argv[1]
    per_query = int(sys.argv[2]) if len(sys.argv) > 2 else 15
    seen_repos = set()
    prs = []
    for q in QUERIES:
        res = gh("search/issues", q=q, per_page=60, sort="updated")
        if not res:
            continue
        taken = 0
        for item in res.get("items", []):
            repo = item["repository_url"].split("/repos/")[1]
            if repo in seen_repos:
                continue
            seen_repos.add(repo)
            prs.append((repo, item["number"], q))
            taken += 1
            if taken >= per_query:
                break
    corpus = []
    for repo, number, q in prs:
        comments = gh(f"repos/{repo}/issues/{number}/comments", per_page=100) or []
        bodies = [c["body"] for c in comments if c.get("user", {}).get("login") == "claude[bot]" and c.get("body")]
        # Keep only substantive comments (a review, not "working on it").
        bodies = [b for b in bodies if len(b) > 400]
        if not bodies:
            continue
        pull = gh(f"repos/{repo}/pulls/{number}")
        if not pull:
            continue
        sha = pull["head"]["sha"]
        tree = gh(f"repos/{repo}/git/trees/{sha}", recursive="1")
        if not tree or tree.get("truncated"):
            continue
        files = [t["path"] for t in tree["tree"] if t["type"] == "blob"]
        by_base = {}
        for f in files:
            by_base.setdefault(f.rsplit("/", 1)[-1], []).append(f)
        text = "\n".join(bodies)
        mentioned = set()
        for tok in re.findall(r"[\w.\-/]+", text):
            base = tok.rsplit("/", 1)[-1]
            for f in by_base.get(base, []):
                mentioned.add(f)
        line_counts = {}
        for f in sorted(mentioned)[:40]:
            blob = gh(f"repos/{repo}/contents/{f}", ref=sha)
            if blob and blob.get("encoding") == "base64":
                import base64

                try:
                    data = base64.b64decode(blob["content"]).decode("utf-8", "replace")
                except Exception:
                    continue
                line_counts[f] = data.count("\n") + (0 if data.endswith("\n") else 1)
        corpus.append(
            {
                "repo": repo,
                "pr": number,
                "url": f"https://github.com/{repo}/pull/{number}",
                "query": q,
                "sha": sha,
                "files": files,
                "line_counts": line_counts,
                "comments": bodies,
            }
        )
        print(f"{repo}#{number}: {len(bodies)} comments, {len(files)} files, {len(line_counts)} counted", file=sys.stderr)
    with open(out_path, "w") as fh:
        json.dump(corpus, fh)
    print(f"{len(corpus)} PRs", file=sys.stderr)


main()
