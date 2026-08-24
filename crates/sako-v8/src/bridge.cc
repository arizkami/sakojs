// SPDX-License-Identifier: BSD-3-Clause

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <limits>
#include <memory>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>

#include "libplatform/libplatform.h"
#include "v8.h"

namespace {

void WriteError(char* output, size_t capacity, const std::string& message) {
  if (output == nullptr || capacity == 0) return;
  const size_t length = message.size() < capacity - 1 ? message.size() : capacity - 1;
  std::memcpy(output, message.data(), length);
  output[length] = '\0';
}

std::string ToUtf8(v8::Isolate* isolate, v8::Local<v8::Value> value) {
  v8::String::Utf8Value utf8(isolate, value);
  return *utf8 == nullptr ? std::string() : std::string(*utf8, utf8.length());
}

std::wstring Utf8ToWide(const std::string& value) {
  if (value.empty()) return {};
  const int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, value.data(),
                                         static_cast<int>(value.size()), nullptr, 0);
  if (length <= 0) return {};
  std::wstring result(static_cast<size_t>(length), L'\0');
  if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, value.data(),
                          static_cast<int>(value.size()), result.data(), length) <= 0) {
    return {};
  }
  return result;
}

std::string WideToUtf8(const std::wstring& value) {
  if (value.empty()) return {};
  const int length = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, value.data(),
                                         static_cast<int>(value.size()), nullptr, 0,
                                         nullptr, nullptr);
  if (length <= 0) return {};
  std::string result(static_cast<size_t>(length), '\0');
  if (WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, value.data(),
                          static_cast<int>(value.size()), result.data(), length,
                          nullptr, nullptr) <= 0) {
    return {};
  }
  return result;
}

std::string PathToUtf8(const std::filesystem::path& path) {
  return WideToUtf8(path.native());
}

bool ReadFile(const std::filesystem::path& path, std::string* source) {
  std::ifstream input(path, std::ios::binary);
  if (!input) return false;
  input.seekg(0, std::ios::end);
  const std::streamoff size = input.tellg();
  if (size < 0 || size > std::numeric_limits<int>::max()) return false;
  input.seekg(0, std::ios::beg);
  source->resize(static_cast<size_t>(size));
  if (size != 0) input.read(source->data(), size);
  return input.good() || input.eof();
}

void WriteStdout(const char* bytes, size_t length) {
  HANDLE output = GetStdHandle(STD_OUTPUT_HANDLE);
  while (output != INVALID_HANDLE_VALUE && output != nullptr && length != 0) {
    const DWORD chunk = length > MAXDWORD ? MAXDWORD : static_cast<DWORD>(length);
    DWORD written = 0;
    if (!WriteFile(output, bytes, chunk, &written, nullptr) || written == 0) return;
    bytes += written;
    length -= written;
  }
}

void ConsoleLog(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::HandleScope scope(isolate);
  v8::Local<v8::Context> context = isolate->GetCurrentContext();

  for (int index = 0; index < info.Length(); ++index) {
    if (index != 0) WriteStdout(" ", 1);
    v8::Local<v8::String> text;
    if (info[index]->ToString(context).ToLocal(&text)) {
      const std::string utf8 = ToUtf8(isolate, text);
      WriteStdout(utf8.data(), utf8.size());
    }
  }
  WriteStdout("\n", 1);
}

std::string FormatException(v8::Isolate* isolate,
                            v8::Local<v8::Context> context,
                            v8::TryCatch& try_catch) {
  std::string output;
  v8::Local<v8::Message> message = try_catch.Message();
  if (!message.IsEmpty()) {
    v8::Local<v8::Value> resource = message->GetScriptResourceName();
    if (!resource.IsEmpty()) {
      output += ToUtf8(isolate, resource);
      const int line = message->GetLineNumber(context).FromMaybe(0);
      const int column = message->GetStartColumn(context).FromMaybe(-1);
      if (line > 0) output += ":" + std::to_string(line);
      if (column >= 0) output += ":" + std::to_string(column + 1);
      output += "\n";
    }
  }

  v8::Local<v8::Value> stack;
  if (try_catch.StackTrace(context).ToLocal(&stack) && stack->IsString()) {
    output += ToUtf8(isolate, stack);
    return output;
  }

  v8::Local<v8::Value> exception = try_catch.Exception();
  if (!exception.IsEmpty()) {
    output += ToUtf8(isolate, exception);
    return output;
  }
  return output.empty() ? "JavaScript execution failed" : output;
}

class Runtime {
 public:
  static std::unique_ptr<Runtime> Create(const char* executable_path,
                                         const char* icu_data_path,
                                         std::string* error) {
    auto runtime = std::unique_ptr<Runtime>(new Runtime());

    if (!v8::V8::InitializeICUDefaultLocation(executable_path, icu_data_path)) {
      *error = std::string("failed to initialize ICU from ") + icu_data_path;
      return nullptr;
    }

    runtime->platform_ = v8::platform::NewDefaultPlatform();
    if (!runtime->platform_) {
      *error = "failed to create the V8 platform";
      return nullptr;
    }

    v8::V8::InitializePlatform(runtime->platform_.get());
    if (!v8::V8::Initialize()) {
      *error = "failed to initialize V8";
      v8::V8::DisposePlatform();
      return nullptr;
    }
    runtime->v8_initialized_ = true;

    runtime->allocator_.reset(v8::ArrayBuffer::Allocator::NewDefaultAllocator());
    if (!runtime->allocator_) {
      *error = "failed to create the V8 ArrayBuffer allocator";
      return nullptr;
    }

    v8::Isolate::CreateParams params;
    params.array_buffer_allocator = runtime->allocator_.get();
    runtime->isolate_ = v8::Isolate::New(params);
    if (runtime->isolate_ == nullptr) {
      *error = "failed to create a V8 isolate";
      return nullptr;
    }
    runtime->isolate_->SetMicrotasksPolicy(v8::MicrotasksPolicy::kExplicit);
    runtime->isolate_->SetData(0, runtime.get());
    if (!runtime->InitializeContext(error)) return nullptr;
    return runtime;
  }

