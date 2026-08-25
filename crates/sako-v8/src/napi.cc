// SPDX-License-Identifier: BSD-3-Clause

//! Node-API for Sako.
//!
//! Node-API is a C ABI, not a V8 one: an addon is a shared library that
//! resolves `napi_*` symbols from whatever process loaded it and never sees a
//! V8 header. That is what makes it implementable outside Node -- everything
//! here is a translation from the C surface onto this runtime's isolate.
//!
//! The pieces that are not simply "call the matching V8 method":
//!
//!   * `napi_value` is a `v8::Local` reinterpreted as an opaque pointer, the
//!     same representation Node uses. It is only valid inside the handle scope
//!     that produced it, which is why the scope calls below are real.
//!   * Finalizers run from a queue rather than from the garbage collector.
//!     A weak callback may not touch the heap, and addon finalizers routinely
//!     do, so collection only schedules them and the event loop runs them.
//!   * Async work and threadsafe functions need a loop that can be woken from
//!     another thread. `RunTasks`/`WaitForWork` are that seam; the runtime
//!     calls them from its own loop instead of this file owning one.
//!
//! Symbols are exported from the executable (`NAPI_EXTERN` expands to
//! `__declspec(dllexport)` on Windows). Addons built with napi-rs look their
//! entry points up with `GetProcAddress` against the host process rather than
//! importing them from `node.exe`, which is what lets a differently named host
//! load them at all.

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <functional>
#include <iterator>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#else
#include <dlfcn.h>
#endif

#include "v8.h"

#include "node/js_native_api.h"
#include "node/node_api.h"

#include "sako_napi.h"

namespace sako_napi {
struct IsolateState;
struct CallbackBundle;
}  // namespace sako_napi

// ---------------------------------------------------------------------------
// The opaque handle types the ABI hands back to addons.
// ---------------------------------------------------------------------------

struct napi_env__ {
  napi_env__(v8::Isolate* isolate_, v8::Local<v8::Context> context_,
             sako_napi::IsolateState* state_, std::string filename_)
      : isolate(isolate_),
        context(isolate_, context_),
        state(state_),
        filename(std::move(filename_)) {}

  v8::Local<v8::Context> Context() const { return context.Get(isolate); }

  v8::Isolate* isolate;
  v8::Global<v8::Context> context;
  sako_napi::IsolateState* state;
  std::string filename;
  napi_extended_error_info last_error{"", nullptr, 0, napi_ok};
  // Set when a V8 call throws. Every entry point refuses to run while it is
  // occupied, which is what makes an addon's "check the status, then bail"
  // error handling behave the way it does under Node.
  v8::Global<v8::Value> last_exception;
  int open_handle_scopes = 0;
  void* instance_data = nullptr;
  napi_finalize instance_data_finalizer = nullptr;
  void* instance_data_hint = nullptr;
  std::vector<std::pair<napi_cleanup_hook, void*>> cleanup_hooks;
  // Handed out by napi_get_uv_event_loop. See that function for why it is a
  // placeholder rather than a real loop.
  void* fake_uv_loop = nullptr;
};

struct napi_ref__ {
  napi_env env = nullptr;
  v8::Global<v8::Value> persistent;
  uint32_t refcount = 0;
  void* data = nullptr;
  napi_finalize finalize_cb = nullptr;
  void* finalize_hint = nullptr;
  // True when the runtime owns the reference rather than the addon: wraps,
  // added finalizers, and the data attached to a created function all get one
  // the addon never receives a handle to, so it is deleted once its finalizer
  // has run.
  bool self_owned = false;
  bool finalized = false;
};

struct napi_deferred__ {
  napi_deferred__(v8::Isolate* isolate, v8::Local<v8::Promise::Resolver> value)
      : resolver(isolate, value) {}
  v8::Global<v8::Promise::Resolver> resolver;
};

struct napi_handle_scope__ {
  explicit napi_handle_scope__(v8::Isolate* isolate) : scope(isolate) {}
  v8::HandleScope scope;
};

struct napi_escapable_handle_scope__ {
  explicit napi_escapable_handle_scope__(v8::Isolate* isolate) : scope(isolate) {}
  v8::EscapableHandleScope scope;
  bool escaped = false;
};

struct napi_callback_info__ {
  const v8::FunctionCallbackInfo<v8::Value>* info;
  void* data;
};

struct napi_async_context__ {
  v8::Global<v8::Value> resource;
};

struct napi_callback_scope__ {
  int unused = 0;
};

struct napi_async_cleanup_hook_handle__ {
  napi_env env = nullptr;
  napi_async_cleanup_hook hook = nullptr;
  void* data = nullptr;
};

struct napi_async_work__ {
  napi_env env = nullptr;
  napi_async_execute_callback execute = nullptr;
  napi_async_complete_callback complete = nullptr;
  void* data = nullptr;
  v8::Global<v8::Value> resource;
  std::atomic<bool> started{false};
  std::atomic<bool> cancelled{false};
  std::atomic<bool> queued{false};
};

struct napi_threadsafe_function__ {
  napi_env env = nullptr;
  v8::Global<v8::Function> function;
  void* context = nullptr;
  napi_threadsafe_function_call_js call_js = nullptr;
  size_t max_queue_size = 0;
  size_t thread_count = 0;
  bool referenced = true;
  bool closing = false;
  bool aborted = false;
  bool finished = false;
  void* finalize_data = nullptr;
  napi_finalize finalize_cb = nullptr;
  void* finalize_hint = nullptr;
  std::deque<void*> queue;
  // Signalled when the queue drains, for callers that asked to block rather
  // than be told the queue is full.
  std::condition_variable room;
};

// ---------------------------------------------------------------------------
// Internals.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

constexpr v8::ExternalPointerTypeTag kExternalTag =
    v8::kExternalPointerTypeTagDefault;

/// Node reports version 9; claiming less would make addons compiled against a
/// newer header refuse to load, and claiming more would promise functions that
/// are not here.
constexpr uint32_t kNapiVersion = 9;

/// Async work runs on this many threads at most. Node's default libuv pool is
/// the same size, and addons are written expecting roughly that much
/// parallelism from `napi_queue_async_work`.
constexpr size_t kMaximumWorkerThreads = 4;

/// A `napi_value` is a `v8::Local` reinterpreted, which is the representation
/// Node settled on and the one addons are compiled against. The round trip is
/// a memcpy in both directions so it stays correct whether a Local holds a
/// handle slot or the value itself.
template <typename T>
inline napi_value JsValue(v8::Local<T> handle) {
  static_assert(sizeof(napi_value) == sizeof(v8::Local<v8::Value>),
                "napi_value must be a v8::Local in disguise");
  v8::Local<v8::Value> local = handle;
  napi_value value = nullptr;
  std::memcpy(&value, static_cast<void*>(&local), sizeof(value));
  return value;
}

inline v8::Local<v8::Value> V8Value(napi_value value) {
  v8::Local<v8::Value> local;
  std::memcpy(static_cast<void*>(&local), &value, sizeof(value));
  return local;
}

/// Runs an addon callback and turns whatever it did with the C API back into
/// something the runtime understands.
struct FinalizerCall {
  napi_finalize callback;
  napi_env env;
  void* data;
  void* hint;
  napi_ref__* owner;  // deleted after the call when it owns itself
};

/// Callback data for a function created through the C API. One is allocated
/// per `napi_create_function`/`napi_define_class` property and freed by a
/// finalizer attached to the function itself.
struct CallbackBundle {
  napi_env env;
  napi_callback callback;
  void* data;
};

class ThreadPool {
 public:
  void Submit(std::function<void()> job) {
    std::unique_lock<std::mutex> lock(mutex_);
    if (stopping_) return;
    jobs_.push_back(std::move(job));
    // Grow only while there is nothing idle to pick the job up, so a
    // single-threaded workload never pays for four threads.
    if (idle_ == 0 && threads_.size() < kMaximumWorkerThreads) {
      threads_.emplace_back([this] { Work(); });
    }
    signal_.notify_one();
  }

  void Stop() {
    {
      std::unique_lock<std::mutex> lock(mutex_);
      if (stopping_) return;
      stopping_ = true;
    }
    signal_.notify_all();
    for (std::thread& thread : threads_) {
      if (thread.joinable()) thread.join();
    }
    threads_.clear();
  }

 private:
  void Work() {
    for (;;) {
      std::function<void()> job;
      {
        std::unique_lock<std::mutex> lock(mutex_);
        idle_ += 1;
        signal_.wait(lock, [this] { return stopping_ || !jobs_.empty(); });
        idle_ -= 1;
        if (stopping_ && jobs_.empty()) return;
        job = std::move(jobs_.front());
        jobs_.pop_front();
      }
      job();
    }
  }

  std::mutex mutex_;
  std::condition_variable signal_;
  std::deque<std::function<void()>> jobs_;
  std::vector<std::thread> threads_;
  size_t idle_ = 0;
  bool stopping_ = false;
};

}  // namespace

/// Everything one isolate's addons share.
///
/// Addons each get their own `napi_env`, as they do under Node, but the loop
/// they post work to and the threads that run it are per isolate.
struct IsolateState {
  explicit IsolateState(v8::Isolate* isolate_) : isolate(isolate_) {}

  v8::Isolate* isolate;
  std::vector<std::unique_ptr<napi_env__>> envs;
  std::vector<std::unique_ptr<napi_threadsafe_function__>> functions;

  // Guards everything below, and is the lock the loop waits on.
  std::mutex mutex;
  std::condition_variable signal;
  std::deque<std::function<void()>> tasks;
  std::vector<FinalizerCall> finalizers;
  /// Non-empty once an addon has reported something the runtime should treat
  /// as the execution's failure.
  std::string fatal_error;
  /// Work that must keep the event loop alive: queued async work, and
  /// threadsafe functions that have not been unreferenced.
  int handles = 0;

  ThreadPool pool;
  /// Handed to addons by napi_get_uv_event_loop. Never dereferenced here.
  uint64_t placeholder_loop[16] = {};

  void Post(std::function<void()> task) {
    {
      std::lock_guard<std::mutex> lock(mutex);
      tasks.push_back(std::move(task));
    }
    signal.notify_all();
  }

  void AddHandles(int delta) {
    {
      std::lock_guard<std::mutex> lock(mutex);
      handles += delta;
    }
    signal.notify_all();
  }

  bool Busy() {
    std::lock_guard<std::mutex> lock(mutex);
    return handles > 0 || !tasks.empty() || !finalizers.empty() ||
           !fatal_error.empty();
  }
};

namespace {

std::mutex& RegistryMutex() {
  static std::mutex mutex;
  return mutex;
}

std::unordered_map<v8::Isolate*, std::unique_ptr<IsolateState>>& Registry() {
  static std::unordered_map<v8::Isolate*, std::unique_ptr<IsolateState>> map;
  return map;
}

IsolateState* StateFor(v8::Isolate* isolate, bool create) {
  std::lock_guard<std::mutex> lock(RegistryMutex());
  auto& map = Registry();
  auto found = map.find(isolate);
  if (found != map.end()) return found->second.get();
  if (!create) return nullptr;
  auto state = std::make_unique<IsolateState>(isolate);
  IsolateState* raw = state.get();
  map.emplace(isolate, std::move(state));
  return raw;
}

// --- status plumbing -------------------------------------------------------

const char* const kErrorMessages[] = {
    "",
    "Invalid argument",
    "An object was expected",
    "A string was expected",
    "A string or symbol was expected",
    "A function was expected",
    "A number was expected",
    "A boolean was expected",
    "An array was expected",
    "Unknown failure",
    "An exception is pending",
    "The async work item was cancelled",
    "napi_escape_handle already called on scope",
    "Invalid handle scope usage",
    "Invalid callback scope usage",
    "Thread-safe function queue is full",
    "Thread-safe function handle is closing",
    "A bigint was expected",
    "A date was expected",
    "An arraybuffer was expected",
    "A detachable arraybuffer was expected",
    "Main thread would deadlock",
    "External buffers are not allowed",
    "Cannot run JavaScript",
};

napi_status SetLastError(napi_env env, napi_status status,
                         uint32_t engine_code = 0,
                         void* engine_reserved = nullptr) {
  if (env == nullptr) return status;
  const size_t index = static_cast<size_t>(status);
  env->last_error.error_code = status;
  env->last_error.engine_error_code = engine_code;
  env->last_error.engine_reserved = engine_reserved;
  env->last_error.error_message =
      index < std::size(kErrorMessages) ? kErrorMessages[index] : "Unknown failure";
  return status;
}

void ClearLastError(napi_env env) {
  if (env == nullptr) return;
  env->last_error.error_code = napi_ok;
  env->last_error.engine_error_code = 0;
  env->last_error.engine_reserved = nullptr;
  env->last_error.error_message = "";
}

/// Mirrors the try/catch Node wraps every entry point in: an exception raised
/// by a V8 call is parked on the environment so the addon sees
/// `napi_pending_exception` from this call and every later one until it is
/// collected with `napi_get_and_clear_last_exception`.
class ScopedTryCatch {
 public:
  explicit ScopedTryCatch(napi_env env) : env_(env), try_catch_(env->isolate) {}

  ~ScopedTryCatch() {
    if (try_catch_.HasCaught()) {
      env_->last_exception.Reset(env_->isolate, try_catch_.Exception());
    }
  }

