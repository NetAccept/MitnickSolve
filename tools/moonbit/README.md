# MoonBit WASM tools

MitnickSolve's tool ABI is WebAssembly-first, so a MoonBit package can implement
a tool directly. A tool is a `foreign_library` that:

- exports linear memory as `memory`;
- exports `run(ptr, len) -> i64`;
- imports the kernel host bridge as `msk.tool_call`.

The kernel writes the JSON input at offset 0 and reads the returned
`(output_ptr << 32) | output_len` from memory. This repository ships
`transform` implemented in MoonBit as a reference; the source is
`mitnick_tools/transform/transform.mbt`.

## Build

MoonBit 0.1.20261104 or newer:

```sh
./tools/moonbit/build.sh        # Linux/macOS
./tools/moonbit/build.ps1       # Windows
```

The script writes `tools/wasm/transform.wasm`. The loader prefers
`<tool>.wat`, then `<tool>.wasm`, then the fallback module, so removing the
old `transform.wat` makes the MoonBit build the active implementation.

## Add another MoonBit tool

1. Create `mitnick_tools/<tool>/moon.pkg` with `pkgtype(kind: "foreign_library")`
   and the `export-memory-name: "memory"` link option.
2. Implement `run` and call the host import, as in `transform`.
3. Build and copy the artifact to `tools/wasm/<tool>.wasm`.
4. Add the matching `tools/wasm/<tool>.json` manifest and grant its tools in
   `mode.rs`/`policy.rs` if it is a first-party kernel tool.

Pure MoonBit tools that do not need envd or filesystem access can also be
executed ad hoc through the `wasm_eval` tool using hand-written WAT. `wasm_eval`
rejects imports, so it cannot reach the host, envd or the network.