  ~Runtime() {
    if (isolate_ != nullptr) {
      {
        v8::Isolate::Scope isolate_scope(isolate_);
        for (auto& [id, timer] : timers_) {
          (void)id;
          timer.Reset();
        }
        timers_.clear();
        for (auto& [path, module] : modules_) {
          (void)path;
          module.Reset();
        }
        modules_.clear();
        for (auto& [path, module] : commonjs_modules_) {
          (void)path;
          module.Reset();
        }
        commonjs_modules_.clear();
        context_.Reset();
        isolate_->SetData(0, nullptr);
      }
      v8::platform::NotifyIsolateShutdown(platform_.get(), isolate_);
      isolate_->Dispose();
      isolate_ = nullptr;
    }
    allocator_.reset();
    if (v8_initialized_) {
      v8::V8::Dispose();
      v8::V8::DisposePlatform();
    }
    platform_.reset();
  }

  bool Execute(const uint8_t* source_bytes, size_t source_length,
               const uint8_t* resource_bytes, size_t resource_length,
               const uint8_t* const* argument_bytes,
               const size_t* argument_lengths, size_t argument_count,
               std::string* error) {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HandleScope handle_scope(isolate_);

    v8::Local<v8::Context> context = context_.Get(isolate_);
    v8::Context::Scope context_scope(context);
    v8::TryCatch try_catch(isolate_);

    if (!InstallProcess(context, argument_bytes, argument_lengths,
                        argument_count)) {
      *error = "failed to install process globals";
      return false;
    }

    v8::Local<v8::String> source;
    if (!v8::String::NewFromUtf8(isolate_,
                                 reinterpret_cast<const char*>(source_bytes),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(source_length))
             .ToLocal(&source)) {
      *error = "source is not valid UTF-8 or exceeds V8 string limits";
      return false;
    }

    v8::Local<v8::String> resource_name;
    if (!v8::String::NewFromUtf8(isolate_,
                                 reinterpret_cast<const char*>(resource_bytes),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(resource_length))
             .ToLocal(&resource_name)) {
      *error = "script path is not valid UTF-8 or exceeds V8 string limits";
      return false;
    }

    v8::ScriptOrigin origin(resource_name);
    v8::Local<v8::Script> script;
    if (!v8::Script::Compile(context, source, &origin).ToLocal(&script)) {
      *error = FormatException(isolate_, context, try_catch);
      return false;
    }
    v8::Local<v8::Value> result;
    if (!script->Run(context).ToLocal(&result)) {
      *error = FormatException(isolate_, context, try_catch);
      return false;
    }
    isolate_->PerformMicrotaskCheckpoint();
    return DrainEventLoop(context, error);
  }

  bool ExecuteModule(const uint8_t* path_bytes, size_t path_length,
                     const uint8_t* const* argument_bytes,
                     const size_t* argument_lengths, size_t argument_count,
                     std::string* error) {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HandleScope handle_scope(isolate_);
    v8::Local<v8::Context> context = context_.Get(isolate_);
    v8::Context::Scope context_scope(context);
    v8::TryCatch try_catch(isolate_);

    if (!InstallProcess(context, argument_bytes, argument_lengths,
                        argument_count)) {
      *error = "failed to install process globals";
      return false;
    }

    const std::string path_text(reinterpret_cast<const char*>(path_bytes),
                                path_length);
    const std::wstring wide_path = Utf8ToWide(path_text);
    if (wide_path.empty()) {
      *error = "module path is not valid UTF-8";
      return false;
    }
    std::error_code path_error;
    const std::filesystem::path entry =
        std::filesystem::weakly_canonical(wide_path, path_error);
    if (path_error) {
      *error = "cannot resolve module path: " + path_text;
      return false;
    }

    v8::Local<v8::Module> module;
    if (!CompileModule(context, entry, &module, error)) {
      if (try_catch.HasCaught()) *error = FormatException(isolate_, context, try_catch);
      return false;
    }
    if (!module->InstantiateModule(context, ResolveModule).FromMaybe(false)) {
      *error = FormatException(isolate_, context, try_catch);
      return false;
    }

    v8::Local<v8::Value> evaluation;
    if (!module->Evaluate(context).ToLocal(&evaluation)) {
      *error = FormatException(isolate_, context, try_catch);
      return false;
    }
    isolate_->PerformMicrotaskCheckpoint();
    if (!DrainEventLoop(context, error)) return false;

    if (evaluation->IsPromise()) {
      v8::Local<v8::Promise> promise = evaluation.As<v8::Promise>();
      if (promise->State() == v8::Promise::PromiseState::kRejected) {
        *error = ToUtf8(isolate_, promise->Result());
        return false;
      }
    }
    return true;
  }

  bool ExecuteCommonJs(const uint8_t* path_bytes, size_t path_length,
                       const uint8_t* const* argument_bytes,
                       const size_t* argument_lengths, size_t argument_count,
                       std::string* error) {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HandleScope handle_scope(isolate_);
    v8::Local<v8::Context> context = context_.Get(isolate_);
    v8::Context::Scope context_scope(context);
    v8::TryCatch try_catch(isolate_);

    if (!InstallProcess(context, argument_bytes, argument_lengths,
                        argument_count)) {
      *error = "failed to install process globals";
      return false;
    }
    const std::string path_text(reinterpret_cast<const char*>(path_bytes),
                                path_length);
    const std::wstring wide_path = Utf8ToWide(path_text);
    std::error_code path_error;
    const std::filesystem::path entry =
        std::filesystem::weakly_canonical(wide_path, path_error);
    if (wide_path.empty() || path_error) {
      *error = "cannot resolve CommonJS entry: " + path_text;
      return false;
    }

    v8::Local<v8::Value> exports;
    if (!LoadCommonJs(context, entry, &exports, error)) {
      if (try_catch.HasCaught()) *error = FormatException(isolate_, context, try_catch);
      return false;
    }
    isolate_->PerformMicrotaskCheckpoint();
    return DrainEventLoop(context, error);
  }

