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
#include <mutex>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <bcrypt.h>

#include "libplatform/libplatform.h"
#include "v8.h"
#include "bootstrap.generated.h"

extern "C" {
struct SakoNativeBytes {
  const uint8_t* data;
  size_t length;
};

struct SakoNativeHeader {
  SakoNativeBytes name;
  SakoNativeBytes value;
};

struct SakoNativeHttpResponse {
  uint16_t status;
  SakoNativeBytes reason;
  const SakoNativeHeader* headers;
  size_t header_count;
  SakoNativeBytes body;
};

using SakoNativeHttpHandler = int (*)(
    void*, SakoNativeBytes, SakoNativeBytes, SakoNativeBytes,
    const SakoNativeHeader*, size_t, SakoNativeHttpResponse*);

void* sako_http_server_new(uint16_t port, uint16_t* output_port, char* error,
                           size_t error_capacity);
void* sako_https_server_new(uint16_t port, SakoNativeBytes certificate,
                            SakoNativeBytes private_key, uint16_t* output_port,
                            char* error, size_t error_capacity);
int sako_http_server_tick(void* server, SakoNativeHttpHandler handler,
                          void* context, char* error, size_t error_capacity);
void sako_http_server_delete(void* server);
int sako_http_server_close(void* server);
int sako_http_server_stats(void* server, uint64_t* connections,
                           uint64_t* rejected_connections);
int sako_dns_resolve(SakoNativeBytes host, int family, char* output,
                     size_t output_capacity, char* error,
                     size_t error_capacity);
void* sako_process_spawn_sync(SakoNativeBytes executable,
                              const SakoNativeBytes* arguments,
                              size_t argument_count, SakoNativeBytes cwd,
                              char* error, size_t error_capacity);
int sako_process_output_status(const void* output);
SakoNativeBytes sako_process_output_stdout(const void* output);
SakoNativeBytes sako_process_output_stderr(const void* output);
void sako_process_output_delete(void* output);
void* sako_fetch_sync(SakoNativeBytes url, SakoNativeBytes method,
                      const SakoNativeHeader* headers, size_t header_count,
                      SakoNativeBytes body, char* error, size_t error_capacity);
uint16_t sako_fetch_output_status(const void* output);
SakoNativeBytes sako_fetch_output_status_text(const void* output);
SakoNativeBytes sako_fetch_output_url(const void* output);
size_t sako_fetch_output_header_count(const void* output);
SakoNativeBytes sako_fetch_output_header_name(const void* output, size_t index);
SakoNativeBytes sako_fetch_output_header_value(const void* output, size_t index);
SakoNativeBytes sako_fetch_output_body(const void* output);
void sako_fetch_output_delete(void* output);
}

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

std::filesystem::path ExtendedPath(const std::filesystem::path& path) {
  std::error_code error;
  std::filesystem::path absolute =
      path.is_absolute() ? path : std::filesystem::absolute(path, error);
  if (error) return path;
  absolute = absolute.lexically_normal();
  absolute.make_preferred();
  const std::wstring& native = absolute.native();
  if (native.starts_with(L"\\\\?\\")) return absolute;
  if (native.starts_with(L"\\\\")) {
    return std::filesystem::path(L"\\\\?\\UNC\\" + native.substr(2));
  }
  return std::filesystem::path(L"\\\\?\\" + native);
}

std::filesystem::path UserPath(const std::filesystem::path& path) {
  const std::wstring& native = path.native();
  if (native.starts_with(L"\\\\?\\UNC\\")) {
    return std::filesystem::path(L"\\\\" + native.substr(8));
  }
  if (native.starts_with(L"\\\\?\\")) {
    return std::filesystem::path(native.substr(4));
  }
  return path;
}

std::filesystem::path CanonicalPath(const std::filesystem::path& path,
                                    std::error_code& error) {
  return UserPath(std::filesystem::weakly_canonical(ExtendedPath(path), error));
}

bool IsRegularFile(const std::filesystem::path& path,
                   std::error_code& error) {
  const std::filesystem::path extended = ExtendedPath(path);
  const DWORD attributes = GetFileAttributesW(extended.native().c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    error = std::error_code(static_cast<int>(GetLastError()),
                            std::system_category());
    return false;
  }
  error.clear();
  return (attributes & FILE_ATTRIBUTE_DIRECTORY) == 0;
}

