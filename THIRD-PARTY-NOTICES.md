# Third-Party Notices

Lost Harness bundles third-party components under separate licenses. This file
lists those components, their copyright holders, and their license terms.

---

## llama.cpp

**Version:** b10088  
**Source:** https://github.com/ggml-org/llama.cpp  
**Copyright:** (c) 2023–2026 The ggml authors  
**License:** MIT (see below)

llama.cpp is vendored as a pre-built binary (`llama-server`) together with its
dynamic libraries under `src-tauri/vendor/llama-cpp/macos-arm64/`. The full
set of vendored files is:

- `llama-server` (the sidecar binary)
- `libllama.0.dylib`
- `libllama-common.0.dylib`
- `libllama-server-impl.dylib`
- `libggml.0.dylib`
- `libggml-base.0.dylib`
- `libggml-blas.0.dylib`
- `libggml-cpu.0.dylib`
- `libggml-metal.0.dylib`
- `libggml-rpc.0.dylib`
- `libmtmd.0.dylib`

### MIT License (llama.cpp)

```
MIT License

Copyright (c) 2023-2026 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

---

## Rust dependencies

All other Rust crate dependencies are sourced from **crates.io** and pulled at
build time. Their respective licenses are listed in each crate's metadata and
are not vendored in this repository.

---

## Icon assets

The application icons under `icons/` are original works created for this
project and are owned by the Lost Harness project.