  void MemoryStats(uint64_t* heap_used, uint64_t* heap_committed,
                   uint64_t* heap_limit, uint64_t* persistent_handles,
                   uint64_t* timers) const {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HeapStatistics statistics;
    isolate_->GetHeapStatistics(&statistics);
    *heap_used = statistics.used_heap_size();
    *heap_committed = statistics.total_heap_size();
    *heap_limit = statistics.heap_size_limit();
    uint64_t handles =
        (context_.IsEmpty() ? 0 : 1) + modules_.size() + commonjs_modules_.size();
    for (const auto& [id, timer] : timers_) {
      (void)id;
      handles += 1 + timer.arguments.size();
    }
    *persistent_handles = handles;
    *timers = timers_.size();
  }

 private:
  static constexpr size_t kMaximumTimers = 65'536;
  static constexpr size_t kMaximumModules = 4'096;
  static constexpr size_t kMaximumModuleBytes = 64 * 1024 * 1024;

  struct Timer {
    uint64_t id = 0;
    std::chrono::steady_clock::time_point deadline;
    uint64_t interval_milliseconds = 0;
    v8::Global<v8::Function> callback;
    std::vector<v8::Global<v8::Value>> arguments;

    void Reset() {
      callback.Reset();
      for (auto& argument : arguments) argument.Reset();
      arguments.clear();
    }
  };

  Runtime() = default;

  bool InitializeContext(std::string* error) {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HandleScope handle_scope(isolate_);
    v8::Local<v8::Context> context = v8::Context::New(isolate_);
    v8::Context::Scope context_scope(context);

    v8::Local<v8::Object> console = v8::Object::New(isolate_);
    v8::Local<v8::Function> log;
    if (!v8::Function::New(context, ConsoleLog).ToLocal(&log) ||
        !Set(context, console, "log", log) ||
        !Set(context, context->Global(), "console", console)) {
      *error = "failed to install console globals";
      return false;
    }

    v8::Local<v8::External> data = v8::External::New(
        isolate_, this, v8::kExternalPointerTypeTagDefault);
    if (!InstallFunction(context, "setTimeout", SetTimeout, data) ||
        !InstallFunction(context, "setInterval", SetInterval, data) ||
        !InstallFunction(context, "clearTimeout", ClearTimer, data) ||
        !InstallFunction(context, "clearInterval", ClearTimer, data) ||
        !InstallFunction(context, "queueMicrotask", QueueMicrotask, data)) {
      *error = "failed to install runtime scheduling globals";
      return false;
    }

    context_.Reset(isolate_, context);
    return true;
  }

  bool CompileModule(v8::Local<v8::Context> context,
                     const std::filesystem::path& path,
                     v8::Local<v8::Module>* output, std::string* error) {
    const std::string canonical_path = PathToUtf8(path);
    auto cached = modules_.find(canonical_path);
    if (cached != modules_.end()) {
      *output = cached->second.Get(isolate_);
      return true;
    }
    if (modules_.size() >= kMaximumModules) {
      *error = "module cache capacity exceeded";
      return false;
    }

    std::string source_text;
    if (!ReadFile(path, &source_text)) {
      *error = "cannot read module: " + canonical_path;
      return false;
    }
    if (module_source_bytes_ + source_text.size() > kMaximumModuleBytes) {
      *error = "module source cache byte limit exceeded";
      return false;
    }

    v8::Local<v8::String> source;
    v8::Local<v8::String> resource_name;
    if (!v8::String::NewFromUtf8(isolate_, source_text.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(source_text.size()))
             .ToLocal(&source) ||
        !v8::String::NewFromUtf8(isolate_, canonical_path.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(canonical_path.size()))
             .ToLocal(&resource_name)) {
      *error = "module source or path exceeds V8 string limits";
      return false;
    }

    v8::ScriptOrigin origin(resource_name, 0, 0, false, -1,
                            v8::Local<v8::Value>(), false, false, true);
    v8::ScriptCompiler::Source compiler_source(source, origin);
    v8::Local<v8::Module> module;
    if (!v8::ScriptCompiler::CompileModule(isolate_, &compiler_source)
             .ToLocal(&module)) {
      *error = "failed to compile module: " + canonical_path;
      return false;
    }

    modules_.emplace(canonical_path, v8::Global<v8::Module>(isolate_, module));
    module_source_bytes_ += source_text.size();
    *output = module;
    return true;
  }

