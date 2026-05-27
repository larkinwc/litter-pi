//! Integration test: confirm that the vendored, patched asupersync
//! crate exposes `create_reactor()` and `RuntimeBuilder` and that the
//! pair constructs successfully on the host. The test compiles on
//! every platform whose reactor cfg gate has been widened by the
//! `litter/ios-android-reactor-cfg` submodule patches; see
//! `library/runtime-topology.md`.

#[test]
fn create_reactor_returns_non_null_handle() {
    let reactor = asupersync::runtime::reactor::create_reactor()
        .expect("create_reactor must succeed on the host target");

    // Trait-object pointer arithmetic isn't stable, so use the public
    // observable contract: a freshly-created reactor reports zero
    // registrations and is empty.
    assert_eq!(reactor.registration_count(), 0);
    assert!(reactor.is_empty());
}

#[test]
fn runtime_builder_attaches_reactor() {
    let reactor = asupersync::runtime::reactor::create_reactor()
        .expect("create_reactor must succeed on the host target");

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .expect("asupersync runtime must build with an attached reactor");

    // block_on a trivial future to prove the runtime is alive.
    let answer = runtime.block_on(async { 1 + 1 });
    assert_eq!(answer, 2);
}
