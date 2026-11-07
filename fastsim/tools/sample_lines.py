#!/usr/bin/env python3
"""Self samples per source line from a macOS `sample` call graph (inlined code keeps its line).

Usage: sample_lines.py SAMPLE.txt [top_n]
"""
import re
import sys
from collections import Counter

lines = open(sys.argv[1]).read().split("Call graph:")[1].split("Total number in stack")[0].splitlines()
entry = re.compile(r"^(?P<indent>[\s+!:|]*?)(?P<count>\d+) (?P<frame>.*?)(?:  \[.*\])?(?:\s+(?P<src>[\w./-]+\.rs:\d+))?\s*$")
parsed = []
for ln in lines:
    m = entry.match(ln)
    if m:
        parsed.append((len(m.group("indent")), int(m.group("count")), m.group("frame"), m.group("src")))
selfc = Counter()
for i, (depth, count, frame, src) in enumerate(parsed):
    child = 0
    for d2, c2, _, _ in parsed[i + 1:]:
        if d2 <= depth:
            break
        if d2 == min(x[0] for x in parsed[i + 1:i + 2]):
            pass
    # children = entries directly one level deeper until indentation returns
    j, kids = i + 1, 0
    first_child_depth = None
    while j < len(parsed) and parsed[j][0] > depth:
        if first_child_depth is None:
            first_child_depth = parsed[j][0]
        if parsed[j][0] == first_child_depth:
            kids += parsed[j][1]
        j += 1
    s = count - kids
    if s > 0:
        name = re.sub(r"_RN\w*?(\d+)(\w+?)(?=\s|$)", r"\2", frame.split(" (in ")[0])[:60]
        selfc[(src or "?", name)] += s
total = sum(selfc.values())
for (src, name), c in selfc.most_common(int(sys.argv[2]) if len(sys.argv) > 2 else 40):
    print(f"{c:6d} {100*c/total:5.1f}%  {src:28s} {name}")
print(f"total self samples {total}")