  bool HasCaught() const { return try_catch_.HasCaught(); }

 private:
  napi_env env_;
  v8::TryCatch try_catch_;
};

#define CHECK_ENV(env)                                                         \
  do {                                                                         \
    if ((env) == nullptr) return napi_invalid_arg;                             \
  } while (0)

#define RETURN_STATUS_IF_FALSE(env, condition, status)                         \
  do {                                                                         \
    if (!(condition)) return SetLastError((env), (status));                    \
  } while (0)

#define CHECK_ARG(env, argument)                                               \
  RETURN_STATUS_IF_FALSE((env), (argument) != nullptr, napi_invalid_arg)

/// For entry points that do not enter JavaScript.
#define NAPI_BASIC(env)                                                        \
  CHECK_ENV(env);                                                              \
  ClearLastError(env)

/// For entry points that do. Refuses to run while an exception is parked, the
/// way Node does, so an addon that ignores a failed status cannot pile a
/// second failure on top of the first.
#define NAPI_PREAMBLE(env)                                                     \
  CHECK_ENV(env);                                                              \
  RETURN_STATUS_IF_FALSE((env), (env)->last_exception.IsEmpty(),               \
                         napi_pending_exception);                              \
  ClearLastError(env);                                                         \
  ScopedTryCatch sako_try_catch(env)

#define GET_RETURN_STATUS(env)                                                 \
  (!sako_try_catch.HasCaught() ? napi_ok                                       \
                               : SetLastError((env), napi_pending_exception))

// --- strings ---------------------------------------------------------------

v8::MaybeLocal<v8::String> Utf8String(v8::Isolate* isolate, const char* text,
                                      size_t length) {
  const int size = length == NAPI_AUTO_LENGTH
                       ? -1
                       : static_cast<int>(std::min<size_t>(
                             length, static_cast<size_t>(v8::String::kMaxLength)));
  return v8::String::NewFromUtf8(isolate, text, v8::NewStringType::kNormal, size);
}

// --- references and finalizers ---------------------------------------------

void SecondPassWeakCallback(const v8::WeakCallbackInfo<napi_ref__>& info);

void FirstPassWeakCallback(const v8::WeakCallbackInfo<napi_ref__>& info) {
  napi_ref__* reference = info.GetParameter();
  reference->persistent.Reset();
  // The finalizer itself has to wait: a first-pass callback may not touch the
  // heap, and a second pass may not either, so all this does is get us to a
  // point where the reference can be queued.
  info.SetSecondPassCallback(SecondPassWeakCallback);
}

void SecondPassWeakCallback(const v8::WeakCallbackInfo<napi_ref__>& info) {
  napi_ref__* reference = info.GetParameter();
  napi_env env = reference->env;
  if (env == nullptr || env->state == nullptr) return;
  IsolateState* state = env->state;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    state->finalizers.push_back(FinalizerCall{reference->finalize_cb, env,
                                              reference->data,
                                              reference->finalize_hint,
                                              reference});
  }
  state->signal.notify_all();
}

void ApplyWeakness(napi_ref__* reference) {
  if (reference->refcount != 0 || reference->persistent.IsEmpty()) return;
  reference->persistent.SetWeak(reference, FirstPassWeakCallback,
                                v8::WeakCallbackType::kParameter);
}

napi_ref__* NewReference(napi_env env, v8::Local<v8::Value> value,
                         uint32_t refcount, void* data,
                         napi_finalize finalize_cb, void* finalize_hint,
                         bool self_owned) {
  auto* reference = new napi_ref__();
  reference->env = env;
  reference->persistent.Reset(env->isolate, value);
  reference->refcount = refcount;
  reference->data = data;
  reference->finalize_cb = finalize_cb;
  reference->finalize_hint = finalize_hint;
  reference->self_owned = self_owned;
  ApplyWeakness(reference);
  return reference;
}

// --- callbacks -------------------------------------------------------------

void DeleteBundle(napi_env, void* data, void*) {
  delete static_cast<CallbackBundle*>(data);
}

void FunctionCallbackWrapper(const v8::FunctionCallbackInfo<v8::Value>& info) {
  auto* bundle = static_cast<CallbackBundle*>(
      info.Data().As<v8::External>()->Value(kExternalTag));
  napi_env env = bundle->env;
  napi_callback_info__ callback_info{&info, bundle->data};
  napi_value result = bundle->callback(
      env, reinterpret_cast<napi_callback_info>(&callback_info));
  if (result != nullptr) info.GetReturnValue().Set(V8Value(result));
  // An addon that threw through napi_throw left the exception on the isolate,
  // where V8 picks it up on return. Nothing to do here beyond not clobbering
  // it -- but the parked copy has to go, or the addon's next call sees a
  // pending exception that JavaScript has already taken ownership of.
  env->last_exception.Reset();
}

/// Builds the `v8::Function` behind napi_create_function and every method a
/// property descriptor declares.
bool CreateFunction(napi_env env, v8::Local<v8::Context> context,
                    const char* utf8name, size_t length, napi_callback callback,
                    void* data, v8::Local<v8::Function>* result) {
  auto* bundle = new CallbackBundle{env, callback, data};
  v8::Local<v8::External> external =
      v8::External::New(env->isolate, bundle, kExternalTag);
  v8::Local<v8::Function> function;
  if (!v8::Function::New(context, FunctionCallbackWrapper, external)
           .ToLocal(&function)) {
    delete bundle;
    return false;
  }
  if (utf8name != nullptr) {
    v8::Local<v8::String> name;
    if (Utf8String(env->isolate, utf8name, length).ToLocal(&name)) {
      function->SetName(name);
    }
  }
  // The bundle outlives this call and nothing else points at it, so its life
  // is tied to the function's: when the function is collected, the finalizer
  // frees it. Without this every created function leaks its callback data.
  NewReference(env, function, 0, bundle, DeleteBundle, nullptr, true);
  *result = function;
  return true;
}

bool CreateFunctionTemplate(napi_env env, napi_callback callback, void* data,
                            v8::Local<v8::FunctionTemplate>* result) {
  auto* bundle = new CallbackBundle{env, callback, data};
  v8::Local<v8::External> external =
      v8::External::New(env->isolate, bundle, kExternalTag);
  *result = v8::FunctionTemplate::New(env->isolate, FunctionCallbackWrapper,
                                      external);
  // Templates are created while a class is being defined and live as long as
  // the class does, which in practice is the process. Freeing the bundle would
  // need a handle the template does not give us, so it is owned by the
  // environment instead.
  return true;
}

v8::PropertyAttribute AttributesFrom(napi_property_attributes attributes) {
  int result = v8::None;
  if ((attributes & napi_writable) == 0) result |= v8::ReadOnly;
  if ((attributes & napi_enumerable) == 0) result |= v8::DontEnum;
  if ((attributes & napi_configurable) == 0) result |= v8::DontDelete;
  return static_cast<v8::PropertyAttribute>(result);
}

bool DescriptorName(napi_env env, const napi_property_descriptor& descriptor,
                    v8::Local<v8::Name>* result) {
  if (descriptor.utf8name != nullptr) {
    v8::Local<v8::String> name;
    if (!Utf8String(env->isolate, descriptor.utf8name, NAPI_AUTO_LENGTH)
             .ToLocal(&name)) {
      return false;
    }
    *result = name;
    return true;
  }
  if (descriptor.name == nullptr) return false;
  v8::Local<v8::Value> value = V8Value(descriptor.name);
  if (!value->IsName()) return false;
  *result = value.As<v8::Name>();
  return true;
}

// --- typed arrays ----------------------------------------------------------

napi_status TypedArrayKind(v8::Local<v8::Value> value,
                           napi_typedarray_type* type) {
  if (value->IsInt8Array()) {
    *type = napi_int8_array;
  } else if (value->IsUint8Array()) {
    *type = napi_uint8_array;
  } else if (value->IsUint8ClampedArray()) {
    *type = napi_uint8_clamped_array;
  } else if (value->IsInt16Array()) {
    *type = napi_int16_array;
  } else if (value->IsUint16Array()) {
    *type = napi_uint16_array;
  } else if (value->IsInt32Array()) {
    *type = napi_int32_array;
  } else if (value->IsUint32Array()) {
    *type = napi_uint32_array;
  } else if (value->IsFloat32Array()) {
    *type = napi_float32_array;
  } else if (value->IsFloat64Array()) {
    *type = napi_float64_array;
  } else if (value->IsBigInt64Array()) {
    *type = napi_bigint64_array;
  } else if (value->IsBigUint64Array()) {
    *type = napi_biguint64_array;
  } else {
    return napi_invalid_arg;
  }
  return napi_ok;
}

// --- buffers ---------------------------------------------------------------

/// Sako's `Buffer` is a `Uint8Array` subclass declared in the bootstrap, so a
/// buffer is a typed array wearing that prototype. Reaching for the global
/// rather than caching it keeps this correct if the bootstrap is reloaded.
v8::MaybeLocal<v8::Object> BufferPrototype(v8::Local<v8::Context> context) {
  v8::Isolate* isolate = v8::Isolate::GetCurrent();
  v8::Local<v8::Value> buffer;
  if (!context->Global()
           ->Get(context, v8::String::NewFromUtf8Literal(isolate, "Buffer"))
           .ToLocal(&buffer) ||
      !buffer->IsObject()) {
    return {};
  }
  v8::Local<v8::Value> prototype;
  if (!buffer.As<v8::Object>()
           ->Get(context, v8::String::NewFromUtf8Literal(isolate, "prototype"))
           .ToLocal(&prototype) ||
      !prototype->IsObject()) {
    return {};
  }
  return prototype.As<v8::Object>();
}

bool WearBufferPrototype(v8::Local<v8::Context> context,
                         v8::Local<v8::Object> array) {
  v8::Local<v8::Object> prototype;
  if (!BufferPrototype(context).ToLocal(&prototype)) return false;
  return array->SetPrototype(context, prototype).FromMaybe(false);
}

// --- private keys ----------------------------------------------------------

v8::Local<v8::Private> WrapperKey(v8::Isolate* isolate) {
  return v8::Private::ForApi(
      isolate, v8::String::NewFromUtf8Literal(isolate, "sako:napi:wrapper"));
}

v8::Local<v8::Private> TypeTagKey(v8::Isolate* isolate) {
  return v8::Private::ForApi(
      isolate, v8::String::NewFromUtf8Literal(isolate, "sako:napi:type_tag"));
}

// --- exception formatting --------------------------------------------------

std::string DescribeException(v8::Isolate* isolate,
                              v8::Local<v8::Context> context,
                              v8::Local<v8::Value> exception) {
  if (exception.IsEmpty()) return "addon callback failed";
  if (exception->IsObject()) {
    v8::Local<v8::Value> stack;
    if (exception.As<v8::Object>()
            ->Get(context, v8::String::NewFromUtf8Literal(isolate, "stack"))
            .ToLocal(&stack) &&
        stack->IsString()) {
      v8::String::Utf8Value text(isolate, stack);
      if (*text != nullptr) return std::string(*text, text.length());
    }
  }
  v8::String::Utf8Value text(isolate, exception);
  return *text != nullptr ? std::string(*text, text.length())
                          : "addon callback failed";
}

}  // namespace
}  // namespace sako_napi

using sako_napi::ClearLastError;
using sako_napi::CreateFunction;
using sako_napi::CreateFunctionTemplate;
using sako_napi::JsValue;
using sako_napi::NewReference;
using sako_napi::ScopedTryCatch;
using sako_napi::SetLastError;
using sako_napi::V8Value;

