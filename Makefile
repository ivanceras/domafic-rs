PKG := todo_mvc

# The garbage in this file is just to copy the build results into a
# deployable output directory (out/debug or out/release).

example:
	cargo build --release --example todo_mvc
	mkdir -p out/release
	cp html/index.html out/release/index.html
	cp target/wasm32-unknown-emscripten/release/examples/${PKG}*.wasm out/release/${PKG}.wasm
	cp target/wasm32-unknown-emscripten/release/examples/${PKG}.js out/release/${PKG}.js
	cargo build --release --target=asmjs-unknown-emscripten --example todo_mvc
	cp target/asmjs-unknown-emscripten/release/examples/${PKG}.js out/release/${PKG}.asm.js

bin:
	nwbuild --platforms win32,win64,osx64,linux32,linux64 --buildDir dist/ src/

clean:
	cargo clean
	rm -rf out

.PHONY: example clean
