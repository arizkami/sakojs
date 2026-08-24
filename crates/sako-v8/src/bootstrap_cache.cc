// SPDX-License-Identifier: BSD-3-Clause
//
// Build-time producer for the runtime bootstrap's V8 code cache.
//
// Compiling runtime/js/bootstrap.js costs about 1.7 ms of every startup, all
// of it spent re-deriving the same result from the same bytes. This tool
// compiles the bootstrap once during the build and emits the code cache as a
// C header the bridge embeds, so a run deserializes instead of parsing.
//
// The cache carries V8's own version and flag hash. A cache produced by a
// different V8 build or under different flags is rejected at consumption
// time and the bridge falls back to a normal compile, so a stale cache costs
// speed and never correctness.

#include <cstdio>
#include <memory>
#include <string>
#include <vector>

#include "libplatform/libplatform.h"
#include "v8.h"

#include "bootstrap.generated.h"

namespace {

bool WriteHeader(const char* path, const uint8_t* data, size_t length) {
  std::string header =
      "// Generated from runtime/js/bootstrap.js by bootstrap_cache.cc.\n"
      "static constexpr unsigned char kSakoBootstrapCache[] = {\n";
  char digits[8];
  for (size_t index = 0; index < length; ++index) {
    if (index % 32 == 0) header += "  ";
    snprintf(digits, sizeof(digits), "%u,", static_cast<unsigned>(data[index]));
    header += digits;
    if (index % 32 == 31) header += "\n";
  }
  if (length % 32 != 0) header += "\n";
  header += "};\n";
#if defined(_WIN32)
  FILE* file = nullptr;
  if (fopen_s(&file, path, "wb") != 0 || file == nullptr) return false;
#else
  FILE* file = fopen(path, "wb");
  if (file == nullptr) return false;
#endif
  const bool written =
      fwrite(header.data(), 1, header.size(), file) == header.size();
  fclose(file);
  return written;
}

}  // namespace

int main(int argc, char** argv) {
  if (argc < 3) {
    fprintf(stderr, "usage: bootstrap_cache <icudtl.dat|empty> <output header>\n");
    return 1;
  }
  // An empty path means the V8 build already has ICU data compiled in
  // (icu_use_data_file=false), so there is no external file to point at.
  if (argv[1][0] != '\0' && !v8::V8::InitializeICUDefaultLocation(argv[0], argv[1])) {
    fprintf(stderr, "cannot initialize ICU from %s\n", argv[1]);
    return 1;
  }
  std::unique_ptr<v8::Platform> platform = v8::platform::NewDefaultPlatform();
  v8::V8::InitializePlatform(platform.get());
  if (!v8::V8::Initialize()) {
    fprintf(stderr, "cannot initialize V8\n");
    return 1;
  }

  int status = 0;
  {
    std::unique_ptr<v8::ArrayBuffer::Allocator> allocator(
        v8::ArrayBuffer::Allocator::NewDefaultAllocator());
    v8::Isolate::CreateParams params;
    params.array_buffer_allocator = allocator.get();
    v8::Isolate* isolate = v8::Isolate::New(params);
    {
      v8::Isolate::Scope isolate_scope(isolate);
      v8::HandleScope handle_scope(isolate);
      v8::Local<v8::Context> context = v8::Context::New(isolate);
      v8::Context::Scope context_scope(context);

      v8::Local<v8::String> source;
      if (!v8::String::NewFromUtf8(
               isolate, reinterpret_cast<const char*>(kSakoBootstrap),
               v8::NewStringType::kNormal,
               static_cast<int>(sizeof(kSakoBootstrap) - 1))
               .ToLocal(&source)) {
        fprintf(stderr, "bootstrap source exceeds V8 string limits\n");
        status = 1;
      } else {
        v8::Local<v8::String> name =
            v8::String::NewFromUtf8Literal(isolate, "[sako:bootstrap]");
        v8::ScriptOrigin origin(name);
        v8::ScriptCompiler::Source compiler_source(source, origin);
        v8::Local<v8::UnboundScript> script;
        if (!v8::ScriptCompiler::CompileUnboundScript(
                 isolate, &compiler_source,
                 v8::ScriptCompiler::kNoCompileOptions)
                 .ToLocal(&script)) {
          fprintf(stderr, "cannot compile the runtime bootstrap\n");
          status = 1;
        } else {
          std::unique_ptr<v8::ScriptCompiler::CachedData> cached(
              v8::ScriptCompiler::CreateCodeCache(script));
          if (!cached || cached->length == 0) {
            fprintf(stderr, "V8 produced no code cache for the bootstrap\n");
            status = 1;
          } else if (!WriteHeader(argv[2], cached->data,
                                  static_cast<size_t>(cached->length))) {
            fprintf(stderr, "cannot write %s\n", argv[2]);
            status = 1;
          } else {
            fprintf(stderr, "bootstrap code cache: %d bytes\n", cached->length);
          }
        }
      }
    }
    isolate->Dispose();
  }
  v8::V8::Dispose();
  v8::V8::DisposePlatform();
  return status;
}