// ---------------------------------------------------------------------------
// Environment and error state.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_get_last_error_info(
    node_api_basic_env env, const napi_extended_error_info** result) {
  CHECK_ENV(env);
  CHECK_ARG(env, result);
  *result = &env->last_error;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_version(node_api_basic_env env,
                                        uint32_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = sako_napi::kNapiVersion;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_node_version(node_api_basic_env env,
                                             const napi_node_version** result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  // Sako is not Node and says so. Addons read this for feature detection, and
  // a plausible-looking Node version would be a worse lie than an honest one:
  // the release string is what tells them which runtime they are actually on.
  static const napi_node_version version = {0, 1, 0, "sako"};
  *result = &version;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_uv_event_loop(node_api_basic_env env,
                                              struct uv_loop_s** loop) {
  NAPI_BASIC(env);
  CHECK_ARG(env, loop);
  // Sako has no libuv, and an addon cannot call into one either: nothing here
  // exports `uv_*`, so a resolved symbol is the only way to reach a loop and
  // there are none to resolve. What addons do with this pointer is store it
  // and hand it back to APIs they cannot call, so a stable non-null value is
  // both harmless and better than a null one, which some addons treat as an
  // assertion failure.
  *loop = reinterpret_cast<struct uv_loop_s*>(env->state->placeholder_loop);
  return napi_ok;
}

napi_status NAPI_CDECL node_api_get_module_file_name(node_api_basic_env env,
                                                     const char** result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = env->filename.c_str();
  return napi_ok;
}

napi_status NAPI_CDECL napi_set_instance_data(node_api_basic_env env, void* data,
                                              napi_finalize finalize_cb,
                                              void* finalize_hint) {
  CHECK_ENV(env);
  env->instance_data = data;
  env->instance_data_finalizer = finalize_cb;
  env->instance_data_hint = finalize_hint;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_instance_data(node_api_basic_env env,
                                              void** data) {
  CHECK_ENV(env);
  CHECK_ARG(env, data);
  *data = env->instance_data;
  return napi_ok;
}

napi_status NAPI_CDECL napi_add_env_cleanup_hook(node_api_basic_env env,
                                                 napi_cleanup_hook fun,
                                                 void* arg) {
  CHECK_ENV(env);
  CHECK_ARG(env, fun);
  env->cleanup_hooks.emplace_back(fun, arg);
  return napi_ok;
}

napi_status NAPI_CDECL napi_remove_env_cleanup_hook(node_api_basic_env env,
                                                    napi_cleanup_hook fun,
                                                    void* arg) {
  CHECK_ENV(env);
  CHECK_ARG(env, fun);
  auto& hooks = env->cleanup_hooks;
  for (auto entry = hooks.rbegin(); entry != hooks.rend(); ++entry) {
    if (entry->first == fun && entry->second == arg) {
      hooks.erase(std::next(entry).base());
      return napi_ok;
    }
  }
  return napi_invalid_arg;
}

napi_status NAPI_CDECL napi_add_async_cleanup_hook(
    node_api_basic_env env, napi_async_cleanup_hook hook, void* arg,
    napi_async_cleanup_hook_handle* remove_handle) {
  CHECK_ENV(env);
  CHECK_ARG(env, hook);
  auto* handle = new napi_async_cleanup_hook_handle__{env, hook, arg};
  if (remove_handle != nullptr) *remove_handle = handle;
  return napi_ok;
}

napi_status NAPI_CDECL
napi_remove_async_cleanup_hook(napi_async_cleanup_hook_handle remove_handle) {
  if (remove_handle == nullptr) return napi_invalid_arg;
  delete remove_handle;
  return napi_ok;
}

napi_status NAPI_CDECL napi_adjust_external_memory(node_api_basic_env env,
                                                   int64_t change_in_bytes,
                                                   int64_t* adjusted_value) {
  NAPI_BASIC(env);
  CHECK_ARG(env, adjusted_value);
  *adjusted_value =
      env->isolate->AdjustAmountOfExternalAllocatedMemory(change_in_bytes);
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Singletons and value creation.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_get_undefined(napi_env env, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Undefined(env->isolate));
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_null(napi_env env, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Null(env->isolate));
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_boolean(napi_env env, bool value,
                                        napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Boolean::New(env->isolate, value));
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_global(napi_env env, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(env->Context()->Global());
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_object(napi_env env, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Object::New(env->isolate));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_array(napi_env env, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Array::New(env->isolate));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_array_with_length(napi_env env, size_t length,
                                                     napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Array::New(env->isolate, static_cast<int>(length)));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_double(napi_env env, double value,
                                          napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Number::New(env->isolate, value));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_int32(napi_env env, int32_t value,
                                         napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Integer::New(env->isolate, value));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_uint32(napi_env env, uint32_t value,
                                          napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Integer::NewFromUnsigned(env->isolate, value));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_int64(napi_env env, int64_t value,
                                         napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::Number::New(env->isolate, static_cast<double>(value)));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_bigint_int64(napi_env env, int64_t value,
                                                napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::BigInt::New(env->isolate, value));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_bigint_uint64(napi_env env, uint64_t value,
                                                 napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = JsValue(v8::BigInt::NewFromUnsigned(env->isolate, value));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_bigint_words(napi_env env, int sign_bit,
                                                size_t word_count,
                                                const uint64_t* words,
                                                napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  if (word_count != 0) CHECK_ARG(env, words);
  v8::Local<v8::BigInt> value;
  if (!v8::BigInt::NewFromWords(env->Context(), sign_bit,
                                static_cast<int>(word_count), words)
           .ToLocal(&value)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(value);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_create_string_latin1(napi_env env, const char* str,
                                                 size_t length,
                                                 napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  if (length != 0) CHECK_ARG(env, str);
  const int size =
      length == NAPI_AUTO_LENGTH ? -1 : static_cast<int>(length);
  v8::Local<v8::String> value;
  if (!v8::String::NewFromOneByte(env->isolate,
                                  reinterpret_cast<const uint8_t*>(str),
                                  v8::NewStringType::kNormal, size)
           .ToLocal(&value)) {
    return SetLastError(env, napi_generic_failure);
  }
  *result = JsValue(value);
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_string_utf8(napi_env env, const char* str,
                                               size_t length,
                                               napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  if (length != 0) CHECK_ARG(env, str);
  v8::Local<v8::String> value;
  if (!sako_napi::Utf8String(env->isolate, str, length).ToLocal(&value)) {
    return SetLastError(env, napi_generic_failure);
  }
  *result = JsValue(value);
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_string_utf16(napi_env env,
                                                const char16_t* str,
                                                size_t length,
                                                napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  if (length != 0) CHECK_ARG(env, str);
  const int size = length == NAPI_AUTO_LENGTH ? -1 : static_cast<int>(length);
  v8::Local<v8::String> value;
  if (!v8::String::NewFromTwoByte(env->isolate,
                                  reinterpret_cast<const uint16_t*>(str),
                                  v8::NewStringType::kNormal, size)
           .ToLocal(&value)) {
    return SetLastError(env, napi_generic_failure);
  }
  *result = JsValue(value);
  return napi_ok;
}

// The external-string entry points copy and report that they did. V8 can adopt
// caller-owned character data, but only for the lifetime of a resource object
// it takes ownership of, and the bookkeeping that buys nothing here: `copied`
// exists in the ABI precisely so a runtime can decline.
napi_status NAPI_CDECL node_api_create_external_string_latin1(
    napi_env env, char* str, size_t length,
    node_api_basic_finalize finalize_callback, void* finalize_hint,
    napi_value* result, bool* copied) {
  const napi_status status =
      napi_create_string_latin1(env, str, length, result);
  if (status == napi_ok) {
    if (copied != nullptr) *copied = true;
    if (finalize_callback != nullptr) finalize_callback(env, str, finalize_hint);
  }
  return status;
}

napi_status NAPI_CDECL node_api_create_external_string_utf16(
    napi_env env, char16_t* str, size_t length,
    node_api_basic_finalize finalize_callback, void* finalize_hint,
    napi_value* result, bool* copied) {
  const napi_status status =
      napi_create_string_utf16(env, str, length, result);
  if (status == napi_ok) {
    if (copied != nullptr) *copied = true;
    if (finalize_callback != nullptr) finalize_callback(env, str, finalize_hint);
  }
  return status;
}

napi_status NAPI_CDECL node_api_create_property_key_latin1(
    napi_env env, const char* str, size_t length, napi_value* result) {
  return napi_create_string_latin1(env, str, length, result);
}

napi_status NAPI_CDECL node_api_create_property_key_utf8(napi_env env,
                                                          const char* str,
                                                          size_t length,
                                                          napi_value* result) {
  return napi_create_string_utf8(env, str, length, result);
}

napi_status NAPI_CDECL node_api_create_property_key_utf16(
    napi_env env, const char16_t* str, size_t length, napi_value* result) {
  return napi_create_string_utf16(env, str, length, result);
}

napi_status NAPI_CDECL napi_create_symbol(napi_env env, napi_value description,
                                          napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  if (description == nullptr) {
    *result = JsValue(v8::Symbol::New(env->isolate));
    return napi_ok;
  }
  v8::Local<v8::Value> text = V8Value(description);
  RETURN_STATUS_IF_FALSE(env, text->IsString(), napi_string_expected);
  *result = JsValue(v8::Symbol::New(env->isolate, text.As<v8::String>()));
  return napi_ok;
}

napi_status NAPI_CDECL node_api_symbol_for(napi_env env,
                                            const char* utf8description,
                                            size_t length, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  v8::Local<v8::String> description;
  if (!sako_napi::Utf8String(env->isolate, utf8description, length)
           .ToLocal(&description)) {
    return SetLastError(env, napi_generic_failure);
  }
  *result = JsValue(v8::Symbol::For(env->isolate, description));
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_function(napi_env env, const char* utf8name,
                                            size_t length, napi_callback cb,
                                            void* data, napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  CHECK_ARG(env, cb);
  v8::Local<v8::Function> function;
  if (!CreateFunction(env, env->Context(), utf8name, length, cb, data,
                      &function)) {
    return SetLastError(env, napi_generic_failure);
  }
  *result = JsValue(function);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_create_date(napi_env env, double time,
                                        napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> date;
  if (!v8::Date::New(env->Context(), time).ToLocal(&date)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(date);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_create_external(napi_env env, void* data,
                                            node_api_basic_finalize finalize_cb,
                                            void* finalize_hint,
                                            napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  v8::Local<v8::External> external =
      v8::External::New(env->isolate, data, sako_napi::kExternalTag);
  if (finalize_cb != nullptr) {
    NewReference(env, external, 0, data, finalize_cb, finalize_hint, true);
  }
  *result = JsValue(external);
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

// v8::Exception's factories take an options argument this never uses; the
// alias has to spell it out because a function pointer carries no defaults.
using ErrorFactory = v8::Local<v8::Value> (*)(v8::Local<v8::String>,
                                             v8::Local<v8::Value>);

napi_status CreateErrorValue(napi_env env, napi_value code, napi_value msg,
                             ErrorFactory factory, napi_value* result) {
  v8::Local<v8::Value> message = V8Value(msg);
  if (!message->IsString()) return SetLastError(env, napi_string_expected);
  v8::Local<v8::Value> error =
      factory(message.As<v8::String>(), v8::Local<v8::Value>());
  if (code != nullptr) {
    v8::Local<v8::Value> code_value = V8Value(code);
    if (!error.As<v8::Object>()
             ->Set(env->Context(),
                   v8::String::NewFromUtf8Literal(env->isolate, "code"),
                   code_value)
             .FromMaybe(false)) {
      return SetLastError(env, napi_generic_failure);
    }
  }
  *result = JsValue(error);
  return napi_ok;
}

napi_status ThrowNamed(napi_env env, const char* code, const char* msg,
                       ErrorFactory factory) {
  v8::Local<v8::String> message;
  if (!Utf8String(env->isolate, msg == nullptr ? "" : msg, NAPI_AUTO_LENGTH)
           .ToLocal(&message)) {
    return SetLastError(env, napi_generic_failure);
  }
  v8::Local<v8::Value> error = factory(message, v8::Local<v8::Value>());
  if (code != nullptr) {
    v8::Local<v8::String> code_string;
    if (Utf8String(env->isolate, code, NAPI_AUTO_LENGTH).ToLocal(&code_string)) {
      (void)error.As<v8::Object>()->Set(
          env->Context(), v8::String::NewFromUtf8Literal(env->isolate, "code"),
          code_string);
    }
  }
  env->isolate->ThrowException(error);
  return ClearLastError(env), napi_ok;
}

}  // namespace
}  // namespace sako_napi

napi_status NAPI_CDECL napi_create_error(napi_env env, napi_value code,
                                         napi_value msg, napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  CHECK_ARG(env, msg);
  return sako_napi::CreateErrorValue(env, code, msg, v8::Exception::Error,
                                     result);
}

napi_status NAPI_CDECL napi_create_type_error(napi_env env, napi_value code,
                                              napi_value msg,
                                              napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  CHECK_ARG(env, msg);
  return sako_napi::CreateErrorValue(env, code, msg, v8::Exception::TypeError,
                                     result);
}

napi_status NAPI_CDECL napi_create_range_error(napi_env env, napi_value code,
                                               napi_value msg,
                                               napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  CHECK_ARG(env, msg);
  return sako_napi::CreateErrorValue(env, code, msg, v8::Exception::RangeError,
                                     result);
}

napi_status NAPI_CDECL node_api_create_syntax_error(napi_env env,
                                                     napi_value code,
                                                     napi_value msg,
                                                     napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  CHECK_ARG(env, msg);
  return sako_napi::CreateErrorValue(env, code, msg, v8::Exception::SyntaxError,
                                     result);
}

napi_status NAPI_CDECL napi_throw(napi_env env, napi_value error) {
  NAPI_BASIC(env);
  CHECK_ARG(env, error);
  env->isolate->ThrowException(V8Value(error));
  return napi_ok;
}

napi_status NAPI_CDECL napi_throw_error(napi_env env, const char* code,
                                        const char* msg) {
  NAPI_BASIC(env);
  return sako_napi::ThrowNamed(env, code, msg, v8::Exception::Error);
}

napi_status NAPI_CDECL napi_throw_type_error(napi_env env, const char* code,
                                             const char* msg) {
  NAPI_BASIC(env);
  return sako_napi::ThrowNamed(env, code, msg, v8::Exception::TypeError);
}

napi_status NAPI_CDECL napi_throw_range_error(napi_env env, const char* code,
                                              const char* msg) {
  NAPI_BASIC(env);
  return sako_napi::ThrowNamed(env, code, msg, v8::Exception::RangeError);
}

napi_status NAPI_CDECL node_api_throw_syntax_error(napi_env env,
                                                    const char* code,
                                                    const char* msg) {
  NAPI_BASIC(env);
  return sako_napi::ThrowNamed(env, code, msg, v8::Exception::SyntaxError);
}

napi_status NAPI_CDECL napi_is_error(napi_env env, napi_value value,
                                     bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = V8Value(value)->IsNativeError();
  return napi_ok;
}

napi_status NAPI_CDECL napi_is_exception_pending(napi_env env, bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = !env->last_exception.IsEmpty();
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_and_clear_last_exception(napi_env env,
                                                         napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  if (env->last_exception.IsEmpty()) {
    *result = JsValue(v8::Undefined(env->isolate));
    return napi_ok;
  }
  *result = JsValue(env->last_exception.Get(env->isolate));
  env->last_exception.Reset();
  return napi_ok;
}

NAPI_NO_RETURN void NAPI_CDECL napi_fatal_error(const char* location,
                                                size_t location_len,
                                                const char* message,
                                                size_t message_len) {
  const std::string where =
      location == nullptr
          ? std::string("addon")
          : std::string(location, location_len == NAPI_AUTO_LENGTH
                                      ? std::strlen(location)
                                      : location_len);
  const std::string what =
      message == nullptr
          ? std::string("fatal error")
          : std::string(message, message_len == NAPI_AUTO_LENGTH
                                     ? std::strlen(message)
                                     : message_len);
  std::fprintf(stderr, "sako: fatal error in native addon: %s: %s\n",
               where.c_str(), what.c_str());
  std::fflush(stderr);
  std::abort();
}

napi_status NAPI_CDECL napi_fatal_exception(napi_env env, napi_value err) {
  NAPI_BASIC(env);
  CHECK_ARG(env, err);
  sako_napi::IsolateState* state = env->state;
  const std::string text =
      sako_napi::DescribeException(env->isolate, env->Context(), V8Value(err));
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    if (state->fatal_error.empty()) state->fatal_error = text;
  }
  state->signal.notify_all();
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Type inspection and conversion.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_typeof(napi_env env, napi_value value,
                                   napi_valuetype* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  if (local->IsNumber()) {
    *result = napi_number;
  } else if (local->IsBigInt()) {
    *result = napi_bigint;
  } else if (local->IsString()) {
    *result = napi_string;
  } else if (local->IsFunction()) {
    *result = napi_function;
  } else if (local->IsExternal()) {
    *result = napi_external;
  } else if (local->IsObject()) {
    *result = napi_object;
  } else if (local->IsBoolean()) {
    *result = napi_boolean;
  } else if (local->IsUndefined()) {
    *result = napi_undefined;
  } else if (local->IsSymbol()) {
    *result = napi_symbol;
  } else if (local->IsNull()) {
    *result = napi_null;
  } else {
    return SetLastError(env, napi_invalid_arg);
  }
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_double(napi_env env, napi_value value,
                                             double* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsNumber(), napi_number_expected);
  *result = local.As<v8::Number>()->Value();
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_int32(napi_env env, napi_value value,
                                            int32_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsNumber(), napi_number_expected);
  *result = local->Int32Value(env->Context()).FromMaybe(0);
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_uint32(napi_env env, napi_value value,
                                             uint32_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsNumber(), napi_number_expected);
  *result = local->Uint32Value(env->Context()).FromMaybe(0);
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_int64(napi_env env, napi_value value,
                                            int64_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsNumber(), napi_number_expected);
  const double number = local.As<v8::Number>()->Value();
  if (std::isfinite(number)) {
    *result = local->IntegerValue(env->Context()).FromMaybe(0);
  } else {
    // Node defines the out-of-range and non-finite cases as zero rather than
    // as an error, and addons rely on that rather than checking first.
    *result = 0;
  }
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_bool(napi_env env, napi_value value,
                                           bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsBoolean(), napi_boolean_expected);
  *result = local.As<v8::Boolean>()->Value();
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_bigint_int64(napi_env env,
                                                   napi_value value,
                                                   int64_t* result,
                                                   bool* lossless) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  CHECK_ARG(env, lossless);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsBigInt(), napi_bigint_expected);
  *result = local.As<v8::BigInt>()->Int64Value(lossless);
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_bigint_uint64(napi_env env,
                                                    napi_value value,
                                                    uint64_t* result,
                                                    bool* lossless) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  CHECK_ARG(env, lossless);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsBigInt(), napi_bigint_expected);
  *result = local.As<v8::BigInt>()->Uint64Value(lossless);
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_bigint_words(napi_env env,
                                                   napi_value value,
                                                   int* sign_bit,
                                                   size_t* word_count,
                                                   uint64_t* words) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, word_count);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsBigInt(), napi_bigint_expected);
  v8::Local<v8::BigInt> big = local.As<v8::BigInt>();
  const int available = big->WordCount();
  if (sign_bit == nullptr && words == nullptr) {
    *word_count = static_cast<size_t>(available);
    return napi_ok;
  }
  CHECK_ARG(env, sign_bit);
  CHECK_ARG(env, words);
  int count = static_cast<int>(*word_count);
  big->ToWordsArray(sign_bit, &count, words);
  *word_count = static_cast<size_t>(count);
  return napi_ok;
}

namespace sako_napi {
namespace {

/// The shared shape of the three string readers: with no buffer the caller is
/// asking how much room it needs, with one it is asking for as much as fits
/// plus a terminator.
napi_status MeasureOrCopy(napi_env env, size_t needed, size_t bufsize,
                          size_t copied, size_t* result, bool have_buffer) {
  if (!have_buffer) {
    if (result != nullptr) *result = needed;
    return napi_ok;
  }
  if (result != nullptr) *result = bufsize == 0 ? 0 : copied;
  return napi_ok;
}

}  // namespace
}  // namespace sako_napi

napi_status NAPI_CDECL napi_get_value_string_latin1(napi_env env,
                                                    napi_value value, char* buf,
                                                    size_t bufsize,
                                                    size_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsString(), napi_string_expected);
  v8::Local<v8::String> text = local.As<v8::String>();
  const size_t length = static_cast<size_t>(text->Length());
  if (buf == nullptr) {
    return sako_napi::MeasureOrCopy(env, length, bufsize, 0, result, false);
  }
  if (bufsize == 0) {
    return sako_napi::MeasureOrCopy(env, length, bufsize, 0, result, true);
  }
  const size_t copied = std::min(length, bufsize - 1);
  text->WriteOneByte(env->isolate, 0, static_cast<uint32_t>(copied),
                     reinterpret_cast<uint8_t*>(buf));
  buf[copied] = '\0';
  return sako_napi::MeasureOrCopy(env, length, bufsize, copied, result, true);
}

napi_status NAPI_CDECL napi_get_value_string_utf8(napi_env env, napi_value value,
                                                   char* buf, size_t bufsize,
                                                   size_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsString(), napi_string_expected);
  v8::Local<v8::String> text = local.As<v8::String>();
  if (buf == nullptr) {
    if (result != nullptr) *result = text->Utf8Length(env->isolate);
    return napi_ok;
  }
  if (bufsize == 0) {
    if (result != nullptr) *result = 0;
    return napi_ok;
  }
  const size_t written = text->WriteUtf8(
      env->isolate, buf, bufsize,
      v8::String::WriteFlags::kNullTerminate |
          v8::String::WriteFlags::kReplaceInvalidUtf8);
  // WriteUtf8 counts the terminator it wrote; the ABI does not.
  if (result != nullptr) *result = written == 0 ? 0 : written - 1;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_string_utf16(napi_env env,
                                                   napi_value value,
                                                   char16_t* buf,
                                                   size_t bufsize,
                                                   size_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsString(), napi_string_expected);
  v8::Local<v8::String> text = local.As<v8::String>();
  const size_t length = static_cast<size_t>(text->Length());
  if (buf == nullptr) {
    if (result != nullptr) *result = length;
    return napi_ok;
  }
  if (bufsize == 0) {
    if (result != nullptr) *result = 0;
    return napi_ok;
  }
  const size_t copied = std::min(length, bufsize - 1);
  text->Write(env->isolate, 0, static_cast<uint32_t>(copied),
              reinterpret_cast<uint16_t*>(buf));
  buf[copied] = u'\0';
  if (result != nullptr) *result = copied;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_value_external(napi_env env, napi_value value,
                                               void** result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsExternal(), napi_invalid_arg);
  *result = local.As<v8::External>()->Value(sako_napi::kExternalTag);
  return napi_ok;
}

napi_status NAPI_CDECL napi_coerce_to_bool(napi_env env, napi_value value,
                                           napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = JsValue(V8Value(value)->ToBoolean(env->isolate));
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_coerce_to_number(napi_env env, napi_value value,
                                             napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Number> number;
  if (!V8Value(value)->ToNumber(env->Context()).ToLocal(&number)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(number);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_coerce_to_object(napi_env env, napi_value value,
                                             napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> object;
  if (!V8Value(value)->ToObject(env->Context()).ToLocal(&object)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(object);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_coerce_to_string(napi_env env, napi_value value,
                                             napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::String> text;
  if (!V8Value(value)->ToString(env->Context()).ToLocal(&text)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(text);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_is_date(napi_env env, napi_value value,
                                    bool* is_date) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, is_date);
  *is_date = V8Value(value)->IsDate();
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_date_value(napi_env env, napi_value value,
                                           double* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsDate(), napi_date_expected);
  *result = local.As<v8::Date>()->ValueOf();
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Properties.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

bool AsObject(napi_value value, v8::Local<v8::Object>* object) {
  v8::Local<v8::Value> local = V8Value(value);
  if (!local->IsObject()) return false;
  *object = local.As<v8::Object>();
  return true;
}

}  // namespace
}  // namespace sako_napi

#define CHECK_OBJECT(env, value, object)                                       \
  do {                                                                         \
    CHECK_ARG((env), (value));                                                 \
    RETURN_STATUS_IF_FALSE((env), sako_napi::AsObject((value), &(object)),     \
                           napi_object_expected);                              \
  } while (0)

napi_status NAPI_CDECL napi_get_prototype(napi_env env, napi_value object,
                                          napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  *result = JsValue(target->GetPrototype());
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_property_names(napi_env env, napi_value object,
                                               napi_value* result) {
  return napi_get_all_property_names(env, object, napi_key_include_prototypes,
                                     static_cast<napi_key_filter>(
                                         napi_key_enumerable |
                                         napi_key_skip_symbols),
                                     napi_key_numbers_to_strings, result);
}

napi_status NAPI_CDECL napi_get_all_property_names(
    napi_env env, napi_value object, napi_key_collection_mode key_mode,
    napi_key_filter key_filter, napi_key_conversion key_conversion,
    napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);

  int filter = v8::ALL_PROPERTIES;
  if (key_filter & napi_key_writable) filter |= v8::ONLY_WRITABLE;
  if (key_filter & napi_key_enumerable) filter |= v8::ONLY_ENUMERABLE;
  if (key_filter & napi_key_configurable) filter |= v8::ONLY_CONFIGURABLE;
  if (key_filter & napi_key_skip_strings) filter |= v8::SKIP_STRINGS;
  if (key_filter & napi_key_skip_symbols) filter |= v8::SKIP_SYMBOLS;

  v8::Local<v8::Array> names;
  if (!target
           ->GetPropertyNames(
               env->Context(),
               key_mode == napi_key_include_prototypes
                   ? v8::KeyCollectionMode::kIncludePrototypes
                   : v8::KeyCollectionMode::kOwnOnly,
               static_cast<v8::PropertyFilter>(filter), v8::IndexFilter::kIncludeIndices,
               key_conversion == napi_key_numbers_to_strings
                   ? v8::KeyConversionMode::kConvertToString
                   : v8::KeyConversionMode::kKeepNumbers)
           .ToLocal(&names)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(names);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_set_property(napi_env env, napi_value object,
                                         napi_value key, napi_value value) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, key);
  CHECK_ARG(env, value);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  if (!target->Set(env->Context(), V8Value(key), V8Value(value))
           .FromMaybe(false)) {
    return GET_RETURN_STATUS(env);
  }
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_property(napi_env env, napi_value object,
                                         napi_value key, napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, key);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::Value> value;
  if (!target->Get(env->Context(), V8Value(key)).ToLocal(&value)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(value);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_has_property(napi_env env, napi_value object,
                                         napi_value key, bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, key);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Maybe<bool> has = target->Has(env->Context(), V8Value(key));
  if (has.IsNothing()) return GET_RETURN_STATUS(env);
  *result = has.FromJust();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_has_own_property(napi_env env, napi_value object,
                                             napi_value key, bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, key);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::Value> name = V8Value(key);
  RETURN_STATUS_IF_FALSE(env, name->IsName(), napi_name_expected);
  v8::Maybe<bool> has =
      target->HasOwnProperty(env->Context(), name.As<v8::Name>());
  if (has.IsNothing()) return GET_RETURN_STATUS(env);
  *result = has.FromJust();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_delete_property(napi_env env, napi_value object,
                                            napi_value key, bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, key);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Maybe<bool> deleted = target->Delete(env->Context(), V8Value(key));
  if (deleted.IsNothing()) return GET_RETURN_STATUS(env);
  if (result != nullptr) *result = deleted.FromJust();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_set_named_property(napi_env env, napi_value object,
                                               const char* utf8name,
                                               napi_value value) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, utf8name);
  CHECK_ARG(env, value);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::String> name;
  if (!sako_napi::Utf8String(env->isolate, utf8name, NAPI_AUTO_LENGTH)
           .ToLocal(&name)) {
    return SetLastError(env, napi_generic_failure);
  }
  (void)target->Set(env->Context(), name, V8Value(value));
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_named_property(napi_env env, napi_value object,
                                               const char* utf8name,
                                               napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, utf8name);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::String> name;
  if (!sako_napi::Utf8String(env->isolate, utf8name, NAPI_AUTO_LENGTH)
           .ToLocal(&name)) {
    return SetLastError(env, napi_generic_failure);
  }
  v8::Local<v8::Value> value;
  if (!target->Get(env->Context(), name).ToLocal(&value)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(value);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_has_named_property(napi_env env, napi_value object,
                                               const char* utf8name,
                                               bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, utf8name);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::String> name;
  if (!sako_napi::Utf8String(env->isolate, utf8name, NAPI_AUTO_LENGTH)
           .ToLocal(&name)) {
    return SetLastError(env, napi_generic_failure);
  }
  v8::Maybe<bool> has = target->Has(env->Context(), name);
  if (has.IsNothing()) return GET_RETURN_STATUS(env);
  *result = has.FromJust();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_set_element(napi_env env, napi_value object,
                                        uint32_t index, napi_value value) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, value);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  (void)target->Set(env->Context(), index, V8Value(value));
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_element(napi_env env, napi_value object,
                                        uint32_t index, napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::Value> value;
  if (!target->Get(env->Context(), index).ToLocal(&value)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(value);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_has_element(napi_env env, napi_value object,
                                        uint32_t index, bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Maybe<bool> has = target->Has(env->Context(), index);
  if (has.IsNothing()) return GET_RETURN_STATUS(env);
  *result = has.FromJust();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_delete_element(napi_env env, napi_value object,
                                           uint32_t index, bool* result) {
  NAPI_PREAMBLE(env);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Maybe<bool> deleted = target->Delete(env->Context(), index);
  if (deleted.IsNothing()) return GET_RETURN_STATUS(env);
  if (result != nullptr) *result = deleted.FromJust();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_define_properties(
    napi_env env, napi_value object, size_t property_count,
    const napi_property_descriptor* properties) {
  NAPI_PREAMBLE(env);
  if (property_count > 0) CHECK_ARG(env, properties);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  v8::Local<v8::Context> context = env->Context();

  for (size_t index = 0; index < property_count; ++index) {
    const napi_property_descriptor& descriptor = properties[index];
    v8::Local<v8::Name> name;
    RETURN_STATUS_IF_FALSE(env, sako_napi::DescriptorName(env, descriptor, &name),
                           napi_name_expected);
    const v8::PropertyAttribute attributes =
        sako_napi::AttributesFrom(descriptor.attributes);

    if (descriptor.getter != nullptr || descriptor.setter != nullptr) {
      v8::Local<v8::Function> getter;
      v8::Local<v8::Function> setter;
      if (descriptor.getter != nullptr &&
          !CreateFunction(env, context, nullptr, 0, descriptor.getter,
                          descriptor.data, &getter)) {
        return SetLastError(env, napi_generic_failure);
      }
      if (descriptor.setter != nullptr &&
          !CreateFunction(env, context, nullptr, 0, descriptor.setter,
                          descriptor.data, &setter)) {
        return SetLastError(env, napi_generic_failure);
      }
      target->SetAccessorProperty(name, getter, setter, attributes);
      continue;
    }

    v8::Local<v8::Value> value;
    if (descriptor.method != nullptr) {
      v8::Local<v8::Function> method;
      if (!CreateFunction(env, context, nullptr, 0, descriptor.method,
                          descriptor.data, &method)) {
        return SetLastError(env, napi_generic_failure);
      }
      method->SetName(name->IsString() ? name.As<v8::String>()
                                       : v8::String::Empty(env->isolate));
      value = method;
    } else {
      CHECK_ARG(env, descriptor.value);
      value = V8Value(descriptor.value);
    }
    if (!target->DefineOwnProperty(context, name, value, attributes)
             .FromMaybe(false)) {
      return GET_RETURN_STATUS(env);
    }
  }
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_object_freeze(napi_env env, napi_value object) {
  NAPI_PREAMBLE(env);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  if (!target->SetIntegrityLevel(env->Context(), v8::IntegrityLevel::kFrozen)
           .FromMaybe(false)) {
    return GET_RETURN_STATUS(env);
  }
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_object_seal(napi_env env, napi_value object) {
  NAPI_PREAMBLE(env);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, object, target);
  if (!target->SetIntegrityLevel(env->Context(), v8::IntegrityLevel::kSealed)
           .FromMaybe(false)) {
    return GET_RETURN_STATUS(env);
  }
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_is_array(napi_env env, napi_value value,
                                     bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = V8Value(value)->IsArray();
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_array_length(napi_env env, napi_value value,
                                             uint32_t* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsArray(), napi_array_expected);
  *result = local.As<v8::Array>()->Length();
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_strict_equals(napi_env env, napi_value lhs,
                                          napi_value rhs, bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, lhs);
  CHECK_ARG(env, rhs);
  CHECK_ARG(env, result);
  *result = V8Value(lhs)->StrictEquals(V8Value(rhs));
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_instanceof(napi_env env, napi_value object,
                                       napi_value constructor, bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, object);
  CHECK_ARG(env, constructor);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> target = V8Value(constructor);
  RETURN_STATUS_IF_FALSE(env, target->IsFunction(), napi_function_expected);
  v8::Maybe<bool> is =
      V8Value(object)->InstanceOf(env->Context(), target.As<v8::Object>());
  if (is.IsNothing()) return GET_RETURN_STATUS(env);
  *result = is.FromJust();
  return GET_RETURN_STATUS(env);
}

// ---------------------------------------------------------------------------
// Calling into JavaScript.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_call_function(napi_env env, napi_value recv,
                                          napi_value func, size_t argc,
                                          const napi_value* argv,
                                          napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, recv);
  CHECK_ARG(env, func);
  if (argc > 0) CHECK_ARG(env, argv);
  v8::Local<v8::Value> callee = V8Value(func);
  RETURN_STATUS_IF_FALSE(env, callee->IsFunction(), napi_function_expected);
  v8::Local<v8::Value> outcome;
  if (!callee.As<v8::Function>()
           ->Call(env->Context(), V8Value(recv), static_cast<int>(argc),
                  reinterpret_cast<v8::Local<v8::Value>*>(
                      const_cast<napi_value*>(argv)))
           .ToLocal(&outcome)) {
    return GET_RETURN_STATUS(env);
  }
  if (result != nullptr) *result = JsValue(outcome);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_new_instance(napi_env env, napi_value constructor,
                                         size_t argc, const napi_value* argv,
                                         napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, constructor);
  CHECK_ARG(env, result);
  if (argc > 0) CHECK_ARG(env, argv);
  v8::Local<v8::Value> callee = V8Value(constructor);
  RETURN_STATUS_IF_FALSE(env, callee->IsFunction(), napi_function_expected);
  v8::Local<v8::Object> instance;
  if (!callee.As<v8::Function>()
           ->NewInstance(env->Context(), static_cast<int>(argc),
                         reinterpret_cast<v8::Local<v8::Value>*>(
                             const_cast<napi_value*>(argv)))
           .ToLocal(&instance)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(instance);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_cb_info(napi_env env, napi_callback_info cbinfo,
                                        size_t* argc, napi_value* argv,
                                        napi_value* this_arg, void** data) {
  NAPI_BASIC(env);
  CHECK_ARG(env, cbinfo);
  auto* callback_info = reinterpret_cast<napi_callback_info__*>(cbinfo);
  const v8::FunctionCallbackInfo<v8::Value>& info = *callback_info->info;

  if (argv != nullptr) {
    CHECK_ARG(env, argc);
    const size_t provided = static_cast<size_t>(info.Length());
    const size_t wanted = *argc;
    const size_t copied = std::min(provided, wanted);
    for (size_t index = 0; index < copied; ++index) {
      argv[index] = JsValue(info[static_cast<int>(index)]);
    }
    for (size_t index = copied; index < wanted; ++index) {
      argv[index] = JsValue(v8::Undefined(env->isolate));
    }
  }
  if (argc != nullptr) *argc = static_cast<size_t>(info.Length());
  if (this_arg != nullptr) *this_arg = JsValue(info.This());
  if (data != nullptr) *data = callback_info->data;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_new_target(napi_env env,
                                           napi_callback_info cbinfo,
                                           napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, cbinfo);
  CHECK_ARG(env, result);
  auto* callback_info = reinterpret_cast<napi_callback_info__*>(cbinfo);
  v8::Local<v8::Value> target = callback_info->info->NewTarget();
  *result = target->IsUndefined() ? nullptr : JsValue(target);
  return napi_ok;
}

napi_status NAPI_CDECL napi_define_class(
    napi_env env, const char* utf8name, size_t length, napi_callback constructor,
    void* data, size_t property_count,
    const napi_property_descriptor* properties, napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  CHECK_ARG(env, constructor);
  if (property_count > 0) CHECK_ARG(env, properties);
  v8::Local<v8::Context> context = env->Context();

  v8::Local<v8::FunctionTemplate> tpl;
  if (!CreateFunctionTemplate(env, constructor, data, &tpl)) {
    return SetLastError(env, napi_generic_failure);
  }
  v8::Local<v8::String> name;
  if (!sako_napi::Utf8String(env->isolate, utf8name, length).ToLocal(&name)) {
    return SetLastError(env, napi_generic_failure);
  }
  tpl->SetClassName(name);

  std::vector<napi_property_descriptor> statics;
  for (size_t index = 0; index < property_count; ++index) {
    const napi_property_descriptor& descriptor = properties[index];
    if ((descriptor.attributes & napi_static) != 0) {
      statics.push_back(descriptor);
      continue;
    }
    v8::Local<v8::Name> key;
    RETURN_STATUS_IF_FALSE(env, sako_napi::DescriptorName(env, descriptor, &key),
                           napi_name_expected);
    const v8::PropertyAttribute attributes =
        sako_napi::AttributesFrom(descriptor.attributes);
    if (descriptor.getter != nullptr || descriptor.setter != nullptr) {
      v8::Local<v8::FunctionTemplate> getter;
      v8::Local<v8::FunctionTemplate> setter;
      if (descriptor.getter != nullptr) {
        CreateFunctionTemplate(env, descriptor.getter, descriptor.data, &getter);
      }
      if (descriptor.setter != nullptr) {
        CreateFunctionTemplate(env, descriptor.setter, descriptor.data, &setter);
      }
      tpl->PrototypeTemplate()->SetAccessorProperty(key, getter, setter,
                                                    attributes);
    } else if (descriptor.method != nullptr) {
      v8::Local<v8::FunctionTemplate> method;
      CreateFunctionTemplate(env, descriptor.method, descriptor.data, &method);
      tpl->PrototypeTemplate()->Set(key, method, attributes);
    } else {
      CHECK_ARG(env, descriptor.value);
      tpl->PrototypeTemplate()->Set(key, V8Value(descriptor.value), attributes);
    }
  }

  v8::Local<v8::Function> function;
  if (!tpl->GetFunction(context).ToLocal(&function)) {
    return GET_RETURN_STATUS(env);
  }
  *result = JsValue(function);
  if (!statics.empty()) {
    const napi_status status = napi_define_properties(
        env, *result, statics.size(), statics.data());
    if (status != napi_ok) return status;
  }
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_run_script(napi_env env, napi_value script,
                                       napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, script);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> source = V8Value(script);
  RETURN_STATUS_IF_FALSE(env, source->IsString(), napi_string_expected);
  v8::Local<v8::Context> context = env->Context();
  v8::Local<v8::Script> compiled;
  if (!v8::Script::Compile(context, source.As<v8::String>()).ToLocal(&compiled)) {
    return GET_RETURN_STATUS(env);
  }
  v8::Local<v8::Value> value;
  if (!compiled->Run(context).ToLocal(&value)) return GET_RETURN_STATUS(env);
  *result = JsValue(value);
  return GET_RETURN_STATUS(env);
}

// ---------------------------------------------------------------------------
// Wrapping native state on JavaScript objects.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

napi_status WrapObject(napi_env env, napi_value js_object, void* native_object,
                       napi_finalize finalize_cb, void* finalize_hint,
                       napi_ref* result, bool require_finalizer) {
  v8::Local<v8::Object> target;
  if (!AsObject(js_object, &target)) {
    return SetLastError(env, napi_object_expected);
  }
  v8::Local<v8::Context> context = env->Context();
  v8::Local<v8::Private> key = WrapperKey(env->isolate);
  if (target->HasPrivate(context, key).FromMaybe(false)) {
    return SetLastError(env, napi_invalid_arg);
  }
  if (require_finalizer && finalize_cb == nullptr && result == nullptr) {
    return SetLastError(env, napi_invalid_arg);
  }
  // A reference the caller asked for is strong: it is holding the object for
  // its own use and will decide when the finalizer may run. One it did not ask
  // for is weak, so the wrap adds no lifetime of its own.
  const bool owned_by_caller = result != nullptr;
  napi_ref__* reference =
      NewReference(env, target, owned_by_caller ? 1 : 0, native_object,
                   finalize_cb, finalize_hint, !owned_by_caller);
  if (!target
           ->SetPrivate(context, key,
                        v8::External::New(env->isolate, reference, kExternalTag))
           .FromMaybe(false)) {
    delete reference;
    return SetLastError(env, napi_generic_failure);
  }
  if (result != nullptr) *result = reference;
  return napi_ok;
}

}  // namespace
}  // namespace sako_napi

napi_status NAPI_CDECL napi_wrap(napi_env env, napi_value js_object,
                                 void* native_object,
                                 node_api_basic_finalize finalize_cb,
                                 void* finalize_hint, napi_ref* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, js_object);
  return sako_napi::WrapObject(env, js_object, native_object, finalize_cb,
                               finalize_hint, result, true);
}

napi_status NAPI_CDECL napi_unwrap(napi_env env, napi_value js_object,
                                   void** result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, js_object);
  CHECK_ARG(env, result);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, js_object, target);
  v8::Local<v8::Value> stored;
  if (!target->GetPrivate(env->Context(), sako_napi::WrapperKey(env->isolate))
           .ToLocal(&stored) ||
      !stored->IsExternal()) {
    return SetLastError(env, napi_invalid_arg);
  }
  *result = static_cast<napi_ref__*>(
                stored.As<v8::External>()->Value(sako_napi::kExternalTag))
                ->data;
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_remove_wrap(napi_env env, napi_value js_object,
                                        void** result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, js_object);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, js_object, target);
  v8::Local<v8::Context> context = env->Context();
  v8::Local<v8::Private> key = sako_napi::WrapperKey(env->isolate);
  v8::Local<v8::Value> stored;
  if (!target->GetPrivate(context, key).ToLocal(&stored) ||
      !stored->IsExternal()) {
    return SetLastError(env, napi_invalid_arg);
  }
  auto* reference = static_cast<napi_ref__*>(
      stored.As<v8::External>()->Value(sako_napi::kExternalTag));
  if (result != nullptr) *result = reference->data;
  (void)target->DeletePrivate(context, key);
  // The wrap is gone, so its finalizer must not run: the native object now
  // belongs to whoever called this.
  reference->finalize_cb = nullptr;
  reference->persistent.Reset();
  if (reference->self_owned) delete reference;
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_add_finalizer(napi_env env, napi_value js_object,
                                          void* finalize_data,
                                          node_api_basic_finalize finalize_cb,
                                          void* finalize_hint,
                                          napi_ref* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, js_object);
  CHECK_ARG(env, finalize_cb);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, js_object, target);
  napi_ref__* reference =
      NewReference(env, target, result != nullptr ? 1 : 0, finalize_data,
                   finalize_cb, finalize_hint, result == nullptr);
  if (result != nullptr) *result = reference;
  return napi_ok;
}

napi_status NAPI_CDECL node_api_post_finalizer(node_api_basic_env env,
                                                napi_finalize finalize_cb,
                                                void* finalize_data,
                                                void* finalize_hint) {
  CHECK_ENV(env);
  CHECK_ARG(env, finalize_cb);
  sako_napi::IsolateState* state = env->state;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    state->finalizers.push_back(sako_napi::FinalizerCall{
        finalize_cb, env, finalize_data, finalize_hint, nullptr});
  }
  state->signal.notify_all();
  return napi_ok;
}

napi_status NAPI_CDECL napi_type_tag_object(napi_env env, napi_value value,
                                            const napi_type_tag* type_tag) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, type_tag);
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, value, target);
  v8::Local<v8::Context> context = env->Context();
  v8::Local<v8::Private> key = sako_napi::TypeTagKey(env->isolate);
  if (target->HasPrivate(context, key).FromMaybe(false)) {
    return SetLastError(env, napi_invalid_arg);
  }
  const uint64_t words[2] = {type_tag->lower, type_tag->upper};
  v8::Local<v8::BigInt> tag;
  if (!v8::BigInt::NewFromWords(context, 0, 2, words).ToLocal(&tag)) {
    return GET_RETURN_STATUS(env);
  }
  if (!target->SetPrivate(context, key, tag).FromMaybe(false)) {
    return SetLastError(env, napi_generic_failure);
  }
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_check_object_type_tag(napi_env env, napi_value value,
                                                  const napi_type_tag* type_tag,
                                                  bool* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, type_tag);
  CHECK_ARG(env, result);
  *result = false;
  v8::Local<v8::Object> target;
  CHECK_OBJECT(env, value, target);
  v8::Local<v8::Value> stored;
  if (!target->GetPrivate(env->Context(), sako_napi::TypeTagKey(env->isolate))
           .ToLocal(&stored) ||
      !stored->IsBigInt()) {
    return GET_RETURN_STATUS(env);
  }
  uint64_t words[2] = {0, 0};
  int sign = 0;
  int count = 2;
  stored.As<v8::BigInt>()->ToWordsArray(&sign, &count, words);
  if (count > 2) return GET_RETURN_STATUS(env);
  for (int index = count; index < 2; ++index) words[index] = 0;
  *result = sign == 0 && words[0] == type_tag->lower &&
            words[1] == type_tag->upper;
  return GET_RETURN_STATUS(env);
}

// ---------------------------------------------------------------------------
// References and handle scopes.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_create_reference(napi_env env, napi_value value,
                                             uint32_t initial_refcount,
                                             napi_ref* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = NewReference(env, V8Value(value), initial_refcount, nullptr, nullptr,
                         nullptr, false);
  return napi_ok;
}

napi_status NAPI_CDECL napi_delete_reference(napi_env env, napi_ref ref) {
  NAPI_BASIC(env);
  CHECK_ARG(env, ref);
  ref->persistent.Reset();
  // A reference whose finalizer is still queued cannot be freed here: the
  // queue entry points at it. Clearing the callback is enough -- the drain
  // then deletes it.
  ref->finalize_cb = nullptr;
  ref->self_owned = true;
  if (!ref->finalized) delete ref;
  return napi_ok;
}

napi_status NAPI_CDECL napi_reference_ref(napi_env env, napi_ref ref,
                                          uint32_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, ref);
  ref->refcount += 1;
  if (ref->refcount == 1 && !ref->persistent.IsEmpty()) {
    ref->persistent.ClearWeak();
  }
  if (result != nullptr) *result = ref->refcount;
  return napi_ok;
}

napi_status NAPI_CDECL napi_reference_unref(napi_env env, napi_ref ref,
                                            uint32_t* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, ref);
  RETURN_STATUS_IF_FALSE(env, ref->refcount > 0, napi_generic_failure);
  ref->refcount -= 1;
  sako_napi::ApplyWeakness(ref);
  if (result != nullptr) *result = ref->refcount;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_reference_value(napi_env env, napi_ref ref,
                                                napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, ref);
  CHECK_ARG(env, result);
  *result = ref->persistent.IsEmpty()
                ? nullptr
                : JsValue(ref->persistent.Get(env->isolate));
  return napi_ok;
}

napi_status NAPI_CDECL napi_open_handle_scope(napi_env env,
                                              napi_handle_scope* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = new napi_handle_scope__(env->isolate);
  env->open_handle_scopes += 1;
  return napi_ok;
}

napi_status NAPI_CDECL napi_close_handle_scope(napi_env env,
                                               napi_handle_scope scope) {
  NAPI_BASIC(env);
  CHECK_ARG(env, scope);
  RETURN_STATUS_IF_FALSE(env, env->open_handle_scopes > 0,
                         napi_handle_scope_mismatch);
  env->open_handle_scopes -= 1;
  delete scope;
  return napi_ok;
}

napi_status NAPI_CDECL napi_open_escapable_handle_scope(
    napi_env env, napi_escapable_handle_scope* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  *result = new napi_escapable_handle_scope__(env->isolate);
  env->open_handle_scopes += 1;
  return napi_ok;
}

napi_status NAPI_CDECL napi_close_escapable_handle_scope(
    napi_env env, napi_escapable_handle_scope scope) {
  NAPI_BASIC(env);
  CHECK_ARG(env, scope);
  RETURN_STATUS_IF_FALSE(env, env->open_handle_scopes > 0,
                         napi_handle_scope_mismatch);
  env->open_handle_scopes -= 1;
  delete scope;
  return napi_ok;
}

napi_status NAPI_CDECL napi_escape_handle(napi_env env,
                                          napi_escapable_handle_scope scope,
                                          napi_value escapee,
                                          napi_value* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, scope);
  CHECK_ARG(env, escapee);
  CHECK_ARG(env, result);
  RETURN_STATUS_IF_FALSE(env, !scope->escaped, napi_escape_called_twice);
  scope->escaped = true;
  *result = JsValue(scope->scope.Escape(V8Value(escapee)));
  return napi_ok;
}

// ---------------------------------------------------------------------------
// ArrayBuffers, typed arrays, and buffers.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_is_arraybuffer(napi_env env, napi_value value,
                                           bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = V8Value(value)->IsArrayBuffer();
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_arraybuffer(napi_env env, size_t byte_length,
                                               void** data,
                                               napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(env->isolate, byte_length);
  if (data != nullptr) *data = buffer->Data();
  *result = JsValue(buffer);
  return GET_RETURN_STATUS(env);
}

/// Adopts caller-owned bytes by copying them.
///
/// V8's sandbox requires every ArrayBuffer's storage to live inside the
/// sandbox address space, and aborts the process when handed a pointer from
/// anywhere else -- so a genuinely external backing store is not available at
/// all in this build. The ABI's other option, `napi_no_external_buffers_allowed`,
/// would fail addons that use this for ordinary results, so the copy wins.
///
/// The contract the addon cares about still holds: its finalizer runs when
/// JavaScript is done with the value, not before, so its memory has exactly
/// the lifetime it was promised. What is lost is the zero copy.
napi_status NAPI_CDECL napi_create_external_arraybuffer(
    napi_env env, void* external_data, size_t byte_length,
    node_api_basic_finalize finalize_cb, void* finalize_hint,
    napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(env->isolate, byte_length);
  if (byte_length != 0 && external_data != nullptr) {
    std::memcpy(buffer->Data(), external_data, byte_length);
  }
  if (finalize_cb != nullptr) {
    NewReference(env, buffer, 0, external_data, finalize_cb, finalize_hint,
                 true);
  }
  *result = JsValue(buffer);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_arraybuffer_info(napi_env env,
                                                 napi_value arraybuffer,
                                                 void** data,
                                                 size_t* byte_length) {
  NAPI_BASIC(env);
  CHECK_ARG(env, arraybuffer);
  v8::Local<v8::Value> local = V8Value(arraybuffer);
  RETURN_STATUS_IF_FALSE(env, local->IsArrayBuffer(), napi_arraybuffer_expected);
  v8::Local<v8::ArrayBuffer> buffer = local.As<v8::ArrayBuffer>();
  if (data != nullptr) *data = buffer->Data();
  if (byte_length != nullptr) *byte_length = buffer->ByteLength();
  return napi_ok;
}

napi_status NAPI_CDECL napi_detach_arraybuffer(napi_env env,
                                               napi_value arraybuffer) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, arraybuffer);
  v8::Local<v8::Value> local = V8Value(arraybuffer);
  RETURN_STATUS_IF_FALSE(env, local->IsArrayBuffer(),
                         napi_arraybuffer_expected);
  v8::Local<v8::ArrayBuffer> buffer = local.As<v8::ArrayBuffer>();
  RETURN_STATUS_IF_FALSE(env, buffer->IsDetachable(),
                         napi_detachable_arraybuffer_expected);
  (void)buffer->Detach(v8::Local<v8::Value>());
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_is_detached_arraybuffer(napi_env env,
                                                    napi_value value,
                                                    bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  *result = local->IsArrayBuffer() && local.As<v8::ArrayBuffer>()->WasDetached();
  return napi_ok;
}

napi_status NAPI_CDECL napi_is_typedarray(napi_env env, napi_value value,
                                          bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = V8Value(value)->IsTypedArray();
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_typedarray(napi_env env,
                                              napi_typedarray_type type,
                                              size_t length,
                                              napi_value arraybuffer,
                                              size_t byte_offset,
                                              napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, arraybuffer);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(arraybuffer);
  RETURN_STATUS_IF_FALSE(env, local->IsArrayBuffer(), napi_arraybuffer_expected);
  v8::Local<v8::ArrayBuffer> buffer = local.As<v8::ArrayBuffer>();
  v8::Local<v8::TypedArray> array;
  switch (type) {
    case napi_int8_array:
      array = v8::Int8Array::New(buffer, byte_offset, length);
      break;
    case napi_uint8_array:
      array = v8::Uint8Array::New(buffer, byte_offset, length);
      break;
    case napi_uint8_clamped_array:
      array = v8::Uint8ClampedArray::New(buffer, byte_offset, length);
      break;
    case napi_int16_array:
      array = v8::Int16Array::New(buffer, byte_offset, length);
      break;
    case napi_uint16_array:
      array = v8::Uint16Array::New(buffer, byte_offset, length);
      break;
    case napi_int32_array:
      array = v8::Int32Array::New(buffer, byte_offset, length);
      break;
    case napi_uint32_array:
      array = v8::Uint32Array::New(buffer, byte_offset, length);
      break;
    case napi_float32_array:
      array = v8::Float32Array::New(buffer, byte_offset, length);
      break;
    case napi_float64_array:
      array = v8::Float64Array::New(buffer, byte_offset, length);
      break;
    case napi_bigint64_array:
      array = v8::BigInt64Array::New(buffer, byte_offset, length);
      break;
    case napi_biguint64_array:
      array = v8::BigUint64Array::New(buffer, byte_offset, length);
      break;
    default:
      return SetLastError(env, napi_invalid_arg);
  }
  *result = JsValue(array);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_get_typedarray_info(
    napi_env env, napi_value typedarray, napi_typedarray_type* type,
    size_t* length, void** data, napi_value* arraybuffer, size_t* byte_offset) {
  NAPI_BASIC(env);
  CHECK_ARG(env, typedarray);
  v8::Local<v8::Value> local = V8Value(typedarray);
  RETURN_STATUS_IF_FALSE(env, local->IsTypedArray(), napi_invalid_arg);
  v8::Local<v8::TypedArray> array = local.As<v8::TypedArray>();
  if (type != nullptr) {
    const napi_status status = sako_napi::TypedArrayKind(local, type);
    if (status != napi_ok) return SetLastError(env, status);
  }
  if (length != nullptr) *length = array->Length();
  v8::Local<v8::ArrayBuffer> buffer = array->Buffer();
  if (data != nullptr) {
    *data = static_cast<uint8_t*>(buffer->Data()) + array->ByteOffset();
  }
  if (arraybuffer != nullptr) *arraybuffer = JsValue(buffer);
  if (byte_offset != nullptr) *byte_offset = array->ByteOffset();
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_dataview(napi_env env, size_t length,
                                            napi_value arraybuffer,
                                            size_t byte_offset,
                                            napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, arraybuffer);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(arraybuffer);
  RETURN_STATUS_IF_FALSE(env, local->IsArrayBuffer(), napi_arraybuffer_expected);
  *result = JsValue(
      v8::DataView::New(local.As<v8::ArrayBuffer>(), byte_offset, length));
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_is_dataview(napi_env env, napi_value value,
                                        bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  *result = V8Value(value)->IsDataView();
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_dataview_info(napi_env env, napi_value dataview,
                                              size_t* bytelength, void** data,
                                              napi_value* arraybuffer,
                                              size_t* byte_offset) {
  NAPI_BASIC(env);
  CHECK_ARG(env, dataview);
  v8::Local<v8::Value> local = V8Value(dataview);
  RETURN_STATUS_IF_FALSE(env, local->IsDataView(), napi_invalid_arg);
  v8::Local<v8::DataView> view = local.As<v8::DataView>();
  v8::Local<v8::ArrayBuffer> buffer = view->Buffer();
  if (bytelength != nullptr) *bytelength = view->ByteLength();
  if (data != nullptr) {
    *data = static_cast<uint8_t*>(buffer->Data()) + view->ByteOffset();
  }
  if (arraybuffer != nullptr) *arraybuffer = JsValue(buffer);
  if (byte_offset != nullptr) *byte_offset = view->ByteOffset();
  return napi_ok;
}

napi_status NAPI_CDECL napi_create_buffer(napi_env env, size_t length,
                                          void** data, napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(env->isolate, length);
  v8::Local<v8::Uint8Array> array = v8::Uint8Array::New(buffer, 0, length);
  if (!sako_napi::WearBufferPrototype(env->Context(), array)) {
    return SetLastError(env, napi_generic_failure);
  }
  if (data != nullptr) *data = buffer->Data();
  *result = JsValue(array);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_create_buffer_copy(napi_env env, size_t length,
                                               const void* data,
                                               void** result_data,
                                               napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(env->isolate, length);
  if (length != 0 && data != nullptr) {
    std::memcpy(buffer->Data(), data, length);
  }
  v8::Local<v8::Uint8Array> array = v8::Uint8Array::New(buffer, 0, length);
  if (!sako_napi::WearBufferPrototype(env->Context(), array)) {
    return SetLastError(env, napi_generic_failure);
  }
  if (result_data != nullptr) *result_data = buffer->Data();
  *result = JsValue(array);
  return GET_RETURN_STATUS(env);
}

/// Copies, for the reason napi_create_external_arraybuffer explains.
napi_status NAPI_CDECL napi_create_external_buffer(
    napi_env env, size_t length, void* data,
    node_api_basic_finalize finalize_cb, void* finalize_hint,
    napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, result);
  v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(env->isolate, length);
  if (length != 0 && data != nullptr) {
    std::memcpy(buffer->Data(), data, length);
  }
  v8::Local<v8::Uint8Array> array = v8::Uint8Array::New(buffer, 0, length);
  if (!sako_napi::WearBufferPrototype(env->Context(), array)) {
    return SetLastError(env, napi_generic_failure);
  }
  if (finalize_cb != nullptr) {
    NewReference(env, array, 0, data, finalize_cb, finalize_hint, true);
  }
  *result = JsValue(array);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL node_api_create_buffer_from_arraybuffer(
    napi_env env, napi_value arraybuffer, size_t byte_offset,
    size_t byte_length, napi_value* result) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, arraybuffer);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(arraybuffer);
  RETURN_STATUS_IF_FALSE(env, local->IsArrayBuffer(), napi_arraybuffer_expected);
  v8::Local<v8::Uint8Array> array =
      v8::Uint8Array::New(local.As<v8::ArrayBuffer>(), byte_offset, byte_length);
  if (!sako_napi::WearBufferPrototype(env->Context(), array)) {
    return SetLastError(env, napi_generic_failure);
  }
  *result = JsValue(array);
  return GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_is_buffer(napi_env env, napi_value value,
                                      bool* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, result);
  v8::Local<v8::Value> local = V8Value(value);
  *result = false;
  if (!local->IsUint8Array()) return napi_ok;
  v8::Local<v8::Object> prototype;
  if (!sako_napi::BufferPrototype(env->Context()).ToLocal(&prototype)) {
    return napi_ok;
  }
  *result = local.As<v8::Object>()->GetPrototype() == prototype;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_buffer_info(napi_env env, napi_value value,
                                            void** data, size_t* length) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  v8::Local<v8::Value> local = V8Value(value);
  RETURN_STATUS_IF_FALSE(env, local->IsTypedArray(), napi_invalid_arg);
  v8::Local<v8::TypedArray> array = local.As<v8::TypedArray>();
  if (data != nullptr) {
    *data = static_cast<uint8_t*>(array->Buffer()->Data()) + array->ByteOffset();
  }
  if (length != nullptr) *length = array->ByteLength();
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Promises.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_create_promise(napi_env env, napi_deferred* deferred,
                                           napi_value* promise) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, deferred);
  CHECK_ARG(env, promise);
  v8::Local<v8::Promise::Resolver> resolver;
  if (!v8::Promise::Resolver::New(env->Context()).ToLocal(&resolver)) {
    return GET_RETURN_STATUS(env);
  }
  *deferred = new napi_deferred__(env->isolate, resolver);
  *promise = JsValue(resolver->GetPromise());
  return GET_RETURN_STATUS(env);
}

namespace sako_napi {
namespace {

napi_status SettleDeferred(napi_env env, napi_deferred deferred,
                           napi_value value, bool resolve) {
  v8::Local<v8::Context> context = env->Context();
  v8::Local<v8::Promise::Resolver> resolver =
      deferred->resolver.Get(env->isolate);
  const v8::Maybe<bool> settled =
      resolve ? resolver->Resolve(context, V8Value(value))
              : resolver->Reject(context, V8Value(value));
  delete deferred;
  return settled.FromMaybe(false) ? napi_ok
                                  : SetLastError(env, napi_generic_failure);
}

}  // namespace
}  // namespace sako_napi

napi_status NAPI_CDECL napi_resolve_deferred(napi_env env,
                                             napi_deferred deferred,
                                             napi_value resolution) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, deferred);
  CHECK_ARG(env, resolution);
  const napi_status status =
      sako_napi::SettleDeferred(env, deferred, resolution, true);
  return status != napi_ok ? status : GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_reject_deferred(napi_env env, napi_deferred deferred,
                                            napi_value rejection) {
  NAPI_PREAMBLE(env);
  CHECK_ARG(env, deferred);
  CHECK_ARG(env, rejection);
  const napi_status status =
      sako_napi::SettleDeferred(env, deferred, rejection, false);
  return status != napi_ok ? status : GET_RETURN_STATUS(env);
}

napi_status NAPI_CDECL napi_is_promise(napi_env env, napi_value value,
                                       bool* is_promise) {
  NAPI_BASIC(env);
  CHECK_ARG(env, value);
  CHECK_ARG(env, is_promise);
  *is_promise = V8Value(value)->IsPromise();
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Async context. Sako has no async_hooks, so these carry the resource object
// and nothing else -- enough for addons that open a scope around a callback.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_async_init(napi_env env, napi_value async_resource,
                                       napi_value async_resource_name,
                                       napi_async_context* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, async_resource_name);
  CHECK_ARG(env, result);
  auto* context = new napi_async_context__();
  if (async_resource != nullptr) {
    context->resource.Reset(env->isolate, V8Value(async_resource));
  }
  *result = context;
  return napi_ok;
}

napi_status NAPI_CDECL napi_async_destroy(napi_env env,
                                          napi_async_context async_context) {
  NAPI_BASIC(env);
  CHECK_ARG(env, async_context);
  delete async_context;
  return napi_ok;
}

napi_status NAPI_CDECL napi_open_callback_scope(napi_env env,
                                                napi_value resource_object,
                                                napi_async_context context,
                                                napi_callback_scope* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, result);
  (void)resource_object;
  (void)context;
  *result = new napi_callback_scope__();
  return napi_ok;
}

napi_status NAPI_CDECL napi_close_callback_scope(napi_env env,
                                                 napi_callback_scope scope) {
  NAPI_BASIC(env);
  CHECK_ARG(env, scope);
  delete scope;
  return napi_ok;
}

napi_status NAPI_CDECL napi_make_callback(napi_env env,
                                          napi_async_context async_context,
                                          napi_value recv, napi_value func,
                                          size_t argc, const napi_value* argv,
                                          napi_value* result) {
  (void)async_context;
  // Microtasks are drained by the event loop after every batch of addon work,
  // so unlike Node this does not need to run a checkpoint of its own -- and
  // must not, because it can be reached from inside one.
  return napi_call_function(env, recv, func, argc, argv, result);
}

// ---------------------------------------------------------------------------
// Async work.
// ---------------------------------------------------------------------------

napi_status NAPI_CDECL napi_create_async_work(
    napi_env env, napi_value async_resource, napi_value async_resource_name,
    napi_async_execute_callback execute, napi_async_complete_callback complete,
    void* data, napi_async_work* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, execute);
  CHECK_ARG(env, result);
  auto* work = new napi_async_work__();
  work->env = env;
  work->execute = execute;
  work->complete = complete;
  work->data = data;
  if (async_resource != nullptr) {
    work->resource.Reset(env->isolate, V8Value(async_resource));
  }
  (void)async_resource_name;
  *result = work;
  return napi_ok;
}

napi_status NAPI_CDECL napi_delete_async_work(napi_env env,
                                              napi_async_work work) {
  NAPI_BASIC(env);
  CHECK_ARG(env, work);
  delete work;
  return napi_ok;
}

napi_status NAPI_CDECL napi_queue_async_work(node_api_basic_env env,
                                             napi_async_work work) {
  NAPI_BASIC(env);
  CHECK_ARG(env, work);
  RETURN_STATUS_IF_FALSE(env, !work->queued.exchange(true),
                         napi_generic_failure);
  sako_napi::IsolateState* state = env->state;
  // Counted before the job is handed over, so the loop cannot decide it has
  // nothing left to do between here and the worker picking it up.
  state->AddHandles(1);
  state->pool.Submit([work, state] {
    const bool cancelled = work->cancelled.load();
    if (!cancelled) {
      work->started.store(true);
      work->execute(work->env, work->data);
    }
    state->Post([work, state, cancelled] {
      if (work->complete != nullptr) {
        work->complete(work->env, cancelled ? napi_cancelled : napi_ok,
                       work->data);
      }
      work->queued.store(false);
      state->AddHandles(-1);
    });
  });
  return napi_ok;
}

napi_status NAPI_CDECL napi_cancel_async_work(node_api_basic_env env,
                                              napi_async_work work) {
  NAPI_BASIC(env);
  CHECK_ARG(env, work);
  // Only work that has not started can be cancelled, which is the same
  // guarantee Node gives.
  RETURN_STATUS_IF_FALSE(env, !work->started.load(), napi_generic_failure);
  work->cancelled.store(true);
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Threadsafe functions.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

/// Runs one queued threadsafe call. Always on the loop thread, inside a handle
/// scope the caller owns.
void InvokeThreadsafe(napi_threadsafe_function function, void* data) {
  napi_env env = function->env;
  v8::Local<v8::Function> callback;
  if (!function->function.IsEmpty()) {
    callback = function->function.Get(env->isolate);
  }
  if (function->call_js != nullptr) {
    function->call_js(env, callback.IsEmpty() ? nullptr : JsValue(callback),
                      function->context, data);
    return;
  }
  if (callback.IsEmpty()) return;
  v8::Local<v8::Context> context = env->Context();
  (void)callback->Call(context, v8::Undefined(env->isolate), 0, nullptr);
}

}  // namespace
}  // namespace sako_napi

napi_status NAPI_CDECL napi_create_threadsafe_function(
    napi_env env, napi_value func, napi_value async_resource,
    napi_value async_resource_name, size_t max_queue_size,
    size_t initial_thread_count, void* thread_finalize_data,
    napi_finalize thread_finalize_cb, void* context,
    napi_threadsafe_function_call_js call_js_cb,
    napi_threadsafe_function* result) {
  NAPI_BASIC(env);
  CHECK_ARG(env, async_resource_name);
  CHECK_ARG(env, result);
  RETURN_STATUS_IF_FALSE(env, initial_thread_count > 0, napi_invalid_arg);
  // Without a JavaScript function there has to be a C callback to run instead,
  // or a call would have nothing to do.
  RETURN_STATUS_IF_FALSE(env, func != nullptr || call_js_cb != nullptr,
                         napi_invalid_arg);
  (void)async_resource;

  auto owned = std::make_unique<napi_threadsafe_function__>();
  napi_threadsafe_function function = owned.get();
  function->env = env;
  if (func != nullptr) {
    v8::Local<v8::Value> callback = V8Value(func);
    RETURN_STATUS_IF_FALSE(env, callback->IsFunction(), napi_function_expected);
    function->function.Reset(env->isolate, callback.As<v8::Function>());
  }
  function->context = context;
  function->call_js = call_js_cb;
  function->max_queue_size = max_queue_size;
  function->thread_count = initial_thread_count;
  function->finalize_data = thread_finalize_data;
  function->finalize_cb = thread_finalize_cb;
  function->finalize_hint = context;

  sako_napi::IsolateState* state = env->state;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    state->functions.push_back(std::move(owned));
  }
  // A referenced threadsafe function holds the loop open, which is the whole
  // point: the thread that will call it has not started yet.
  state->AddHandles(1);
  *result = function;
  return napi_ok;
}

