// SPDX-License-Identifier: BSD-3-Clause

// A Node-API addon exercising the parts of the C surface a real binding uses:
// functions and strings, objects and arrays, buffers both owned and adopted,
// errors with codes, a class with native state wrapped on the instance, a
// promise resolved from work on another thread, and a threadsafe function
// called from one.

#include <stdlib.h>
#include <string.h>
#include <windows.h>

#include <node_api.h>

#define CHECK(call)                                                            \
  do {                                                                         \
    napi_status status_ = (call);                                              \
    if (status_ != napi_ok) {                                                  \
      const napi_extended_error_info* info_;                                   \
      napi_get_last_error_info(env, &info_);                                   \
      napi_throw_error(env, NULL, info_->error_message);                       \
      return NULL;                                                             \
    }                                                                          \
  } while (0)

static napi_value Add(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  double a = 0, b = 0;
  CHECK(napi_get_value_double(env, argv[0], &a));
  CHECK(napi_get_value_double(env, argv[1], &b));
  napi_value result;
  CHECK(napi_create_double(env, a + b, &result));
  return result;
}

static napi_value Shout(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  size_t needed = 0;
  CHECK(napi_get_value_string_utf8(env, argv[0], NULL, 0, &needed));
  char* text = (char*)malloc(needed + 1);
  size_t copied = 0;
  CHECK(napi_get_value_string_utf8(env, argv[0], text, needed + 1, &copied));
  for (size_t i = 0; i < copied; i++) {
    if (text[i] >= 97 && text[i] <= 122) text[i] = (char)(text[i] - 32);
  }
  napi_value result;
  CHECK(napi_create_string_utf8(env, text, copied, &result));
  free(text);
  return result;
}

static napi_value MakeObject(napi_env env, napi_callback_info info) {
  (void)info;
  napi_value object, name, list, item;
  CHECK(napi_create_object(env, &object));
  CHECK(napi_create_string_utf8(env, "sako", NAPI_AUTO_LENGTH, &name));
  CHECK(napi_set_named_property(env, object, "name", name));
  CHECK(napi_create_array_with_length(env, 3, &list));
  for (uint32_t i = 0; i < 3; i++) {
    CHECK(napi_create_uint32(env, i * i, &item));
    CHECK(napi_set_element(env, list, i, item));
  }
  CHECK(napi_set_named_property(env, object, "squares", list));
  return object;
}

static napi_value MakeBuffer(napi_env env, napi_callback_info info) {
  (void)info;
  void* data = NULL;
  napi_value buffer;
  CHECK(napi_create_buffer(env, 4, &data, &buffer));
  memcpy(data, "sako", 4);
  return buffer;
}

static void FreeExternal(napi_env env, void* data, void* hint) {
  (void)env;
  (void)hint;
  free(data);
}

static napi_value MakeExternalBuffer(napi_env env, napi_callback_info info) {
  (void)info;
  char* data = (char*)malloc(5);
  memcpy(data, "bytes", 5);
  napi_value buffer;
  CHECK(napi_create_external_buffer(env, 5, data, FreeExternal, NULL, &buffer));
  return buffer;
}

static napi_value Throws(napi_env env, napi_callback_info info) {
  (void)info;
  napi_throw_type_error(env, "ERR_SAKO", "addon said no");
  return NULL;
}

typedef struct {
  int32_t value;
} Counter;

static void FreeCounter(napi_env env, void* data, void* hint) {
  (void)env;
  (void)hint;
  free(data);
}

static napi_value CounterNew(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1], self;
  CHECK(napi_get_cb_info(env, info, &argc, argv, &self, NULL));
  Counter* counter = (Counter*)malloc(sizeof(Counter));
  counter->value = 0;
  if (argc > 0) napi_get_value_int32(env, argv[0], &counter->value);
  CHECK(napi_wrap(env, self, counter, FreeCounter, NULL, NULL));
  return self;
}

static napi_value CounterBump(napi_env env, napi_callback_info info) {
  napi_value self;
  size_t argc = 0;
  CHECK(napi_get_cb_info(env, info, &argc, NULL, &self, NULL));
  Counter* counter = NULL;
  CHECK(napi_unwrap(env, self, (void**)&counter));
  counter->value += 1;
  napi_value result;
  CHECK(napi_create_int32(env, counter->value, &result));
  return result;
}

typedef struct {
  napi_async_work work;
  napi_deferred deferred;
  double input;
  double output;
} Job;

static void RunJob(napi_env env, void* data) {
  (void)env;
  Job* job = (Job*)data;
  Sleep(10);
  job->output = job->input * 2;
}

