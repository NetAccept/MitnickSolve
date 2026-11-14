(module
  (memory (export "memory") 16)
  (func (export "run") (param i32 i32) (result i64)
    (memory.copy (i32.const 4096) (local.get 0) (local.get 1))
    (i64.or
      (i64.shl (i64.const 4096) (i64.const 32))
      (i64.extend_i32_u (local.get 1)))))