napi_status NAPI_CDECL napi_get_threadsafe_function_context(
    napi_threadsafe_function func, void** result) {
  if (func == nullptr || result == nullptr) return napi_invalid_arg;
  *result = func->context;
  return napi_ok;
}

napi_status NAPI_CDECL napi_call_threadsafe_function(
    napi_threadsafe_function func, void* data,
    napi_threadsafe_function_call_mode is_blocking) {
  if (func == nullptr) return napi_invalid_arg;
  sako_napi::IsolateState* state = func->env->state;
  {
    std::unique_lock<std::mutex> lock(state->mutex);
    if (func->closing || func->aborted) return napi_closing;
    if (func->max_queue_size > 0 && func->queue.size() >= func->max_queue_size) {
      if (is_blocking != napi_tsfn_blocking) return napi_queue_full;
      func->room.wait(lock, [func] {
        return func->closing || func->aborted ||
               func->queue.size() < func->max_queue_size;
      });
      if (func->closing || func->aborted) return napi_closing;
    }
    func->queue.push_back(data);
  }
  state->signal.notify_all();
  return napi_ok;
}

napi_status NAPI_CDECL
napi_acquire_threadsafe_function(napi_threadsafe_function func) {
  if (func == nullptr) return napi_invalid_arg;
  sako_napi::IsolateState* state = func->env->state;
  std::lock_guard<std::mutex> lock(state->mutex);
  if (func->closing || func->aborted) return napi_closing;
  func->thread_count += 1;
  return napi_ok;
}

