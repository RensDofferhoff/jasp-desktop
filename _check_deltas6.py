with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()


def measure_delta(text, start_pos):
    """Measure optionMetaDelta JSON value in the raw escaped string."""
    p = start_pos + len("optionMetaDelta")
    # skip colon
    while p < len(text) and text[p] in (":", " "):
        p += 1
    if p >= len(text) or text[p] != "{":
        return 0, "NOT_FOUND"

    # Count brace depth, handling escaped quotes
    depth = 1
    i = p + 1
    in_str = False
    while i < len(text) and depth > 0:
        ch = text[i]
        if in_str:
            if ch == "\\":
                i += 2  # skip escaped char (e.g., \n, \\, \")
                continue
            elif ch == '"':
                in_str = False
        else:
            if ch == '"':
                in_str = True
            elif ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
        i += 1

    delta = text[p:i]
    return len(delta), delta[:500]


positions = [
    (100960, "V1 (all 4) - first run"),
    (177559, "contGamma (no dists - BUG)"),
    (202163, "contGamma (all 4)"),
    (275795, "contExpon (no dists - BUG)"),
    (299826, "contExpon (all 4)"),
    (374627, "contcor2 (all 4 - Gamma err)"),
    (486482, "contcor2 (Normal+Uniform)"),
]

for pos, label in positions:
    size, preview = measure_delta(t, pos)
    # Categorize by top-level keys
    if '"variable"' in preview[:100] and '"distributions"' not in preview[:100]:
        typ = "VARIABLE-ONLY delta"
    elif '"distributions"' in preview[:100]:
        typ = "DISTRIBUTIONS delta"
    elif size == 0:
        typ = "NOT FOUND"
    else:
        typ = "OTHER"
    print(f"  {label:35s}  size={size:>8,}  {typ}")
