use wasmi::{Engine, Linker, Module, Store};

#[test]
fn foreign_io_cannot_be_instantiated() {
    let wat = r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func (param i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "run")))
    "#;
    let wasm = wat::parse_str(wat).unwrap();
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm[..]).unwrap();
    let mut store = Store::new(&engine, ());
    let linker = Linker::<()>::new(&engine);
    let err = linker.instantiate(&mut store, &module).err();
    assert!(err.is_some());
    let mut world = aos_branch::MemWorld::memory(1);
    let err = aos_egress::reject_foreign_import(
        &mut world,
        "main",
        &aos_types::Actor::System,
        "wasi_snapshot_preview1",
        "fd_write",
    )
    .unwrap_err();
    assert!(matches!(err, aos_action::ActionError::DirectIo));
}

#[test]
fn aos_import_runs_and_undeclared_read_is_refused() {
    let wat = r#"
        (module
          (import "aos" "read" (func $read (param i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "run") (result i32)
            (call $read (i32.const 0) (i32.const 16))))
    "#;
    let wasm = wat::parse_str(wat).unwrap();
    let engine = Engine::default();
    let module = Module::new(&engine, &wasm[..]).unwrap();
    let mut store = Store::new(&engine, 0i32);
    let mut linker = Linker::new(&engine);
    linker
        .func_wrap(
            "aos",
            "read",
            |mut caller: wasmi::Caller<i32>, _ptr: i32, _len: i32| -> i32 {
                // The host, not the module, decides the type is undeclared.
                *caller.data_mut() = 1;
                -1
            },
        )
        .unwrap();
    let instance = linker
        .instantiate(&mut store, &module)
        .unwrap()
        .start(&mut store)
        .unwrap();
    let run = instance.get_typed_func::<(), i32>(&store, "run").unwrap();
    let rc = run.call(&mut store, ()).unwrap();
    assert_eq!(rc, -1);
    assert_eq!(*store.data(), 1);
    assert!(aos_action::check_type_allowed(&["sys/Note".into()], "sys/Fact").is_err());
}
