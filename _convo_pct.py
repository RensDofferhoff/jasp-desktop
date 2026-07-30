import json

with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()

file_size = len(t)
print("Total file size: {:,} chars".format(file_size))

calls = [
    ("call_00_qACCVQqO02Wq7p4c5LM33029", "#1: correlation"),
    ("call_00_sTxLrYgFBM0p1fGcKlrM2225", "#2: + descriptives"),
    ("call_00_aBVnCXZfUJHzAhofKNw42442", "#3: re-run"),
]

total_options = 0
total_delta = 0
total_results = 0

for cid, label in calls:
    resp_pos = t.find('"tool_call_id":"' + cid + '"')
    before = t[:resp_pos]
    c_start = before.rfind('"content":"')
    content_start = c_start + 11
    i = content_start
    while i < resp_pos:
        ch = t[i]
        if ch == "\\":
            i += 2
        elif ch == '"':
            rest = t[i : i + 20]
            if rest.startswith('","role"') or rest.startswith('"}\n'):
                break
            else:
                i += 1
        else:
            i += 1
    content_raw = t[content_start:i]
    unescaped = (
        content_raw.replace('\\"', '"').replace("\\n", "\n").replace("\\\\", "\\")
    )
    obj = json.loads(unescaped)

    opts_size = len(json.dumps(obj.get("options", {}))) if obj.get("options") else 0
    delta_size = (
        len(json.dumps(obj.get("optionMetaDelta", {})))
        if "optionMetaDelta" in obj
        else 0
    )
    results_size = len(json.dumps(obj.get("results", {}))) if obj.get("results") else 0

    total_options += opts_size
    total_delta += delta_size
    total_results += results_size

    pct = opts_size * 100.0 / file_size
    print("  {}: options={:,} ({:.1f}% of file)".format(label, opts_size, pct))

print()
print(
    "All options combined: {:,} chars  ({:.1f}% of file)".format(
        total_options, total_options * 100.0 / file_size
    )
)
print(
    "All deltas combined:  {:,} chars  ({:.1f}% of file)".format(
        total_delta, total_delta * 100.0 / file_size
    )
)
print(
    "All results combined: {:,} chars  ({:.1f}% of file)".format(
        total_results, total_results * 100.0 / file_size
    )
)
print(
    "Sum of components:    {:,} chars".format(
        total_options + total_delta + total_results
    )
)
