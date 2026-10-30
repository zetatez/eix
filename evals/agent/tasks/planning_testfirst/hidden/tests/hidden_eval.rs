use plan_testfirst::slugify;

#[test]
fn slugifies() {
    assert_eq!(slugify("Hello,  World!"), "hello-world");
    assert_eq!(slugify("  a--b  "), "a-b");
    assert_eq!(slugify("!!!"), "");
    assert_eq!(slugify("Rust 2024"), "rust-2024");
}