  static v8::MaybeLocal<v8::Module> ResolveModule(
      v8::Local<v8::Context> context, v8::Local<v8::String> specifier,
      v8::Local<v8::FixedArray> import_attributes,
      v8::Local<v8::Module> referrer) {
    (void)import_attributes;
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    Runtime* runtime = static_cast<Runtime*>(isolate->GetData(0));
    if (runtime == nullptr) return {};

    const std::string request = ToUtf8(isolate, specifier);
    const std::string referrer_name = ToUtf8(isolate, referrer->GetResourceName());
    std::filesystem::path resolved;
    std::string message;
    if (!runtime->ResolvePath(request, referrer_name, &resolved, &message)) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, message.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(message.size()))
              .ToLocalChecked()));
      return {};
    }

    v8::Local<v8::Module> module;
    if (!runtime->CompileModule(context, resolved, &module, &message)) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, message.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(message.size()))
              .ToLocalChecked()));
      return {};
    }
    return module;
  }

  bool ResolvePath(const std::string& request, const std::string& referrer,
                   std::filesystem::path* output, std::string* error) {
    if (request.starts_with("node:")) {
      *error = "unsupported built-in module: " + request;
      return false;
    }
    const std::wstring request_wide = Utf8ToWide(request);
    const std::wstring referrer_wide = Utf8ToWide(referrer);
    if (request_wide.empty() || referrer_wide.empty()) {
      *error = "module specifier is not valid UTF-8";
      return false;
    }

    std::filesystem::path candidate(request_wide);
    if (!candidate.is_absolute()) {
      if (!(request.starts_with("./") || request.starts_with("../") ||
            request.starts_with(".\\") || request.starts_with("..\\"))) {
        *error = "bare package imports are not supported yet: " + request;
        return false;
      }
      candidate = std::filesystem::path(referrer_wide).parent_path() / candidate;
    }

    std::vector<std::filesystem::path> candidates = {candidate};
    if (!candidate.has_extension()) {
      candidates.push_back(candidate.native() + std::wstring(L".js"));
      candidates.push_back(candidate.native() + std::wstring(L".mjs"));
      candidates.push_back(candidate / L"index.js");
    }
    for (const auto& path : candidates) {
      std::error_code status_error;
      if (std::filesystem::is_regular_file(path, status_error)) {
        std::error_code canonical_error;
        *output = std::filesystem::weakly_canonical(path, canonical_error);
        if (!canonical_error) return true;
      }
    }
    *error = "module not found: " + request + " imported from " + referrer;
    return false;
  }

  bool LoadCommonJs(v8::Local<v8::Context> context,
                    const std::filesystem::path& path,
                    v8::Local<v8::Value>* output, std::string* error) {
    const std::string canonical_path = PathToUtf8(path);
    auto cached = commonjs_modules_.find(canonical_path);
    if (cached != commonjs_modules_.end()) {
      return cached->second.Get(isolate_)
          ->Get(context, v8::String::NewFromUtf8Literal(isolate_, "exports"))
          .ToLocal(output);
    }
    if (commonjs_modules_.size() >= kMaximumModules) {
      *error = "CommonJS module cache capacity exceeded";
      return false;
    }

    std::string source_text;
    if (!ReadFile(path, &source_text)) {
      *error = "cannot read CommonJS module: " + canonical_path;
      return false;
    }
    if (module_source_bytes_ + source_text.size() > kMaximumModuleBytes) {
      *error = "module source cache byte limit exceeded";
      return false;
    }

    v8::Local<v8::Object> module = v8::Object::New(isolate_);
    if (path.extension() == L".json") {
      v8::Local<v8::String> json_source;
      v8::Local<v8::Value> parsed;
      if (!v8::String::NewFromUtf8(isolate_, source_text.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(source_text.size()))
               .ToLocal(&json_source) ||
          !v8::JSON::Parse(context, json_source).ToLocal(&parsed) ||
          !Set(context, module, "exports", parsed)) {
        *error = "invalid JSON module: " + canonical_path;
        return false;
      }
      commonjs_modules_.emplace(
          canonical_path, v8::Global<v8::Object>(isolate_, module));
      module_source_bytes_ += source_text.size();
      *output = parsed;
      return true;
    }

    v8::Local<v8::Object> exports = v8::Object::New(isolate_);
    if (!Set(context, module, "exports", exports) ||
        !Set(context, module, "filename",
             v8::String::NewFromUtf8(isolate_, canonical_path.data(),
                                     v8::NewStringType::kNormal,
                                     static_cast<int>(canonical_path.size()))
                 .ToLocalChecked()) ||
        !Set(context, module, "loaded", v8::False(isolate_))) {
      *error = "failed to initialize CommonJS module";
      return false;
    }
    commonjs_modules_.emplace(canonical_path,
                              v8::Global<v8::Object>(isolate_, module));
    module_source_bytes_ += source_text.size();

    const std::string wrapped =
        "(function(exports, require, module, __filename, __dirname) {\n" +
        source_text + "\n})";
    v8::Local<v8::String> source;
    v8::Local<v8::String> resource_name;
    if (!v8::String::NewFromUtf8(isolate_, wrapped.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(wrapped.size()))
             .ToLocal(&source) ||
        !v8::String::NewFromUtf8(isolate_, canonical_path.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(canonical_path.size()))
             .ToLocal(&resource_name)) {
      *error = "CommonJS source exceeds V8 string limits";
      commonjs_modules_.erase(canonical_path);
      return false;
    }

    v8::ScriptOrigin origin(resource_name, -1);
    v8::Local<v8::Script> script;
    v8::Local<v8::Value> wrapper_value;
    if (!v8::Script::Compile(context, source, &origin).ToLocal(&script) ||
        !script->Run(context).ToLocal(&wrapper_value) ||
        !wrapper_value->IsFunction()) {
      *error = "failed to compile CommonJS module: " + canonical_path;
      commonjs_modules_.erase(canonical_path);
      return false;
    }

    v8::Local<v8::Function> require;
    if (!CreateRequire(context, canonical_path, &require)) {
      *error = "failed to create require for " + canonical_path;
      commonjs_modules_.erase(canonical_path);
      return false;
    }
    const std::string directory = PathToUtf8(path.parent_path());
    v8::Local<v8::Value> arguments[] = {
        exports,
        require,
        module,
        resource_name,
        v8::String::NewFromUtf8(isolate_, directory.data(),
                                v8::NewStringType::kNormal,
                                static_cast<int>(directory.size()))
            .ToLocalChecked(),
    };
    v8::Local<v8::Value> ignored;
    if (!wrapper_value.As<v8::Function>()
             ->Call(context, exports, 5, arguments)
             .ToLocal(&ignored) ||
        !Set(context, module, "loaded", v8::True(isolate_)) ||
        !module
             ->Get(context,
                   v8::String::NewFromUtf8Literal(isolate_, "exports"))
             .ToLocal(output)) {
      *error = "failed to execute CommonJS module: " + canonical_path;
      commonjs_modules_.erase(canonical_path);
      return false;
    }
    return true;
  }

  bool CreateRequire(v8::Local<v8::Context> context,
                     const std::string& referrer,
                     v8::Local<v8::Function>* output) {
    v8::Local<v8::Array> data = v8::Array::New(isolate_, 2);
    v8::Local<v8::External> runtime = v8::External::New(
        isolate_, this, v8::kExternalPointerTypeTagDefault);
    v8::Local<v8::String> filename;
    v8::Local<v8::Function> require;
    v8::Local<v8::Function> resolve;
    if (!v8::String::NewFromUtf8(isolate_, referrer.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(referrer.size()))
             .ToLocal(&filename) ||
        !data->Set(context, 0, runtime).FromMaybe(false) ||
        !data->Set(context, 1, filename).FromMaybe(false) ||
        !v8::Function::New(context, Require, data).ToLocal(&require) ||
        !v8::Function::New(context, RequireResolve, data).ToLocal(&resolve) ||
        !Set(context, require, "resolve", resolve)) {
      return false;
    }
    *output = require;
    return true;
  }

  static void Require(const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (info.Length() == 0 || !info[0]->IsString() || !info.Data()->IsArray()) {
      isolate->ThrowException(v8::Exception::TypeError(
          v8::String::NewFromUtf8Literal(isolate, "require needs a module specifier")));
      return;
    }
    v8::Local<v8::Array> data = info.Data().As<v8::Array>();
    v8::Local<v8::Value> runtime_value;
    v8::Local<v8::Value> referrer_value;
    if (!data->Get(context, 0).ToLocal(&runtime_value) ||
        !data->Get(context, 1).ToLocal(&referrer_value) ||
        !runtime_value->IsExternal()) {
      return;
    }
    Runtime* runtime = static_cast<Runtime*>(
        runtime_value.As<v8::External>()->Value(
            v8::kExternalPointerTypeTagDefault));
    const std::string request = ToUtf8(isolate, info[0]);
    const std::string referrer = ToUtf8(isolate, referrer_value);
    v8::Local<v8::Value> builtin;
    if (request.starts_with("node:") &&
        runtime->LoadBuiltin(context, request, &builtin)) {
      info.GetReturnValue().Set(builtin);
      return;
    }
    std::filesystem::path resolved;
    std::string error;
    if (!runtime->ResolveCommonJs(context, request, referrer, &resolved, &error)) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(error.size()))
              .ToLocalChecked()));
      return;
    }
    v8::Local<v8::Value> exports;
    if (!runtime->LoadCommonJs(context, resolved, &exports, &error)) {
      if (!isolate->HasPendingException()) {
        isolate->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8(isolate, error.data(),
                                    v8::NewStringType::kNormal,
                                    static_cast<int>(error.size()))
                .ToLocalChecked()));
      }
      return;
    }
    info.GetReturnValue().Set(exports);
  }

  static void RequireResolve(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (info.Length() == 0 || !info[0]->IsString() || !info.Data()->IsArray()) {
      isolate->ThrowException(v8::Exception::TypeError(
          v8::String::NewFromUtf8Literal(
              isolate, "require.resolve needs a module specifier")));
      return;
    }
    v8::Local<v8::Array> data = info.Data().As<v8::Array>();
    v8::Local<v8::Value> runtime_value;
    v8::Local<v8::Value> referrer_value;
    if (!data->Get(context, 0).ToLocal(&runtime_value) ||
        !data->Get(context, 1).ToLocal(&referrer_value) ||
        !runtime_value->IsExternal()) {
      return;
    }
    Runtime* runtime = static_cast<Runtime*>(
        runtime_value.As<v8::External>()->Value(
            v8::kExternalPointerTypeTagDefault));
    const std::string request = ToUtf8(isolate, info[0]);
    if (request.starts_with("node:")) {
      v8::Local<v8::Value> builtin;
      if (runtime->LoadBuiltin(context, request, &builtin)) {
        info.GetReturnValue().Set(info[0]);
        return;
      }
    }
    std::filesystem::path resolved;
    std::string error;
    if (!runtime->ResolveCommonJs(context, request,
                                  ToUtf8(isolate, referrer_value), &resolved,
                                  &error)) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(error.size()))
              .ToLocalChecked()));
      return;
    }
    const std::string resolved_text = PathToUtf8(resolved);
    info.GetReturnValue().Set(
        v8::String::NewFromUtf8(isolate, resolved_text.data(),
                                v8::NewStringType::kNormal,
                                static_cast<int>(resolved_text.size()))
            .ToLocalChecked());
  }

  static void Assert(const v8::FunctionCallbackInfo<v8::Value>& info) {
    if (info.Length() != 0 && info[0]->BooleanValue(info.GetIsolate())) return;
    ThrowAssertion(info, 1);
  }

  static void AssertStrictEqual(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    if (info.Length() >= 2 && info[0]->StrictEquals(info[1])) return;
    ThrowAssertion(info, 2);
  }

  static void ThrowAssertion(
      const v8::FunctionCallbackInfo<v8::Value>& info, int message_index) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::String> message =
        v8::String::NewFromUtf8Literal(isolate, "Assertion failed");
    if (info.Length() > message_index && info[message_index]->IsString()) {
      message = info[message_index].As<v8::String>();
    }
    isolate->ThrowException(v8::Exception::Error(message));
  }

  bool LoadBuiltin(v8::Local<v8::Context> context,
                   const std::string& request,
                   v8::Local<v8::Value>* output) {
    if (request == "node:assert") {
      v8::Local<v8::Function> assert;
      v8::Local<v8::Function> strict_equal;
      if (!v8::Function::New(context, Assert).ToLocal(&assert) ||
          !v8::Function::New(context, AssertStrictEqual)
               .ToLocal(&strict_equal) ||
          !Set(context, assert, "ok", assert) ||
          !Set(context, assert, "strictEqual", strict_equal)) {
        return false;
      }
      *output = assert;
      return true;
    }

    const char* global_name = nullptr;
    if (request == "node:console") global_name = "console";
    if (request == "node:process") global_name = "process";
    if (global_name != nullptr) {
      return context->Global()
          ->Get(context,
                v8::String::NewFromUtf8(isolate_, global_name,
                                        v8::NewStringType::kNormal)
                    .ToLocalChecked())
          .ToLocal(output);
    }

    if (request == "node:timers") {
      v8::Local<v8::Object> timers = v8::Object::New(isolate_);
      constexpr const char* names[] = {"setTimeout", "setInterval",
                                       "clearTimeout", "clearInterval"};
      for (const char* name : names) {
        v8::Local<v8::Value> function;
        if (!context->Global()
                 ->Get(context,
                       v8::String::NewFromUtf8(isolate_, name,
                                               v8::NewStringType::kNormal)
                           .ToLocalChecked())
                 .ToLocal(&function) ||
            !Set(context, timers, name, function)) {
          return false;
        }
      }
      *output = timers;
      return true;
    }
    return false;
  }

  bool ResolveCommonJs(v8::Local<v8::Context> context,
                       const std::string& request,
                       const std::string& referrer,
                       std::filesystem::path* output, std::string* error) {
    if (request.starts_with("node:")) {
      *error = "unsupported built-in module: " + request;
      return false;
    }
    const std::wstring request_wide = Utf8ToWide(request);
    const std::filesystem::path referrer_path(Utf8ToWide(referrer));
    if (request_wide.empty()) {
      *error = "require specifier is not valid UTF-8";
      return false;
    }

    std::filesystem::path candidate(request_wide);
    if (candidate.is_absolute() || request.starts_with("./") ||
        request.starts_with("../") || request.starts_with(".\\") ||
        request.starts_with("..\\")) {
      if (!candidate.is_absolute()) {
        candidate = referrer_path.parent_path() / candidate;
      }
      if (ResolveCommonJsCandidate(context, candidate, output)) return true;
    } else {
      std::string package_name;
      std::string package_subpath;
      if (request.starts_with('@')) {
        const size_t first = request.find('/');
        const size_t second = first == std::string::npos
                                  ? std::string::npos
                                  : request.find('/', first + 1);
        package_name = second == std::string::npos ? request : request.substr(0, second);
        package_subpath = second == std::string::npos ? "" : request.substr(second + 1);
      } else {
        const size_t slash = request.find('/');
        package_name = request.substr(0, slash);
        package_subpath = slash == std::string::npos ? "" : request.substr(slash + 1);
      }
      std::filesystem::path directory = referrer_path.parent_path();
      while (!directory.empty()) {
        std::filesystem::path package_root =
            directory / L"node_modules" / Utf8ToWide(package_name);
        candidate = package_subpath.empty()
                        ? package_root
                        : package_root / Utf8ToWide(package_subpath);
        if (ResolveCommonJsCandidate(context, candidate, output)) return true;
        const auto parent = directory.parent_path();
        if (parent == directory) break;
        directory = parent;
      }
    }
    *error = "module not found: " + request + " required from " + referrer;
    return false;
  }

  bool ResolveCommonJsCandidate(v8::Local<v8::Context> context,
                                const std::filesystem::path& candidate,
                                std::filesystem::path* output) {
    std::vector<std::filesystem::path> files = {
        candidate,
        candidate.native() + std::wstring(L".js"),
        candidate.native() + std::wstring(L".cjs"),
        candidate.native() + std::wstring(L".json"),
    };
    for (const auto& file : files) {
      std::error_code status_error;
      if (std::filesystem::is_regular_file(file, status_error)) {
        std::error_code canonical_error;
        *output = std::filesystem::weakly_canonical(file, canonical_error);
        if (!canonical_error) return true;
      }
    }

    std::error_code directory_error;
    if (!std::filesystem::is_directory(candidate, directory_error)) return false;
    const std::filesystem::path manifest_path = candidate / L"package.json";
    std::string manifest_source;
    if (ReadFile(manifest_path, &manifest_source)) {
      v8::Local<v8::String> json;
      v8::Local<v8::Value> value;
      if (v8::String::NewFromUtf8(isolate_, manifest_source.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(manifest_source.size()))
              .ToLocal(&json) &&
          v8::JSON::Parse(context, json).ToLocal(&value) && value->IsObject()) {
        v8::Local<v8::Value> main;
        if (value.As<v8::Object>()
                ->Get(context,
                      v8::String::NewFromUtf8Literal(isolate_, "main"))
                .ToLocal(&main) &&
            main->IsString()) {
          const std::filesystem::path main_path = candidate / Utf8ToWide(ToUtf8(isolate_, main));
          const std::vector<std::filesystem::path> main_files = {
              main_path,
              main_path.native() + std::wstring(L".js"),
              main_path.native() + std::wstring(L".cjs"),
          };
          for (const auto& file : main_files) {
            std::error_code status_error;
            if (std::filesystem::is_regular_file(file, status_error)) {
              std::error_code canonical_error;
              *output = std::filesystem::weakly_canonical(file, canonical_error);
              if (!canonical_error) return true;
            }
          }
        }
      }
    }
    return ResolveCommonJsCandidate(context, candidate / L"index", output);
  }

  bool InstallProcess(v8::Local<v8::Context> context,
                      const uint8_t* const* argument_bytes,
                      const size_t* argument_lengths, size_t argument_count) {
    if (argument_count > static_cast<size_t>(std::numeric_limits<int>::max())) {
      return false;
    }
    v8::Local<v8::Array> arguments =
        v8::Array::New(isolate_, static_cast<int>(argument_count));
    for (size_t index = 0; index < argument_count; ++index) {
      if (argument_lengths[index] >
          static_cast<size_t>(std::numeric_limits<int>::max())) {
        return false;
      }
      v8::Local<v8::String> argument;
      if (!v8::String::NewFromUtf8(
               isolate_, reinterpret_cast<const char*>(argument_bytes[index]),
               v8::NewStringType::kNormal,
               static_cast<int>(argument_lengths[index]))
               .ToLocal(&argument) ||
          arguments->Set(context, static_cast<uint32_t>(index), argument)
              .FromMaybe(false) == false) {
        return false;
      }
    }

    v8::Local<v8::Object> process = v8::Object::New(isolate_);
    v8::Local<v8::Object> versions = v8::Object::New(isolate_);
    return Set(context, process, "argv", arguments) &&
           Set(context, process, "platform",
               v8::String::NewFromUtf8Literal(isolate_, "win32")) &&
           Set(context, process, "arch",
               v8::String::NewFromUtf8Literal(isolate_, "x64")) &&
           Set(context, versions, "sako",
               v8::String::NewFromUtf8Literal(isolate_, "0.1.0")) &&
           Set(context, versions, "v8",
               v8::String::NewFromUtf8(isolate_, v8::V8::GetVersion())
                   .ToLocalChecked()) &&
           Set(context, process, "versions", versions) &&
           Set(context, context->Global(), "process", process) &&
           Set(context, context->Global(), "global", context->Global());
  }

  bool InstallFunction(v8::Local<v8::Context> context, const char* name,
                       v8::FunctionCallback callback,
                       v8::Local<v8::Value> data) {
    v8::Local<v8::Function> function;
    return v8::Function::New(context, callback, data).ToLocal(&function) &&
           Set(context, context->Global(), name, function);
  }

  bool Set(v8::Local<v8::Context> context, v8::Local<v8::Object> object,
           const char* name, v8::Local<v8::Value> value) {
    v8::Local<v8::String> key;
    return v8::String::NewFromUtf8(isolate_, name).ToLocal(&key) &&
           object->Set(context, key, value).FromMaybe(false);
  }

  static Runtime* FromCallback(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    if (!info.Data()->IsExternal()) return nullptr;
    return static_cast<Runtime*>(info.Data().As<v8::External>()->Value(
        v8::kExternalPointerTypeTagDefault));
  }

  static void SetTimeout(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime != nullptr) runtime->ScheduleTimer(info, false);
  }

  static void SetInterval(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime != nullptr) runtime->ScheduleTimer(info, true);
  }

  static void ClearTimer(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr || info.Length() == 0) return;
    v8::Local<v8::Context> context = info.GetIsolate()->GetCurrentContext();
    const uint64_t id = info[0]->IntegerValue(context).FromMaybe(0);
    auto timer = runtime->timers_.find(id);
    if (timer != runtime->timers_.end()) {
      timer->second.Reset();
      runtime->timers_.erase(timer);
    }
  }

  static void QueueMicrotask(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Isolate* isolate = info.GetIsolate();
    if (info.Length() == 0 || !info[0]->IsFunction()) {
      isolate->ThrowException(v8::Exception::TypeError(
          v8::String::NewFromUtf8Literal(isolate, "callback must be a function")));
      return;
    }
    isolate->EnqueueMicrotask(info[0].As<v8::Function>());
  }

  void ScheduleTimer(const v8::FunctionCallbackInfo<v8::Value>& info,
                     bool repeat) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (info.Length() == 0 || !info[0]->IsFunction()) {
      isolate->ThrowException(v8::Exception::TypeError(
          v8::String::NewFromUtf8Literal(isolate, "callback must be a function")));
      return;
    }
    if (timers_.size() >= kMaximumTimers) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate, "timer capacity exceeded")));
      return;
    }

    double delay = info.Length() > 1 ? info[1]->NumberValue(context).FromMaybe(0) : 0;
    if (!std::isfinite(delay) || delay < 0) delay = 0;
    delay = std::min(delay, static_cast<double>(std::numeric_limits<int32_t>::max()));
    const uint64_t milliseconds = static_cast<uint64_t>(delay);
    uint64_t id = next_timer_id_++;
    if (id == 0) id = next_timer_id_++;

    Timer timer;
    timer.id = id;
    timer.deadline = std::chrono::steady_clock::now() +
                     std::chrono::milliseconds(milliseconds);
    timer.interval_milliseconds = repeat ? std::max<uint64_t>(milliseconds, 1) : 0;
    timer.callback.Reset(isolate, info[0].As<v8::Function>());
    for (int index = 2; index < info.Length(); ++index) {
      timer.arguments.emplace_back(isolate, info[index]);
    }
    timers_.emplace(id, std::move(timer));
    info.GetReturnValue().Set(v8::Number::New(isolate, static_cast<double>(id)));
  }

  bool DrainEventLoop(v8::Local<v8::Context> context, std::string* error) {
    while (true) {
      while (v8::platform::PumpMessageLoop(platform_.get(), isolate_)) {
        isolate_->PerformMicrotaskCheckpoint();
      }
      if (timers_.empty()) return true;

      auto next = std::min_element(
          timers_.begin(), timers_.end(),
          [](const auto& left, const auto& right) {
            return left.second.deadline < right.second.deadline;
          });
      const auto now = std::chrono::steady_clock::now();
      if (next->second.deadline > now) {
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            next->second.deadline - now + std::chrono::milliseconds(1));
        Sleep(static_cast<DWORD>(std::min<int64_t>(remaining.count(), MAXDWORD)));
      }

      const auto ready_at = std::chrono::steady_clock::now();
      std::vector<uint64_t> ready;
      ready.reserve(timers_.size());
      for (const auto& [id, timer] : timers_) {
        if (timer.deadline <= ready_at) ready.push_back(id);
      }
      std::sort(ready.begin(), ready.end());

      for (uint64_t id : ready) {
        auto current = timers_.find(id);
        if (current == timers_.end()) continue;
        v8::HandleScope callback_scope(isolate_);
        v8::Local<v8::Function> callback = current->second.callback.Get(isolate_);
        std::vector<v8::Local<v8::Value>> arguments;
        arguments.reserve(current->second.arguments.size());
        for (const auto& argument : current->second.arguments) {
          arguments.push_back(argument.Get(isolate_));
        }

        if (current->second.interval_milliseconds == 0) {
          current->second.Reset();
          timers_.erase(current);
        } else {
          current->second.deadline =
              ready_at + std::chrono::milliseconds(current->second.interval_milliseconds);
        }

        v8::TryCatch try_catch(isolate_);
        v8::Local<v8::Value> result;
        if (!callback
                 ->Call(context, v8::Undefined(isolate_),
                        static_cast<int>(arguments.size()), arguments.data())
                 .ToLocal(&result)) {
          *error = FormatException(isolate_, context, try_catch);
          return false;
        }
        isolate_->PerformMicrotaskCheckpoint();
      }
    }
  }

  std::unique_ptr<v8::Platform> platform_;
  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator_;
  v8::Isolate* isolate_ = nullptr;
  v8::Global<v8::Context> context_;
  std::unordered_map<uint64_t, Timer> timers_;
  std::unordered_map<std::string, v8::Global<v8::Module>> modules_;
  std::unordered_map<std::string, v8::Global<v8::Object>> commonjs_modules_;
  size_t module_source_bytes_ = 0;
  uint64_t next_timer_id_ = 1;
  bool v8_initialized_ = false;
};

}  // namespace

