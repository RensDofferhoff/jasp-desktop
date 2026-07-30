with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()

# Look at the raw content around position 486482 (last contcor2 delta)
pos = 486482
snippet = t[pos - 50 : pos + 500]
print("Raw bytes around contcor2 FINAL optionMetaDelta:")
print(repr(snippet))
print()

# Now find the actual optionMetaDelta value bounds
# The format is: ,"optionMetaDelta":{...}
# Let me see where the next field after optionMetaDelta starts
val_start = pos + len("optionMetaDelta") + 1  # skip past the key and :
print(f"val_start char: {repr(t[val_start])}")

# Count depth
depth = 0
i = val_start
in_str = False
while i < min(val_start + 2000, len(t)):
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
                # Found the end
                break
    i += 1

delta_raw = t[val_start : i + 1]
print(f"delta_raw length: {len(delta_raw)}")
print(f"delta_raw first 200: {delta_raw[:200]}")
print(f"delta_raw last 200: {delta_raw[-200:]}")

# What comes after the closing brace?
if i + 1 < len(t):
    print(f"After delta: {repr(t[i + 1 : i + 50])}")

# Now do the same for position 374627 (contcor2 first attempt)
print("\n--- contcor2 first attempt ---")
pos2 = 374627
val_start2 = pos2 + len("optionMetaDelta") + 1
depth = 0
i = val_start2
in_str = False
while i < min(val_start2 + 2000, len(t)):
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

delta_raw2 = t[val_start2 : i + 1]
print(f"delta_raw length: {len(delta_raw2)}")
print(f"delta_raw: {delta_raw2[:300]}")
print(f"What follows: {repr(t[i + 1 : i + 100])}")
