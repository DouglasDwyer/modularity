wit_bindgen::generate!({
    world: "guest",
    exports: {
        "test:guest3/baz": Foo
    }
});

struct Foo;

impl exports::test::guest3::baz::Baz for Foo {
    fn select_nth(x: Vec<String>, i: u32) -> String {
        test::guest2::bar::select_nth(&x, i)
    }
}