extern "C" void* sako_v8_runtime_new(const char* executable_path,
                                      const char* icu_data_path, char* error,
                                      size_t error_capacity) {
  if (executable_path == nullptr || icu_data_path == nullptr) {
    WriteError(error, error_capacity, "invalid V8 initialization paths");
    return nullptr;
  }
  std::string message;
  std::unique_ptr<Runtime> runtime =
      Runtime::Create(executable_path, icu_data_path, &message);
  if (!runtime) {
    WriteError(error, error_capacity, message);
    return nullptr;
  }
  return runtime.release();
}

extern "C" int sako_v8_runtime_execute(
    void* runtime, const uint8_t* source, size_t source_length,
    const uint8_t* resource_name, size_t resource_name_length,
    const uint8_t* const* argument_bytes, const size_t* argument_lengths,
    size_t argument_count, char* error, size_t error_capacity) {
  if (runtime == nullptr || source == nullptr || resource_name == nullptr) {
    WriteError(error, error_capacity, "invalid script execution input");
    return 1;
  }
  if (argument_count != 0 &&
      (argument_bytes == nullptr || argument_lengths == nullptr)) {
    WriteError(error, error_capacity, "invalid script argument input");
    return 1;
  }
  std::string message;
  const bool success = static_cast<Runtime*>(runtime)->Execute(
      source, source_length, resource_name, resource_name_length, argument_bytes,
      argument_lengths, argument_count, &message);
  if (!success) WriteError(error, error_capacity, message);
  return success ? 0 : 1;
}

