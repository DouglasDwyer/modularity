# modularity

[![Crates.io](https://img.shields.io/crates/v/modularity.svg)](https://crates.io/crates/modularity)
[![Docs.rs](https://docs.rs/modularity/badge.svg)](https://docs.rs/modularity)
[![Unsafe Forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

`modularity` is a bare-bones library for loading and linking [WebAssembly components](https://github.com/WebAssembly/component-model).
It serves as a foundation for WASM-based plugin and modding systems by providing the following functionality:

- Resolving dependency graphs of WASM packages from arbitrary sources
- Instantiating WASM packages with imports from other components and the host
- Allowing the host to inspect and call package exports

## Usage

The example below illustrates how to use this crate. A complete version may be found in [the examples folder](examples/).
It first creates a `PackageResolver`, specifying the list of packages that the application desires to load.
Then, it repeatedly calls `PackageResolver::resolve`, supplying new components whenever the resolver reports that it needs them.
Once the resolver has finished building the dependency graph, it produces a `PackageContextImage`. The image is subsequently applied
to the `PackageContext`, where all of the components are linked and instantiated. After this, the package exports may be accessed through the context.

```rust
// Create the WASM engine and store
let engine = Engine::new(wasmi::Engine::default());
let mut store = Store::new(&engine, ());

// Create a context to hold packages
let mut ctx = PackageContext::default();

// Create a resolver with the list of top-level dependencies
let mut resolver = Some(PackageResolver::new(package_ids), Linker::default());

while let Some(r) = take(&mut resolver) {
    match r.resolve() {
        Ok(x) => {
            // Create a transition to move the context to the new set of packages.
            // The linking process can be customized here.
            let transition = PackageContextTransitionBuilder::new(&x, &ctx)
                .build(&mut store, &ctx)
                .unwrap();

            // Apply the transition to the package context.
            transition.apply(&mut store, &mut ctx);

            println!("Loaded packages are {:?}", ctx.packages().collect::<Vec<_>>());
        }
        Err(PackageResolverError::MissingPackages(mut r)) => {
            for u in r.unresolved() {
                // Gets the component with the specified ID from a source
                u.resolve(u.id(), get_package(&u));
            }
            resolver = Some(r);
        }
        x => panic!("{x:?}"),
    }
}
```