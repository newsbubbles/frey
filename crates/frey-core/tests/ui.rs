//! Compile-fail evidence for ADR-0011.
//!
//! The runtime tests in `taint_ergonomics.rs` show that the *permitted* flows are pleasant to
//! write. These show that the forbidden ones do not compile — which is the entire argument for
//! carrying labels in the type system rather than checking them at runtime.
//!
//! Regenerate expected output with `TRYBUILD=overwrite cargo test --test ui`.

#[test]
fn forbidden_flows_do_not_compile() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
