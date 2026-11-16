// MitnickSolve WASM ABI:
// - the kernel writes the JSON input at linear-memory offset 0;
// - it calls run(ptr, len);
// - the host import returns (output_ptr << 32) | output_len.
// This MoonBit tool forwards the transform call to the kernel host bridge.
extern "wasm" fn msk_tool_call(ptr : Int, len : Int) -> Int64 = "msk" "tool_call"

#export_name("run")
pub fn run(ptr : Int, len : Int) -> Int64 {
  msk_tool_call(ptr, len)
}
