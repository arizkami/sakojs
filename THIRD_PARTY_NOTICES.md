# Third-party notices

Sako.js links against Google V8 and its bundled third-party dependencies. Those components are not relicensed under Sako.js's BSD 3-Clause License.

The V8 source and headers included under `.deps/v8` retain their original copyright and BSD-style license notices. This supplied artifact does not contain V8's root `LICENSE` file. Dependency-specific licenses that are present remain under `.deps/v8/third_party`.

Binary distributions of Sako.js must restore the license file matching this exact V8 revision and include all applicable V8 and dependency notices after auditing the build inputs. Packaging is blocked until that license bundle is complete.
