cd a
cargo build
wasm-tools.exe component new target/wasm32-unknown-unknown/debug/single_component.wasm -o component.wasm
cd ..
cd b
cargo build
wasm-tools.exe component new target/wasm32-unknown-unknown/debug/single_component.wasm -o component.wasm
cd ..
cd c
cargo build
wasm-tools.exe component new target/wasm32-unknown-unknown/debug/single_component.wasm -o component.wasm
cd ..
cd d
cargo build
wasm-tools.exe component new target/wasm32-unknown-unknown/debug/single_component.wasm -o component.wasm
cd ..