bool IsDirectory(const std::filesystem::path& path, std::error_code& error) {
  const std::filesystem::path extended = ExtendedPath(path);
  const DWORD attributes = GetFileAttributesW(extended.native().c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    error = std::error_code(static_cast<int>(GetLastError()),
                            std::system_category());
    return false;
  }
  error.clear();
  return (attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
}

std::string PathToUtf8(const std::filesystem::path& path) {
  return WideToUtf8(UserPath(path).native());
}

bool ReadFile(const std::filesystem::path& path, std::string* source) {
  std::ifstream input(ExtendedPath(path), std::ios::binary);
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

void WriteHandle(HANDLE output, const char* bytes, size_t length) {
  while (output != INVALID_HANDLE_VALUE && output != nullptr && length != 0) {
    const DWORD chunk = length > MAXDWORD ? MAXDWORD : static_cast<DWORD>(length);
    DWORD written = 0;
    if (!WriteFile(output, bytes, chunk, &written, nullptr) || written == 0) return;
    bytes += written;
    length -= written;
  }
}

void ThrowTypeError(v8::Isolate* isolate, const char* message);
bool ReadBytes(v8::Local<v8::Value> value, const uint8_t** bytes,
               size_t* length);

void WriteStream(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  if (info.Length() == 0) {
    info.GetReturnValue().Set(v8::True(isolate));
    return;
  }
  const uint8_t* bytes = nullptr;
  size_t length = 0;
  std::string text;
  if (info[0]->IsString()) {
    text = ToUtf8(isolate, info[0]);
    bytes = reinterpret_cast<const uint8_t*>(text.data());
    length = text.size();
  } else if (!ReadBytes(info[0], &bytes, &length)) {
    ThrowTypeError(isolate, "stream write needs a string or byte array");
    return;
  }
  const DWORD stream = info.Data()->Int32Value(isolate->GetCurrentContext())
                           .FromMaybe(STD_OUTPUT_HANDLE);
  WriteHandle(GetStdHandle(stream), reinterpret_cast<const char*>(bytes),
              length);
  info.GetReturnValue().Set(v8::True(isolate));
}

void IsTty(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const int descriptor =
      info.Length() == 0 ? -1 : info[0]->Int32Value(context).FromMaybe(-1);
  const DWORD stream = descriptor == 1   ? STD_OUTPUT_HANDLE
                       : descriptor == 2 ? STD_ERROR_HANDLE
                                         : STD_INPUT_HANDLE;
  DWORD mode = 0;
  info.GetReturnValue().Set(descriptor >= 0 &&
                            GetConsoleMode(GetStdHandle(stream), &mode) != 0);
}

void ResolveHost(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (info.Length() == 0 || !info[0]->IsString()) {
    ThrowTypeError(isolate, "DNS lookup needs a hostname");
    return;
  }
  const std::string host = ToUtf8(isolate, info[0]);
  if (host.empty() || host.size() > 253) {
    isolate->ThrowException(v8::Exception::RangeError(
        v8::String::NewFromUtf8Literal(isolate, "DNS hostname length is invalid")));
    return;
  }
  const int family = info.Length() > 1
                         ? info[1]->Int32Value(context).FromMaybe(-1)
                         : 0;
  char output[2048] = {};
  char error[512] = {};
  const int count = sako_dns_resolve(
      {reinterpret_cast<const uint8_t*>(host.data()), host.size()}, family,
      output, sizeof(output), error, sizeof(error));
  if (count < 0) {
    isolate->ThrowException(v8::Exception::Error(
        v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
    return;
  }
  v8::Local<v8::Array> addresses = v8::Array::New(isolate, count);
  std::string values(output);
  size_t start = 0;
  uint32_t index = 0;
  while (start <= values.size() && index < static_cast<uint32_t>(count)) {
    const size_t end = values.find('\n', start);
    const size_t length =
        end == std::string::npos ? values.size() - start : end - start;
    v8::Local<v8::String> address;
    if (!v8::String::NewFromUtf8(isolate, values.data() + start,
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(length))
             .ToLocal(&address) ||
        !addresses->Set(context, index++, address).FromMaybe(false)) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(isolate, "cannot materialize DNS result")));
      return;
    }
    if (end == std::string::npos) break;
    start = end + 1;
  }
  info.GetReturnValue().Set(addresses);
}

void SpawnSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumArguments = 256;
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (info.Length() == 0 || !info[0]->IsString() ||
      (info.Length() > 1 && !info[1]->IsArray())) {
    ThrowTypeError(isolate, "spawnSync needs an executable and argument array");
    return;
  }
  const std::string executable = ToUtf8(isolate, info[0]);
  v8::Local<v8::Array> values =
      info.Length() > 1 ? info[1].As<v8::Array>() : v8::Array::New(isolate);
  if (values->Length() > kMaximumArguments) {
    isolate->ThrowException(v8::Exception::RangeError(
        v8::String::NewFromUtf8Literal(isolate, "child argument limit exceeded")));
    return;
  }
  std::vector<std::string> argument_storage;
  std::vector<SakoNativeBytes> arguments;
  argument_storage.reserve(values->Length());
  arguments.reserve(values->Length());
  for (uint32_t index = 0; index < values->Length(); ++index) {
    v8::Local<v8::Value> value;
    if (!values->Get(context, index).ToLocal(&value)) return;
    argument_storage.push_back(ToUtf8(isolate, value));
  }
  for (const std::string& argument : argument_storage) {
    arguments.push_back({reinterpret_cast<const uint8_t*>(argument.data()),
                         argument.size()});
  }
  const std::string cwd = info.Length() > 2 && info[2]->IsString()
                              ? ToUtf8(isolate, info[2])
                              : std::string();
  char error[1024] = {};
  void* raw = sako_process_spawn_sync(
      {reinterpret_cast<const uint8_t*>(executable.data()), executable.size()},
      arguments.data(), arguments.size(),
      {reinterpret_cast<const uint8_t*>(cwd.data()), cwd.size()}, error,
      sizeof(error));
  if (raw == nullptr) {
    isolate->ThrowException(v8::Exception::Error(
        v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
    return;
  }
  std::unique_ptr<void, void (*)(void*)> output(raw,
                                                sako_process_output_delete);
  auto make_bytes = [isolate](SakoNativeBytes bytes)
      -> v8::MaybeLocal<v8::Uint8Array> {
    if (bytes.length != 0 && bytes.data == nullptr) return {};
    std::unique_ptr<v8::BackingStore> backing =
        v8::ArrayBuffer::NewBackingStore(isolate, bytes.length);
    if (bytes.length != 0) {
      std::memcpy(backing->Data(), bytes.data, bytes.length);
    }
    v8::Local<v8::ArrayBuffer> buffer =
        v8::ArrayBuffer::New(isolate, std::move(backing));
    return v8::Uint8Array::New(buffer, 0, bytes.length);
  };
  v8::Local<v8::Uint8Array> stdout_value;
  v8::Local<v8::Uint8Array> stderr_value;
  if (!make_bytes(sako_process_output_stdout(raw)).ToLocal(&stdout_value) ||
      !make_bytes(sako_process_output_stderr(raw)).ToLocal(&stderr_value)) {
    isolate->ThrowException(v8::Exception::Error(v8::String::NewFromUtf8Literal(
        isolate, "cannot materialize child output")));
    return;
  }
  v8::Local<v8::Object> result = v8::Object::New(isolate);
  const auto set = [&](const char* name, v8::Local<v8::Value> value) {
    return result
        ->Set(context, v8::String::NewFromUtf8(isolate, name).ToLocalChecked(),
              value)
        .FromMaybe(false);
  };
  if (!set("status",
           v8::Integer::New(isolate, sako_process_output_status(raw))) ||
      !set("stdout", stdout_value) || !set("stderr", stderr_value)) {
    return;
  }
  info.GetReturnValue().Set(result);
}

void FetchSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumHeaders = 128;
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (info.Length() < 4 || !info[0]->IsString() || !info[1]->IsString() ||
      !info[2]->IsArray()) {
    ThrowTypeError(isolate, "fetch needs URL, method, headers, and body");
    return;
  }
  const std::string url = ToUtf8(isolate, info[0]);
  const std::string method = ToUtf8(isolate, info[1]);
  v8::Local<v8::Array> pairs = info[2].As<v8::Array>();
  if (pairs->Length() % 2 != 0 || pairs->Length() / 2 > kMaximumHeaders) {
    isolate->ThrowException(v8::Exception::RangeError(
        v8::String::NewFromUtf8Literal(isolate, "fetch header limit exceeded")));
    return;
  }
  std::vector<std::string> names;
  std::vector<std::string> values;
  names.reserve(pairs->Length() / 2);
  values.reserve(pairs->Length() / 2);
  for (uint32_t index = 0; index < pairs->Length(); index += 2) {
    v8::Local<v8::Value> name;
    v8::Local<v8::Value> value;
    if (!pairs->Get(context, index).ToLocal(&name) ||
        !pairs->Get(context, index + 1).ToLocal(&value)) {
      return;
    }
    names.push_back(ToUtf8(isolate, name));
    values.push_back(ToUtf8(isolate, value));
  }
  std::vector<SakoNativeHeader> headers;
  headers.reserve(names.size());
  for (size_t index = 0; index < names.size(); ++index) {
    headers.push_back(
        {{reinterpret_cast<const uint8_t*>(names[index].data()), names[index].size()},
         {reinterpret_cast<const uint8_t*>(values[index].data()), values[index].size()}});
  }
  const uint8_t* body = nullptr;
  size_t body_length = 0;
  if (!ReadBytes(info[3], &body, &body_length)) {
    ThrowTypeError(isolate, "fetch body must be a byte array");
    return;
  }
  char error[1024] = {};
  void* raw = sako_fetch_sync(
      {reinterpret_cast<const uint8_t*>(url.data()), url.size()},
      {reinterpret_cast<const uint8_t*>(method.data()), method.size()},
      headers.data(), headers.size(), {body, body_length}, error, sizeof(error));
  if (raw == nullptr) {
    isolate->ThrowException(v8::Exception::Error(
        v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
    return;
  }
  std::unique_ptr<void, void (*)(void*)> output(raw, sako_fetch_output_delete);
  const auto make_string = [isolate](SakoNativeBytes bytes)
      -> v8::MaybeLocal<v8::String> {
    if ((bytes.length != 0 && bytes.data == nullptr) ||
        bytes.length > static_cast<size_t>(std::numeric_limits<int>::max())) {
      return {};
    }
    return v8::String::NewFromUtf8(
        isolate, reinterpret_cast<const char*>(bytes.data),
        v8::NewStringType::kNormal, static_cast<int>(bytes.length));
  };
  v8::Local<v8::String> status_text;
  v8::Local<v8::String> final_url;
  if (!make_string(sako_fetch_output_status_text(raw)).ToLocal(&status_text) ||
      !make_string(sako_fetch_output_url(raw)).ToLocal(&final_url)) {
    isolate->ThrowException(v8::Exception::Error(v8::String::NewFromUtf8Literal(
        isolate, "cannot materialize fetch response strings")));
    return;
  }
  const size_t header_count = sako_fetch_output_header_count(raw);
  if (header_count > kMaximumHeaders) return;
  v8::Local<v8::Array> response_headers =
      v8::Array::New(isolate, static_cast<int>(header_count * 2));
  for (size_t index = 0; index < header_count; ++index) {
    v8::Local<v8::String> name;
    v8::Local<v8::String> value;
    if (!make_string(sako_fetch_output_header_name(raw, index)).ToLocal(&name) ||
        !make_string(sako_fetch_output_header_value(raw, index)).ToLocal(&value) ||
        !response_headers->Set(context, static_cast<uint32_t>(index * 2), name)
             .FromMaybe(false) ||
        !response_headers
             ->Set(context, static_cast<uint32_t>(index * 2 + 1), value)
             .FromMaybe(false)) {
      return;
    }
  }
  const SakoNativeBytes response_body = sako_fetch_output_body(raw);
  if (response_body.length != 0 && response_body.data == nullptr) return;
  std::unique_ptr<v8::BackingStore> backing =
      v8::ArrayBuffer::NewBackingStore(isolate, response_body.length);
  if (response_body.length != 0) {
    std::memcpy(backing->Data(), response_body.data, response_body.length);
  }
  v8::Local<v8::ArrayBuffer> body_buffer =
      v8::ArrayBuffer::New(isolate, std::move(backing));
  v8::Local<v8::Object> result = v8::Object::New(isolate);
  const auto set = [&](const char* name, v8::Local<v8::Value> value) {
    return result
        ->Set(context, v8::String::NewFromUtf8(isolate, name).ToLocalChecked(),
              value)
        .FromMaybe(false);
  };
  if (!set("status", v8::Integer::NewFromUnsigned(
                         isolate, sako_fetch_output_status(raw))) ||
      !set("statusText", status_text) || !set("url", final_url) ||
      !set("headers", response_headers) ||
      !set("body", v8::Uint8Array::New(body_buffer, 0, response_body.length))) {
    return;
  }
  info.GetReturnValue().Set(result);
}

void Sha1(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumHashBytes = 256 * 1024 * 1024;
  v8::Isolate* isolate = info.GetIsolate();
  const uint8_t* bytes = nullptr;
  size_t length = 0;
  if (info.Length() == 0 || !ReadBytes(info[0], &bytes, &length)) {
    ThrowTypeError(isolate, "SHA-1 needs a byte array");
    return;
  }
  if (length > kMaximumHashBytes) {
    isolate->ThrowException(v8::Exception::RangeError(
        v8::String::NewFromUtf8Literal(isolate, "hash input exceeds byte limit")));
    return;
  }
  BCRYPT_ALG_HANDLE algorithm = nullptr;
  BCRYPT_HASH_HANDLE hash = nullptr;
  DWORD object_length = 0;
  DWORD hash_length = 0;
  DWORD written = 0;
  NTSTATUS status = BCryptOpenAlgorithmProvider(
      &algorithm, BCRYPT_SHA1_ALGORITHM, nullptr, 0);
  if (BCRYPT_SUCCESS(status)) {
    status = BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH,
                               reinterpret_cast<PUCHAR>(&object_length),
                               sizeof(object_length), &written, 0);
  }
  if (BCRYPT_SUCCESS(status)) {
    status = BCryptGetProperty(algorithm, BCRYPT_HASH_LENGTH,
                               reinterpret_cast<PUCHAR>(&hash_length),
                               sizeof(hash_length), &written, 0);
  }
  std::vector<uint8_t> object(object_length);
  std::vector<uint8_t> digest(hash_length);
  if (BCRYPT_SUCCESS(status)) {
    status = BCryptCreateHash(algorithm, &hash, object.data(), object_length,
                              nullptr, 0, 0);
  }
  size_t offset = 0;
  while (BCRYPT_SUCCESS(status) && offset < length) {
    const ULONG chunk = static_cast<ULONG>(
        std::min<size_t>(length - offset, std::numeric_limits<ULONG>::max()));
    status = BCryptHashData(hash, const_cast<PUCHAR>(bytes + offset), chunk, 0);
    offset += chunk;
  }
  if (BCRYPT_SUCCESS(status)) {
    status = BCryptFinishHash(hash, digest.data(), hash_length, 0);
  }
  if (hash != nullptr) BCryptDestroyHash(hash);
  if (algorithm != nullptr) BCryptCloseAlgorithmProvider(algorithm, 0);
  if (!BCRYPT_SUCCESS(status)) {
    isolate->ThrowException(v8::Exception::Error(
        v8::String::NewFromUtf8Literal(isolate, "Windows SHA-1 failed")));
    return;
  }
  std::unique_ptr<v8::BackingStore> backing =
      v8::ArrayBuffer::NewBackingStore(isolate, digest.size());
  std::memcpy(backing->Data(), digest.data(), digest.size());
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(isolate, std::move(backing));
  info.GetReturnValue().Set(v8::Uint8Array::New(buffer, 0, digest.size()));
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

void ThrowTypeError(v8::Isolate* isolate, const char* message) {
  isolate->ThrowException(v8::Exception::TypeError(
      v8::String::NewFromUtf8(isolate, message).ToLocalChecked()));
}

void EncodeUtf8(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (info.Length() == 0) {
    ThrowTypeError(isolate, "UTF-8 encoder needs a value");
    return;
  }
  v8::Local<v8::String> text;
  if (!info[0]->ToString(context).ToLocal(&text)) return;
  const size_t length = text->Utf8LengthV2(isolate);
  std::unique_ptr<v8::BackingStore> backing =
      v8::ArrayBuffer::NewBackingStore(isolate, static_cast<size_t>(length));
  text->WriteUtf8V2(isolate, static_cast<char*>(backing->Data()), length,
                    v8::String::WriteFlags::kReplaceInvalidUtf8);
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(isolate, std::move(backing));
  info.GetReturnValue().Set(v8::Uint8Array::New(buffer, 0, length));
}

bool ReadBytes(v8::Local<v8::Value> value, const uint8_t** bytes,
               size_t* length) {
  if (!value->IsArrayBufferView()) return false;
  v8::Local<v8::ArrayBufferView> view = value.As<v8::ArrayBufferView>();
  std::shared_ptr<v8::BackingStore> backing = view->Buffer()->GetBackingStore();
  *bytes = static_cast<const uint8_t*>(backing->Data()) + view->ByteOffset();
  *length = view->ByteLength();
  return true;
}

void DecodeUtf8(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const uint8_t* bytes = nullptr;
  size_t length = 0;
  if (info.Length() == 0 || !ReadBytes(info[0], &bytes, &length) ||
      length > static_cast<size_t>(std::numeric_limits<int>::max())) {
    ThrowTypeError(isolate, "UTF-8 decoder needs a byte array");
    return;
  }
  v8::Local<v8::String> text;
  if (v8::String::NewFromUtf8(isolate,
                              reinterpret_cast<const char*>(bytes),
                              v8::NewStringType::kNormal,
                              static_cast<int>(length))
          .ToLocal(&text)) {
    info.GetReturnValue().Set(text);
  }
}

std::filesystem::path CallbackPath(
    const v8::FunctionCallbackInfo<v8::Value>& info) {
  if (info.Length() == 0 || !info[0]->IsString()) return {};
  return std::filesystem::path(Utf8ToWide(ToUtf8(info.GetIsolate(), info[0])));
}

std::filesystem::path ValuePath(v8::Isolate* isolate,
                                v8::Local<v8::Value> value) {
  if (!value->IsString()) return {};
  return std::filesystem::path(Utf8ToWide(ToUtf8(isolate, value)));
}

void ThrowFileError(v8::Isolate* isolate, const std::string& operation,
                    const std::filesystem::path& path) {
  const std::string message =
      operation + " failed for " + PathToUtf8(path) + ": " +
      std::system_category().message(static_cast<int>(GetLastError()));
  isolate->ThrowException(v8::Exception::Error(
      v8::String::NewFromUtf8(isolate, message.data(),
                              v8::NewStringType::kNormal,
                              static_cast<int>(message.size()))
          .ToLocalChecked()));
}

void ThrowFileError(v8::Isolate* isolate, const std::string& operation,
                    const std::filesystem::path& path,
                    const std::error_code& error) {
  const std::string message = operation + " failed for " + PathToUtf8(path) +
                              ": " + error.message();
  isolate->ThrowException(v8::Exception::Error(
      v8::String::NewFromUtf8(isolate, message.data(),
                              v8::NewStringType::kNormal,
                              static_cast<int>(message.size()))
          .ToLocalChecked()));
}

void ReadFileSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumFileBytes = 256 * 1024 * 1024;
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "readFileSync needs a string path");
    return;
  }
  std::string bytes;
  if (!ReadFile(path, &bytes) || bytes.size() > kMaximumFileBytes) {
    ThrowFileError(isolate, "read", path);
    return;
  }
  const bool text =
      info.Length() > 1 && info[1]->IsString() &&
      (ToUtf8(isolate, info[1]) == "utf8" || ToUtf8(isolate, info[1]) == "utf-8");
  if (text) {
    v8::Local<v8::String> value;
    if (v8::String::NewFromUtf8(isolate, bytes.data(),
                                v8::NewStringType::kNormal,
                                static_cast<int>(bytes.size()))
            .ToLocal(&value)) {
      info.GetReturnValue().Set(value);
    }
    return;
  }
  std::unique_ptr<v8::BackingStore> backing =
      v8::ArrayBuffer::NewBackingStore(isolate, bytes.size());
  if (!bytes.empty()) std::memcpy(backing->Data(), bytes.data(), bytes.size());
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(isolate, std::move(backing));
  info.GetReturnValue().Set(v8::Uint8Array::New(buffer, 0, bytes.size()));
}

void WriteFileSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumFileBytes = 256 * 1024 * 1024;
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty() || info.Length() < 2) {
    ThrowTypeError(isolate, "writeFileSync needs a path and data");
    return;
  }
  const uint8_t* bytes = nullptr;
  size_t length = 0;
  std::string text;
  if (info[1]->IsString()) {
    text = ToUtf8(isolate, info[1]);
    bytes = reinterpret_cast<const uint8_t*>(text.data());
    length = text.size();
  } else if (!ReadBytes(info[1], &bytes, &length)) {
    ThrowTypeError(isolate, "writeFileSync data must be a string or byte array");
    return;
  }
  if (length > kMaximumFileBytes) {
    isolate->ThrowException(v8::Exception::RangeError(
        v8::String::NewFromUtf8Literal(isolate, "file exceeds byte limit")));
    return;
  }
  std::ofstream output(ExtendedPath(path), std::ios::binary | std::ios::trunc);
  if (!output ||
      (length != 0 &&
       !output.write(reinterpret_cast<const char*>(bytes), length))) {
    ThrowFileError(isolate, "write", path);
  }
}

void ExistsSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  const std::filesystem::path path = CallbackPath(info);
  std::error_code error;
  info.GetReturnValue().Set(!path.empty() &&
                            std::filesystem::exists(ExtendedPath(path), error));
}

void StatSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "statSync needs a string path");
    return;
  }
  const bool follow_links =
      info.Length() < 2 || info[1]->BooleanValue(isolate);
  std::error_code error;
  const std::filesystem::file_status status =
      follow_links ? std::filesystem::status(ExtendedPath(path), error)
                   : std::filesystem::symlink_status(ExtendedPath(path), error);
  if (error || status.type() == std::filesystem::file_type::not_found) {
    if (!error) error = std::make_error_code(std::errc::no_such_file_or_directory);
    ThrowFileError(isolate, "stat", path, error);
    return;
  }
  uintmax_t size = 0;
  if (std::filesystem::is_regular_file(status)) {
    size = std::filesystem::file_size(ExtendedPath(path), error);
    if (error) {
      ThrowFileError(isolate, "stat", path, error);
      return;
    }
  }
  const auto modified = std::filesystem::last_write_time(ExtendedPath(path), error);
  if (error) {
    ThrowFileError(isolate, "stat", path, error);
    return;
  }
  const double modified_milliseconds = static_cast<double>(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          modified.time_since_epoch())
          .count());
  v8::Local<v8::Object> result = v8::Object::New(isolate);
  const auto set = [&](const char* name, v8::Local<v8::Value> value) {
    return result
        ->Set(context, v8::String::NewFromUtf8(isolate, name).ToLocalChecked(),
              value)
        .FromMaybe(false);
  };
  if (!set("file", v8::Boolean::New(isolate, std::filesystem::is_regular_file(status))) ||
      !set("directory", v8::Boolean::New(isolate, std::filesystem::is_directory(status))) ||
      !set("symbolicLink", v8::Boolean::New(isolate, std::filesystem::is_symlink(status))) ||
      !set("size", v8::Number::New(isolate, static_cast<double>(size))) ||
      !set("mtimeMs", v8::Number::New(isolate, modified_milliseconds))) {
    return;
  }
  info.GetReturnValue().Set(result);
}

void ReadDirectorySync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumDirectoryEntries = 100'000;
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "readdirSync needs a string path");
    return;
  }
  std::error_code error;
  std::filesystem::directory_iterator entries(ExtendedPath(path), error);
  if (error) {
    ThrowFileError(isolate, "read directory", path, error);
    return;
  }
  std::vector<std::string> names;
  for (const auto& entry : entries) {
    if (names.size() >= kMaximumDirectoryEntries) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate,
                                         "directory entry limit exceeded")));
      return;
    }
    names.push_back(PathToUtf8(entry.path().filename()));
  }
  std::sort(names.begin(), names.end());
  v8::Local<v8::Array> output =
      v8::Array::New(isolate, static_cast<int>(names.size()));
  for (uint32_t index = 0; index < names.size(); ++index) {
    const std::string& name = names[index];
    v8::Local<v8::String> value;
    if (!v8::String::NewFromUtf8(isolate, name.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(name.size()))
             .ToLocal(&value) ||
        !output->Set(context, index, value).FromMaybe(false)) {
      return;
    }
  }
  info.GetReturnValue().Set(output);
}

void MakeDirectorySync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "mkdirSync needs a string path");
    return;
  }
  const bool recursive = info.Length() > 1 && info[1]->BooleanValue(isolate);
  std::error_code error;
  const bool created =
      recursive ? std::filesystem::create_directories(ExtendedPath(path), error)
                : std::filesystem::create_directory(ExtendedPath(path), error);
  if (!recursive && !created && !error) {
    error = std::make_error_code(std::errc::file_exists);
  }
  if (error) {
    ThrowFileError(isolate, "create directory", path, error);
    return;
  }
  info.GetReturnValue().Set(created);
}

void RemovePathSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "remove needs a string path");
    return;
  }
  const bool recursive = info.Length() > 1 && info[1]->BooleanValue(isolate);
  const bool force = info.Length() > 2 && info[2]->BooleanValue(isolate);
  const bool files_only = info.Length() > 3 && info[3]->BooleanValue(isolate);
  std::error_code error;
  if (files_only && IsDirectory(path, error)) {
    error = std::make_error_code(std::errc::operation_not_permitted);
  }
  if (error) {
    ThrowFileError(isolate, "remove", path, error);
    return;
  }
  const uintmax_t removed =
      recursive ? std::filesystem::remove_all(ExtendedPath(path), error)
                : std::filesystem::remove(ExtendedPath(path), error);
  if (removed == 0 && !force && !error) {
    error = std::make_error_code(std::errc::no_such_file_or_directory);
  }
  if (error && !(force && error == std::errc::no_such_file_or_directory)) {
    ThrowFileError(isolate, "remove", path, error);
    return;
  }
  info.GetReturnValue().Set(v8::Number::New(isolate, static_cast<double>(removed)));
}

void RenamePathSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path source = CallbackPath(info);
  const std::filesystem::path destination =
      info.Length() > 1 ? ValuePath(isolate, info[1]) : std::filesystem::path();
  if (source.empty() || destination.empty()) {
    ThrowTypeError(isolate, "renameSync needs source and destination paths");
    return;
  }
  std::error_code error;
  std::filesystem::rename(ExtendedPath(source), ExtendedPath(destination), error);
  if (error) ThrowFileError(isolate, "rename", source, error);
}

void LinkPathSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path source = CallbackPath(info);
  const std::filesystem::path destination =
      info.Length() > 1 ? ValuePath(isolate, info[1]) : std::filesystem::path();
  if (source.empty() || destination.empty()) {
    ThrowTypeError(isolate, "linkSync needs source and destination paths");
    return;
  }
  std::error_code error;
  std::filesystem::create_hard_link(ExtendedPath(source),
                                    ExtendedPath(destination), error);
  if (error) ThrowFileError(isolate, "create hard link", destination, error);
}

void SymlinkPathSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path target = CallbackPath(info);
  const std::filesystem::path link =
      info.Length() > 1 ? ValuePath(isolate, info[1]) : std::filesystem::path();
  if (target.empty() || link.empty()) {
    ThrowTypeError(isolate, "symlinkSync needs target and link paths");
    return;
  }
  const bool directory = info.Length() > 2 && info[2]->BooleanValue(isolate);
  std::error_code error;
  if (directory) {
    std::filesystem::create_directory_symlink(
        target.is_absolute() ? ExtendedPath(target) : target, ExtendedPath(link),
        error);
  } else {
    std::filesystem::create_symlink(
        target.is_absolute() ? ExtendedPath(target) : target, ExtendedPath(link),
        error);
  }
  if (error) ThrowFileError(isolate, "create symbolic link", link, error);
}

void ReadLinkSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "readlinkSync needs a string path");
    return;
  }
  std::error_code error;
  const std::filesystem::path target =
      UserPath(std::filesystem::read_symlink(ExtendedPath(path), error));
  if (error) {
    ThrowFileError(isolate, "read symbolic link", path, error);
    return;
  }
  const std::string value = PathToUtf8(target);
  info.GetReturnValue().Set(
      v8::String::NewFromUtf8(isolate, value.data(), v8::NewStringType::kNormal,
                              static_cast<int>(value.size()))
          .ToLocalChecked());
}

void RealPathSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "realpathSync needs a string path");
    return;
  }
  std::error_code error;
  const std::filesystem::path canonical =
      UserPath(std::filesystem::canonical(ExtendedPath(path), error));
  if (error) {
    ThrowFileError(isolate, "realpath", path, error);
    return;
  }
  const std::string value = PathToUtf8(canonical);
  info.GetReturnValue().Set(
      v8::String::NewFromUtf8(isolate, value.data(), v8::NewStringType::kNormal,
                              static_cast<int>(value.size()))
          .ToLocalChecked());
}

void ProcessCwd(const v8::FunctionCallbackInfo<v8::Value>& info) {
  const DWORD required = GetCurrentDirectoryW(0, nullptr);
  std::wstring path(required, L'\0');
  const DWORD written =
      required == 0 ? 0 : GetCurrentDirectoryW(required, path.data());
  if (written == 0) {
    ThrowFileError(info.GetIsolate(), "read current directory", {});
    return;
  }
  path.resize(written);
  const std::string utf8 = WideToUtf8(path);
  info.GetReturnValue().Set(
      v8::String::NewFromUtf8(info.GetIsolate(), utf8.data(),
                              v8::NewStringType::kNormal,
                              static_cast<int>(utf8.size()))
          .ToLocalChecked());
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

std::string FormatRejection(v8::Isolate* isolate,
                            v8::Local<v8::Context> context,
                            v8::Local<v8::Value> reason) {
  if (reason->IsObject()) {
    v8::Local<v8::Value> stack;
    if (reason.As<v8::Object>()
            ->Get(context,
                  v8::String::NewFromUtf8Literal(isolate, "stack"))
            .ToLocal(&stack) &&
        stack->IsString()) {
      return ToUtf8(isolate, stack);
    }
  }
  return ToUtf8(isolate, reason);
}

class Engine {
 public:
  bool Initialize(const char* executable_path, const char* icu_data_path,
                  std::string* error) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (initialized_) return true;
    if (!v8::V8::InitializeICUDefaultLocation(executable_path, icu_data_path)) {
      *error = std::string("failed to initialize ICU from ") + icu_data_path;
      return false;
    }
    platform_ = v8::platform::NewDefaultPlatform();
    if (!platform_) {
      *error = "failed to create the V8 platform";
      return false;
    }
    v8::V8::InitializePlatform(platform_.get());
    if (!v8::V8::Initialize()) {
      *error = "failed to initialize V8";
      v8::V8::DisposePlatform();
      platform_.reset();
      return false;
    }
    initialized_ = true;
    return true;
  }

  ~Engine() {
    if (initialized_) {
      v8::V8::Dispose();
      v8::V8::DisposePlatform();
    }
  }

  v8::Platform* platform() const { return platform_.get(); }

 private:
  std::mutex mutex_;
  std::unique_ptr<v8::Platform> platform_;
  bool initialized_ = false;
};

Engine& GetEngine() {
  static Engine engine;
  return engine;
}

class Runtime {
 public:
  static std::unique_ptr<Runtime> Create(const char* executable_path,
                                         const char* icu_data_path,
                                         std::string* error) {
    auto runtime = std::unique_ptr<Runtime>(new Runtime());

    Engine& engine = GetEngine();
    if (!engine.Initialize(executable_path, icu_data_path, error)) return nullptr;
    runtime->platform_ = engine.platform();

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
    runtime->isolate_->SetHostImportModuleDynamicallyCallback(
        ImportModuleDynamically);
    if (!runtime->InitializeContext(error)) return nullptr;
    return runtime;
  }

