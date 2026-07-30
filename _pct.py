import json
import re

with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()

calls = [
    ("#1: correlation only", "call_00_qACCVQqO02Wq7p4c5LM33029"),
    ("#2: + descriptives", "call_00_sTxLrYgFBM0p1fGcKlrM2225"),
    ("#3: + descriptives (repeat)", "call_00_aBVnCXZfUJHzAhofKNw42442"),
]

print(
    f"{'Label':30s} {'status':8s} {'delta':>8s} {'%':>5s} {'options':>10s} {'%':>5s} {'results':>10s} {'%':>5s} {'total':>8s}"
)
print("-" * 95)
for label, cid in calls:
    resp_pos = t.find('"tool_call_id":"' + cid + '"')
    if resp_pos < 0:
        continue
    before = t[:resp_pos]
    c_start = before.rfind('"content":"')
    if c_start < 0:
        continue
    content_start = c_start + 11
    i = content_start
    while i < resp_pos:
        ch = t[i]
        if ch == "\\":
            i += 2
        elif ch == '"':
            rest = t[i : i + 20]
            if (
                rest.startswith('","role"')
                or rest.startswith('"}\n')
                or rest.startswith('",\n')
            ):
                break
            else:
                i += 1
        else:
            i += 1
    content_raw = t[content_start:i]
    unescaped = (
        content_raw.replace('\\"', '"').replace("\\n", "\n").replace("\\\\", "\\")
    )
    try:
        obj = json.loads(unescaped)
    except:
        continue

    total = len(unescaped)
    delta_size = (
        len(json.dumps(obj.get("optionMetaDelta", {})))
        if "optionMetaDelta" in obj
        else 0
    )
    opts_size = (
        len(json.dumps(obj.get("options", {})))
        if "options" in obj and obj["options"] is not None
        else 0
    )
    results_size = (
        len(json.dumps(obj.get("results", {})))
        if "results" in obj and obj["results"] is not None
        else 0
    )
    status = obj.get("status", "?")[:8]

    def pct(s):
        return f"{s * 100 // total:3d}%" if total > 0 else "  0%"

    print(
        f"  {label:28s} {status:8s} {delta_size:>8,} {pct(delta_size):>5s} {opts_size:>10,} {pct(opts_size):>5s} {results_size:>10,} {pct(results_size):>5s} {total:>8,}"
    )
