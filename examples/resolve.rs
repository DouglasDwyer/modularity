#![allow(warnings)]

use modularity::*;
use std::collections::*;
use std::mem::*;
use wasm_component_layer::*;

const WASM_A: &[u8] = include_bytes!("a/component.wasm");
const WASM_B: &[u8] = include_bytes!("b/component.wasm");
const WASM_C: &[u8] = include_bytes!("c/component.wasm");
const WASM_D: &[u8] = include_bytes!("d/component.wasm");

pub fn main() {
    let engine = Engine::new(wasmi::Engine::default());
    let mut store = Store::new(&engine, ());

    let a = Component::new(&engine, WASM_A).unwrap();
    let b = Component::new(&engine, WASM_B).unwrap();
    let c = Component::new(&engine, WASM_C).unwrap();
    let d = Component::new(&engine, WASM_D).unwrap();

    let package_names = [
        "test:guest@0.1.0",
        "test:guest@0.1.1",
        "test:guest2",
        "test:guest3",
        "test:guest4",
    ]
    .into_iter()
    .map(PackageIdentifier::try_from)
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let mut resolver = Some(PackageResolver::new(
        [package_names[3].clone(), package_names[4].clone()],
        Linker::default(),
    ));
    let packages = package_names
        .into_iter()
        .zip([a.clone(), a, b, c, d].into_iter())
        .collect::<Vec<_>>();

    while let Some(r) = take(&mut resolver) {
        match r.resolve() {
            Ok(x) => {
                let mut ctx = PackageContext::new();
                let transition = PackageContextTransitionBuilder::new(&x, &ctx)
                    .build(&mut store)
                    .unwrap();
                assert!(
                    transition.apply(&mut store, &mut ctx).is_empty(),
                    "Errors occurred during transition."
                );
                println!("The context was {:?}", ctx);

                let pkg = ctx.package(&"test:guest4".try_into().unwrap()).unwrap();
                let select_nth = pkg
                    .exports()
                    .instance(&"test:guest4/bar".try_into().unwrap())
                    .unwrap()
                    .func("select-nth")
                    .unwrap()
                    .typed::<(Vec<String>, u32), (String,)>()
                    .unwrap();

                let example = ["a", "b", "c"]
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();

                println!(
                    "Calling select-nth({example:?}, 1) == {}",
                    select_nth.call(&mut store, (example.clone(), 1)).unwrap().0
                );
            }
            Err(PackageResolverError::MissingPackages(x)) => {
                let r = resolver.insert(x);

                for u in r.unresolved() {
                    let (id, component) = packages
                        .iter()
                        .find(|(id, _)| {
                            id.name() == u.id().name()
                                && u.id()
                                    .version()
                                    .zip(id.version())
                                    .map(|(a, b)| a <= b)
                                    .unwrap_or(true)
                        })
                        .unwrap();
                    u.resolve(id.clone(), component.clone());
                }
            }
            x => panic!("{x:?}"),
        }
    }
}