  ~Runtime() {
    for (auto& [descriptor, file] : file_descriptors_) {
      (void)descriptor;
      if (file.handle != INVALID_HANDLE_VALUE) CloseHandle(file.handle);
    }
    file_descriptors_.clear();
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
        for (auto& [id, module] : synthetic_commonjs_) {
          (void)id;
          module.exports.Reset();
        }
        synthetic_commonjs_.clear();
        for (auto& [id, binding] : http_servers_) {
          (void)id;
          binding->handler.Reset();
          sako_http_server_delete(binding->server);
          binding->server = nullptr;
        }
        http_servers_.clear();
        context_.Reset();
        isolate_->SetData(0, nullptr);
      }
      v8::platform::NotifyIsolateShutdown(platform_, isolate_);
      isolate_->Dispose();
      isolate_ = nullptr;
    }
    allocator_.reset();
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
        CanonicalPath(wide_path, path_error);
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
        *error = FormatRejection(isolate_, context, promise->Result());
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
        CanonicalPath(wide_path, path_error);
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
                   uint64_t* timers, uint64_t* external_memory,
                   uint64_t* http_servers, uint64_t* sockets,
                   uint64_t* http_buffer_bytes, uint64_t* native_memory_bytes,
                   uint64_t* module_cache_entries,
                   uint64_t* queued_operations) const {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HeapStatistics statistics;
    isolate_->GetHeapStatistics(&statistics);
    *heap_used = statistics.used_heap_size();
    *heap_committed = statistics.total_heap_size();
    *heap_limit = statistics.heap_size_limit();
    *external_memory = statistics.external_memory();
    uint64_t handles =
        (context_.IsEmpty() ? 0 : 1) + modules_.size() +
        commonjs_modules_.size() + synthetic_commonjs_.size() +
        http_servers_.size();
    for (const auto& [id, timer] : timers_) {
      (void)id;
      handles += 1 + timer.arguments.size();
    }
    *persistent_handles = handles;
    *timers = timers_.size();
    *http_servers = http_servers_.size();
    *sockets = 0;
    *http_buffer_bytes = 0;
    *module_cache_entries =
        modules_.size() + commonjs_modules_.size() + synthetic_commonjs_.size();
    *queued_operations = closing_http_servers_.size();
    *native_memory_bytes = module_source_bytes_ +
                           timers_.size() * sizeof(Timer) +
                           http_servers_.size() * sizeof(HttpBinding);
    for (const auto& [id, binding] : http_servers_) {
      (void)id;
      uint64_t connections = 0;
      uint64_t rejected = 0;
      if (sako_http_server_stats(binding->server, &connections, &rejected) == 0) {
        *sockets += connections;
      }
      *http_buffer_bytes += binding->response_reason.capacity() +
                            binding->response_body.capacity();
      for (const auto& name : binding->response_header_names) {
        *http_buffer_bytes += name.capacity();
      }
      for (const auto& value : binding->response_header_values) {
        *http_buffer_bytes += value.capacity();
      }
      *http_buffer_bytes += binding->response_headers.capacity() *
                            sizeof(SakoNativeHeader);
    }
    *native_memory_bytes += *http_buffer_bytes;
  }

 private:
  static constexpr size_t kMaximumTimers = 65'536;
  static constexpr size_t kMaximumModules = 4'096;
  static constexpr size_t kMaximumModuleBytes = 64 * 1024 * 1024;
  static constexpr size_t kMaximumHttpServers = 64;
  static constexpr size_t kMaximumFileDescriptors = 1'024;

  struct FileDescriptor {
    HANDLE handle = INVALID_HANDLE_VALUE;
    bool append = false;
  };

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

  struct SyntheticCommonJs {
    v8::Global<v8::Value> exports;
    std::vector<std::string> names;
  };

  struct HttpBinding {
    Runtime* runtime = nullptr;
    void* server = nullptr;
    v8::Global<v8::Function> handler;
    std::string response_reason;
    std::string response_body;
    std::vector<std::string> response_header_names;
    std::vector<std::string> response_header_values;
    std::vector<SakoNativeHeader> response_headers;
    bool closing = false;
    bool secure = false;
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
        !InstallFunction(context, "queueMicrotask", QueueMicrotask, data) ||
        !InstallFunction(context, "__sakoEncodeUtf8", EncodeUtf8,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoDecodeUtf8", DecodeUtf8,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoReadFileSync", ReadFileSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoWriteFileSync", WriteFileSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoExistsSync", ExistsSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoStatSync", StatSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoReadDirectorySync", ReadDirectorySync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoMakeDirectorySync", MakeDirectorySync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoRemovePathSync", RemovePathSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoRenamePathSync", RenamePathSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoLinkPathSync", LinkPathSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoSymlinkPathSync", SymlinkPathSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoReadLinkSync", ReadLinkSync,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoRealPathSync", RealPathSync,
                          v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoOpenSync", OpenSync, data) ||
        !InstallFunction(context, "__sakoCloseSync", CloseSync, data) ||
        !InstallFunction(context, "__sakoReadSync", ReadSync, data) ||
        !InstallFunction(context, "__sakoWriteSync", WriteSync, data) ||
        !InstallFunction(context, "__sakoIsTty", IsTty,
                          v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoResolveHost", ResolveHost,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoSpawnSync", SpawnSync,
                          v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoFetchSync", FetchSync,
                          v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoSha1", Sha1,
                         v8::Undefined(isolate_)) ||
        !InstallFunction(context, "__sakoHttpListen", HttpListen, data) ||
        !InstallFunction(context, "__sakoHttpsListen", HttpsListen, data) ||
        !InstallFunction(context, "__sakoHttpClose", HttpClose, data) ||
        !InstallFunction(context, "__sakoHttpAddress", HttpAddress, data)) {
      *error = "failed to install runtime scheduling globals";
      return false;
    }

    const DWORD cwd_length = GetCurrentDirectoryW(0, nullptr);
    std::wstring cwd(cwd_length, L'\0');
    const DWORD cwd_written =
        cwd_length == 0 ? 0 : GetCurrentDirectoryW(cwd_length, cwd.data());
    if (cwd_written != 0) cwd.resize(cwd_written);
    const std::string cwd_utf8 = WideToUtf8(cwd);
    if (cwd_written == 0 ||
        !Set(context, context->Global(), "__sakoCwd",
             v8::String::NewFromUtf8(isolate_, cwd_utf8.data(),
                                     v8::NewStringType::kNormal,
                                     static_cast<int>(cwd_utf8.size()))
                 .ToLocalChecked()) ||
        !RunBootstrap(context, error)) {
      if (error->empty()) *error = "failed to install runtime bootstrap";
      return false;
    }

    context_.Reset(isolate_, context);
    return true;
  }

  bool RunBootstrap(v8::Local<v8::Context> context, std::string* error) {
    v8::TryCatch try_catch(isolate_);
    v8::Local<v8::String> source;
    if (!v8::String::NewFromUtf8(isolate_,
                                 reinterpret_cast<const char*>(kSakoBootstrap),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(sizeof(kSakoBootstrap) - 1))
             .ToLocal(&source)) {
      *error = "runtime bootstrap exceeds V8 string limits";
      return false;
    }
    v8::Local<v8::String> name =
        v8::String::NewFromUtf8Literal(isolate_, "[sako:bootstrap]");
    v8::ScriptOrigin origin(name);
    v8::Local<v8::Script> script;
    v8::Local<v8::Value> ignored;
    if (!v8::Script::Compile(context, source, &origin).ToLocal(&script) ||
        !script->Run(context).ToLocal(&ignored)) {
      *error = FormatException(isolate_, context, try_catch);
      return false;
    }
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
    v8::Local<v8::Module> builtin;
    std::string builtin_error;
    if (runtime->CompileSyntheticBuiltin(context, request, &builtin,
                                         &builtin_error)) {
      return builtin;
    }
    if (request.starts_with("node:")) {
      if (!isolate->HasPendingException()) {
        isolate->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8(isolate, builtin_error.data(),
                                    v8::NewStringType::kNormal,
                                    static_cast<int>(builtin_error.size()))
                .ToLocalChecked()));
      }
      return {};
    }
    std::filesystem::path resolved;
    std::string message;
    if (!runtime->ResolvePath(context, request, referrer_name, &resolved,
                              &message)) {
      if (!isolate->HasPendingException()) {
        isolate->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8(isolate, message.data(),
                                    v8::NewStringType::kNormal,
                                    static_cast<int>(message.size()))
                .ToLocalChecked()));
      }
      return {};
    }

    v8::Local<v8::Module> module;
    const bool compiled = runtime->IsCommonJsPath(context, resolved)
                              ? runtime->CompileSyntheticCommonJs(
                                    context, resolved, &module, &message)
                              : runtime->CompileModule(context, resolved,
                                                       &module, &message);
    if (!compiled) {
      if (!isolate->HasPendingException()) {
        isolate->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8(isolate, message.data(),
                                    v8::NewStringType::kNormal,
                                    static_cast<int>(message.size()))
                .ToLocalChecked()));
      }
      return {};
    }
    return module;
  }

  static v8::MaybeLocal<v8::Promise> ImportModuleDynamically(
      v8::Local<v8::Context> context,
      v8::Local<v8::Data> host_defined_options,
      v8::Local<v8::Value> resource_name,
      v8::Local<v8::String> specifier,
      v8::Local<v8::FixedArray> import_attributes) {
    (void)host_defined_options;
    (void)import_attributes;
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    Runtime* runtime = static_cast<Runtime*>(isolate->GetData(0));
    if (runtime == nullptr) return {};

    v8::Local<v8::Promise::Resolver> resolver;
    if (!v8::Promise::Resolver::New(context).ToLocal(&resolver)) return {};
    v8::TryCatch try_catch(isolate);
    const std::string request = ToUtf8(isolate, specifier);
    const std::string referrer = ToUtf8(isolate, resource_name);
    std::string error;
    v8::Local<v8::Module> module;
    bool loaded = false;
    loaded = runtime->CompileSyntheticBuiltin(context, request, &module, &error);
    if (!loaded && !request.starts_with("node:")) {
      std::filesystem::path resolved;
      if (runtime->ResolvePath(context, request, referrer, &resolved, &error)) {
        loaded = runtime->IsCommonJsPath(context, resolved)
                     ? runtime->CompileSyntheticCommonJs(context, resolved,
                                                         &module, &error)
                     : runtime->CompileModule(context, resolved, &module,
                                              &error);
      }
    }
    if (!loaded) {
      return RejectDynamicImport(context, resolver, try_catch, error);
    }
    if (module->GetStatus() == v8::Module::kUninstantiated &&
        !module->InstantiateModule(context, ResolveModule).FromMaybe(false)) {
      return RejectDynamicImport(context, resolver, try_catch,
                                 "failed to instantiate dynamic import");
    }
    if (module->GetStatus() == v8::Module::kErrored) {
      return RejectDynamicImport(context, resolver, try_catch,
                                 "dynamic import module is errored",
                                 module->GetException());
    }
    if (module->GetStatus() == v8::Module::kEvaluated) {
      if (!resolver->Resolve(context, module->GetModuleNamespace())
               .FromMaybe(false)) {
        return {};
      }
      return resolver->GetPromise();
    }
    if (module->GetStatus() != v8::Module::kInstantiated) {
      return RejectDynamicImport(context, resolver, try_catch,
                                 "dynamic import module is already evaluating");
    }

    v8::Local<v8::Value> evaluation;
    if (!module->Evaluate(context).ToLocal(&evaluation) ||
        !evaluation->IsPromise()) {
      return RejectDynamicImport(context, resolver, try_catch,
                                 "failed to evaluate dynamic import");
    }
    v8::Local<v8::Function> namespace_callback;
    if (!v8::Function::New(context, ReturnCallbackData,
                           module->GetModuleNamespace())
             .ToLocal(&namespace_callback)) {
      return {};
    }
    return evaluation.As<v8::Promise>()->Then(context, namespace_callback);
  }

  static v8::MaybeLocal<v8::Promise> RejectDynamicImport(
      v8::Local<v8::Context> context,
      v8::Local<v8::Promise::Resolver> resolver, v8::TryCatch& try_catch,
      const std::string& message,
      v8::Local<v8::Value> explicit_reason = v8::Local<v8::Value>()) {
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    v8::Local<v8::Value> reason = explicit_reason;
    if (reason.IsEmpty() && try_catch.HasCaught()) reason = try_catch.Exception();
    if (reason.IsEmpty()) {
      const std::string fallback =
          message.empty() ? "dynamic import failed" : message;
      reason = v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, fallback.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(fallback.size()))
              .ToLocalChecked());
    }
    if (!resolver->Reject(context, reason).FromMaybe(false)) return {};
    return resolver->GetPromise();
  }

  static void ReturnCallbackData(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    info.GetReturnValue().Set(info.Data());
  }

  bool IsCommonJsPath(v8::Local<v8::Context> context,
                      const std::filesystem::path& path) {
    if (path.extension() == L".cjs" || path.extension() == L".json") {
      return true;
    }
    if (path.extension() == L".mjs") return false;
    std::filesystem::path directory = path.parent_path();
    while (!directory.empty()) {
      v8::Local<v8::Object> manifest;
      if (ReadJsonObject(context, directory / L"package.json", &manifest)) {
        v8::Local<v8::Value> type;
        return !(GetProperty(context, manifest, "type", &type) &&
                 type->IsString() && ToUtf8(isolate_, type) == "module");
      }
      const auto parent = directory.parent_path();
      if (parent == directory) break;
      directory = parent;
    }
    return true;
  }

  bool CompileSyntheticCommonJs(v8::Local<v8::Context> context,
                                const std::filesystem::path& path,
                                v8::Local<v8::Module>* output,
                                std::string* error) {
    const std::string cache_key = "commonjs:" + PathToUtf8(path);
    auto cached = modules_.find(cache_key);
    if (cached != modules_.end()) {
      *output = cached->second.Get(isolate_);
      return true;
    }
    if (modules_.size() >= kMaximumModules) {
      *error = "module cache capacity exceeded";
      return false;
    }
    v8::Local<v8::Value> exports;
    if (!LoadCommonJs(context, path, &exports, error)) return false;
    return CompileSyntheticValue(context, cache_key, exports, output, error);
  }

  bool CompileSyntheticBuiltin(v8::Local<v8::Context> context,
                               const std::string& request,
                               v8::Local<v8::Module>* output,
                               std::string* error) {
    const std::string cache_key = "builtin:" + request;
    auto cached = modules_.find(cache_key);
    if (cached != modules_.end()) {
      *output = cached->second.Get(isolate_);
      return true;
    }
    v8::Local<v8::Value> exports;
    if (!LoadBuiltin(context, request, &exports)) {
      *error = "unsupported built-in module: " + request;
      return false;
    }
    return CompileSyntheticValue(context, cache_key, exports, output, error);
  }

  bool CompileSyntheticValue(v8::Local<v8::Context> context,
                             const std::string& cache_key,
                             v8::Local<v8::Value> exports,
                             v8::Local<v8::Module>* output,
                             std::string* error) {
    auto cached = modules_.find(cache_key);
    if (cached != modules_.end()) {
      *output = cached->second.Get(isolate_);
      return true;
    }
    if (modules_.size() >= kMaximumModules) {
      *error = "module cache capacity exceeded";
      return false;
    }
    std::vector<std::string> names = {"default"};
    if (exports->IsObject()) {
      v8::Local<v8::Array> properties;
      if (!exports.As<v8::Object>()
               ->GetOwnPropertyNames(context)
               .ToLocal(&properties)) {
        *error = "cannot enumerate synthetic module exports: " + cache_key;
        return false;
      }
      for (uint32_t index = 0; index < properties->Length(); ++index) {
        v8::Local<v8::Value> property;
        if (properties->Get(context, index).ToLocal(&property) &&
            property->IsString()) {
          const std::string name = ToUtf8(isolate_, property);
          if (name != "default") names.push_back(name);
        }
      }
    }

    std::vector<v8::Local<v8::String>> export_names;
    export_names.reserve(names.size());
    for (const std::string& name : names) {
      v8::Local<v8::String> export_name;
      if (!v8::String::NewFromUtf8(isolate_, name.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(name.size()))
               .ToLocal(&export_name)) {
        *error = "synthetic export name exceeds V8 string limits";
        return false;
      }
      export_names.push_back(export_name);
    }
    v8::Local<v8::String> module_name =
        v8::String::NewFromUtf8(isolate_, cache_key.data(),
                                v8::NewStringType::kNormal,
                                static_cast<int>(cache_key.size()))
            .ToLocalChecked();
    v8::Local<v8::Module> module = v8::Module::CreateSyntheticModule(
        isolate_, module_name,
        v8::MemorySpan<const v8::Local<v8::String>>(export_names.data(),
                                                    export_names.size()),
        EvaluateSyntheticCommonJs);
    const int identity = module->GetIdentityHash();
    synthetic_commonjs_.emplace(
        identity,
        SyntheticCommonJs{v8::Global<v8::Value>(isolate_, exports), names});
    modules_.emplace(cache_key, v8::Global<v8::Module>(isolate_, module));
    *output = module;
    return true;
  }

  static v8::MaybeLocal<v8::Value> EvaluateSyntheticCommonJs(
      v8::Local<v8::Context> context, v8::Local<v8::Module> module) {
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    Runtime* runtime = static_cast<Runtime*>(isolate->GetData(0));
    if (runtime == nullptr) return {};
    auto found = runtime->synthetic_commonjs_.find(module->GetIdentityHash());
    if (found == runtime->synthetic_commonjs_.end()) return {};
    v8::Local<v8::Value> exports = found->second.exports.Get(isolate);
    for (const std::string& name : found->second.names) {
      v8::Local<v8::String> export_name =
          v8::String::NewFromUtf8(isolate, name.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(name.size()))
              .ToLocalChecked();
      v8::Local<v8::Value> value = exports;
      if (name != "default" &&
          !exports.As<v8::Object>()
               ->Get(context, export_name)
               .ToLocal(&value)) {
        return {};
      }
      if (!module
               ->SetSyntheticModuleExport(isolate, export_name, value)
               .FromMaybe(false)) {
        return {};
      }
    }
    v8::Local<v8::Promise::Resolver> resolver;
    if (!v8::Promise::Resolver::New(context).ToLocal(&resolver) ||
        !resolver->Resolve(context, v8::Undefined(isolate)).FromMaybe(false)) {
      return {};
    }
    return resolver->GetPromise();
  }

  bool LoadEsModuleNamespace(v8::Local<v8::Context> context,
                             const std::filesystem::path& path,
                             v8::Local<v8::Value>* output,
                             std::string* error) {
    v8::Local<v8::Module> module;
    if (!CompileModule(context, path, &module, error)) return false;
    if (module->GetStatus() == v8::Module::kUninstantiated &&
        !module->InstantiateModule(context, ResolveModule).FromMaybe(false)) {
      *error = "failed to instantiate ES module: " + PathToUtf8(path);
      return false;
    }
    if (module->GetStatus() == v8::Module::kInstantiated) {
      v8::Local<v8::Value> evaluation;
      if (!module->Evaluate(context).ToLocal(&evaluation)) {
        *error = "failed to evaluate ES module: " + PathToUtf8(path);
        return false;
      }
      isolate_->PerformMicrotaskCheckpoint();
      if (evaluation->IsPromise() &&
          evaluation.As<v8::Promise>()->State() ==
              v8::Promise::PromiseState::kPending) {
        *error = "require cannot load an ES module with pending top-level await";
        return false;
      }
    }
    if (module->GetStatus() == v8::Module::kErrored) {
      isolate_->ThrowException(module->GetException());
      return false;
    }
    *output = module->GetModuleNamespace();
    return true;
  }

  bool ResolvePath(v8::Local<v8::Context> context,
                   const std::string& request, const std::string& referrer,
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

    if (request.starts_with('#')) {
      return ResolvePackageImport(context, request, referrer, "import", output,
                                  error);
    }

    std::filesystem::path candidate(request_wide);
    if (!candidate.is_absolute()) {
      if (!(request.starts_with("./") || request.starts_with("../") ||
            request.starts_with(".\\") || request.starts_with("..\\"))) {
        return ResolvePackage(context, request, referrer, "import", output,
                              error);
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
      if (IsRegularFile(path, status_error)) {
        std::error_code canonical_error;
        *output = CanonicalPath(path, canonical_error);
        if (!canonical_error) return true;
      }
    }
    *error = "module not found: " + request + " imported from " + referrer;
    return false;
  }

  bool ReadJsonObject(v8::Local<v8::Context> context,
                      const std::filesystem::path& path,
                      v8::Local<v8::Object>* output) {
    std::string source;
    if (!ReadFile(path, &source)) return false;
    v8::Local<v8::String> json;
    v8::Local<v8::Value> value;
    if (!v8::String::NewFromUtf8(isolate_, source.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(source.size()))
             .ToLocal(&json) ||
        !v8::JSON::Parse(context, json).ToLocal(&value) ||
        !value->IsObject()) {
      return false;
    }
    *output = value.As<v8::Object>();
    return true;
  }

  bool GetProperty(v8::Local<v8::Context> context,
                   v8::Local<v8::Object> object, const std::string& name,
                   v8::Local<v8::Value>* output) {
    v8::Local<v8::String> key;
    return v8::String::NewFromUtf8(isolate_, name.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(name.size()))
               .ToLocal(&key) &&
           object->Get(context, key).ToLocal(output);
  }

  bool SelectPackageTarget(v8::Local<v8::Context> context,
                           v8::Local<v8::Value> value,
                           const std::string& condition,
                           std::string* output) {
    if (value->IsString()) {
      *output = ToUtf8(isolate_, value);
      return true;
    }
    if (value->IsArray()) {
      v8::Local<v8::Array> alternatives = value.As<v8::Array>();
      for (uint32_t index = 0; index < alternatives->Length(); ++index) {
        v8::Local<v8::Value> alternative;
        if (alternatives->Get(context, index).ToLocal(&alternative) &&
            SelectPackageTarget(context, alternative, condition, output)) {
          return true;
        }
      }
      return false;
    }
    if (!value->IsObject()) return false;
    v8::Local<v8::Object> conditions = value.As<v8::Object>();
    for (const std::string& name : {condition, std::string("node"),
                                    std::string("default")}) {
      v8::Local<v8::Value> target;
      if (GetProperty(context, conditions, name, &target) &&
          !target->IsUndefined() &&
          SelectPackageTarget(context, target, condition, output)) {
        return true;
      }
    }
    return false;
  }

  bool SelectPackageMap(v8::Local<v8::Context> context,
                        v8::Local<v8::Value> map, const std::string& key,
                        const std::string& condition, std::string* output) {
    if (!map->IsObject()) return false;
    v8::Local<v8::Object> object = map.As<v8::Object>();
    v8::Local<v8::Value> exact;
    if (GetProperty(context, object, key, &exact) && !exact->IsUndefined() &&
        SelectPackageTarget(context, exact, condition, output)) {
      return true;
    }

    v8::Local<v8::Array> names;
    if (!object->GetOwnPropertyNames(context).ToLocal(&names)) return false;
    size_t best_prefix = 0;
    std::string best_target;
    for (uint32_t index = 0; index < names->Length(); ++index) {
      v8::Local<v8::Value> name_value;
      if (!names->Get(context, index).ToLocal(&name_value) ||
          !name_value->IsString()) {
        continue;
      }
      const std::string name = ToUtf8(isolate_, name_value);
      const size_t wildcard = name.find('*');
      if (wildcard == std::string::npos) continue;
      const std::string prefix = name.substr(0, wildcard);
      const std::string suffix = name.substr(wildcard + 1);
      if (!key.starts_with(prefix) || !key.ends_with(suffix) ||
          key.size() < prefix.size() + suffix.size() ||
          prefix.size() < best_prefix) {
        continue;
      }
      v8::Local<v8::Value> candidate;
      std::string target;
      if (!object->Get(context, name_value).ToLocal(&candidate) ||
          !SelectPackageTarget(context, candidate, condition, &target)) {
        continue;
      }
      const std::string replacement = key.substr(
          prefix.size(), key.size() - prefix.size() - suffix.size());
      const size_t target_wildcard = target.find('*');
      if (target_wildcard != std::string::npos) {
        target.replace(target_wildcard, 1, replacement);
      }
      best_prefix = prefix.size();
      best_target = std::move(target);
    }
    if (best_target.empty()) return false;
    *output = std::move(best_target);
    return true;
  }

  bool ResolvePackageTarget(v8::Local<v8::Context> context,
                            const std::filesystem::path& package_root,
                            const std::string& target,
                            std::filesystem::path* output) {
    if (!target.starts_with("./")) return false;
    const std::filesystem::path candidate =
        package_root / Utf8ToWide(target.substr(2));
    std::error_code root_error;
    std::error_code target_error;
    const std::filesystem::path canonical_root =
        CanonicalPath(package_root, root_error);
    const std::filesystem::path canonical_target =
        CanonicalPath(candidate, target_error);
    if (root_error || target_error ||
        !canonical_target.native().starts_with(canonical_root.native())) {
      return false;
    }
    return ResolveCommonJsCandidate(context, canonical_target, output);
  }

  bool ResolvePackageImport(v8::Local<v8::Context> context,
                            const std::string& request,
                            const std::string& referrer,
                            const std::string& condition,
                            std::filesystem::path* output,
                            std::string* error) {
    std::filesystem::path directory =
        std::filesystem::path(Utf8ToWide(referrer)).parent_path();
    while (!directory.empty()) {
      const std::filesystem::path manifest_path = directory / L"package.json";
      v8::Local<v8::Object> manifest;
      if (ReadJsonObject(context, manifest_path, &manifest)) {
        v8::Local<v8::Value> imports;
        std::string target;
        if (GetProperty(context, manifest, "imports", &imports) &&
            SelectPackageMap(context, imports, request, condition, &target) &&
            ResolvePackageTarget(context, directory, target, output)) {
          return true;
        }
        *error = "package import is not defined: " + request;
        return false;
      }
      const auto parent = directory.parent_path();
      if (parent == directory) break;
      directory = parent;
    }
    *error = "package import has no package scope: " + request;
    return false;
  }

  bool ResolvePackage(v8::Local<v8::Context> context,
                      const std::string& request,
                      const std::string& referrer,
                      const std::string& condition,
                      std::filesystem::path* output, std::string* error) {
    std::string package_name;
    std::string package_subpath;
    if (request.starts_with('@')) {
      const size_t first = request.find('/');
      const size_t second = first == std::string::npos
                                ? std::string::npos
                                : request.find('/', first + 1);
      package_name =
          second == std::string::npos ? request : request.substr(0, second);
      package_subpath =
          second == std::string::npos ? "" : request.substr(second + 1);
    } else {
      const size_t slash = request.find('/');
      package_name = request.substr(0, slash);
      package_subpath =
          slash == std::string::npos ? "" : request.substr(slash + 1);
    }
    if (package_name.empty()) {
      *error = "invalid package specifier: " + request;
      return false;
    }

    std::filesystem::path directory =
        std::filesystem::path(Utf8ToWide(referrer)).parent_path();
    while (!directory.empty()) {
      const std::filesystem::path package_root =
          directory / L"node_modules" / Utf8ToWide(package_name);
      std::error_code directory_error;
      if (IsDirectory(package_root, directory_error)) {
        v8::Local<v8::Object> manifest;
        if (ReadJsonObject(context, package_root / L"package.json", &manifest)) {
          v8::Local<v8::Value> exports;
          if (GetProperty(context, manifest, "exports", &exports) &&
              !exports->IsUndefined()) {
            const std::string key = package_subpath.empty()
                                        ? "."
                                        : "./" + package_subpath;
            std::string target;
            bool selected = false;
            if (package_subpath.empty() &&
                (exports->IsString() || exports->IsArray())) {
              selected = SelectPackageTarget(context, exports, condition,
                                             &target);
            } else if (package_subpath.empty() && exports->IsObject()) {
              v8::Local<v8::Array> keys;
              if (exports.As<v8::Object>()
                      ->GetOwnPropertyNames(context)
                      .ToLocal(&keys) &&
                  keys->Length() != 0) {
                v8::Local<v8::Value> first;
                if (keys->Get(context, 0).ToLocal(&first) &&
                    !ToUtf8(isolate_, first).starts_with('.')) {
                  selected = SelectPackageTarget(context, exports, condition,
                                                 &target);
                }
              }
            }
            if (!selected) {
              selected = SelectPackageMap(context, exports, key, condition,
                                          &target);
            }
            if (selected &&
                ResolvePackageTarget(context, package_root, target, output)) {
              return true;
            }
            *error = "package export is not defined: " + request;
            return false;
          }
        }
        const std::filesystem::path candidate =
            package_subpath.empty()
                ? package_root
                : package_root / Utf8ToWide(package_subpath);
        if (ResolveCommonJsCandidate(context, candidate, output)) return true;
      }
      const auto parent = directory.parent_path();
      if (parent == directory) break;
      directory = parent;
    }
    *error = "module not found: " + request + " from " + referrer;
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
    if (runtime->LoadBuiltin(context, request, &builtin)) {
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
    const bool loaded = runtime->IsCommonJsPath(context, resolved)
                            ? runtime->LoadCommonJs(context, resolved, &exports,
                                                    &error)
                            : runtime->LoadEsModuleNamespace(context, resolved,
                                                             &exports, &error);
    if (!loaded) {
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
    v8::Local<v8::Value> builtin;
    if (runtime->LoadBuiltin(context, request, &builtin)) {
      info.GetReturnValue().Set(info[0]);
      return;
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
    const std::string canonical_request =
        request.starts_with("node:") ? request : "node:" + request;
    v8::Local<v8::Value> builtins;
    if (context->Global()
            ->Get(context,
                  v8::String::NewFromUtf8Literal(isolate_, "__sakoBuiltins"))
            .ToLocal(&builtins) &&
        builtins->IsObject()) {
      v8::Local<v8::Value> builtin;
      if (GetProperty(context, builtins.As<v8::Object>(), canonical_request,
                      &builtin) &&
          !builtin->IsUndefined()) {
        *output = builtin;
        return true;
      }
    }
    if (canonical_request == "node:assert") {
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
    if (canonical_request == "node:console") global_name = "console";
    if (canonical_request == "node:process") global_name = "process";
    if (global_name != nullptr) {
      return context->Global()
          ->Get(context,
                v8::String::NewFromUtf8(isolate_, global_name,
                                        v8::NewStringType::kNormal)
                    .ToLocalChecked())
          .ToLocal(output);
    }

    if (canonical_request == "node:timers") {
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
    if (request.starts_with('#')) {
      return ResolvePackageImport(context, request, referrer, "require",
                                  output, error);
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
      return ResolvePackage(context, request, referrer, "require", output,
                            error);
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
      if (IsRegularFile(file, status_error)) {
        std::error_code canonical_error;
        *output = CanonicalPath(file, canonical_error);
        if (!canonical_error) return true;
      }
    }

    std::error_code directory_error;
    if (!IsDirectory(candidate, directory_error)) return false;
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
            if (IsRegularFile(file, status_error)) {
              std::error_code canonical_error;
              *output = CanonicalPath(file, canonical_error);
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
    v8::Local<v8::Object> environment = v8::Object::New(isolate_);
    v8::Local<v8::Object> stdout_stream = v8::Object::New(isolate_);
    v8::Local<v8::Object> stderr_stream = v8::Object::New(isolate_);
    v8::Local<v8::Function> stdout_write;
    v8::Local<v8::Function> stderr_write;
    if (!v8::Function::New(
             context, WriteStream,
             v8::Integer::New(isolate_, static_cast<int32_t>(STD_OUTPUT_HANDLE)))
             .ToLocal(&stdout_write) ||
        !v8::Function::New(
             context, WriteStream,
             v8::Integer::New(isolate_, static_cast<int32_t>(STD_ERROR_HANDLE)))
             .ToLocal(&stderr_write) ||
        !Set(context, stdout_stream, "fd", v8::Integer::New(isolate_, 1)) ||
        !Set(context, stdout_stream, "write", stdout_write) ||
        !Set(context, stderr_stream, "fd", v8::Integer::New(isolate_, 2)) ||
        !Set(context, stderr_stream, "write", stderr_write)) {
      return false;
    }
    LPWCH environment_block = GetEnvironmentStringsW();
    if (environment_block == nullptr) return false;
    bool environment_ok = true;
    for (const wchar_t* entry = environment_block; *entry != L'\0';) {
      const std::wstring item(entry);
      entry += item.size() + 1;
      if (item.starts_with(L'=')) continue;
      const size_t equals = item.find(L'=');
      if (equals == std::wstring::npos) continue;
      const std::string name = WideToUtf8(item.substr(0, equals));
      const std::string value = WideToUtf8(item.substr(equals + 1));
      v8::Local<v8::String> text;
      if (name.empty() ||
          !v8::String::NewFromUtf8(isolate_, value.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(value.size()))
               .ToLocal(&text) ||
          !Set(context, environment, name, text)) {
        environment_ok = false;
        break;
      }
    }
    FreeEnvironmentStringsW(environment_block);
    if (!environment_ok) return false;

    v8::Local<v8::Function> cwd;
    v8::Local<v8::Value> next_tick;
    if (!v8::Function::New(context, ProcessCwd).ToLocal(&cwd) ||
        !context->Global()
             ->Get(context,
                   v8::String::NewFromUtf8Literal(isolate_, "queueMicrotask"))
             .ToLocal(&next_tick)) {
      return false;
    }
    v8::Local<v8::Value> executable =
        argument_count == 0
            ? v8::String::Empty(isolate_).As<v8::Value>()
            : arguments->Get(context, 0).ToLocalChecked();
    return Set(context, process, "argv", arguments) &&
           Set(context, process, "env", environment) &&
           Set(context, process, "execPath", executable) &&
           Set(context, process, "cwd", cwd) &&
           Set(context, process, "nextTick", next_tick) &&
           Set(context, process, "stdout", stdout_stream) &&
           Set(context, process, "stderr", stderr_stream) &&
           Set(context, process, "exitCode", v8::Integer::New(isolate_, 0)) &&
           Set(context, process, "version",
               v8::String::NewFromUtf8Literal(isolate_, "v0.1.0")) &&
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

  bool Set(v8::Local<v8::Context> context, v8::Local<v8::Object> object,
           const std::string& name, v8::Local<v8::Value> value) {
    v8::Local<v8::String> key;
    return v8::String::NewFromUtf8(isolate_, name.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(name.size()))
               .ToLocal(&key) &&
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

  static FileDescriptor* FindFileDescriptor(
      Runtime* runtime, const v8::FunctionCallbackInfo<v8::Value>& info,
      int argument = 0) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (runtime == nullptr || info.Length() <= argument ||
        !info[argument]->IsInt32()) {
      ThrowTypeError(isolate, "file descriptor must be an integer");
      return nullptr;
    }
    const int descriptor = info[argument]->Int32Value(context).FromMaybe(-1);
    auto found = runtime->file_descriptors_.find(descriptor);
    if (found == runtime->file_descriptors_.end()) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(isolate, "file descriptor is not open")));
      return nullptr;
    }
    return &found->second;
  }

  static void OpenSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    if (runtime == nullptr || info.Length() < 2 || !info[0]->IsString() ||
        !info[1]->IsString()) {
      ThrowTypeError(isolate, "openSync needs a path and string flags");
      return;
    }
    if (runtime->file_descriptors_.size() >= kMaximumFileDescriptors) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate, "file descriptor capacity exceeded")));
      return;
    }
    const std::filesystem::path path = CallbackPath(info);
    const std::string flags = ToUtf8(isolate, info[1]);
    DWORD access = 0;
    DWORD creation = OPEN_EXISTING;
    bool append = false;
    if (flags == "r") {
      access = GENERIC_READ;
    } else if (flags == "r+") {
      access = GENERIC_READ | GENERIC_WRITE;
    } else if (flags == "w" || flags == "wx") {
      access = GENERIC_WRITE;
      creation = flags == "wx" ? CREATE_NEW : CREATE_ALWAYS;
    } else if (flags == "w+" || flags == "wx+") {
      access = GENERIC_READ | GENERIC_WRITE;
      creation = flags == "wx+" ? CREATE_NEW : CREATE_ALWAYS;
    } else if (flags == "a" || flags == "ax") {
      access = GENERIC_WRITE;
      creation = flags == "ax" ? CREATE_NEW : OPEN_ALWAYS;
      append = true;
    } else if (flags == "a+" || flags == "ax+") {
      access = GENERIC_READ | GENERIC_WRITE;
      creation = flags == "ax+" ? CREATE_NEW : OPEN_ALWAYS;
      append = true;
    } else {
      ThrowTypeError(isolate, "unsupported file open flags");
      return;
    }
    HANDLE handle = CreateFileW(
        ExtendedPath(path).native().c_str(), access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr, creation,
        FILE_ATTRIBUTE_NORMAL, nullptr);
    if (handle == INVALID_HANDLE_VALUE) {
      ThrowFileError(isolate, "open", path);
      return;
    }
    int descriptor = runtime->next_file_descriptor_++;
    if (descriptor < 100) {
      descriptor = 100;
      runtime->next_file_descriptor_ = 101;
    }
    runtime->file_descriptors_.emplace(
        descriptor, FileDescriptor{.handle = handle, .append = append});
    info.GetReturnValue().Set(v8::Integer::New(isolate, descriptor));
  }

  static void CloseSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    FileDescriptor* file = FindFileDescriptor(runtime, info);
    if (file == nullptr) return;
    v8::Local<v8::Context> context = info.GetIsolate()->GetCurrentContext();
    const int descriptor = info[0]->Int32Value(context).FromMaybe(-1);
    const HANDLE handle = file->handle;
    runtime->file_descriptors_.erase(descriptor);
    if (!CloseHandle(handle)) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "close failed")));
    }
  }

  static bool DescriptorTransferArguments(
      const v8::FunctionCallbackInfo<v8::Value>& info, uint8_t** bytes,
      DWORD* length, bool* positioned, LARGE_INTEGER* position) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    const uint8_t* data = nullptr;
    size_t byte_length = 0;
    if (info.Length() < 4 || !ReadBytes(info[1], &data, &byte_length)) {
      ThrowTypeError(isolate, "descriptor I/O needs a byte array");
      return false;
    }
    const int64_t offset = info[2]->IntegerValue(context).FromMaybe(-1);
    const int64_t requested = info[3]->IntegerValue(context).FromMaybe(-1);
    if (offset < 0 || requested < 0 || requested > MAXDWORD ||
        static_cast<uint64_t>(offset) > byte_length ||
        static_cast<uint64_t>(requested) > byte_length - offset) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate, "descriptor I/O range is invalid")));
      return false;
    }
    *bytes = const_cast<uint8_t*>(data) + offset;
    *length = static_cast<DWORD>(requested);
    *positioned = info.Length() > 4 && !info[4]->IsNullOrUndefined();
    position->QuadPart = 0;
    if (*positioned) {
      const int64_t value = info[4]->IntegerValue(context).FromMaybe(-1);
      if (value < 0) {
        isolate->ThrowException(v8::Exception::RangeError(
            v8::String::NewFromUtf8Literal(isolate, "file position is invalid")));
        return false;
      }
      position->QuadPart = value;
    }
    return true;
  }

  static void ReadSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
    FileDescriptor* file = FindFileDescriptor(FromCallback(info), info);
    if (file == nullptr) return;
    uint8_t* bytes = nullptr;
    DWORD length = 0;
    bool positioned = false;
    LARGE_INTEGER position{};
    if (!DescriptorTransferArguments(info, &bytes, &length, &positioned,
                                     &position)) {
      return;
    }
    LARGE_INTEGER saved{};
    LARGE_INTEGER zero{};
    if (positioned &&
        (!SetFilePointerEx(file->handle, zero, &saved, FILE_CURRENT) ||
         !SetFilePointerEx(file->handle, position, nullptr, FILE_BEGIN))) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "read seek failed")));
      return;
    }
    DWORD transferred = 0;
    const BOOL succeeded = ::ReadFile(file->handle, bytes, length, &transferred, nullptr);
    if (positioned) SetFilePointerEx(file->handle, saved, nullptr, FILE_BEGIN);
    if (!succeeded) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "read failed")));
      return;
    }
    info.GetReturnValue().Set(v8::Integer::NewFromUnsigned(info.GetIsolate(), transferred));
  }

  static void WriteSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
    FileDescriptor* file = FindFileDescriptor(FromCallback(info), info);
    if (file == nullptr) return;
    uint8_t* bytes = nullptr;
    DWORD length = 0;
    bool positioned = false;
    LARGE_INTEGER position{};
    if (!DescriptorTransferArguments(info, &bytes, &length, &positioned,
                                     &position)) {
      return;
    }
    LARGE_INTEGER saved{};
    LARGE_INTEGER zero{};
    if (file->append) {
      position.QuadPart = 0;
      if (!SetFilePointerEx(file->handle, position, nullptr, FILE_END)) {
        info.GetIsolate()->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8Literal(info.GetIsolate(), "append seek failed")));
        return;
      }
      positioned = false;
    } else if (positioned &&
               (!SetFilePointerEx(file->handle, zero, &saved, FILE_CURRENT) ||
                !SetFilePointerEx(file->handle, position, nullptr, FILE_BEGIN))) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "write seek failed")));
      return;
    }
    DWORD transferred = 0;
    const BOOL succeeded =
        ::WriteFile(file->handle, bytes, length, &transferred, nullptr);
    if (positioned) SetFilePointerEx(file->handle, saved, nullptr, FILE_BEGIN);
    if (!succeeded) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "write failed")));
      return;
    }
    info.GetReturnValue().Set(v8::Integer::NewFromUnsigned(info.GetIsolate(), transferred));
  }

  static void HttpListen(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (runtime == nullptr || info.Length() < 2 || !info[0]->IsFunction()) {
      ThrowTypeError(isolate, "HTTP listen needs a handler and port");
      return;
    }
    if (runtime->http_servers_.size() >= kMaximumHttpServers) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate,
                                         "HTTP server capacity exceeded")));
      return;
    }
    const int64_t requested_port = info[1]->IntegerValue(context).FromMaybe(-1);
    if (requested_port < 0 || requested_port > 65'535) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate, "invalid HTTP port")));
      return;
    }
    auto binding = std::make_unique<HttpBinding>();
    binding->runtime = runtime;
    uint16_t local_port = 0;
    char error[1024] = {};
    binding->server = sako_http_server_new(
        static_cast<uint16_t>(requested_port), &local_port, error,
        sizeof(error));
    if (binding->server == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
      return;
    }
    binding->handler.Reset(isolate, info[0].As<v8::Function>());
    uint64_t id = runtime->next_http_server_id_++;
    if (id == 0) id = runtime->next_http_server_id_++;
    runtime->http_servers_.emplace(id, std::move(binding));
    v8::Local<v8::Array> result = v8::Array::New(isolate, 2);
    if (!result->Set(context, 0, v8::Number::New(isolate, id))
             .FromMaybe(false) ||
        !result->Set(context, 1, v8::Integer::New(isolate, local_port))
             .FromMaybe(false)) {
      return;
    }
    info.GetReturnValue().Set(result);
  }

  static void HttpsListen(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (runtime == nullptr || info.Length() < 4 || !info[0]->IsFunction()) {
      ThrowTypeError(isolate, "HTTPS listen needs a handler, port, key, and certificate");
      return;
    }
    if (runtime->http_servers_.size() >= kMaximumHttpServers) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate,
                                         "HTTP server capacity exceeded")));
      return;
    }
    const int64_t requested_port = info[1]->IntegerValue(context).FromMaybe(-1);
    const uint8_t* private_key = nullptr;
    size_t private_key_length = 0;
    const uint8_t* certificate = nullptr;
    size_t certificate_length = 0;
    if (requested_port < 0 || requested_port > 65'535 ||
        !ReadBytes(info[2], &private_key, &private_key_length) ||
        !ReadBytes(info[3], &certificate, &certificate_length) ||
        private_key_length > 1024 * 1024 || certificate_length > 1024 * 1024) {
      ThrowTypeError(isolate, "HTTPS key or certificate input is invalid");
      return;
    }
    auto binding = std::make_unique<HttpBinding>();
    binding->runtime = runtime;
    binding->secure = true;
    uint16_t local_port = 0;
    char error[1024] = {};
    binding->server = sako_https_server_new(
        static_cast<uint16_t>(requested_port),
        SakoNativeBytes{certificate, certificate_length},
        SakoNativeBytes{private_key, private_key_length}, &local_port, error,
        sizeof(error));
    if (binding->server == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
      return;
    }
    binding->handler.Reset(isolate, info[0].As<v8::Function>());
    uint64_t id = runtime->next_http_server_id_++;
    if (id == 0) id = runtime->next_http_server_id_++;
    runtime->http_servers_.emplace(id, std::move(binding));
    v8::Local<v8::Array> result = v8::Array::New(isolate, 2);
    if (!result->Set(context, 0, v8::Number::New(isolate, id))
             .FromMaybe(false) ||
        !result->Set(context, 1, v8::Integer::New(isolate, local_port))
             .FromMaybe(false)) {
      return;
    }
    info.GetReturnValue().Set(result);
  }

  static void HttpClose(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr || info.Length() == 0) return;
    v8::Local<v8::Context> context = info.GetIsolate()->GetCurrentContext();
    const uint64_t id = info[0]->IntegerValue(context).FromMaybe(0);
    auto found = runtime->http_servers_.find(id);
    if (found == runtime->http_servers_.end()) return;
    if (std::find(runtime->closing_http_servers_.begin(),
                  runtime->closing_http_servers_.end(),
                  id) == runtime->closing_http_servers_.end()) {
      runtime->closing_http_servers_.push_back(id);
    }
  }

  static void HttpAddress(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr || info.Length() == 0) return;
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    const uint64_t id = info[0]->IntegerValue(context).FromMaybe(0);
    if (!runtime->http_servers_.contains(id)) {
      info.GetReturnValue().Set(v8::Null(isolate));
      return;
    }
    v8::Local<v8::Object> result = v8::Object::New(isolate);
    const int port = info.Length() > 1
                         ? info[1]->Int32Value(context).FromMaybe(0)
                         : 0;
    if (!runtime->Set(context, result, "address",
                      v8::String::NewFromUtf8Literal(isolate, "127.0.0.1")) ||
        !runtime->Set(context, result, "family",
                      v8::String::NewFromUtf8Literal(isolate, "IPv4")) ||
        !runtime->Set(context, result, "port",
                      v8::Integer::New(isolate, port))) {
      return;
    }
    info.GetReturnValue().Set(result);
  }

  static int DispatchHttp(void* context, SakoNativeBytes method,
                          SakoNativeBytes target,
                          SakoNativeBytes body,
                          const SakoNativeHeader* headers,
                          size_t header_count,
                          SakoNativeHttpResponse* response) {
    auto* binding = static_cast<HttpBinding*>(context);
    if (binding == nullptr || binding->runtime == nullptr || response == nullptr ||
        (header_count != 0 && headers == nullptr)) {
      return 1;
    }
    Runtime* runtime = binding->runtime;
    v8::Isolate* isolate = runtime->isolate_;
    v8::HandleScope handle_scope(isolate);
    v8::Local<v8::Context> js_context = isolate->GetCurrentContext();
    v8::TryCatch try_catch(isolate);
    v8::Local<v8::Value> dispatcher_value;
    if (!js_context->Global()
             ->Get(js_context, v8::String::NewFromUtf8Literal(
                                  isolate, "__sakoDispatchHttpRequest"))
             .ToLocal(&dispatcher_value) ||
        !dispatcher_value->IsFunction()) {
      runtime->async_error_ = "HTTP JavaScript dispatcher is unavailable";
      return 1;
    }
    auto make_string = [isolate](SakoNativeBytes bytes) {
      return v8::String::NewFromUtf8(
          isolate, reinterpret_cast<const char*>(bytes.data),
          v8::NewStringType::kNormal, static_cast<int>(bytes.length));
    };
    v8::Local<v8::String> method_value;
    v8::Local<v8::String> target_value;
    v8::Local<v8::ArrayBuffer> body_buffer;
    if (method.length > static_cast<size_t>(std::numeric_limits<int>::max()) ||
        target.length > static_cast<size_t>(std::numeric_limits<int>::max()) ||
        !make_string(method).ToLocal(&method_value) ||
        !make_string(target).ToLocal(&target_value)) {
      runtime->async_error_ = "HTTP request strings exceed V8 limits";
      return 1;
    }
    std::unique_ptr<v8::BackingStore> body_backing =
        v8::ArrayBuffer::NewBackingStore(isolate, body.length);
    if (body.length != 0) {
      std::memcpy(body_backing->Data(), body.data, body.length);
    }
    body_buffer = v8::ArrayBuffer::New(isolate, std::move(body_backing));
    v8::Local<v8::Uint8Array> body_value =
        v8::Uint8Array::New(body_buffer, 0, body.length);
    size_t header_bytes_length = 0;
    for (size_t index = 0; index < header_count; ++index) {
      const size_t item_length = headers[index].name.length +
                                 headers[index].value.length;
      if (item_length < headers[index].name.length ||
          header_bytes_length > std::numeric_limits<uint32_t>::max() -
                                    item_length) {
        runtime->async_error_ = "HTTP request header exceeds V8 limits";
        return 1;
      }
      header_bytes_length += item_length;
    }
    std::unique_ptr<v8::BackingStore> header_backing =
        v8::ArrayBuffer::NewBackingStore(isolate, header_bytes_length);
    const size_t range_count = header_count * 4;
    std::unique_ptr<v8::BackingStore> range_backing =
        v8::ArrayBuffer::NewBackingStore(isolate, range_count * sizeof(uint32_t));
    auto* header_output = static_cast<uint8_t*>(header_backing->Data());
    auto* ranges = static_cast<uint32_t*>(range_backing->Data());
    size_t header_offset = 0;
    for (size_t index = 0; index < header_count; ++index) {
      ranges[index * 4] = static_cast<uint32_t>(header_offset);
      ranges[index * 4 + 1] = static_cast<uint32_t>(headers[index].name.length);
      if (headers[index].name.length != 0) {
        std::memcpy(header_output + header_offset, headers[index].name.data,
                    headers[index].name.length);
      }
      header_offset += headers[index].name.length;
      ranges[index * 4 + 2] = static_cast<uint32_t>(header_offset);
      ranges[index * 4 + 3] = static_cast<uint32_t>(headers[index].value.length);
      if (headers[index].value.length != 0) {
        std::memcpy(header_output + header_offset, headers[index].value.data,
                    headers[index].value.length);
      }
      header_offset += headers[index].value.length;
    }
    v8::Local<v8::ArrayBuffer> header_buffer =
        v8::ArrayBuffer::New(isolate, std::move(header_backing));
    v8::Local<v8::Uint8Array> header_values =
        v8::Uint8Array::New(header_buffer, 0, header_bytes_length);
    v8::Local<v8::ArrayBuffer> range_buffer =
        v8::ArrayBuffer::New(isolate, std::move(range_backing));
    v8::Local<v8::Uint32Array> header_ranges =
        v8::Uint32Array::New(range_buffer, 0, range_count);
    v8::Local<v8::Value> arguments[] = {
        binding->handler.Get(isolate), method_value, target_value, header_values,
        header_ranges, body_value, v8::Boolean::New(isolate, binding->secure)};
    v8::Local<v8::Value> result;
    if (!dispatcher_value.As<v8::Function>()
              ->Call(js_context, v8::Undefined(isolate), 7, arguments)
             .ToLocal(&result) ||
        !result->IsObject()) {
      runtime->async_error_ = FormatException(isolate, js_context, try_catch);
      return 1;
    }
    isolate->PerformMicrotaskCheckpoint();
    v8::Local<v8::Value> finalize_value;
    v8::Local<v8::Value> finalized;
    if (!js_context->Global()
             ->Get(js_context,
                   v8::String::NewFromUtf8Literal(
                       isolate, "__sakoFinalizeHttpResponse"))
             .ToLocal(&finalize_value) ||
        !finalize_value->IsFunction() ||
        !finalize_value.As<v8::Function>()
             ->Call(js_context, v8::Undefined(isolate), 1, &result)
             .ToLocal(&finalized) ||
        !finalized->IsObject()) {
      runtime->async_error_ = FormatException(isolate, js_context, try_catch);
      return 1;
    }
    v8::Local<v8::Object> object = finalized.As<v8::Object>();
    v8::Local<v8::Value> status;
    v8::Local<v8::Value> reason;
    v8::Local<v8::Value> response_body;
    v8::Local<v8::Value> response_headers;
    if (!runtime->GetProperty(js_context, object, "status", &status) ||
        !runtime->GetProperty(js_context, object, "reason", &reason) ||
        !runtime->GetProperty(js_context, object, "body", &response_body) ||
        !runtime->GetProperty(js_context, object, "headers", &response_headers) ||
        !reason->IsString() || !response_headers->IsArray()) {
      runtime->async_error_ = "HTTP dispatcher returned an invalid response";
      return 1;
    }
    const int64_t status_code = status->IntegerValue(js_context).FromMaybe(0);
    if (status_code < 100 || status_code > 999) {
      runtime->async_error_ = "HTTP dispatcher returned an invalid status";
      return 1;
    }
    binding->response_reason = ToUtf8(isolate, reason);
    if (response_body->IsString()) {
      binding->response_body = ToUtf8(isolate, response_body);
    } else {
      const uint8_t* bytes = nullptr;
      size_t length = 0;
      if (!ReadBytes(response_body, &bytes, &length)) {
        runtime->async_error_ = "HTTP response body must be a string or byte array";
        return 1;
      }
      binding->response_body.assign(reinterpret_cast<const char*>(bytes), length);
    }
    v8::Local<v8::Array> pairs = response_headers.As<v8::Array>();
    if (pairs->Length() % 2 != 0 || pairs->Length() / 2 > 128) {
      runtime->async_error_ = "HTTP dispatcher returned invalid headers";
      return 1;
    }
    const size_t pair_count = pairs->Length() / 2;
    binding->response_header_names.clear();
    binding->response_header_values.clear();
    binding->response_header_names.reserve(pair_count);
    binding->response_header_values.reserve(pair_count);
    for (uint32_t index = 0; index < pairs->Length(); index += 2) {
      v8::Local<v8::Value> name;
      v8::Local<v8::Value> value;
      if (!pairs->Get(js_context, index).ToLocal(&name) ||
          !pairs->Get(js_context, index + 1).ToLocal(&value)) {
        runtime->async_error_ = "cannot read HTTP response headers";
        return 1;
      }
      binding->response_header_names.push_back(ToUtf8(isolate, name));
      binding->response_header_values.push_back(ToUtf8(isolate, value));
    }
    binding->response_headers.clear();
    binding->response_headers.reserve(pair_count);
    for (size_t index = 0; index < pair_count; ++index) {
      const std::string& name = binding->response_header_names[index];
      const std::string& value = binding->response_header_values[index];
      binding->response_headers.push_back(
          {{reinterpret_cast<const uint8_t*>(name.data()), name.size()},
           {reinterpret_cast<const uint8_t*>(value.data()), value.size()}});
    }
    response->status = static_cast<uint16_t>(status_code);
    response->reason = {
        reinterpret_cast<const uint8_t*>(binding->response_reason.data()),
        binding->response_reason.size()};
    response->headers = binding->response_headers.data();
    response->header_count = binding->response_headers.size();
    response->body = {
        reinterpret_cast<const uint8_t*>(binding->response_body.data()),
        binding->response_body.size()};
    return 0;
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
      while (v8::platform::PumpMessageLoop(platform_, isolate_)) {
        isolate_->PerformMicrotaskCheckpoint();
      }

      bool handled_http = false;
      for (auto& [id, binding] : http_servers_) {
        (void)id;
        char native_error[1024] = {};
        const int handled = sako_http_server_tick(
            binding->server, DispatchHttp, binding.get(), native_error,
            sizeof(native_error));
        if (handled < 0) {
          *error = async_error_.empty() ? native_error : async_error_;
          async_error_.clear();
          return false;
        }
        handled_http |= handled != 0;
        isolate_->PerformMicrotaskCheckpoint();
        if (!async_error_.empty()) {
          *error = async_error_;
          async_error_.clear();
          return false;
        }
      }
      if (!closing_http_servers_.empty()) {
        std::sort(closing_http_servers_.begin(), closing_http_servers_.end());
        closing_http_servers_.erase(
            std::unique(closing_http_servers_.begin(),
                        closing_http_servers_.end()),
            closing_http_servers_.end());
        for (uint64_t id : closing_http_servers_) {
          auto found = http_servers_.find(id);
          if (found == http_servers_.end()) continue;
          HttpBinding& binding = *found->second;
          if (!binding.closing) {
            if (sako_http_server_close(binding.server) != 0) {
              *error = "cannot close native HTTP server";
              return false;
            }
            binding.closing = true;
          }
          uint64_t connections = 0;
          uint64_t rejected = 0;
          if (sako_http_server_stats(binding.server, &connections, &rejected) != 0) {
            *error = "cannot query closing HTTP server";
            return false;
          }
          if (connections == 0) {
            binding.handler.Reset();
            sako_http_server_delete(binding.server);
            binding.server = nullptr;
            http_servers_.erase(found);
          }
        }
        std::erase_if(closing_http_servers_, [this](uint64_t id) {
          return !http_servers_.contains(id);
        });
      }
      if (timers_.empty()) {
        if (http_servers_.empty()) return true;
        if (!handled_http) Sleep(1);
        continue;
      }

      auto next = std::min_element(
          timers_.begin(), timers_.end(),
          [](const auto& left, const auto& right) {
            return left.second.deadline < right.second.deadline;
          });
      const auto now = std::chrono::steady_clock::now();
      if (next->second.deadline > now) {
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            next->second.deadline - now + std::chrono::milliseconds(1));
        int64_t sleep_milliseconds = remaining.count();
        if (!http_servers_.empty()) sleep_milliseconds = std::min<int64_t>(1, sleep_milliseconds);
        Sleep(static_cast<DWORD>(std::min<int64_t>(sleep_milliseconds, MAXDWORD)));
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

  v8::Platform* platform_ = nullptr;
  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator_;
  v8::Isolate* isolate_ = nullptr;
  v8::Global<v8::Context> context_;
  std::unordered_map<uint64_t, Timer> timers_;
  std::unordered_map<std::string, v8::Global<v8::Module>> modules_;
  std::unordered_map<std::string, v8::Global<v8::Object>> commonjs_modules_;
  std::unordered_map<int, SyntheticCommonJs> synthetic_commonjs_;
  std::unordered_map<uint64_t, std::unique_ptr<HttpBinding>> http_servers_;
  std::unordered_map<int, FileDescriptor> file_descriptors_;
  std::vector<uint64_t> closing_http_servers_;
  std::string async_error_;
  size_t module_source_bytes_ = 0;
  uint64_t next_timer_id_ = 1;
  uint64_t next_http_server_id_ = 1;
  int next_file_descriptor_ = 100;
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
    uint64_t* heap_limit, uint64_t* persistent_handles, uint64_t* timers,
    uint64_t* external_memory, uint64_t* http_servers, uint64_t* sockets,
    uint64_t* http_buffer_bytes, uint64_t* native_memory_bytes,
    uint64_t* module_cache_entries, uint64_t* queued_operations) {
  if (runtime == nullptr || heap_used == nullptr || heap_committed == nullptr ||
      heap_limit == nullptr || persistent_handles == nullptr || timers == nullptr ||
      external_memory == nullptr || http_servers == nullptr || sockets == nullptr ||
      http_buffer_bytes == nullptr || native_memory_bytes == nullptr ||
      module_cache_entries == nullptr || queued_operations == nullptr) {
    return 1;
  }
  static_cast<Runtime*>(runtime)->MemoryStats(
      heap_used, heap_committed, heap_limit, persistent_handles, timers,
      external_memory, http_servers, sockets, http_buffer_bytes,
      native_memory_bytes, module_cache_entries, queued_operations);
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
