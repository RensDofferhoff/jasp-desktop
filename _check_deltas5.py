with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()

# Properly measure optionMetaDelta by parsing the escaped JSON string
# The content is: "content":"...\"optionMetaDelta\":{...}..."
# The delta value is escaped JSON inside the larger JSON string


def measure_delta_in_escaped_string(text, start_pos):
    """
    Given a position pointing to 'optionMetaDelta' in the escaped string,
    find the end of its JSON value, respecting escape sequences.
    text is the raw file text which uses escaping like \", \\, etc.
    """
    # Skip past "optionMetaDelta":
    p = start_pos + len("optionMetaDelta")
    # Skip colon and optional whitespace
    while p < len(text) and text[p] in (":", " "):
        p += 1
    if p >= len(text):
        return 0, ""

    # Now we should be at '{'
    if text[p] != "\\" and text[p] != "{":
        # try next char (might have a backslash)
        pass

    # Find the actual opening brace (might be '{' or '\{' in the raw file)
    start_val = p
    if text[p] == "\\" and p + 1 < len(text) and text[p + 1] == "{":
        depth_start = p + 1  # the '{'
    elif text[p] == "{":
        depth_start = p
    else:
        return 0, ""

    depth = 1
    i = depth_start + 1
    in_str = False
    while i < len(text) and depth > 0:
        ch = text[i]
        if in_str:
            if ch == "\\":
                i += 2  # skip escaped char
                continue
            elif ch == '"':
                in_str = False
        else:
            if ch == "\\" and i + 1 < len(text) and text[i + 1] == '"':
                # escaped quote: \"
                in_str = not in_str
                i += 2
                continue
            elif ch == '"':
                in_str = True
            elif ch == "\\" and i + 1 < len(text) and text[i + 1] == "{":
                depth += 1
                i += 1
            elif ch == "\\" and i + 1 < len(text) and text[i + 1] == "}":
                depth -= 1
                i += 1
            elif ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
        i += 1

    delta_raw = text[depth_start:i]
    # Remove the leading/trailing backslashes for measurement
    # The actual delta content in the escaped string
    actual = delta_raw
    # Unescape for display
    display = delta_raw[:200].replace('\\"', '"').replace("\\n", "\n")
    return len(delta_raw), delta_raw


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
    size, raw = measure_delta_in_escaped_string(t, pos)
    # Check if it's a variable-only delta (tiny) or distributions delta (big)
    if '"variable"' in raw[:200]:
        typ = "variable-only delta"
    elif '"distributions"' in raw[:200]:
        typ = "distributions delta"
    else:
        typ = "other"
    print(f"  {label:35s}  optionMetaDelta={size:>8,} chars  {typ}")