napi_status NAPI_CDECL napi_release_threadsafe_function(
    napi_threadsafe_function func, napi_threadsafe_function_release_mode mode) {
  if (func == nullptr) return napi_invalid_arg;
  sako_napi::IsolateState* state = func->env->state;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    if (func->thread_count == 0) return napi_invalid_arg;
    func->thread_count -= 1;
    if (mode == napi_tsfn_abort) {
      func->aborted = true;
      func->queue.clear();
    }
    if (func->thread_count == 0) func->closing = true;
    func->room.notify_all();
  }
  state->signal.notify_all();
  return napi_ok;
}

napi_status NAPI_CDECL napi_ref_threadsafe_function(node_api_basic_env env,
                                                    napi_threadsafe_function func) {
  CHECK_ENV(env);
  if (func == nullptr) return napi_invalid_arg;
  sako_napi::IsolateState* state = env->state;
  bool changed = false;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    if (!func->referenced && !func->finished) {
      func->referenced = true;
      changed = true;
    }
  }
  if (changed) state->AddHandles(1);
  return napi_ok;
}

napi_status NAPI_CDECL napi_unref_threadsafe_function(
    node_api_basic_env env, napi_threadsafe_function func) {
  CHECK_ENV(env);
  if (func == nullptr) return napi_invalid_arg;
  sako_napi::IsolateState* state = env->state;
  bool changed = false;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    if (func->referenced && !func->finished) {
      func->referenced = false;
      changed = true;
    }
  }
  if (changed) state->AddHandles(-1);
  return napi_ok;
}

