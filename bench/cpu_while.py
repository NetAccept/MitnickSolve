import time

n = int(__import__("sys").argv[1]) if len(__import__("sys").argv) > 1 else 10_000_000
start = time.perf_counter()
total = 0
i = 0
while i < n:
    total += i
    i += 1
elapsed = time.perf_counter() - start
print(f"python-while n={n} sum={total} elapsed_ms={elapsed * 1000:.3f}")
