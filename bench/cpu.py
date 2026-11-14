import sys
import time

n = int(sys.argv[1]) if len(sys.argv) > 1 else 10_000_000
start = time.perf_counter()
total = 0
for i in range(n):
    total += i
elapsed = time.perf_counter() - start
print(f"python n={n} sum={total} elapsed_ms={elapsed * 1000:.3f}")
