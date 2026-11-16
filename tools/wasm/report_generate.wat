(module
  (import "msk" "tool_call" (func $tool_call (param i32 i32) (result i64)))
  (memory (export "memory") 16)
  (func (export "run") (param i32 i32) (result i64)
    (call $tool_call (local.get 0) (local.get 1))))
