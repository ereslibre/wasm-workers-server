test/rust-basic.wasm: rust-basic/target/wasm32-wasi/release/rust-basic.wasm
	mkdir -p test
	nix run nixpkgs/e8f52037cf543841be16dc7ef6a8803ff77344cf#wasm-tools -- component new $< \
	  --adapt wasi_snapshot_preview1=wasi-component-adapter/wasi_snapshot_preview1-command.wasm \
	  -o $@

rust-basic/target/wasm32-wasi/release/rust-basic.wasm:
	make -C rust-basic

.PHONY: clean
clean:
	make -C rust-basic clean
	rm -rf test