extern "C" int sako_v8_runtime_memory_stats(
    void* runtime, uint64_t* heap_used, uint64_t* heap_committed,
    uint64_t* heap_limit, uint64_t* persistent_handles, uint64_t* timers) {
  if (runtime == nullptr || heap_used == nullptr || heap_committed == nullptr ||
      heap_limit == nullptr || persistent_handles == nullptr || timers == nullptr) {
    return 1;
  }
  static_cast<Runtime*>(runtime)->MemoryStats(
      heap_used, heap_committed, heap_limit, persistent_handles, timers);
  return 0;
}

extern "C" int sako_v8_runtime_execute_module(
    void* runtime, const uint8_t* path, size_t path_length,
    const uint8_t* const* argument_bytes, const size_t* argument_lengths,
    size_t argument_count, char* error, size_t error_capacity) {
  if (runtime == nullptr || path == nullptr ||
      (argument_count != 0 &&
       (argument_bytes == nullptr || argument_lengths == nullptr))) {
    WriteError(error, error_capacity, "invalid module execution input");
    return 1;
  }
  std::string message;
  const bool success = static_cast<Runtime*>(runtime)->ExecuteModule(
      path, path_length, argument_bytes, argument_lengths, argument_count,
      &message);
  if (!success) WriteError(error, error_capacity, message);
  return success ? 0 : 1;
}

extern "C" int sako_v8_runtime_execute_commonjs(
    void* runtime, const uint8_t* path, size_t path_length,
    const uint8_t* const* argument_bytes, const size_t* argument_lengths,
    size_t argument_count, char* error, size_t error_capacity) {
  if (runtime == nullptr || path == nullptr ||
      (argument_count != 0 &&
       (argument_bytes == nullptr || argument_lengths == nullptr))) {
    WriteError(error, error_capacity, "invalid CommonJS execution input");
    return 1;
  }
  std::string message;
  const bool success = static_cast<Runtime*>(runtime)->ExecuteCommonJs(
      path, path_length, argument_bytes, argument_lengths, argument_count,
      &message);
  if (!success) WriteError(error, error_capacity, message);
  return success ? 0 : 1;
}

extern "C" void sako_v8_runtime_delete(void* runtime) {
  delete static_cast<Runtime*>(runtime);
}
