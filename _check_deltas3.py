with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()

# All optionMetaDelta positions with descriptions
# #4=100960 V1 first run, #5=177559 contGamma(no dists), #7=202163 contGamma(all4)
# #8=275795 contExpon(no dists), #9=299826 contExpon(all4)
# #10=374627 contcor2(all4), #11=486482 contcor2(Norm+Unif)
positions = [
    (100960, "V1 (all 4) - first run"),
    (177559, "contGamma (no dists - BUG)"),
    (202163, "contGamma (all 4)"),
    (275795, "contExpon (no dists - BUG)"),
    (299826, "contExpon (all 4)"),
    (374627, "contcor2 (all 4 - Gamma err)"),
    (486482, "contcor2 (Normal+Uniform - FINAL)"),
]

for pos, label in positions:
    # Find the JSON value after "optionMetaDelta":
    val_start = pos + len("optionMetaDelta") + 1  # +1 for ':'

    # Parse the JSON value (nested object)
    if t[val_start] == "{":
        depth = 0
        i = val_start
        in_str = False
        while i < len(t):
            ch = t[i]
            if in_str:
                if ch == "\\":
                    i += 2
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
                    if depth == 0:
                        break
            i += 1
        delta_json = t[val_start : i + 1]
        delta_size = len(delta_json)
    elif t[val_start] == '"':
        i = val_start + 1
        while i < len(t):
            if t[i] == "\\":
                i += 2
            elif t[i] == '"':
                break
            else:
                i += 1
        delta_json = t[val_start : i + 1]
        delta_size = len(delta_json)
    else:
        delta_json = "?"
        delta_size = 0

    # Show the top-level keys in the delta
    top_keys = []
    if delta_json.startswith("{"):
        import re

        key_start = 0
        while True:
            km = re.search(r'"([^"]+)"\s*:', delta_json[key_start:])
            if not km:
                break
            top_keys.append(km.group(1))
            key_start += km.end()
            # Skip to the matching value - count depth
            if delta_json[key_start:].lstrip()[0] == "{":
                # Skip nested object
                depth = 1
                j = key_start
                while delta_json[j] != "{":
                    j += 1
                j += 1
                in_str = False
                while depth > 0 and j < len(delta_json):
                    c = delta_json[j]
                    if in_str:
                        if c == "\\":
                            j += 2
                            continue
                        elif c == '"':
                            in_str = False
                    else:
                        if c == '"':
                            in_str = True
                        elif c == "{":
                            depth += 1
                        elif c == "}":
                            depth -= 1
                    j += 1
                key_start = j
            elif delta_json[key_start:].lstrip()[0] == "[":
                # Skip array
                pass  # simplified
            elif delta_json[key_start:].lstrip()[0] == '"':
                pass
            if len(top_keys) > 10:
                break

    print(f"  {label:35s}  delta_size={delta_size:>8,}  keys={top_keys}")
