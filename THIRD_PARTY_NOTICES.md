# Third-party notices

Sako.js links against Google V8 and its bundled third-party dependencies. Those components are not relicensed under Sako.js's BSD 3-Clause License.

The V8 source and headers included under `.deps/v8` retain their original copyright and BSD-style license notices. This supplied artifact does not contain V8's root `LICENSE` file. Dependency-specific licenses that are present remain under `.deps/v8/third_party`.

Binary distributions of Sako.js must restore the license file matching this exact V8 revision and include all applicable V8 and dependency notices after auditing the build inputs. Packaging is blocked until that license bundle is complete.

The Node-API headers under `crates/sako-v8/include/node` (`js_native_api.h`, `js_native_api_types.h`, `node_api.h`, and `node_api_types.h`) are copied unmodified from the Node.js project and remain under Node's MIT license. They define the ABI compiled addons are built against; reproducing it from scratch would be a guess, and a wrong guess is a memory-safety bug rather than a compile error. Distributors must include Node's license text alongside them.

Sako.js also uses the Rust crates recorded in `Cargo.lock`, including `base64`, `deno_ast` and its SWC stack, `flate2`, `hmac`, `md-5`, `pbkdf2`, `rand`, `semver`, `serde`, `serde_json`, `sha2`, `tar`, and `ureq` and their transitive dependencies. The RustCrypto crates (`hmac`, `md-5`, `pbkdf2`, `sha2`) implement the primitives `crates/sako-postgres` needs for PostgreSQL's `md5` and SCRAM-SHA-256 authentication; hand-rolling those would be a worse risk than the dependency. Each crate remains under its own license; distributors must retain the applicable crate notices and license texts.
