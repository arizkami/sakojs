// SPDX-License-Identifier: BSD-3-Clause

#include <cstring>
#include <memory>
#include <string>

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
    return runtime;
  }

  ~Runtime() {
    if (isolate_ != nullptr) {
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
               std::string* error) {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HandleScope handle_scope(isolate_);

    v8::Local<v8::Context> context = v8::Context::New(isolate_);
    v8::Context::Scope context_scope(context);
    v8::TryCatch try_catch(isolate_);

    v8::Local<v8::Object> console = v8::Object::New(isolate_);
    v8::Local<v8::Function> log;
    if (!v8::Function::New(context, ConsoleLog).ToLocal(&log) ||
        console->Set(context, v8::String::NewFromUtf8Literal(isolate_, "log"), log)
                .IsNothing() ||
        context->Global()
            ->Set(context, v8::String::NewFromUtf8Literal(isolate_, "console"),
                  console)
            .IsNothing()) {
      *error = "failed to install console.log";
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
    return true;
  }

 private:
  Runtime() = default;

  std::unique_ptr<v8::Platform> platform_;
  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator_;
  v8::Isolate* isolate_ = nullptr;
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
    const uint8_t* resource_name, size_t resource_name_length, char* error,
    size_t error_capacity) {
  if (runtime == nullptr || source == nullptr || resource_name == nullptr) {
    WriteError(error, error_capacity, "invalid script execution input");
    return 1;
  }
  std::string message;
  const bool success = static_cast<Runtime*>(runtime)->Execute(
      source, source_length, resource_name, resource_name_length, &message);
  if (!success) WriteError(error, error_capacity, message);
  return success ? 0 : 1;
}

extern "C" void sako_v8_runtime_delete(void* runtime) {
  delete static_cast<Runtime*>(runtime);
}