// ---------------------------------------------------------------------------
// Module registration.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

/// Set while an addon library is being loaded, so the deprecated
/// `napi_module_register` entry point -- which addons call from a static
/// constructor rather than returning anything -- knows which load it belongs
/// to.
thread_local napi_module* pending_module = nullptr;

}  // namespace
}  // namespace sako_napi

void NAPI_CDECL napi_module_register(napi_module* mod) {
  sako_napi::pending_module = mod;
}

// ---------------------------------------------------------------------------
// The runtime-facing surface declared in sako_napi.h.
// ---------------------------------------------------------------------------

namespace sako_napi {
namespace {

using RegisterFunction = napi_value (*)(napi_env, napi_value);

#if defined(_WIN32)

std::wstring WidePath(const std::filesystem::path& path) { return path.wstring(); }

void* OpenLibrary(const std::filesystem::path& path, std::string* error) {
  // The addon's own directory goes on the search path for its dependencies:
  // a napi-rs binding that ships helper DLLs next to itself finds them the way
  // it would under Node, which uses the same flag.
  HMODULE handle = LoadLibraryExW(WidePath(path).c_str(), nullptr,
                                  LOAD_WITH_ALTERED_SEARCH_PATH);
  if (handle == nullptr) {
    *error = "cannot load native addon " + path.string() + ": Windows error " +
             std::to_string(static_cast<unsigned long>(GetLastError()));
    return nullptr;
  }
  return handle;
}

void* FindSymbol(void* handle, const char* name) {
  return reinterpret_cast<void*>(
      GetProcAddress(static_cast<HMODULE>(handle), name));
}

#else

void* OpenLibrary(const std::filesystem::path& path, std::string* error) {
  void* handle = dlopen(path.c_str(), RTLD_LAZY | RTLD_LOCAL);
  if (handle == nullptr) {
    const char* reason = dlerror();
    *error = "cannot load native addon " + path.string() + ": " +
             (reason == nullptr ? "unknown error" : reason);
  }
  return handle;
}

void* FindSymbol(void* handle, const char* name) { return dlsym(handle, name); }

#endif

}  // namespace

bool LoadAddon(v8::Local<v8::Context> context, const std::filesystem::path& path,
               v8::Local<v8::Value>* exports, std::string* error) {
  v8::Isolate* isolate = v8::Isolate::GetCurrent();
  IsolateState* state = StateFor(isolate, true);

  pending_module = nullptr;
  void* handle = OpenLibrary(path, error);
  if (handle == nullptr) return false;

  auto entry = reinterpret_cast<RegisterFunction>(
      FindSymbol(handle, "napi_register_module_v1"));
  napi_module* legacy = pending_module;
  pending_module = nullptr;
  if (entry == nullptr && legacy != nullptr) entry = legacy->nm_register_func;
  if (entry == nullptr) {
    *error = "native addon has no Node-API entry point: " + path.string();
    return false;
  }

  auto owned = std::make_unique<napi_env__>(isolate, context, state,
                                            path.string());
  napi_env env = owned.get();
  state->envs.push_back(std::move(owned));

  v8::Local<v8::Object> module_exports = v8::Object::New(isolate);
  v8::TryCatch try_catch(isolate);
  napi_value returned = entry(env, JsValue(module_exports));
  if (try_catch.HasCaught()) {
    // Rethrowing keeps the addon's own error -- which is usually the useful
    // one -- instead of replacing it with a generic load failure.
    try_catch.ReThrow();
    *error = "native addon registration failed: " + path.string();
    return false;
  }
  *exports = returned == nullptr ? module_exports.As<v8::Value>()
                                 : V8Value(returned);
  return true;
}

bool RunTasks(v8::Local<v8::Context> context, bool* ran, std::string* error) {
  if (ran != nullptr) *ran = false;
  v8::Isolate* isolate = v8::Isolate::GetCurrent();
  IsolateState* state = StateFor(isolate, false);
  if (state == nullptr) return true;

  std::deque<std::function<void()>> tasks;
  std::vector<FinalizerCall> finalizers;
  std::vector<std::pair<napi_threadsafe_function, void*>> calls;
  std::vector<napi_threadsafe_function> finishing;
  {
    std::lock_guard<std::mutex> lock(state->mutex);
    if (!state->fatal_error.empty()) {
      *error = state->fatal_error;
      state->fatal_error.clear();
      return false;
    }
    tasks.swap(state->tasks);
    finalizers.swap(state->finalizers);
    for (const std::unique_ptr<napi_threadsafe_function__>& function :
         state->functions) {
      napi_threadsafe_function raw = function.get();
      if (raw->finished) continue;
      while (!raw->queue.empty()) {
        calls.emplace_back(raw, raw->queue.front());
        raw->queue.pop_front();
      }
      raw->room.notify_all();
      if (raw->closing && raw->queue.empty()) {
        raw->finished = true;
        finishing.push_back(raw);
      }
    }
  }

  const bool any = !tasks.empty() || !finalizers.empty() || !calls.empty() ||
                   !finishing.empty();
  if (!any) return true;
  if (ran != nullptr) *ran = true;

  v8::HandleScope handle_scope(isolate);
  v8::TryCatch try_catch(isolate);

  for (const FinalizerCall& call : finalizers) {
    if (call.callback != nullptr) {
      call.callback(call.env, call.data, call.hint);
    }
    if (call.owner != nullptr) {
      call.owner->finalized = true;
      if (call.owner->self_owned) delete call.owner;
    }
  }
  for (std::function<void()>& task : tasks) task();
  for (const auto& [function, data] : calls) InvokeThreadsafe(function, data);
  for (napi_threadsafe_function function : finishing) {
    if (function->finalize_cb != nullptr) {
      function->finalize_cb(function->env, function->finalize_data,
                            function->finalize_hint);
    }
    bool release = false;
    {
      std::lock_guard<std::mutex> lock(state->mutex);
      release = function->referenced;
      function->referenced = false;
    }
    if (release) state->AddHandles(-1);
  }

  isolate->PerformMicrotaskCheckpoint();

  if (try_catch.HasCaught()) {
    *error = DescribeException(isolate, context, try_catch.Exception());
    return false;
  }
  return true;
}

bool HasPendingWork(v8::Isolate* isolate) {
  IsolateState* state = StateFor(isolate, false);
  return state != nullptr && state->Busy();
}

void WaitForWork(v8::Isolate* isolate, uint32_t milliseconds) {
  IsolateState* state = StateFor(isolate, false);
  if (state == nullptr) return;
  std::unique_lock<std::mutex> lock(state->mutex);
  if (!state->tasks.empty() || !state->finalizers.empty()) return;
  state->signal.wait_for(lock, std::chrono::milliseconds(milliseconds), [state] {
    if (!state->tasks.empty() || !state->finalizers.empty()) return true;
    for (const std::unique_ptr<napi_threadsafe_function__>& function :
         state->functions) {
      if (!function->queue.empty() || (function->closing && !function->finished)) {
        return true;
      }
    }
    return false;
  });
}

void Shutdown(v8::Isolate* isolate) {
  IsolateState* state = nullptr;
  std::unique_ptr<IsolateState> owned;
  {
    std::lock_guard<std::mutex> lock(RegistryMutex());
    auto& map = Registry();
    auto found = map.find(isolate);
    if (found == map.end()) return;
    owned = std::move(found->second);
    map.erase(found);
    state = owned.get();
  }

  // Stop the workers before anything is torn down: a job still running would
  // otherwise reach into an environment that has gone away.
  state->pool.Stop();

  for (const std::unique_ptr<napi_env__>& env : state->envs) {
    // Reverse order, so an addon that registered a hook depending on an
    // earlier one still finds it. Object finalizers are deliberately not run:
    // the isolate is going away, and an addon finalizer that touches the heap
    // during teardown is the classic way to crash on exit.
    for (auto hook = env->cleanup_hooks.rbegin();
         hook != env->cleanup_hooks.rend(); ++hook) {
      hook->first(hook->second);
    }
    env->cleanup_hooks.clear();
    if (env->instance_data_finalizer != nullptr) {
      env->instance_data_finalizer(env.get(), env->instance_data,
                                   env->instance_data_hint);
      env->instance_data_finalizer = nullptr;
    }
  }
}

}  // namespace sako_napi
