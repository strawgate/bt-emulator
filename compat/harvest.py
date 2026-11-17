"""Harvest recorded Braintrust API traffic from upstream repos into compat/corpus/.

Usage: uv run --with pyyaml python -I compat/harvest.py [--cache DIR] [--only NAME]

Each upstream repo is fetched at its pinned commit (compat/sources.toml). Every
cassette is parsed, interactions not addressed to Braintrust are dropped, auth is
scrubbed, and the result is written as one normalized JSON file per cassette:

  {"source", "cassette", "interactions": [{"method", "path", "request": {...},
   "response": {"status", "headers", "body"}}]}

Cassettes duplicated byte-for-byte across SDK version directories are deduped.
"""

import argparse
import base64
import gzip
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys
import tomllib
import urllib.parse

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]
COMPAT = ROOT / "compat"
BRAINTRUST_HOSTS = {
    "api.braintrust.dev",
    "www.braintrust.dev",
    "braintrust.dev",
    "api.braintrustdata.com",
    "www.braintrustdata.com",
}
# Headers worth keeping: content negotiation and Braintrust's own x-bt-* protocol headers.
KEEP_HEADER = lambda h: h.lower() == "content-type" or h.lower().startswith("x-bt-")  # noqa: E731
DROP_HEADER_VALUES = {"x-bt-internal-trace-id", "x-bt-api-duration-ms", "x-bt-brainstore-duration-ms"}


def fetch(src, cache):
    dest = cache / src["name"]
    if (dest / ".git").exists():
        have = subprocess.run(["git", "-C", dest, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        if have == src["commit"]:
            return dest
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    git = lambda *a: subprocess.run(["git", "-C", dest, *a], check=True, capture_output=True)  # noqa: E731
    git("init", "-q")
    git("remote", "add", "origin", src["repo"])
    git("fetch", "-q", "--depth", "1", "origin", src["commit"])
    git("checkout", "-q", "FETCH_HEAD")
    return dest


def decode_body(raw):
    """Return parsed JSON if possible, else text, else base64 marker."""
    if raw is None:
        return None
    if isinstance(raw, dict):  # vcrpy: {"string": ...}
        raw = raw.get("string")
        if raw is None:
            return None
    if isinstance(raw, bytes):
        try:
            raw = gzip.decompress(raw)
        except OSError:
            pass
        try:
            raw = raw.decode()
        except UnicodeDecodeError:
            return {"$base64": base64.b64encode(raw).decode()}
    if raw == "":
        return None
    try:
        return json.loads(raw)
    except (json.JSONDecodeError, TypeError):
        return raw


def headers(h):
    out = {}
    for k, v in (h or {}).items():
        if KEEP_HEADER(k) and k.lower() not in DROP_HEADER_VALUES:
            out[k.lower()] = v[0] if isinstance(v, list) and len(v) == 1 else v
    return out


def normalize(fmt, doc):
    for it in doc.get("interactions") or []:
        req, resp = it["request"], it["response"]
        url = req.get("uri") if fmt == "vcrpy" else req.get("url")
        parsed = urllib.parse.urlsplit(url)
        if parsed.hostname not in BRAINTRUST_HOSTS:
            continue
        if fmt == "vcrpy":
            status = resp["status"]["code"]
        else:
            status = resp.get("code") or int(str(resp.get("status", "0")).split()[0])
        yield {
            "method": req["method"].upper(),
            "path": parsed.path + (f"?{parsed.query}" if parsed.query else ""),
            "host": parsed.hostname,
            "request": {"headers": headers(req.get("headers")), "body": decode_body(req.get("body"))},
            "response": {"status": status, "headers": headers(resp.get("headers")), "body": decode_body(resp.get("body"))},
        }


def harvest(src, cache):
    repo = fetch(src, cache)
    out_dir = COMPAT / "corpus" / src["name"]
    shutil.rmtree(out_dir, ignore_errors=True)
    out_dir.mkdir(parents=True)
    for lic in ("LICENSE", "LICENSE.md", "LICENSE.txt", "NOTICE"):
        if (repo / lic).exists():
            shutil.copy(repo / lic, out_dir / lic)
    seen, files, interactions = set(), 0, 0
    paths = sorted({p for g in src["globs"] for p in repo.glob(g) if p.is_file()})
    for path in paths:
        try:
            doc = yaml.safe_load(path.read_bytes()) or {}
        except yaml.YAMLError as exc:
            print(f"  skip {path.relative_to(repo)}: {exc}", file=sys.stderr)
            continue
        items = list(normalize(src["format"], doc))
        if not items:
            continue
        digest = hashlib.sha256(json.dumps(items, sort_keys=True).encode()).hexdigest()
        if digest in seen:
            continue
        seen.add(digest)
        rel = path.relative_to(repo).as_posix()
        out = out_dir / (rel.replace("/", "__").rsplit(".", 1)[0] + ".json")
        out.write_text(json.dumps({"source": src["name"], "commit": src["commit"], "cassette": rel, "interactions": items}, indent=1, sort_keys=True) + "\n")
        files += 1
        interactions += len(items)
    print(f"{src['name']}: {files} cassettes, {interactions} Braintrust interactions")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cache", default=str(ROOT / "target" / "compat-src"))
    ap.add_argument("--only")
    args = ap.parse_args()
    sources = tomllib.loads((COMPAT / "sources.toml").read_text())["source"]
    for src in sources:
        if args.only in (None, src["name"]):
            harvest(src, pathlib.Path(args.cache))


if __name__ == "__main__":
    main()
