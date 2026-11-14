const n = Number(process.argv[2] || 10_000_000);
const start = process.hrtime.bigint();
let total = 0;
for (let i = 0; i < n; i++) total += i;
const end = process.hrtime.bigint();
console.log(`node n=${n} sum=${total} elapsed_ms=${Number(end - start) / 1e6}`);
