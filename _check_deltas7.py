with open("/home/sp42/.local/share/JASP/JASP/temp/ai-request.json") as f:
    t = f.read()

pos = 374627
# Skip past "optionMetaDelta" (16 chars)
p = pos + 16
print("After optionMetaDelta keyword:")
for off in range(-2, 20):
    idx = p + off
    if 0 <= idx < len(t):
        print(f"  offset {off:3d}: char={repr(t[idx])}")

print()

# Also check position 486482
pos2 = 486482
p2 = pos2 + 16
print("Second delta:")
for off in range(-2, 20):
    idx = p2 + off
    if 0 <= idx < len(t):
        print(f"  offset {off:3d}: char={repr(t[idx])}")