static void FinishJob(napi_env env, napi_status status, void* data) {
  (void)status;
  Job* job = (Job*)data;
  napi_value result;
  napi_create_double(env, job->output, &result);
  napi_resolve_deferred(env, job->deferred, result);
  napi_delete_async_work(env, job->work);
  free(job);
}

static napi_value DoubleLater(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1];
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  Job* job = (Job*)malloc(sizeof(Job));
  job->output = 0;
  CHECK(napi_get_value_double(env, argv[0], &job->input));
  napi_value promise, name;
  CHECK(napi_create_promise(env, &job->deferred, &promise));
  CHECK(napi_create_string_utf8(env, "double", NAPI_AUTO_LENGTH, &name));
  CHECK(napi_create_async_work(env, NULL, name, RunJob, FinishJob, job,
                               &job->work));
  CHECK(napi_queue_async_work(env, job->work));
  return promise;
}

static DWORD WINAPI TicketThread(LPVOID parameter) {
  napi_threadsafe_function tsfn = (napi_threadsafe_function)parameter;
  for (intptr_t i = 1; i <= 3; i++) {
    Sleep(5);
    napi_call_threadsafe_function(tsfn, (void*)i, napi_tsfn_blocking);
  }
  napi_release_threadsafe_function(tsfn, napi_tsfn_release);
  return 0;
}

static void CallTicket(napi_env env, napi_value js_callback, void* context,
                       void* data) {
  (void)context;
  if (env == NULL || js_callback == NULL) return;
  napi_value undefined, argument;
  napi_get_undefined(env, &undefined);
  napi_create_int32(env, (int32_t)(intptr_t)data, &argument);
  napi_call_function(env, undefined, js_callback, 1, &argument, NULL);
}

static napi_value CountFromThread(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value argv[1], name;
  CHECK(napi_get_cb_info(env, info, &argc, argv, NULL, NULL));
  CHECK(napi_create_string_utf8(env, "tickets", NAPI_AUTO_LENGTH, &name));
  napi_threadsafe_function tsfn;
  CHECK(napi_create_threadsafe_function(env, argv[0], NULL, name, 0, 1, NULL,
                                        NULL, NULL, CallTicket, &tsfn));
  CreateThread(NULL, 0, TicketThread, tsfn, 0, NULL);
  napi_value undefined;
  CHECK(napi_get_undefined(env, &undefined));
  return undefined;
}

static napi_value Version(napi_env env, napi_callback_info info) {
  (void)info;
  uint32_t version = 0;
  CHECK(napi_get_version(env, &version));
  const napi_node_version* node_version = NULL;
  CHECK(napi_get_node_version(env, &node_version));
  napi_value result, value;
  CHECK(napi_create_object(env, &result));
  CHECK(napi_create_uint32(env, version, &value));
  CHECK(napi_set_named_property(env, result, "napi", value));
  CHECK(napi_create_string_utf8(env, node_version->release, NAPI_AUTO_LENGTH,
                                &value));
  CHECK(napi_set_named_property(env, result, "release", value));
  return result;
}

NAPI_MODULE_INIT() {
  napi_property_descriptor entries[] = {
      {"add", NULL, Add, NULL, NULL, NULL, napi_default_jsproperty, NULL},
      {"shout", NULL, Shout, NULL, NULL, NULL, napi_default_jsproperty, NULL},
      {"makeObject", NULL, MakeObject, NULL, NULL, NULL,
       napi_default_jsproperty, NULL},
      {"makeBuffer", NULL, MakeBuffer, NULL, NULL, NULL,
       napi_default_jsproperty, NULL},
      {"makeExternalBuffer", NULL, MakeExternalBuffer, NULL, NULL, NULL,
       napi_default_jsproperty, NULL},
      {"throws", NULL, Throws, NULL, NULL, NULL, napi_default_jsproperty, NULL},
      {"doubleLater", NULL, DoubleLater, NULL, NULL, NULL,
       napi_default_jsproperty, NULL},
      {"countFromThread", NULL, CountFromThread, NULL, NULL, NULL,
       napi_default_jsproperty, NULL},
      {"version", NULL, Version, NULL, NULL, NULL, napi_default_jsproperty,
       NULL},
  };
  if (napi_define_properties(env, exports,
                             sizeof(entries) / sizeof(entries[0]),
                             entries) != napi_ok) {
    return NULL;
  }

  napi_property_descriptor methods[] = {
      {"bump", NULL, CounterBump, NULL, NULL, NULL, napi_default_method, NULL},
  };
  napi_value counter;
  if (napi_define_class(env, "Counter", NAPI_AUTO_LENGTH, CounterNew, NULL, 1,
                        methods, &counter) != napi_ok) {
    return NULL;
  }
  if (napi_set_named_property(env, exports, "Counter", counter) != napi_ok) {
    return NULL;
  }
  return exports;
}
