"""Replace typographic punctuation (em/en dashes, ellipsis, curly quotes) with plain ASCII.
Usage: python tools/scrub_text.py [paths...]  (defaults to src, README.md, docs)"""
import pathlib, sys
check = "--check" in sys.argv
args = [a for a in sys.argv[1:] if a != "--check"]
roots = args or ["src", "README.md", "THIRD-PARTY-NOTICES.md", "docs"]
# Curly double quotes map to a single quote: a bare " would end a Rust string literal.
REPL = [(" — ", " - "), ("—", " - "), (" – ", " - "), ("–", "-"), ("…", "..."),
        ("“", "'"), ("”", "'"), ("‘", "'"), ("’", "'")]
changed = 0
for r in roots:
    p = pathlib.Path(r)
    files = [p] if p.is_file() else list(p.rglob("*.rs")) + list(p.rglob("*.md"))
    for f in files:
        s = f.read_text(encoding="utf8")
        o = s
        for a, b in REPL:
            # Never turn a char literal like '…' into an invalid multi-char literal.
            s = s.replace("'" + a + "'", "'" + a + "'" if len(b) > 1 else "'" + b + "'")
            parts = s.split("'" + a + "'")
            s = ("'" + a + "'").join(p.replace(a, b) for p in parts)
        if s != o:
            changed += 1
            if check:
                print("typographic punctuation found in", f)
            else:
                f.write_text(s, encoding="utf8")
print("files changed:" if not check else "files flagged:", changed)
sys.exit(1 if check and changed else 0)
