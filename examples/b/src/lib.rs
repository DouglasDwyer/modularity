wit_bindgen::generate!({
    world: "guest",
    exports: {
        "test:guest2/bar": Foo
    }
});

struct Foo;

impl exports::test::guest2::bar::Bar for Foo {
    fn select_nth(x: Vec<String>, i: u32) -> String {
        test::guest::foo::select_nth(&x, i)
    }
}