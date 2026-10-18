"""Replace typographic punctuation (em/en dashes, ellipsis, curly quotes) with plain ASCII.
Usage: python tools/scrub_text.py [paths...]  (defaults to src, README.md, docs)"""
import pathlib, sys
roots = sys.argv[1:] or ["src", "README.md", "THIRD-PARTY-NOTICES.md", "docs"]
REPL = [(" — ", " - "), ("—", " - "), (" – ", " - "), ("–", "-"), ("…", "..."),
        ("“", '"'), ("”", '"'), ("‘", "'"), ("’", "'")]
changed = 0
for r in roots:
    p = pathlib.Path(r)
    files = [p] if p.is_file() else list(p.rglob("*.rs")) + list(p.rglob("*.md"))
    for f in files:
        s = f.read_text(encoding="utf8")
        o = s
        for a, b in REPL:
            s = s.replace(a, b)
        if s != o:
            f.write_text(s, encoding="utf8")
            changed += 1
print("files changed:", changed)
