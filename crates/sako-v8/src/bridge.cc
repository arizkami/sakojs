// SPDX-License-Identifier: BSD-3-Clause

#include <algorithm>
#include <atomic>
#include <cctype>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <cwctype>
#include <filesystem>
#include <fstream>
#include <limits>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <bcrypt.h>
#else
#include <cerrno>
#include <cstdlib>
#include <ctime>
#include <fcntl.h>
#include <csignal>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <termios.h>
#include <unistd.h>
extern char** environ;
#endif

#include "libplatform/libplatform.h"
#include "v8.h"
#include "sako_napi.h"
#include "bootstrap.generated.h"
#include "bootstrap_cache.generated.h"
#include "libraries.generated.h"
#if !defined(SAKO_SNAPSHOT_GENERATOR)
// Produced by this same file compiled as the snapshot generator; see
// the SAKO_SNAPSHOT_GENERATOR section at the bottom.
#include "snapshot.generated.h"
#endif

// A single alias for the path representation that differs between platforms
// (UTF-16 on Windows, UTF-8 directly on POSIX) so the module-resolution and
// path-candidate logic below can be written once instead of duplicated per
// platform. `SAKO_PATH_LITERAL` mirrors it for string literals.
#if defined(_WIN32)
using PathChar = wchar_t;
using PathString = std::wstring;
#define SAKO_PATH_LITERAL(x) L##x
#else
using PathChar = char;
using PathString = std::string;
#define SAKO_PATH_LITERAL(x) x
#endif

// --- Startup phase instrumentation -----------------------------------------
//
// `sako --perf-breakdown script.js` records one high-resolution timestamp per
// startup phase and prints the deltas to stderr when the process finishes.
// Recording is off unless the CLI enables it, so a normal run pays one
// predictable branch on a process-wide flag per phase boundary and touches no
// timer, no allocation, and no output handle.

extern "C" {
void sako_perf_enable(void);
void sako_perf_mark(const char* name);
void sako_perf_report(void);
int sako_perf_enabled(void);
}

namespace sako_perf {

constexpr size_t kMaximumMarks = 96;

struct Mark {
  const char* name;
  int64_t counter;
};

bool g_enabled = false;
size_t g_count = 0;
Mark g_marks[kMaximumMarks];
int64_t g_frequency = 1;
int64_t g_process_start_offset_100ns = 0;

inline int64_t Counter() {
#if defined(_WIN32)
  LARGE_INTEGER value;
  QueryPerformanceCounter(&value);
  return value.QuadPart;
#else
  struct timespec value;
  clock_gettime(CLOCK_MONOTONIC, &value);
  return static_cast<int64_t>(value.tv_sec) * 1'000'000'000 + value.tv_nsec;
#endif
}

inline void Record(const char* name) {
  if (!g_enabled || g_count >= kMaximumMarks) return;
  g_marks[g_count].name = name;
  g_marks[g_count].counter = Counter();
  ++g_count;
}

// Buckets for phases that repeat too often to record one mark each. The HTTP
// request path accumulates into these instead, so the report can show where a
// request's time goes without one timestamp per phase per request surviving to
// the end of the run.
enum Bucket {
  kBucketHttpMarshalRequest,
  kBucketHttpHandler,
  kBucketHttpMicrotasks,
  kBucketHttpFinalize,
  kBucketHttpMarshalResponse,
  kBucketCount,
};

constexpr const char* kBucketNames[kBucketCount] = {
    "http.marshal-request", "http.handler",         "http.microtasks",
    "http.finalize",        "http.marshal-response",
};

int64_t g_bucket_counters[kBucketCount] = {};
uint64_t g_bucket_events[kBucketCount] = {};

// Times one phase of a repeating path. Constructing and destroying it costs
// nothing while recording is off.
class Span {
 public:
  explicit Span(Bucket bucket)
      : bucket_(bucket), started_(g_enabled ? Counter() : 0) {}

  ~Span() {
    if (!g_enabled) return;
    g_bucket_counters[bucket_] += Counter() - started_;
    g_bucket_events[bucket_] += 1;
  }

  Span(const Span&) = delete;
  Span& operator=(const Span&) = delete;

 private:
  Bucket bucket_;
  int64_t started_;
};

}  // namespace sako_perf

// Records phase marks from C++ hot-path-free startup code.
#define SAKO_PERF_MARK(name)                         do {                                                 if (sako_perf::g_enabled) sako_perf::Record(name);   } while (false)

extern "C" {

void sako_perf_enable(void) {
#if defined(_WIN32)
  LARGE_INTEGER frequency;
  QueryPerformanceFrequency(&frequency);
  sako_perf::g_frequency = frequency.QuadPart == 0 ? 1 : frequency.QuadPart;

  // Charge image load, CRT startup, and dynamic linking to the run by
  // measuring from the kernel's process creation time to this call.
  FILETIME creation = {};
  FILETIME exited = {};
  FILETIME kernel = {};
  FILETIME user = {};
  FILETIME now = {};
  if (GetProcessTimes(GetCurrentProcess(), &creation, &exited, &kernel,
                      &user)) {
    GetSystemTimeAsFileTime(&now);
    const int64_t created = (static_cast<int64_t>(creation.dwHighDateTime)
                             << 32) |
                            creation.dwLowDateTime;
    const int64_t current =
        (static_cast<int64_t>(now.dwHighDateTime) << 32) | now.dwLowDateTime;
    sako_perf::g_process_start_offset_100ns = current - created;
  }
#else
  // Counter() already reports nanoseconds, so the "frequency" scale is fixed
  // and there is no equivalent of Windows' process-creation-time query
  // without parsing /proc/self/stat; the image-load line just reports 0 here.
  sako_perf::g_frequency = 1'000'000'000;
  sako_perf::g_process_start_offset_100ns = 0;
#endif
  sako_perf::g_enabled = true;
  sako_perf::Record("cli.enter");
}

void sako_perf_mark(const char* name) { SAKO_PERF_MARK(name); }

int sako_perf_enabled(void) { return sako_perf::g_enabled ? 1 : 0; }

void sako_perf_report(void) {
  if (!sako_perf::g_enabled || sako_perf::g_count == 0) return;
  const double scale = 1000.0 / static_cast<double>(sako_perf::g_frequency);
  const double before_main =
      static_cast<double>(sako_perf::g_process_start_offset_100ns) / 10000.0;
  std::string report = "sako perf breakdown (milliseconds)\n";
  char line[256];
  snprintf(line, sizeof(line), "  %-28s %9.3f %9.3f\n", "process.image-load",
           before_main, before_main);
  report += line;
  const int64_t origin = sako_perf::g_marks[0].counter;
  for (size_t index = 1; index < sako_perf::g_count; ++index) {
    const double delta =
        static_cast<double>(sako_perf::g_marks[index].counter -
                            sako_perf::g_marks[index - 1].counter) *
        scale;
    const double total =
        before_main +
        static_cast<double>(sako_perf::g_marks[index].counter - origin) * scale;
    snprintf(line, sizeof(line), "  %-28s %9.3f %9.3f\n",
             sako_perf::g_marks[index].name, delta, total);
    report += line;
  }
  bool any_bucket = false;
  for (int index = 0; index < sako_perf::kBucketCount; ++index) {
    if (sako_perf::g_bucket_events[index] == 0) continue;
    if (!any_bucket) {
      report += "  --- repeating phases: total ms, microseconds each ---\n";
      any_bucket = true;
    }
    const double total =
        static_cast<double>(sako_perf::g_bucket_counters[index]) * scale;
    const double each =
        total * 1000.0 /
        static_cast<double>(sako_perf::g_bucket_events[index]);
    snprintf(line, sizeof(line), "  %-28s %9.3f %9.3f us x%llu\n",
             sako_perf::kBucketNames[index], total, each,
             static_cast<unsigned long long>(sako_perf::g_bucket_events[index]));
    report += line;
  }
#if defined(_WIN32)
  const HANDLE error_handle = GetStdHandle(STD_ERROR_HANDLE);
  if (error_handle != INVALID_HANDLE_VALUE && error_handle != nullptr) {
    DWORD written = 0;
    WriteFile(error_handle, report.data(),
              static_cast<DWORD>(report.size()), &written, nullptr);
  }
#else
  {
    size_t written_total = 0;
    while (written_total < report.size()) {
      const ssize_t written = write(STDERR_FILENO, report.data() + written_total,
                                    report.size() - written_total);
      if (written <= 0) break;
      written_total += static_cast<size_t>(written);
    }
  }
#endif
}

}  // extern "C"

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

/// Returns 0 once the response is filled in, kNativeHttpDeferred when the
/// answer will arrive later through `sako_http_server_respond`, and anything
/// else on failure.
using SakoNativeHttpHandler = int (*)(
    void*, uint64_t, SakoNativeBytes, SakoNativeBytes, SakoNativeBytes,
    const SakoNativeHeader*, size_t, SakoNativeHttpResponse*);

constexpr int kNativeHttpDeferred = 2;

// The event kinds sako_child_drain reports. Keep in sync with the constants
// in crates/sako-v8/src/lib.rs and with the dispatcher in bootstrap.js.
constexpr int kSakoChildStdout = 0;
constexpr int kSakoChildStderr = 1;
constexpr int kSakoChildStdoutEnd = 2;
constexpr int kSakoChildStderrEnd = 3;
constexpr int kSakoChildExited = 4;
constexpr int kSakoChildFailed = 5;
constexpr int kSakoChildStdinFailed = 6;
constexpr int kSakoChildStdinDrained = 7;

void* sako_http_server_new(uint16_t port, uint16_t* output_port, char* error,
                           size_t error_capacity);
void* sako_https_server_new(uint16_t port, SakoNativeBytes certificate,
                            SakoNativeBytes private_key, uint16_t* output_port,
                            char* error, size_t error_capacity);
int sako_http_server_tick(void* server, SakoNativeHttpHandler handler,
                          void* context, char* error, size_t error_capacity);
int sako_http_server_wait(void* server, uint32_t timeout_milliseconds);
void sako_http_server_delete(void* server);
int sako_http_server_close(void* server);
int sako_http_server_respond(void* server, uint64_t ticket, uint16_t status,
                             SakoNativeBytes reason,
                             const SakoNativeHeader* headers,
                             size_t header_count, SakoNativeBytes body);
int sako_http_server_stats(void* server, uint64_t* connections,
                           uint64_t* rejected_connections);
int sako_dns_resolve(SakoNativeBytes host, int family, char* output,
                     size_t output_capacity, char* error,
                     size_t error_capacity);
void* sako_process_spawn_sync(SakoNativeBytes executable,
                              const SakoNativeBytes* arguments,
                              size_t argument_count, SakoNativeBytes cwd,
                              int verbatim_arguments, char* error, size_t error_capacity);
int sako_process_output_status(const void* output);
SakoNativeBytes sako_process_output_stdout(const void* output);
SakoNativeBytes sako_process_output_stderr(const void* output);
void sako_process_output_delete(void* output);
// What a live child says, one event at a time. `kind` is one of the
// kSakoChild* values below; `bytes` carries output or an error message and
// `status` an exit code, whichever the kind implies.
using SakoNativeChildEvent = void (*)(void*, int, SakoNativeBytes, int);
void* sako_child_spawn(SakoNativeBytes executable,
                       const SakoNativeBytes* arguments, size_t argument_count,
                       SakoNativeBytes cwd, int verbatim_arguments,
                       const SakoNativeBytes* environment,
                       size_t environment_count, int replace_environment,
                       int piped_stdin, char* error, size_t error_capacity);
uint32_t sako_child_pid(const void* child);
int sako_child_drain(const void* child, SakoNativeChildEvent callback,
                     void* user);
int sako_child_write(const void* child, SakoNativeBytes bytes, char* error,
                     size_t error_capacity);
int sako_child_close_stdin(const void* child);
int sako_child_kill(const void* child, int force);
void sako_child_delete(void* child);
uint64_t sako_child_activity_tick();
uint64_t sako_child_wait_activity(uint64_t since, uint32_t timeout_milliseconds);
void* sako_postgres_connect(SakoNativeBytes url, char* error, size_t error_capacity);
void* sako_postgres_query(void* connection, SakoNativeBytes sql,
                          const SakoNativeBytes* parameters, const uint8_t* nulls,
                          size_t parameter_count, char* error,
                          size_t error_capacity);
void sako_postgres_close(void* connection);
void sako_postgres_delete(void* connection);
SakoNativeBytes sako_postgres_parameter(const void* connection, SakoNativeBytes name);
size_t sako_postgres_result_column_count(const void* result);
SakoNativeBytes sako_postgres_result_column_name(const void* result, size_t index);
uint32_t sako_postgres_result_column_type(const void* result, size_t index);
size_t sako_postgres_result_row_count(const void* result);
SakoNativeBytes sako_postgres_result_value(const void* result, size_t row,
                                           size_t column, int* is_null);
SakoNativeBytes sako_postgres_result_command(const void* result);
uint64_t sako_postgres_result_affected(const void* result);
void sako_postgres_result_delete(void* result);
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
void* sako_typescript_transpile(SakoNativeBytes path, SakoNativeBytes source,
                                int commonjs, char* error,
                                size_t error_capacity);
SakoNativeBytes sako_typescript_output_source(const void* output);
void sako_typescript_output_delete(void* output);
// Writes exactly 20 bytes (the SHA-1 digest length) to `digest`.
size_t sako_hash(uint32_t algorithm, SakoNativeBytes bytes,
                 uint8_t* digest);
}

namespace {

/// The Node release `process.versions.node` reports. Packages branch on it to
/// decide which APIs exist; the number therefore has to name the surface Sako
/// implements rather than Sako's own version, which `process.versions.sako`
/// carries instead.
constexpr const char* kNodeApiLevel = "22.12.0";

}  // namespace

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

// Encodes a JavaScript string straight into storage the caller already owns.
//
// v8::String::Utf8Value allocates its own buffer and a std::string copy of it
// allocates a second, so every string handed across the boundary cost two
// allocations and two copies. Writing into a reused std::string keeps its
// capacity between calls, which matters on the HTTP response path where the
// same handful of strings crosses once per request.
bool ToUtf8Into(v8::Isolate* isolate, v8::Local<v8::Value> value,
                std::string* output) {
  if (!value->IsString()) {
    v8::Local<v8::String> converted;
    if (!value->ToString(isolate->GetCurrentContext()).ToLocal(&converted)) {
      output->clear();
      return false;
    }
    return ToUtf8Into(isolate, converted, output);
  }
  v8::Local<v8::String> text = value.As<v8::String>();
  const size_t length = text->Utf8LengthV2(isolate);
  output->resize(length);
  if (length != 0) {
    text->WriteUtf8V2(isolate, output->data(), length,
                      v8::String::WriteFlags::kReplaceInvalidUtf8);
  }
  return true;
}

#if defined(_WIN32)
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
#endif

// Converts UTF-8 text into this platform's native path representation:
// UTF-16 on Windows, identity on POSIX (already UTF-8).
PathString Utf8ToPathString(const std::string& value) {
#if defined(_WIN32)
  return Utf8ToWide(value);
#else
  return value;
#endif
}

// On Windows this rewrites an absolute path with the `\\?\` prefix that lifts
// MAX_PATH and reparse-point traversal limits. POSIX has neither limit, so
// the path is returned unchanged there; every ExtendedPath/UserPath call site
// below stays the same on both platforms.
std::filesystem::path ExtendedPath(const std::filesystem::path& path) {
#if defined(_WIN32)
  // An already-extended path has to be returned untouched. std::filesystem
  // parses `\\?\W:\dir\file` with root_name `\\?` and root_directory `\`, so
  // lexically_normal() below rewrites it to `W:dir\file` -- dropping the
  // separator after the drive letter and producing a path that cannot be
  // opened. Rust hands us exactly this shape: Path::canonicalize() returns a
  // `\\?\`-prefixed path, so the check has to happen before normalizing, not
  // after it (where the original prefix test sat, by which point the path was
  // already destroyed).
  if (path.native().starts_with(L"\\\\?\\")) return path;
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
#else
  return path;
#endif
}

std::filesystem::path UserPath(const std::filesystem::path& path) {
#if defined(_WIN32)
  const std::wstring& native = path.native();
  if (native.starts_with(L"\\\\?\\UNC\\")) {
    return std::filesystem::path(L"\\\\" + native.substr(8));
  }
  if (native.starts_with(L"\\\\?\\")) {
    return std::filesystem::path(native.substr(4));
  }
  return path;
#else
  return path;
#endif
}

std::filesystem::path CanonicalPath(const std::filesystem::path& path,
                                    std::error_code& error) {
  return UserPath(std::filesystem::weakly_canonical(ExtendedPath(path), error));
}

bool IsRegularFile(const std::filesystem::path& path,
                   std::error_code& error) {
#if defined(_WIN32)
  const std::filesystem::path extended = ExtendedPath(path);
  const DWORD attributes = GetFileAttributesW(extended.native().c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    error = std::error_code(static_cast<int>(GetLastError()),
                            std::system_category());
    return false;
  }
  error.clear();
  return (attributes & FILE_ATTRIBUTE_DIRECTORY) == 0;
#else
  const bool result = std::filesystem::is_regular_file(path, error);
  return !error && result;
#endif
}

bool IsDirectory(const std::filesystem::path& path, std::error_code& error) {
#if defined(_WIN32)
  const std::filesystem::path extended = ExtendedPath(path);
  const DWORD attributes = GetFileAttributesW(extended.native().c_str());
  if (attributes == INVALID_FILE_ATTRIBUTES) {
    error = std::error_code(static_cast<int>(GetLastError()),
                            std::system_category());
    return false;
  }
  error.clear();
  return (attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
#else
  const bool result = std::filesystem::is_directory(path, error);
  return !error && result;
#endif
}

std::string PathToUtf8(const std::filesystem::path& path) {
#if defined(_WIN32)
  return WideToUtf8(UserPath(path).native());
#else
  return UserPath(path).native();
#endif
}

// Opens a file for a sequential whole-file read. The caller owns the handle.
#if defined(_WIN32)
using NativeFile = HANDLE;
const NativeFile kInvalidFile = INVALID_HANDLE_VALUE;
#else
using NativeFile = int;
constexpr NativeFile kInvalidFile = -1;
#endif

void CloseNativeFile(NativeFile file) {
#if defined(_WIN32)
  CloseHandle(file);
#else
  close(file);
#endif
}

NativeFile OpenFileForRead(const std::filesystem::path& path, uint64_t* size) {
#if defined(_WIN32)
  const HANDLE file = CreateFileW(
      ExtendedPath(path).c_str(), GENERIC_READ,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
      OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_SEQUENTIAL_SCAN,
      nullptr);
  if (file == INVALID_HANDLE_VALUE) return file;
  LARGE_INTEGER file_size = {};
  if (!GetFileSizeEx(file, &file_size) || file_size.QuadPart < 0) {
    CloseHandle(file);
    return INVALID_HANDLE_VALUE;
  }
  *size = static_cast<uint64_t>(file_size.QuadPart);
  return file;
#else
  const int file = open(ExtendedPath(path).c_str(), O_RDONLY);
  if (file < 0) return kInvalidFile;
  struct stat info;
  if (fstat(file, &info) != 0 || info.st_size < 0) {
    close(file);
    return kInvalidFile;
  }
  *size = static_cast<uint64_t>(info.st_size);
  return file;
#endif
}

// Fills exactly `size` bytes of `destination` from `file`. Reading straight
// into the caller's storage keeps a whole-file read to one copy: no stream
// buffer, no intermediate string, and no second pass to hand the bytes on.
bool ReadFileBytes(NativeFile file, void* destination, uint64_t size) {
  constexpr uint64_t kMaximumChunk = 1ull << 30;
  auto* output = static_cast<uint8_t*>(destination);
  uint64_t offset = 0;
  while (offset < size) {
    const uint64_t remaining = size - offset;
    const uint64_t chunk = remaining > kMaximumChunk ? kMaximumChunk : remaining;
#if defined(_WIN32)
    DWORD read = 0;
    if (!::ReadFile(file, output + offset, static_cast<DWORD>(chunk), &read,
                    nullptr)) {
      return false;
    }
#else
    const ssize_t read =
        ::read(file, output + offset, static_cast<size_t>(chunk));
    if (read < 0) return false;
#endif
    if (read == 0) break;
    offset += static_cast<uint64_t>(read);
  }
  return offset == size;
}

// Reads one byte range of an already-sized file through a private handle.
//
// The caller owns `destination` and guarantees the range [offset, offset+size)
// stays inside it for the whole call. Each worker opens its own handle so the
// reads do not serialize on one file object's lock, and every read carries an
// explicit offset so no worker depends on another's file pointer.
bool ReadFileRange(const PathString& path, uint8_t* destination,
                   uint64_t offset, uint64_t size) {
#if defined(_WIN32)
  const HANDLE file = CreateFileW(
      path.c_str(), GENERIC_READ,
      FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
      OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
  if (file == INVALID_HANDLE_VALUE) return false;
#else
  const int file = open(path.c_str(), O_RDONLY);
  if (file < 0) return false;
#endif
  constexpr uint64_t kMaximumChunk = 1ull << 24;
  uint64_t done = 0;
  while (done < size) {
    const uint64_t remaining = size - done;
    const uint64_t chunk = remaining > kMaximumChunk ? kMaximumChunk : remaining;
    const uint64_t position = offset + done;
#if defined(_WIN32)
    OVERLAPPED overlapped = {};
    overlapped.Offset = static_cast<DWORD>(position & 0xFFFFFFFFull);
    overlapped.OffsetHigh = static_cast<DWORD>(position >> 32);
    DWORD read = 0;
    if (!::ReadFile(file, destination + done, static_cast<DWORD>(chunk), &read,
                    &overlapped) ||
        read == 0) {
      CloseHandle(file);
      return false;
    }
#else
    const ssize_t read = pread(file, destination + done,
                               static_cast<size_t>(chunk),
                               static_cast<off_t>(position));
    if (read <= 0) {
      close(file);
      return false;
    }
#endif
    done += static_cast<uint64_t>(read);
  }
#if defined(_WIN32)
  CloseHandle(file);
#else
  close(file);
#endif
  return true;
}

// A warm cached read costs one page-by-page copy out of the Windows cache
// manager, which saturates a single core near 2 GiB/s no matter how the read is
// chunked. Splitting a large file across a few threads scales that copy with
// cores. Small reads stay on the calling thread, where thread dispatch would
// cost more than the copy it parallelizes.
constexpr uint64_t kParallelReadThresholdBytes = 4ull << 20;
constexpr size_t kParallelReadWorkers = 4;

// Fills `destination` with the whole file, splitting the copy across worker
// threads. `destination` must have room for `size` bytes and must stay alive
// until this returns; every worker is joined before that happens, and the
// workers write disjoint ranges, so no byte is written twice and nothing
// outlives the buffer.
bool ReadFileParallel(const std::filesystem::path& path, uint8_t* destination,
                      uint64_t size) {
  const PathString native = ExtendedPath(path).native();
  const uint64_t span = (size + kParallelReadWorkers - 1) / kParallelReadWorkers;
  std::atomic<bool> failed(false);
  std::vector<std::thread> workers;
  workers.reserve(kParallelReadWorkers - 1);
  for (size_t index = 1; index < kParallelReadWorkers; ++index) {
    const uint64_t offset = span * index;
    if (offset >= size) break;
    const uint64_t length = std::min(span, size - offset);
    workers.emplace_back([&native, destination, offset, length, &failed]() {
      if (!ReadFileRange(native, destination + offset, offset, length)) {
        failed.store(true, std::memory_order_relaxed);
      }
    });
  }
  if (!ReadFileRange(native, destination, 0, std::min(span, size))) {
    failed.store(true, std::memory_order_relaxed);
  }
  for (std::thread& worker : workers) worker.join();
  return !failed.load(std::memory_order_relaxed);
}

// Fills `destination` with the first `size` bytes of `file`. Large reads are
// split across worker threads; anything smaller stays on the calling thread.
// The caller keeps ownership of `file`, which the serial path reads from its
// current position.
bool ReadWholeFile(NativeFile file, const std::filesystem::path& path,
                   void* destination, uint64_t size) {
  if (size == 0) return true;
  if (size >= kParallelReadThresholdBytes &&
      ReadFileParallel(path, static_cast<uint8_t*>(destination), size)) {
    return true;
  }
  return ReadFileBytes(file, destination, size);
}

bool ReadFile(const std::filesystem::path& path, std::string* source) {
  uint64_t size = 0;
  const NativeFile file = OpenFileForRead(path, &size);
  if (file == kInvalidFile) return false;
  if (size > static_cast<uint64_t>(std::numeric_limits<int>::max())) {
    CloseNativeFile(file);
    return false;
  }
  source->resize(static_cast<size_t>(size));
  const bool read = ReadWholeFile(file, path, source->data(), size);
  CloseNativeFile(file);
  return read;
}

bool IsTypeScriptPath(const std::filesystem::path& path) {
  PathString extension = path.extension().native();
#if defined(_WIN32)
  std::transform(extension.begin(), extension.end(), extension.begin(),
                 [](wchar_t value) { return static_cast<wchar_t>(std::towlower(value)); });
#else
  std::transform(extension.begin(), extension.end(), extension.begin(),
                 [](char value) {
                   return static_cast<char>(
                       std::tolower(static_cast<unsigned char>(value)));
                 });
#endif
  return extension == SAKO_PATH_LITERAL(".ts") ||
         extension == SAKO_PATH_LITERAL(".mts") ||
         extension == SAKO_PATH_LITERAL(".cts") ||
         extension == SAKO_PATH_LITERAL(".tsx");
}

/// Removes a leading `#!` line, leaving the newline behind.
///
/// A hashbang is only valid at offset zero, and the CommonJS wrapper puts a
/// function header in front of the source -- so without this every `#!`-headed
/// script fails to parse. That is most of what `node_modules/.bin` points at:
/// `tsc`, and every other extensionless bin stub npm publishes.
///
/// ES modules need no such help; V8 accepts the hashbang grammar directly, and
/// module sources are compiled unwrapped.
void StripShebang(std::string* source) {
  if (!source->starts_with("#!")) return;
  const size_t line_end = source->find('\n');
  // Keeping the newline preserves every later line number, so stack traces
  // still point at the line the file actually has.
  source->erase(0, line_end == std::string::npos ? source->size() : line_end);
}

bool TranspileTypeScript(const std::filesystem::path& path, bool commonjs,
                         std::string* source, std::string* error) {
  if (!IsTypeScriptPath(path)) return true;
  const std::string path_utf8 = PathToUtf8(path);
  char native_error[16 * 1024] = {};
  void* raw = sako_typescript_transpile(
      {reinterpret_cast<const uint8_t*>(path_utf8.data()), path_utf8.size()},
      {reinterpret_cast<const uint8_t*>(source->data()), source->size()},
      commonjs ? 1 : 0, native_error, sizeof(native_error));
  if (raw == nullptr) {
    *error = native_error;
    return false;
  }
  std::unique_ptr<void, void (*)(void*)> output(
      raw, sako_typescript_output_delete);
  const SakoNativeBytes emitted = sako_typescript_output_source(raw);
  if (emitted.length != 0 && emitted.data == nullptr) {
    *error = "TypeScript transpiler returned an invalid output range";
    return false;
  }
  source->assign(reinterpret_cast<const char*>(emitted.data), emitted.length);
  return true;
}

/// Serializes writes to the standard streams.
///
/// `--workers=N` runs N isolates on N threads against the same two handles, and
/// a write is not atomic: one thread's partial WriteFile could land in the
/// middle of another's line, so two workers printing one line each could
/// produce two lines that were neither. One lock across both handles, because
/// stdout and stderr usually share a console and interleave there too.
std::mutex g_output_mutex;

#if defined(_WIN32)
void WriteHandle(HANDLE output, const char* bytes, size_t length) {
  const std::lock_guard<std::mutex> guard(g_output_mutex);
  while (output != INVALID_HANDLE_VALUE && output != nullptr && length != 0) {
    const DWORD chunk = length > MAXDWORD ? MAXDWORD : static_cast<DWORD>(length);
    DWORD written = 0;
    if (!WriteFile(output, bytes, chunk, &written, nullptr) || written == 0) return;
    bytes += written;
    length -= written;
  }
}

void WriteStdout(const char* bytes, size_t length) {
  WriteHandle(GetStdHandle(STD_OUTPUT_HANDLE), bytes, length);
}
#else
void WriteHandle(int output, const char* bytes, size_t length) {
  const std::lock_guard<std::mutex> guard(g_output_mutex);
  while (output >= 0 && length != 0) {
    const ssize_t written = ::write(output, bytes, length);
    if (written <= 0) return;
    bytes += written;
    length -= static_cast<size_t>(written);
  }
}

void WriteStdout(const char* bytes, size_t length) {
  WriteHandle(STDOUT_FILENO, bytes, length);
}
#endif

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
#if defined(_WIN32)
  const DWORD stream = info.Data()->Int32Value(isolate->GetCurrentContext())
                           .FromMaybe(STD_OUTPUT_HANDLE);
  WriteHandle(GetStdHandle(stream), reinterpret_cast<const char*>(bytes),
              length);
#else
  const int stream = info.Data()->Int32Value(isolate->GetCurrentContext())
                          .FromMaybe(STDOUT_FILENO);
  WriteHandle(stream, reinterpret_cast<const char*>(bytes), length);
#endif
  info.GetReturnValue().Set(v8::True(isolate));
}

/// Writes to standard output or error by descriptor number.
///
/// The per-execution `process.stdout` object binds its handle through
/// `info.Data()`, which a JavaScript-side stream cannot reach. The fs
/// `writeSync` is no substitute: it resolves descriptors through Sako's own
/// table of opened files, where 1 and 2 were never registered, so it reports
/// "file descriptor is not open" for the very streams that are always open.
void WriteStandard(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (info.Length() < 2) {
    info.GetReturnValue().Set(v8::True(isolate));
    return;
  }
  const int descriptor = info[0]->Int32Value(context).FromMaybe(1);
  const uint8_t* bytes = nullptr;
  size_t length = 0;
  std::string text;
  if (info[1]->IsString()) {
    text = ToUtf8(isolate, info[1]);
    bytes = reinterpret_cast<const uint8_t*>(text.data());
    length = text.size();
  } else if (!ReadBytes(info[1], &bytes, &length)) {
    ThrowTypeError(isolate, "stream write needs a string or byte array");
    return;
  }
#if defined(_WIN32)
  const DWORD stream =
      descriptor == 2 ? STD_ERROR_HANDLE : STD_OUTPUT_HANDLE;
  WriteHandle(GetStdHandle(stream), reinterpret_cast<const char*>(bytes),
              length);
#else
  WriteHandle(descriptor, reinterpret_cast<const char*>(bytes), length);
#endif
  info.GetReturnValue().Set(v8::True(isolate));
}

/// Whether a POSIX-numbered descriptor (0/1/2) is attached to a terminal.
bool IsTerminal(int descriptor) {
#if defined(_WIN32)
  const DWORD stream = descriptor == 1   ? STD_OUTPUT_HANDLE
                       : descriptor == 2 ? STD_ERROR_HANDLE
                                         : STD_INPUT_HANDLE;
  DWORD mode = 0;
  return descriptor >= 0 &&
         GetConsoleMode(GetStdHandle(stream), &mode) != 0;
#else
  return descriptor >= 0 && isatty(descriptor) != 0;
#endif
}

// Standard input.
//
// Nothing here existed before, and `process.stdin` was wired to the fs
// `readSync`, which resolves descriptors through Sako's own table of opened
// files. Descriptor 0 was never in that table, so every read threw and the
// JavaScript side read the throw as end-of-input: `process.stdin` reported EOF
// on the first read whether input was a pipe, a file, or a keyboard.
//
// Raw mode was missing for the same reason -- `setRawMode` returned `this` and
// did nothing. Together those two gaps are why an interactive scaffolder
// (`sako create vite`) printed its first prompt and exited instead of waiting:
// it asked for raw mode, got a silent yes, attached a keypress listener that
// nothing could ever feed, and the event loop found no work left to do.

/// The console output code page to put back on the way out.
///
/// The CLI switches it to UTF-8 for the duration of a run and restores it when
/// `run()` returns -- but `process.exit` terminates instead of returning, so a
/// script that called it left the code page switched for whatever ran next in
/// that console. Recorded here by the CLI so the exit path can undo it too.
#if defined(_WIN32)
UINT g_saved_output_code_page = 0;
#endif

/// The console mode to put back when raw mode ends.
///
/// Saved on the first switch into raw mode rather than at startup: a mode
/// captured before the program ran would also be restored over a mode the user
/// set deliberately in between.
#if defined(_WIN32)
DWORD g_saved_console_mode = 0;
#else
struct termios g_saved_terminal_mode;
#endif
bool g_terminal_mode_saved = false;

/// Puts the terminal back the way it was found.
///
/// Raw mode outlives the process that set it: a console left with echo and
/// line editing off stays that way for the shell that follows, which is the
/// difference between a prompt that exits cleanly and a terminal the user has
/// to reset by hand. Called from every exit path, including `process.exit`,
/// which terminates rather than unwinding and so runs no destructors.
#if !defined(_WIN32)
void InstallTerminalSignalHandlers();
#endif

void RestoreTerminalMode() {
#if defined(_WIN32)
  if (g_saved_output_code_page != 0) {
    SetConsoleOutputCP(g_saved_output_code_page);
    g_saved_output_code_page = 0;
  }
#endif
  if (!g_terminal_mode_saved) return;
  g_terminal_mode_saved = false;
#if defined(_WIN32)
  SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), g_saved_console_mode);
#else
  tcsetattr(STDIN_FILENO, TCSANOW, &g_saved_terminal_mode);
#endif
}

#if !defined(_WIN32)
/// Puts the terminal back before dying on a signal that would otherwise leave
/// it in raw mode -- a killed prompt should not cost the user their echo. The
/// handler restores, reinstates the default disposition, and re-raises, so the
/// process still ends the way the sender asked it to.
extern "C" void TerminalSignalHandler(int number) {
  RestoreTerminalMode();
  signal(number, SIG_DFL);
  raise(number);
}

void InstallTerminalSignalHandlers() {
  // SIGINT is included for the line-mode case: raw mode clears ISIG, so there
  // Ctrl+C arrives as a byte instead.
  for (int number : {SIGHUP, SIGINT, SIGTERM, SIGQUIT}) {
    struct sigaction action {};
    action.sa_handler = TerminalSignalHandler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = SA_RESETHAND;
    sigaction(number, &action, nullptr);
  }
}
#endif

#if defined(_WIN32)
BOOL WINAPI ConsoleControlHandler(DWORD) {
  // Ctrl+C with ENABLE_PROCESSED_INPUT cleared is delivered as a byte rather
  // than an event, so this only runs for the cases the program cannot see --
  // a console close or a logoff. Restoring and declining to handle lets the
  // default terminator run against a sane terminal.
  RestoreTerminalMode();
  return FALSE;
}
#endif

/// Switches descriptor 0 between line mode and raw mode.
///
/// Returns whether raw mode is now in effect, which is false for a redirected
/// stdin: there is no terminal to configure, and a caller that checks can fall
/// back to reading lines.
bool SetStdinRawMode(bool enable) {
#if defined(_WIN32)
  const HANDLE handle = GetStdHandle(STD_INPUT_HANDLE);
  DWORD mode = 0;
  if (handle == INVALID_HANDLE_VALUE || handle == nullptr ||
      GetConsoleMode(handle, &mode) == 0) {
    return false;
  }
  if (!enable) {
    RestoreTerminalMode();
    return false;
  }
  if (!g_terminal_mode_saved) {
    g_saved_console_mode = mode;
    g_terminal_mode_saved = true;
    // Once per process. Registering again on each entry into raw mode would
    // stack duplicate entries in the handler list that nothing removes.
    static bool handler_installed = false;
    if (!handler_installed) {
      handler_installed = SetConsoleCtrlHandler(ConsoleControlHandler, TRUE) != 0;
    }
  }
  // ENABLE_VIRTUAL_TERMINAL_INPUT is what makes this tractable: the console
  // itself turns arrow keys and function keys into the escape sequences the
  // rest of the world already speaks, so the decoder in bootstrap.js is the
  // same one that reads a POSIX terminal.
  DWORD raw = mode & ~static_cast<DWORD>(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT |
                                         ENABLE_PROCESSED_INPUT);
  raw |= ENABLE_VIRTUAL_TERMINAL_INPUT;
  return SetConsoleMode(handle, raw) != 0;
#else
  if (isatty(STDIN_FILENO) == 0) return false;
  if (!enable) {
    RestoreTerminalMode();
    return false;
  }
  if (!g_terminal_mode_saved) {
    if (tcgetattr(STDIN_FILENO, &g_saved_terminal_mode) != 0) return false;
    g_terminal_mode_saved = true;
    InstallTerminalSignalHandlers();
  }
  struct termios raw = g_saved_terminal_mode;
  // ISIG off is deliberate and matches Node: in raw mode Ctrl+C arrives as
  // 0x03 for the program to interpret, which is how a prompt offers "cancel"
  // rather than dying mid-render and leaving the terminal dressed.
  raw.c_lflag &= ~static_cast<tcflag_t>(ECHO | ICANON | IEXTEN | ISIG);
  raw.c_iflag &= ~static_cast<tcflag_t>(IXON | ICRNL | BRKINT | INPCK | ISTRIP);
  raw.c_cc[VMIN] = 1;
  raw.c_cc[VTIME] = 0;
  return tcsetattr(STDIN_FILENO, TCSANOW, &raw) == 0;
#endif
}

/// What one read of standard input produced.
enum class StdinRead { Bytes, Timeout, End };

#if defined(_WIN32)
/// Spends what is left of a read's budget asleep.
///
/// A console handle stays signalled for as long as anything is queued on it,
/// so once the user has typed a character that does not yet finish a line,
/// waiting on the handle returns immediately every time. Returning straight
/// away turned that into a spin -- a hundred per cent of a core for as long as
/// someone was mid-word. Sleeping the remainder keeps the poll rate the caller
/// asked for.
StdinRead WaitOutTheBudget(uint32_t timeout_milliseconds) {
  if (timeout_milliseconds != 0) Sleep(timeout_milliseconds);
  return StdinRead::Timeout;
}

/// The leading half of a character a console read cut in two. See ReadStdin.
wchar_t g_pending_high_surrogate = 0;

/// Whether the console has a complete line queued.
///
/// The records sit in the input queue until a read consumes them, so a
/// carriage return among them means `ReadConsoleW` has a line to hand back and
/// will not have to wait for the keyboard.
bool ConsoleLineIsReady(HANDLE handle) {
  DWORD queued = 0;
  if (GetNumberOfConsoleInputEvents(handle, &queued) == 0 || queued == 0) {
    return false;
  }
  // Bounded: a paste can queue a great many records, and scanning all of them
  // on every poll would cost more than the wait it saves. Anything past the
  // cap is found by a later poll, once the front of the queue is consumed.
  constexpr DWORD kMaximumScan = 4096;
  std::vector<INPUT_RECORD> records(std::min(queued, kMaximumScan));
  DWORD peeked = 0;
  if (PeekConsoleInputW(handle, records.data(),
                        static_cast<DWORD>(records.size()), &peeked) == 0) {
    return false;
  }
  for (DWORD index = 0; index < peeked; ++index) {
    const INPUT_RECORD& record = records[index];
    if (record.EventType != KEY_EVENT || record.Event.KeyEvent.bKeyDown == 0) {
      continue;
    }
    const wchar_t character = record.Event.KeyEvent.uChar.UnicodeChar;
    // Ctrl+Z ends the input just as Enter does, and the console returns from
    // the read either way.
    if (character == L'\r' || character == L'\n' || character == 0x1a) return true;
  }
  return false;
}
#endif

/// Reads whatever standard input has, waiting at most `timeout_milliseconds`.
///
/// The timeout is what lets a blocking read live inside a single-threaded
/// event loop: the pump asks for input, and either gets some or hands control
/// back so timers still fire.
StdinRead ReadStdin(uint32_t timeout_milliseconds, std::string* out) {
#if defined(_WIN32)
  const HANDLE handle = GetStdHandle(STD_INPUT_HANDLE);
  if (handle == INVALID_HANDLE_VALUE || handle == nullptr) return StdinRead::End;
  DWORD mode = 0;
  if (GetConsoleMode(handle, &mode) != 0) {
    if (WaitForSingleObject(handle, timeout_milliseconds) != WAIT_OBJECT_0) {
      return StdinRead::Timeout;
    }
    // In raw mode the handle signals for every input record, most of which
    // carry no characters: focus changes, key releases, window resizes, and
    // mouse movement all wake it. Discarding them keeps ReadConsoleW from
    // blocking past the timeout on a record it would ignore anyway.
    //
    // Only in raw mode. With line input still on, the console host does the
    // editing itself when the read happens, and it needs the very records this
    // loop throws away -- an arrow key carries no character but does move the
    // cursor.
    if ((mode & ENABLE_LINE_INPUT) == 0) {
      for (;;) {
        INPUT_RECORD record{};
        DWORD peeked = 0;
        if (PeekConsoleInputW(handle, &record, 1, &peeked) == 0) return StdinRead::End;
        if (peeked == 0) return WaitOutTheBudget(timeout_milliseconds);
        if (record.EventType == KEY_EVENT && record.Event.KeyEvent.bKeyDown != 0 &&
            record.Event.KeyEvent.uChar.UnicodeChar != 0) {
          break;
        }
        DWORD consumed = 0;
        if (ReadConsoleInputW(handle, &record, 1, &consumed) == 0) return StdinRead::End;
      }
    } else if (!ConsoleLineIsReady(handle)) {
      // With line input on, ReadConsoleW does not return until the user
      // presses Enter -- and this runtime has one thread, so calling it early
      // froze every timer for as long as someone was still typing. Waiting
      // until the whole line is queued keeps the read short.
      return WaitOutTheBudget(timeout_milliseconds);
    }
    wchar_t wide[2048];
    // The high half of a character the previous read cut in two goes first, so
    // the pair converts as one.
    DWORD carried = 0;
    if (g_pending_high_surrogate != 0) {
      wide[0] = g_pending_high_surrogate;
      g_pending_high_surrogate = 0;
      carried = 1;
    }
    DWORD read = 0;
    if (ReadConsoleW(handle, wide + carried,
                     static_cast<DWORD>(std::size(wide)) - carried, &read,
                     nullptr) == 0) {
      return StdinRead::End;
    }
    read += carried;
    // Zero characters from a console read is Ctrl+Z at a line prompt, the
    // Windows spelling of end-of-input.
    if (read == 0) return StdinRead::End;
    // A character outside the basic plane is two UTF-16 units, and a read can
    // land between them. Converting a lone surrogate substitutes U+FFFD and
    // loses the character for good, so the tail is held back for the next read
    // to complete.
    if (wide[read - 1] >= 0xd800 && wide[read - 1] <= 0xdbff) {
      g_pending_high_surrogate = wide[read - 1];
      --read;
      if (read == 0) return StdinRead::Timeout;
    }
    const int needed = WideCharToMultiByte(CP_UTF8, 0, wide, static_cast<int>(read),
                                           nullptr, 0, nullptr, nullptr);
    if (needed <= 0) return StdinRead::Timeout;
    out->resize(static_cast<size_t>(needed));
    WideCharToMultiByte(CP_UTF8, 0, wide, static_cast<int>(read), out->data(),
                        needed, nullptr, nullptr);
    return StdinRead::Bytes;
  }

  // A pipe has to be peeked rather than read: ReadFile on an empty pipe blocks
  // until the writer sends something or closes, which would hold the event
  // loop for as long as the other end stays quiet.
  if (GetFileType(handle) == FILE_TYPE_PIPE) {
    const auto deadline =
        std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_milliseconds);
    for (;;) {
      DWORD available = 0;
      // Every failure here means the same thing to a reader: nothing more is
      // coming. A broken pipe is the writer having finished, and anything else
      // is a handle that can no longer be read.
      if (PeekNamedPipe(handle, nullptr, 0, nullptr, &available, nullptr) == 0) {
        return StdinRead::End;
      }
      if (available != 0) break;
      if (std::chrono::steady_clock::now() >= deadline) return StdinRead::Timeout;
      Sleep(1);
    }
  }
  char buffer[8192];
  DWORD read = 0;
  // Qualified: this translation unit has its own ReadFile, which reads a whole
  // file by path.
  if (::ReadFile(handle, buffer, static_cast<DWORD>(sizeof(buffer)), &read,
                 nullptr) == 0) {
    return StdinRead::End;
  }
  if (read == 0) return StdinRead::End;
  out->assign(buffer, read);
  return StdinRead::Bytes;
#else
  struct pollfd watched {};
  watched.fd = STDIN_FILENO;
  watched.events = POLLIN;
  const int ready = poll(&watched, 1, static_cast<int>(timeout_milliseconds));
  if (ready == 0) return StdinRead::Timeout;
  if (ready < 0) return errno == EINTR ? StdinRead::Timeout : StdinRead::End;
  char buffer[8192];
  const ssize_t read_bytes = read(STDIN_FILENO, buffer, sizeof(buffer));
  if (read_bytes == 0) return StdinRead::End;
  if (read_bytes < 0) {
    return (errno == EINTR || errno == EAGAIN) ? StdinRead::Timeout : StdinRead::End;
  }
  out->assign(buffer, static_cast<size_t>(read_bytes));
  return StdinRead::Bytes;
#endif
}

/// Backs `process.stdin.setRawMode`.
void StdinSetRawMode(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  const bool enable = info.Length() > 0 && info[0]->BooleanValue(isolate);
  info.GetReturnValue().Set(SetStdinRawMode(enable));
}

/// Backs the stdin pump. Returns the bytes read, an empty array when the wait
/// expired with nothing to show for it, or null at end of input.
void StdinRead(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  double requested = info.Length() > 0 ? info[0]->NumberValue(context).FromMaybe(0) : 0;
  if (!std::isfinite(requested) || requested < 0) requested = 0;
  const uint32_t timeout = static_cast<uint32_t>(std::min(requested, 60000.0));

  std::string bytes;
  switch (ReadStdin(timeout, &bytes)) {
    case StdinRead::End:
      info.GetReturnValue().SetNull();
      return;
    case StdinRead::Timeout:
      // Undefined rather than an empty array: the pump asks about fifty times
      // a second while a prompt waits, and every empty array was a buffer, a
      // backing store, and a typed array for the collector to walk.
      info.GetReturnValue().SetUndefined();
      return;
    case StdinRead::Bytes:
      break;
  }
  v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(isolate, bytes.size());
  std::memcpy(buffer->Data(), bytes.data(), bytes.size());
  info.GetReturnValue().Set(v8::Uint8Array::New(buffer, 0, bytes.size()));
}

/// Backs process.exit.
///
/// Terminates immediately like Node's, rather than unwinding: callers use it
/// to stop mid-script, and returning would let the remaining statements run.
/// Streams are flushed first because the C runtime's own flush does not cover
/// a handle written through WriteFile.
/// Ends the process from inside a running script.
///
/// Not `std::exit`: that walks the C runtime's onexit table, which disposes
/// V8 while this isolate is still alive -- still running the very script that
/// asked to exit. V8 checks for exactly that and aborts with a fatal error
/// instead of exiting, so `process.exit(7)` reported a crash rather than 7.
/// Everything the process wrote is already flushed by the time this runs, and
/// the kernel reclaims the rest.
void ProcessExit(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const int code =
      info.Length() == 0 ? 0 : info[0]->Int32Value(context).FromMaybe(0);
  // Before the terminate below, which runs nothing else.
  RestoreTerminalMode();
  std::fflush(stdout);
  std::fflush(stderr);
#if defined(_WIN32)
  TerminateProcess(GetCurrentProcess(), static_cast<UINT>(code));
#else
  _exit(code);
#endif
  std::abort();  // Not reached.
}

/// Reports the terminal size for a descriptor, or undefined when it is not a
/// terminal. TUI output -- progress bars, prompt frames, wrapped help -- needs
/// the real width; guessing 80 misrenders on every other console.
void TerminalSize(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const int descriptor =
      info.Length() == 0 ? 1 : info[0]->Int32Value(context).FromMaybe(1);
  int columns = 0;
  int rows = 0;
#if defined(_WIN32)
  const DWORD stream = descriptor == 2 ? STD_ERROR_HANDLE : STD_OUTPUT_HANDLE;
  CONSOLE_SCREEN_BUFFER_INFO screen;
  if (GetConsoleScreenBufferInfo(GetStdHandle(stream), &screen) == 0) return;
  columns = screen.srWindow.Right - screen.srWindow.Left + 1;
  rows = screen.srWindow.Bottom - screen.srWindow.Top + 1;
#else
  struct winsize size;
  if (ioctl(descriptor, TIOCGWINSZ, &size) != 0) return;
  columns = size.ws_col;
  rows = size.ws_row;
#endif
  if (columns <= 0 || rows <= 0) return;
  v8::Local<v8::Object> result = v8::Object::New(isolate);
  if (result
          ->Set(context, v8::String::NewFromUtf8Literal(isolate, "columns"),
                v8::Integer::New(isolate, columns))
          .FromMaybe(false) &&
      result
          ->Set(context, v8::String::NewFromUtf8Literal(isolate, "rows"),
                v8::Integer::New(isolate, rows))
          .FromMaybe(false)) {
    info.GetReturnValue().Set(result);
  }
}

void IsTty(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const int descriptor =
      info.Length() == 0 ? -1 : info[0]->Int32Value(context).FromMaybe(-1);
  (void)isolate;
  info.GetReturnValue().Set(IsTerminal(descriptor));
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
  // The caller has already built the command line the way the child expects.
  const int verbatim =
      info.Length() > 3 && info[3]->BooleanValue(isolate) ? 1 : 0;
  char error[1024] = {};
  void* raw = sako_process_spawn_sync(
      {reinterpret_cast<const uint8_t*>(executable.data()), executable.size()},
      arguments.data(), arguments.size(),
      {reinterpret_cast<const uint8_t*>(cwd.data()), cwd.size()}, verbatim,
      error, sizeof(error));
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

// Digests are computed by the Rust side (RustCrypto) on every platform: it
// needs no OS crypto API at all, which removes a Windows-only BCrypt
// dependency here without adding an OpenSSL/libcrypto one on POSIX.
//
// The algorithm crosses as a number rather than a name so this callback does
// no string comparison and no allocation; bootstrap.js owns the mapping from
// the names node:crypto accepts to the `SAKO_HASH_*` values in lib.rs, and an
// algorithm the Rust side does not know is reported by writing nothing.
constexpr size_t kMaximumDigestBytes = 64;

void Hash(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumHashBytes = 256 * 1024 * 1024;
  v8::Isolate* isolate = info.GetIsolate();
  const uint8_t* bytes = nullptr;
  size_t length = 0;
  if (info.Length() < 2 || !info[0]->IsUint32() ||
      !ReadBytes(info[1], &bytes, &length)) {
    ThrowTypeError(isolate, "hash needs an algorithm and a byte array");
    return;
  }
  if (length > kMaximumHashBytes) {
    isolate->ThrowException(v8::Exception::RangeError(
        v8::String::NewFromUtf8Literal(isolate, "hash input exceeds byte limit")));
    return;
  }
  const uint32_t algorithm =
      info[0]->Uint32Value(isolate->GetCurrentContext()).FromMaybe(0);
  uint8_t digest[kMaximumDigestBytes];
  const size_t written = sako_hash(algorithm, {bytes, length}, digest);
  if (written == 0 || written > sizeof(digest)) {
    ThrowTypeError(isolate, "unsupported hash algorithm");
    return;
  }
  std::unique_ptr<v8::BackingStore> backing =
      v8::ArrayBuffer::NewBackingStore(isolate, written);
  std::memcpy(backing->Data(), digest, written);
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(isolate, std::move(backing));
  info.GetReturnValue().Set(v8::Uint8Array::New(buffer, 0, written));
}

void ConsoleLog(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::HandleScope scope(isolate);
  v8::Local<v8::Context> context = isolate->GetCurrentContext();

  // Assembled first and written once. Writing each argument, each separator,
  // and the newline separately meant a `--workers=N` run could interleave two
  // workers' lines -- the text of one landing between the text and the newline
  // of another. It is also three to four fewer write calls per line.
  std::string line;
  for (int index = 0; index < info.Length(); ++index) {
    if (index != 0) line.push_back(' ');
    v8::Local<v8::String> text;
    if (info[index]->ToString(context).ToLocal(&text)) {
      line += ToUtf8(isolate, text);
    }
  }
  line.push_back('\n');
  WriteStdout(line.data(), line.size());
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
  const std::string utf8 = ToUtf8(info.GetIsolate(), info[0]);
#if defined(_WIN32)
  return std::filesystem::path(Utf8ToWide(utf8));
#else
  return std::filesystem::path(utf8);
#endif
}

std::filesystem::path ValuePath(v8::Isolate* isolate,
                                v8::Local<v8::Value> value) {
  if (!value->IsString()) return {};
  const std::string utf8 = ToUtf8(isolate, value);
#if defined(_WIN32)
  return std::filesystem::path(Utf8ToWide(utf8));
#else
  return std::filesystem::path(utf8);
#endif
}

/// Node's `error.code` for a filesystem failure.
///
/// Real code branches on the code, never on the message: a watcher stats a
/// file that may have been deleted and treats `ENOENT` as ordinary, a mkdir
/// treats `EEXIST` as success. Without a code every miss reads as a hard
/// failure and takes the program down instead.
const char* ErrorCodeName(const std::error_condition& condition) {
  switch (static_cast<std::errc>(condition.value())) {
    case std::errc::no_such_file_or_directory: return "ENOENT";
    case std::errc::permission_denied: return "EACCES";
    case std::errc::operation_not_permitted: return "EPERM";
    case std::errc::file_exists: return "EEXIST";
    case std::errc::not_a_directory: return "ENOTDIR";
    case std::errc::is_a_directory: return "EISDIR";
    case std::errc::directory_not_empty: return "ENOTEMPTY";
    case std::errc::device_or_resource_busy: return "EBUSY";
    case std::errc::too_many_files_open: return "EMFILE";
    case std::errc::too_many_files_open_in_system: return "ENFILE";
    case std::errc::no_space_on_device: return "ENOSPC";
    case std::errc::invalid_argument: return "EINVAL";
    case std::errc::filename_too_long: return "ENAMETOOLONG";
    case std::errc::too_many_symbolic_link_levels: return "ELOOP";
    case std::errc::cross_device_link: return "EXDEV";
    case std::errc::read_only_file_system: return "EROFS";
    default: return "UNKNOWN";
  }
}

/// Throws the error shape Node's fs module produces: a message that reads
/// well, plus the `code`, `errno`, `syscall`, and `path` properties callers
/// actually inspect.
void ThrowFileErrorWithCode(v8::Isolate* isolate, const std::string& operation,
                            const std::filesystem::path& path,
                            const std::string& reason,
                            const std::error_condition& condition) {
  const std::string path_text = PathToUtf8(path);
  const char* code = ErrorCodeName(condition);
  const std::string message = std::string(code) + ": " + reason + ", " +
                              operation + " " + path_text;
  v8::Local<v8::String> text;
  if (!v8::String::NewFromUtf8(isolate, message.data(),
                               v8::NewStringType::kNormal,
                               static_cast<int>(message.size()))
           .ToLocal(&text)) {
    return;
  }
  v8::Local<v8::Value> error = v8::Exception::Error(text);
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  if (!context.IsEmpty() && error->IsObject()) {
    v8::Local<v8::Object> object = error.As<v8::Object>();
    (void)object->Set(context, v8::String::NewFromUtf8Literal(isolate, "code"),
                      v8::String::NewFromUtf8(isolate, code).ToLocalChecked());
    (void)object->Set(context, v8::String::NewFromUtf8Literal(isolate, "errno"),
                      v8::Integer::New(isolate, -condition.value()));
    (void)object->Set(
        context, v8::String::NewFromUtf8Literal(isolate, "syscall"),
        v8::String::NewFromUtf8(isolate, operation.data(),
                                v8::NewStringType::kNormal,
                                static_cast<int>(operation.size()))
            .ToLocalChecked());
    if (!path_text.empty()) {
      (void)object->Set(
          context, v8::String::NewFromUtf8Literal(isolate, "path"),
          v8::String::NewFromUtf8(isolate, path_text.data(),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(path_text.size()))
              .ToLocalChecked());
    }
  }
  isolate->ThrowException(error);
}

void ThrowFileError(v8::Isolate* isolate, const std::string& operation,
                    const std::filesystem::path& path) {
#if defined(_WIN32)
  const std::error_code error(static_cast<int>(GetLastError()),
                              std::system_category());
#else
  const std::error_code error(errno, std::generic_category());
#endif
  ThrowFileErrorWithCode(isolate, operation, path, error.message(),
                         error.default_error_condition());
}

void ThrowFileError(v8::Isolate* isolate, const std::string& operation,
                    const std::filesystem::path& path,
                    const std::error_code& error) {
  ThrowFileErrorWithCode(isolate, operation, path, error.message(),
                         error.default_error_condition());
}

void ReadFileSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
  constexpr size_t kMaximumFileBytes = 256 * 1024 * 1024;
  v8::Isolate* isolate = info.GetIsolate();
  const std::filesystem::path path = CallbackPath(info);
  if (path.empty()) {
    ThrowTypeError(isolate, "readFileSync needs a string path");
    return;
  }
  const bool text =
      info.Length() > 1 && info[1]->IsString() &&
      (ToUtf8(isolate, info[1]) == "utf8" || ToUtf8(isolate, info[1]) == "utf-8");
  uint64_t size = 0;
  const NativeFile file = OpenFileForRead(path, &size);
  if (file == kInvalidFile || size > kMaximumFileBytes) {
    if (file != kInvalidFile) CloseNativeFile(file);
    ThrowFileError(isolate, "read", path);
    return;
  }
  const size_t length = static_cast<size_t>(size);
  if (text) {
    std::string bytes;
    bytes.resize(length);
    const bool read = ReadWholeFile(file, path, bytes.data(), length);
    CloseNativeFile(file);
    if (!read || length > static_cast<size_t>(std::numeric_limits<int>::max())) {
      ThrowFileError(isolate, "read", path);
      return;
    }
    v8::Local<v8::String> value;
    if (v8::String::NewFromUtf8(isolate, bytes.data(),
                                v8::NewStringType::kNormal,
                                static_cast<int>(bytes.size()))
            .ToLocal(&value)) {
      info.GetReturnValue().Set(value);
    }
    return;
  }
  // The backing store is left uninitialized because the read below overwrites
  // every byte of it; zeroing first would double the cost of a large read.
  std::unique_ptr<v8::BackingStore> backing =
      v8::ArrayBuffer::NewBackingStore(
          isolate, length, v8::BackingStoreInitializationMode::kUninitialized);
  const bool read = ReadWholeFile(file, path, backing->Data(), length);
  CloseNativeFile(file);
  if (!read) {
    ThrowFileError(isolate, "read", path);
    return;
  }
  v8::Local<v8::ArrayBuffer> buffer =
      v8::ArrayBuffer::New(isolate, std::move(backing));
  info.GetReturnValue().Set(v8::Uint8Array::New(buffer, 0, length));
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
  // file_clock's epoch is not the Unix epoch -- on Windows it starts in 1601 --
  // so the raw count is only good for comparing two files. Rebasing it is what
  // makes `new Date(stats.mtimeMs)` mean anything, and a static file server
  // stamping Last-Modified needs it to.
  //
  // The offset is measured once. Sampling both clocks per stat would fold the
  // gap between the two reads into the result, and a watcher comparing
  // timestamps would then see every file change every time it looked.
  static const std::chrono::system_clock::duration epoch_offset = [] {
    const auto file_now = std::filesystem::file_time_type::clock::now();
    const auto system_now = std::chrono::system_clock::now();
    return system_now.time_since_epoch() -
           std::chrono::duration_cast<std::chrono::system_clock::duration>(
               file_now.time_since_epoch());
  }();
  const double modified_milliseconds = static_cast<double>(
      std::chrono::duration_cast<std::chrono::milliseconds>(
          std::chrono::duration_cast<std::chrono::system_clock::duration>(
              modified.time_since_epoch()) +
          epoch_offset)
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

bool ReadCurrentDirectory(std::string* output) {
#if defined(_WIN32)
  const DWORD required = GetCurrentDirectoryW(0, nullptr);
  std::wstring path(required, L'\0');
  const DWORD written =
      required == 0 ? 0 : GetCurrentDirectoryW(required, path.data());
  if (written == 0) return false;
  path.resize(written);
  *output = WideToUtf8(path);
#else
  std::string utf8;
  utf8.resize(4096);
  while (getcwd(utf8.data(), utf8.size()) == nullptr) {
    if (errno != ERANGE) return false;
    utf8.resize(utf8.size() * 2);
  }
  utf8.resize(std::strlen(utf8.c_str()));
  *output = std::move(utf8);
#endif
  return true;
}

void ProcessCwd(const v8::FunctionCallbackInfo<v8::Value>& info) {
  std::string utf8;
  if (!ReadCurrentDirectory(&utf8)) {
    ThrowFileError(info.GetIsolate(), "read current directory", {});
    return;
  }
  info.GetReturnValue().Set(
      v8::String::NewFromUtf8(info.GetIsolate(), utf8.data(),
                              v8::NewStringType::kNormal,
                              static_cast<int>(utf8.size()))
          .ToLocalChecked());
}

/// Changes the working directory, and the copy of it the path helpers read.
///
/// `__sakoCwd` is a global rather than a call because `path.resolve` reads it
/// on every join. Leaving it stale after a chdir would make every relative
/// path resolve against the directory the program started in -- which is
/// exactly what a scaffolding tool does not want, having just chdir'd into the
/// project it created.
void ProcessChdir(const v8::FunctionCallbackInfo<v8::Value>& info) {
  v8::Isolate* isolate = info.GetIsolate();
  v8::Local<v8::Context> context = isolate->GetCurrentContext();
  const std::filesystem::path target = CallbackPath(info);
  if (target.empty()) {
    ThrowTypeError(isolate, "chdir needs a string path");
    return;
  }
#if defined(_WIN32)
  // Deliberately not the extended-length form. A working directory cannot
  // exceed MAX_PATH anyway, and the extended-length prefix would survive
  // through GetCurrentDirectoryW into every child's inherited cwd --
  // where cmd.exe refuses it outright and falls back to the Windows
  // directory.
  if (SetCurrentDirectoryW(target.native().c_str()) == 0) {
    ThrowFileError(isolate, "chdir", target);
    return;
  }
#else
  if (chdir(target.c_str()) != 0) {
    ThrowFileError(isolate, "chdir", target);
    return;
  }
#endif
  std::string utf8;
  if (!ReadCurrentDirectory(&utf8)) return;
  (void)context->Global()->Set(
      context, v8::String::NewFromUtf8Literal(isolate, "__sakoCwd"),
      v8::String::NewFromUtf8(isolate, utf8.data(), v8::NewStringType::kNormal,
                              static_cast<int>(utf8.size()))
          .ToLocalChecked());
}

/// Appends the `cause` chain, the way Node prints a wrapped error.
///
/// A library that tries several strategies before giving up -- which is every
/// package that probes for a native binding -- reports its own summary and
/// hangs the real reasons off `cause`. Without this the only thing a user sees
/// is that summary, which is usually a guess ("reinstall your node_modules")
/// and never the actual failure.
void AppendCauses(v8::Isolate* isolate, v8::Local<v8::Context> context,
                  v8::Local<v8::Value> error, std::string* output) {
  constexpr int kMaximumCauseDepth = 8;
  // The walk reads properties, which can run a getter and throw. That must not
  // replace the exception being reported, so it gets a TryCatch of its own.
  v8::TryCatch try_catch(isolate);
  try_catch.SetVerbose(false);
  std::string indent = "  ";
  for (int depth = 0; depth < kMaximumCauseDepth; ++depth) {
    if (!error->IsObject()) break;
    v8::Local<v8::Value> cause;
    if (!error.As<v8::Object>()
             ->Get(context, v8::String::NewFromUtf8Literal(isolate, "cause"))
             .ToLocal(&cause) ||
        cause->IsUndefined() || cause->IsNull()) {
      break;
    }
    v8::Local<v8::Value> text = cause;
    if (cause->IsObject()) {
      v8::Local<v8::Value> stack;
      if (cause.As<v8::Object>()
              ->Get(context, v8::String::NewFromUtf8Literal(isolate, "stack"))
              .ToLocal(&stack) &&
          stack->IsString()) {
        text = stack;
      }
    }
    v8::Local<v8::String> rendered;
    if (!text->ToString(context).ToLocal(&rendered)) break;
    *output += "\n" + indent + "[cause]: ";
    // Continuation lines line up under the first, so a nested stack still
    // reads as one block rather than running back to column zero.
    for (const char character : ToUtf8(isolate, rendered)) {
      *output += character;
      if (character == '\n') *output += indent + "  ";
    }
    error = cause;
    indent += "  ";
  }
  try_catch.Reset();
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

  v8::Local<v8::Value> exception = try_catch.Exception();
  v8::Local<v8::Value> stack;
  if (try_catch.StackTrace(context).ToLocal(&stack) && stack->IsString()) {
    output += ToUtf8(isolate, stack);
    if (!exception.IsEmpty()) AppendCauses(isolate, context, exception, &output);
    return output;
  }

  if (!exception.IsEmpty()) {
    output += ToUtf8(isolate, exception);
    AppendCauses(isolate, context, exception, &output);
    return output;
  }
  return output.empty() ? "JavaScript execution failed" : output;
}

std::string FormatRejection(v8::Isolate* isolate,
                            v8::Local<v8::Context> context,
                            v8::Local<v8::Value> reason) {
  std::string output;
  if (reason->IsObject()) {
    v8::Local<v8::Value> stack;
    if (reason.As<v8::Object>()
            ->Get(context,
                  v8::String::NewFromUtf8Literal(isolate, "stack"))
            .ToLocal(&stack) &&
        stack->IsString()) {
      output = ToUtf8(isolate, stack);
      AppendCauses(isolate, context, reason, &output);
      return output;
    }
  }
  output = ToUtf8(isolate, reason);
  AppendCauses(isolate, context, reason, &output);
  return output;
}

// Keeps a few recently freed large ArrayBuffer blocks instead of returning them
// to the system, and hands them back to uninitialized allocations. A workload
// that reads a file repeatedly allocates and frees the same size every
// iteration, and reusing the block skips the soft page faults that fresh
// storage would take as the read fills it.
class PooledArrayBufferAllocator : public v8::ArrayBuffer::Allocator {
 public:
  explicit PooledArrayBufferAllocator(
      std::unique_ptr<v8::ArrayBuffer::Allocator> base)
      : base_(std::move(base)) {}

  ~PooledArrayBufferAllocator() override {
    for (const Block& block : blocks_) base_->Free(block.data, block.length);
  }

  void* Allocate(size_t length) override {
    // Deliberately not pooled. Fresh pages arrive zeroed and are only faulted
    // in as they are touched, which beats memsetting a reused block that the
    // caller may never write to.
    return base_->Allocate(length);
  }

  void* AllocateUninitialized(size_t length) override {
    if (void* reused = Take(length)) return reused;
    return base_->AllocateUninitialized(length);
  }

  void Free(void* data, size_t length) override {
    if (Retain(data, length)) return;
    base_->Free(data, length);
  }

  size_t MaxAllocationSize() const override {
    return base_->MaxAllocationSize();
  }

  v8::PageAllocator* GetPageAllocator() override {
    return base_->GetPageAllocator();
  }

  size_t pooled_bytes() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return pooled_bytes_;
  }

 private:
  struct Block {
    void* data;
    size_t length;
  };

  // Only whole-buffer sizes worth pooling, and never more than a few of them:
  // the point is to absorb a repeating allocation, not to hold memory back.
  static constexpr size_t kMinimumPooledBytes = 1 << 20;
  static constexpr size_t kMaximumPooledBytes = 32u << 20;
  static constexpr size_t kMaximumBlocks = 4;

  void* Take(size_t length) {
    if (length < kMinimumPooledBytes) return nullptr;
    std::lock_guard<std::mutex> lock(mutex_);
    for (auto block = blocks_.begin(); block != blocks_.end(); ++block) {
      if (block->length != length) continue;
      void* data = block->data;
      pooled_bytes_ -= length;
      blocks_.erase(block);
      return data;
    }
    return nullptr;
  }

  bool Retain(void* data, size_t length) {
    if (length < kMinimumPooledBytes || length > kMaximumPooledBytes) {
      return false;
    }
    std::lock_guard<std::mutex> lock(mutex_);
    if (blocks_.size() >= kMaximumBlocks ||
        pooled_bytes_ + length > kMaximumPooledBytes) {
      return false;
    }
    blocks_.push_back({data, length});
    pooled_bytes_ += length;
    return true;
  }

  std::unique_ptr<v8::ArrayBuffer::Allocator> base_;
  mutable std::mutex mutex_;
  std::vector<Block> blocks_;
  size_t pooled_bytes_ = 0;
};

// --- Snapshot-safe context construction ------------------------------------
//
// Everything below is shared verbatim by two callers: the build-time snapshot
// generator (this file compiled with SAKO_SNAPSHOT_GENERATOR) and the runtime
// fallback that rebuilds the context when no usable snapshot exists. Neither
// gets its own copy of the bootstrap sequence, so the two cannot drift.

// Sets one property on an object under a UTF-8 name.
bool SetNamed(v8::Isolate* isolate, v8::Local<v8::Context> context,
              v8::Local<v8::Object> object, const char* name,
              v8::Local<v8::Value> value) {
  v8::Local<v8::String> key;
  return v8::String::NewFromUtf8(isolate, name).ToLocal(&key) &&
         object->Set(context, key, value).FromMaybe(false);
}

// Installs a native callback on the global object.
//
// The callback data is `undefined` rather than a `v8::External` wrapping the
// Runtime: a heap address is meaningless on the next run, and a context that
// holds one cannot be serialized. Callbacks recover the Runtime from the
// isolate instead -- see Runtime::FromCallback.
bool InstallGlobalFunction(v8::Isolate* isolate, v8::Local<v8::Context> context,
                           const char* name, v8::FunctionCallback callback) {
  v8::Local<v8::Function> function;
  return v8::Function::New(context, callback, v8::Undefined(isolate))
             .ToLocal(&function) &&
         SetNamed(isolate, context, context->Global(), name, function);
}

// Reads an environment variable into `value`. False when it is not set.
bool ReadEnvironment(const char* name, std::string* value) {
#if defined(_WIN32)
  // Straight to the API rather than the CRT's getenv/_dupenv_s, whose first
  // call converts the whole environment block into a second, narrow copy. On
  // a machine with a hundred variables that conversion measured forty
  // microseconds, spent to answer one lookup that usually misses.
  wchar_t wide_name[64];
  size_t index = 0;
  for (; name[index] != '\0'; ++index) {
    if (index + 1 >= sizeof(wide_name) / sizeof(wide_name[0])) return false;
    wide_name[index] = static_cast<wchar_t>(name[index]);
  }
  wide_name[index] = L'\0';
  wchar_t inline_buffer[256];
  constexpr DWORD kInlineCount =
      sizeof(inline_buffer) / sizeof(inline_buffer[0]);
  const DWORD length =
      GetEnvironmentVariableW(wide_name, inline_buffer, kInlineCount);
  if (length == 0) return false;
  if (length < kInlineCount) {
    *value = WideToUtf8(std::wstring(inline_buffer, length));
    return true;
  }
  std::wstring buffer(length, L'\0');
  const DWORD written = GetEnvironmentVariableW(
      wide_name, buffer.data(), static_cast<DWORD>(buffer.size()));
  if (written == 0 || written > buffer.size()) return false;
  buffer.resize(written);
  *value = WideToUtf8(buffer);
  return true;
#else
  const char* text = getenv(name);
  if (text == nullptr) return false;
  value->assign(text);
  return true;
#endif
}

#if !defined(SAKO_SNAPSHOT_GENERATOR)
// The blob generated at build time by this same file compiled with
// SAKO_SNAPSHOT_GENERATOR. The address of a global array is a constant, so
// this costs no static constructor at image load.
const v8::StartupData kSnapshotData = {
    reinterpret_cast<const char*>(kSakoSnapshot),
    static_cast<int>(sizeof(kSakoSnapshot))};
#endif

/// The build-time context snapshot, or nullptr when this run has to rebuild
/// the context from source instead.
///
/// A snapshot carries compiled code that V8 validates against its own version
/// and flag hash. `SAKO_V8_FLAGS` moves that hash, and a mismatch is a hard V8
/// check failure rather than a graceful rejection, so an override sends the
/// run down the rebuild path. `SAKO_NO_SNAPSHOT` forces the same path, which
/// is how the fallback stays exercised.
const v8::StartupData* SnapshotBlob() {
#if defined(SAKO_SNAPSHOT_GENERATOR)
  return nullptr;
#else
  if (kSnapshotData.raw_size <= 0) return nullptr;
  std::string value;
  if (ReadEnvironment("SAKO_V8_FLAGS", &value) && !value.empty()) return nullptr;
  if (ReadEnvironment("SAKO_NO_SNAPSHOT", &value) && !value.empty() &&
      value != "0") {
    return nullptr;
  }
  return &kSnapshotData;
#endif
}

// Baseline V8 tuning applied to every isolate before initialization.
//
// Empty on purpose. `--no-short-builtin-calls` was measured here: it drops a
// megabyte of private memory by not remapping V8's builtins into the code
// range, but bought no measurable startup time and makes every builtin call
// indirect, so it is not worth the trade.
constexpr const char* kSakoV8Flags = "";

class Engine {
 public:
  bool Initialize(const char* executable_path, const char* icu_data_path,
                  std::string* error) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (initialized_) return true;
    // Baseline tuning first, so an embedder override from SAKO_V8_FLAGS wins.
    if (kSakoV8Flags[0] != '\0') v8::V8::SetFlagsFromString(kSakoV8Flags);
    std::string override_flags;
    if (ReadEnvironment("SAKO_V8_FLAGS", &override_flags) &&
        !override_flags.empty()) {
      v8::V8::SetFlagsFromString(override_flags.c_str());
    }
    SAKO_PERF_MARK("v8.flags");
    // An empty path means this V8 build already has ICU data compiled in
    // (icu_use_data_file=false), so there is no external file to point at
    // and V8 initializes its built-in data without this call.
    if (icu_data_path[0] != '\0' &&
        !v8::V8::InitializeICUDefaultLocation(executable_path, icu_data_path)) {
      *error = std::string("failed to initialize ICU from ") + icu_data_path;
      return false;
    }
    SAKO_PERF_MARK("v8.icu-data");
    // NewDefaultPlatform() with no size spawns one worker per logical
    // processor minus one -- eleven threads on a twelve-thread machine. A
    // short-lived script never schedules enough parallel background work to
    // use them, and each one costs creation time at startup and a stack in
    // the idle footprint. Four matches what Node settles on, and leaves
    // concurrent marking and background compilation with room to work.
    constexpr int kMaximumWorkerThreads = 4;
    const unsigned int processors = std::thread::hardware_concurrency();
    const int workers =
        processors == 0
            ? kMaximumWorkerThreads
            : std::min<int>(kMaximumWorkerThreads,
                            std::max<int>(1, static_cast<int>(processors) - 1));
    platform_ = v8::platform::NewDefaultPlatform(workers);
    if (!platform_) {
      *error = "failed to create the V8 platform";
      return false;
    }
    SAKO_PERF_MARK("v8.platform");
    v8::V8::InitializePlatform(platform_.get());
    if (!v8::V8::Initialize()) {
      *error = "failed to initialize V8";
      v8::V8::DisposePlatform();
      platform_.reset();
      return false;
    }
    SAKO_PERF_MARK("v8.initialize");
    initialized_ = true;
    return true;
  }

  ~Engine() {
    if (initialized_) {
      v8::V8::Dispose();
      v8::V8::DisposePlatform();
    }
  }

  /// Cancels the process-exit teardown.
  ///
  /// A run that abandons its isolate leaves it alive on purpose, and
  /// V8::Dispose checks that every isolate is gone before tearing the process
  /// state down -- so running it afterwards is a crash, not a cleanup.
  ///
  /// The platform is released rather than destroyed for a measured reason:
  /// its destructor joins the worker threads, and waiting for four sleeping
  /// threads to be scheduled just so they can exit is what put an eight
  /// millisecond tail on the ninety-fifth percentile of startup. The kernel
  /// terminates them at ExitProcess either way.
  void Abandon() {
    initialized_ = false;
    static_cast<void>(platform_.release());
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
  /// Every native callback reachable from a snapshotted context.
  ///
  /// A snapshot cannot store a function pointer: the address differs on the
  /// next run. V8 instead stores an *index* into this table, so the identical
  /// list -- same entries, same order -- has to be supplied both when the
  /// snapshot is written and when it is read. A mismatch does not fail
  /// cleanly; it deserializes into the wrong function. Adding a callback to
  /// the context means adding it here too.
  static const intptr_t* ExternalReferences();

  /// The whole snapshot-safe context: native bindings plus everything the
  /// bootstrap builds. The build-time generator runs this once and serializes
  /// the result; a run without a usable snapshot runs the identical code
  /// itself, so the two cannot describe different contexts.
  static bool BuildContext(v8::Isolate* isolate, v8::Local<v8::Context> context,
                           std::string* error) {
    if (!InstallStaticGlobals(isolate, context, error)) return false;
    SAKO_PERF_MARK("runtime.builtins");
    return RunBootstrap(isolate, context, error);
  }

  static std::unique_ptr<Runtime> Create(const char* executable_path,
                                         const char* icu_data_path,
                                         std::string* error) {
    auto runtime = std::unique_ptr<Runtime>(new Runtime());

    Engine& engine = GetEngine();
    if (!engine.Initialize(executable_path, icu_data_path, error)) return nullptr;
    runtime->platform_ = engine.platform();

    std::unique_ptr<v8::ArrayBuffer::Allocator> base_allocator(
        v8::ArrayBuffer::Allocator::NewDefaultAllocator());
    if (!base_allocator) {
      *error = "failed to create the V8 ArrayBuffer allocator";
      return nullptr;
    }
    runtime->allocator_ =
        std::make_unique<PooledArrayBufferAllocator>(std::move(base_allocator));

    SAKO_PERF_MARK("v8.allocator");
    v8::Isolate::CreateParams params;
    params.array_buffer_allocator = runtime->allocator_.get();
    // With a snapshot, Context::New restores the whole bootstrapped context
    // instead of compiling and running the bootstrap again. The external
    // reference table has to be handed over with it: the snapshot names
    // native callbacks by index into that table, never by address.
    if (const v8::StartupData* snapshot = SnapshotBlob()) {
      params.snapshot_blob = snapshot;
      params.external_references = ExternalReferences();
      runtime->from_snapshot_ = true;
    }
    runtime->isolate_ = v8::Isolate::New(params);
    if (runtime->isolate_ == nullptr) {
      *error = "failed to create a V8 isolate";
      return nullptr;
    }
    SAKO_PERF_MARK("v8.isolate");
    runtime->isolate_->SetMicrotasksPolicy(v8::MicrotasksPolicy::kExplicit);
    runtime->isolate_->SetData(0, runtime.get());
    runtime->isolate_->SetHostImportModuleDynamicallyCallback(
        ImportModuleDynamically);
    runtime->isolate_->SetHostInitializeImportMetaObjectCallback(
        InitializeImportMeta);
    runtime->isolate_->SetPromiseRejectCallback(HandlePromiseRejection);
    if (!runtime->InitializeContext(error)) return nullptr;
    SAKO_PERF_MARK("runtime.ready");
    return runtime;
  }

  ~Runtime() {
    for (auto& [descriptor, file] : file_descriptors_) {
      (void)descriptor;
      if (file.handle != kInvalidFile) CloseNativeFile(file.handle);
    }
    file_descriptors_.clear();
    // Each one says goodbye to its server rather than leaving a session for
    // the far end to discover through a reset socket.
    for (auto& [id, connection] : databases_) {
      (void)id;
      sako_postgres_delete(connection);
    }
    databases_.clear();
    if (isolate_ != nullptr) {
      {
        v8::Isolate::Scope isolate_scope(isolate_);
        {
          // Addon cleanup hooks run first and with a live context, because
          // that is what they were promised; everything below this point
          // dismantles the state they would be reaching into.
          v8::HandleScope handle_scope(isolate_);
          if (!context_.IsEmpty()) {
            v8::Local<v8::Context> context = context_.Get(isolate_);
            v8::Context::Scope context_scope(context);
            sako_napi::Shutdown(isolate_);
          } else {
            sako_napi::Shutdown(isolate_);
          }
        }
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
        // Every persistent handle has to be released while the isolate is
        // still alive: a v8::Global destroyed after Dispose reaches into freed
        // isolate state, and the failure is an access violation at teardown
        // rather than anything that names the handle responsible.
        http_dispatcher_.Reset();
        http_finalizer_.Reset();
        raw_http_dispatcher_.Reset();
        raw_http_finalizer_.Reset();
        child_dispatcher_.Reset();
        empty_bytes_.Reset();
        empty_ranges_.Reset();
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

    SAKO_PERF_MARK("runtime.process-globals");
    const std::string path_text(reinterpret_cast<const char*>(path_bytes),
                                path_length);
    const PathString wide_path = Utf8ToPathString(path_text);
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
    SAKO_PERF_MARK("module.instantiate");

    v8::Local<v8::Value> evaluation;
    if (!module->Evaluate(context).ToLocal(&evaluation)) {
      *error = FormatException(isolate_, context, try_catch);
      return false;
    }
    SAKO_PERF_MARK("module.evaluate");
    isolate_->PerformMicrotaskCheckpoint();
    if (!DrainEventLoop(context, error)) return false;
    SAKO_PERF_MARK("event-loop.drain");

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
    SAKO_PERF_MARK("runtime.process-globals");
    const std::string path_text(reinterpret_cast<const char*>(path_bytes),
                                path_length);
    const PathString wide_path = Utf8ToPathString(path_text);
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
    SAKO_PERF_MARK("module.evaluate");
    isolate_->PerformMicrotaskCheckpoint();
    const bool drained = DrainEventLoop(context, error);
    SAKO_PERF_MARK("event-loop.drain");
    return drained;
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
    *queued_operations = closing_http_servers_.size() + children_.size();
    *native_memory_bytes = module_source_bytes_ +
                           timers_.size() * sizeof(Timer) +
                           http_servers_.size() * sizeof(HttpBinding) +
                           children_.size() * sizeof(ChildBinding);
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
  // Live children one execution may own at once. A build tool starting one
  // worker per core is ordinary; hundreds at once is a runaway loop.
  static constexpr size_t kMaximumChildren = 256;
  // Connections are blocking and one statement at a time, so a program that
  // wants hundreds is describing a pool this runtime cannot honour yet.
  static constexpr size_t kMaximumDatabaseConnections = 64;
  static constexpr uint32_t kMaximumDatabaseParameters = 4'096;
  static constexpr size_t kMaximumChildArguments = 256;
  static constexpr size_t kMaximumChildEnvironmentVariables = 512;
  // Longest the loop blocks with an idle HTTP server before looping back to
  // check timers and other runtime work.
  static constexpr uint32_t kIdleHttpWaitMilliseconds = 50;

  struct FileDescriptor {
    NativeFile handle = kInvalidFile;
    bool append = false;
  };

  struct Timer {
    uint64_t id = 0;
    std::chrono::steady_clock::time_point deadline;
    uint64_t interval_milliseconds = 0;
    /// False after `unref()`. The timer still fires; it just stops being a
    /// reason for the program to keep running.
    bool referenced = true;
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
    /// The id JavaScript knows this server by, so a deferred response can name
    /// the server it belongs to on its way back in.
    uint64_t id = 0;
    void* server = nullptr;
    v8::Global<v8::Function> handler;
    std::string response_reason;
    std::string response_body;
    std::vector<std::string> response_header_names;
    std::vector<std::string> response_header_values;
    std::vector<SakoNativeHeader> response_headers;
    bool closing = false;
    bool secure = false;
    /// Dispatch straight to a Request/Response handler instead of building the
    /// node-shaped request and response objects. `sako:http` sets it.
    bool raw = false;
  };

  /// One live child, from the moment it starts until JavaScript has seen it
  /// exit and both of its output streams end.
  struct ChildBinding {
    void* child = nullptr;
    /// Whether this child still holds the event loop open. `unref()` clears
    /// it, which is how a caller says the process may leave without it.
    bool referenced = true;
    bool saw_exit = false;
    bool saw_stdout_end = false;
    bool saw_stderr_end = false;

    /// Nothing more will ever arrive, so the handle can go.
    bool finished() const {
      return saw_exit && saw_stdout_end && saw_stderr_end;
    }

    ~ChildBinding() {
      if (child != nullptr) sako_child_delete(child);
    }
  };

  /// Carries one drain's destination through the C callback back into
  /// JavaScript.
  struct ChildPump {
    Runtime* runtime = nullptr;
    v8::Local<v8::Context> context;
    uint64_t id = 0;
    ChildBinding* binding = nullptr;
    std::string* error = nullptr;
    bool failed = false;
  };

  Runtime() = default;

  /// Installs everything that is identical on every run: the console and the
  /// `__sako*` native bindings, plus the platform tag the bootstrap needs
  /// before `process` exists.
  ///
  /// Nothing here may capture a heap address, a handle, or a clock reading:
  /// this is exactly the state the build-time snapshot serializes, and a
  /// per-run value baked into it would be wrong on every later execution.
  /// Callback data is `undefined` for the same reason -- see FromCallback.
  static bool InstallStaticGlobals(v8::Isolate* isolate,
                                   v8::Local<v8::Context> context,
                                   std::string* error) {
    v8::Local<v8::Object> console = v8::Object::New(isolate);
    v8::Local<v8::Function> log;
    if (!v8::Function::New(context, ConsoleLog, v8::Undefined(isolate))
             .ToLocal(&log) ||
        !SetNamed(isolate, context, console, "log", log) ||
        !SetNamed(isolate, context, context->Global(), "console", console)) {
      *error = "failed to install console globals";
      return false;
    }

    // Every entry here also has to appear in ExternalReferences(), in any
    // order but exactly once: the snapshot stores an index into that table
    // rather than the callback address.
    struct Binding {
      const char* name;
      v8::FunctionCallback callback;
    };
    static constexpr Binding kBindings[] = {
        {"setTimeout", SetTimeout},
        {"setInterval", SetInterval},
        {"clearTimeout", ClearTimer},
        {"__sakoTimerRef", TimerRef},
        {"clearInterval", ClearTimer},
        {"queueMicrotask", QueueMicrotask},
        {"__sakoWriteStandard", WriteStandard},
        {"__sakoTerminalSize", TerminalSize},
        {"__sakoExit", ProcessExit},
        {"__sakoCreateRequire", CreateRequireCallback},
        {"__sakoEncodeUtf8", EncodeUtf8},
        {"__sakoDecodeUtf8", DecodeUtf8},
        {"__sakoReadFileSync", ReadFileSync},
        {"__sakoWriteFileSync", WriteFileSync},
        {"__sakoExistsSync", ExistsSync},
        {"__sakoStatSync", StatSync},
        {"__sakoReadDirectorySync", ReadDirectorySync},
        {"__sakoMakeDirectorySync", MakeDirectorySync},
        {"__sakoRemovePathSync", RemovePathSync},
        {"__sakoRenamePathSync", RenamePathSync},
        {"__sakoLinkPathSync", LinkPathSync},
        {"__sakoSymlinkPathSync", SymlinkPathSync},
        {"__sakoReadLinkSync", ReadLinkSync},
        {"__sakoRealPathSync", RealPathSync},
        {"__sakoOpenSync", OpenSync},
        {"__sakoCloseSync", CloseSync},
        {"__sakoReadSync", ReadSync},
        {"__sakoWriteSync", WriteSync},
        {"__sakoIsTty", IsTty},
        {"__sakoStdinRead", StdinRead},
        {"__sakoStdinSetRawMode", StdinSetRawMode},
        {"__sakoResolveHost", ResolveHost},
        {"__sakoSpawnSync", SpawnSync},
        {"__sakoChildSpawn", ChildSpawn},
        {"__sakoChildWrite", ChildWrite},
        {"__sakoChildEndStdin", ChildEndStdin},
        {"__sakoChildKill", ChildKill},
        {"__sakoChildRef", ChildRef},
        {"__sakoPostgresConnect", PostgresConnect},
        {"__sakoPostgresQuery", PostgresQuery},
        {"__sakoPostgresParameter", PostgresParameter},
        {"__sakoPostgresClose", PostgresClose},
        {"__sakoFetchSync", FetchSync},
        {"__sakoHash", Hash},
        {"__sakoHttpListen", HttpListen},
        {"__sakoHttpsListen", HttpsListen},
        {"__sakoHttpClose", HttpClose},
        {"__sakoHttpRespond", HttpRespond},
        {"__sakoHttpAddress", HttpAddress},
    };
    for (const Binding& binding : kBindings) {
      if (!InstallGlobalFunction(isolate, context, binding.name,
                                 binding.callback)) {
        *error =
            std::string("failed to install the ") + binding.name + " binding";
        return false;
      }
    }

    // The bootstrap picks its path flavour (win32 or posix) from this before
    // `process` exists, since InstallProcess runs per script execution while
    // this runs once. It describes the build target, not the run, so it is
    // safe to serialize.
    if (!SetNamed(isolate, context, context->Global(), "__sakoPlatform",
                  v8::String::NewFromUtf8Literal(isolate,
#if defined(_WIN32)
                                                 "win32"
#else
                                                 "linux"
#endif
                                                 ))) {
      *error = "failed to install the platform tag";
      return false;
    }
    return true;
  }

  /// Per-run state, applied after the snapshot is restored.
  ///
  /// The snapshot was produced on another machine, in another directory, at
  /// another time, so none of this can come from it. `process.argv`, the
  /// epoch behind `process.uptime` and `performance.timeOrigin`, and the
  /// terminal state are installed per execution by InstallProcess; the
  /// working directory is installed here because the bootstrap's path
  /// helpers read `__sakoCwd` on every call and must see this run's value.
  bool InstallDynamicGlobals(v8::Local<v8::Context> context,
                             std::string* error) {
#if defined(_WIN32)
    const DWORD cwd_length = GetCurrentDirectoryW(0, nullptr);
    std::wstring cwd(cwd_length, L'\0');
    const DWORD cwd_written =
        cwd_length == 0 ? 0 : GetCurrentDirectoryW(cwd_length, cwd.data());
    if (cwd_written != 0) cwd.resize(cwd_written);
    const std::string cwd_utf8 = WideToUtf8(cwd);
    const bool cwd_ok = cwd_written != 0;
#else
    std::string cwd_utf8;
    cwd_utf8.resize(4096);
    bool cwd_ok = getcwd(cwd_utf8.data(), cwd_utf8.size()) != nullptr;
    while (!cwd_ok && errno == ERANGE) {
      cwd_utf8.resize(cwd_utf8.size() * 2);
      cwd_ok = getcwd(cwd_utf8.data(), cwd_utf8.size()) != nullptr;
    }
    if (cwd_ok) cwd_utf8.resize(std::strlen(cwd_utf8.c_str()));
#endif
    v8::Local<v8::String> cwd_value;
    if (!cwd_ok ||
        !v8::String::NewFromUtf8(isolate_, cwd_utf8.data(),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(cwd_utf8.size()))
             .ToLocal(&cwd_value) ||
        !SetNamed(isolate_, context, context->Global(), "__sakoCwd",
                  cwd_value)) {
      *error = "failed to read the current working directory";
      return false;
    }
    return true;
  }

  bool InitializeContext(std::string* error) {
    v8::Isolate::Scope isolate_scope(isolate_);
    v8::HandleScope handle_scope(isolate_);
    // The blob's default context is the bootstrapped one, so this restores it
    // rather than building a fresh one. (Adding it as an indexed context and
    // restoring through Context::FromSnapshot was measured instead: identical
    // restore time for a blob fifty kilobytes larger.)
    v8::Local<v8::Context> context = v8::Context::New(isolate_);
    v8::Context::Scope context_scope(context);
    SAKO_PERF_MARK("v8.context");

    // With a snapshot, Context::New above already carries the bindings and
    // everything the bootstrap built, so all that is left is patching in this
    // run's state. Without one the same code that produced the snapshot runs
    // here instead, which costs the compile and the bootstrap but behaves
    // identically.
    if (!from_snapshot_ && !BuildContext(isolate_, context, error)) {
      if (error->empty()) *error = "failed to install runtime bootstrap";
      return false;
    }
    if (!InstallDynamicGlobals(context, error)) return false;
    SAKO_PERF_MARK("runtime.dynamic-state");

    context_.Reset(isolate_, context);
    return true;
  }

  static bool RunBootstrap(v8::Isolate* isolate, v8::Local<v8::Context> context,
                           std::string* error) {
    v8::TryCatch try_catch(isolate);
    v8::Local<v8::String> source;
    if (!v8::String::NewFromUtf8(isolate,
                                 reinterpret_cast<const char*>(kSakoBootstrap),
                                 v8::NewStringType::kNormal,
                                 static_cast<int>(sizeof(kSakoBootstrap) - 1))
             .ToLocal(&source)) {
      *error = "runtime bootstrap exceeds V8 string limits";
      return false;
    }
    v8::Local<v8::String> name =
        v8::String::NewFromUtf8Literal(isolate, "[sako:bootstrap]");
    v8::ScriptOrigin origin(name);
    // The build compiled this exact source with this exact V8, so hand V8 its
    // own compiled form instead of the parser. A cache that does not match the
    // running V8 or its flags is rejected and V8 compiles the source normally,
    // which costs the parse it would have cost anyway.
    v8::ScriptCompiler::CachedData* cached = new v8::ScriptCompiler::CachedData(
        kSakoBootstrapCache, static_cast<int>(sizeof(kSakoBootstrapCache)),
        v8::ScriptCompiler::CachedData::BufferNotOwned);
    v8::ScriptCompiler::Source compiler_source(source, origin, cached);
    v8::Local<v8::Script> script;
    v8::Local<v8::Value> ignored;
    if (!v8::ScriptCompiler::Compile(context, &compiler_source,
                                     v8::ScriptCompiler::kConsumeCodeCache)
             .ToLocal(&script)) {
      *error = FormatException(isolate, context, try_catch);
      return false;
    }
    SAKO_PERF_MARK(cached->rejected ? "bootstrap.compile-uncached"
                                    : "bootstrap.compile");
    if (!script->Run(context).ToLocal(&ignored)) {
      *error = FormatException(isolate, context, try_catch);
      return false;
    }
    SAKO_PERF_MARK("bootstrap.run");
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
    SAKO_PERF_MARK("module.read");
    if (!TranspileTypeScript(path, false, &source_text, error)) return false;
    SAKO_PERF_MARK("module.transpile");
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
    // V8 hands the module, not its path, to the import.meta callback, so keep
    // a reverse mapping. Identity hashes can collide, but the value is only
    // ever used to populate import.meta and a wrong-but-valid path there is
    // preferable to failing the import.
    module_paths_[module->GetIdentityHash()] = canonical_path;

    SAKO_PERF_MARK("module.compile");
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
    // A built-in specifier that did not resolve is an error in itself: there
    // is no file on disk for "node:fs" or "sako:http" to fall back to.
    if (request.starts_with("node:") || request.starts_with("sako:")) {
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
    if (!loaded && !request.starts_with("node:") && !request.starts_with("sako:")) {
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
    if (path.extension() == SAKO_PATH_LITERAL(".cjs") ||
        path.extension() == SAKO_PATH_LITERAL(".cts") ||
        path.extension() == SAKO_PATH_LITERAL(".json")) {
      return true;
    }
    if (path.extension() == SAKO_PATH_LITERAL(".mjs") ||
        path.extension() == SAKO_PATH_LITERAL(".mts")) {
      return false;
    }
    std::filesystem::path directory = path.parent_path();
    while (!directory.empty()) {
      v8::Local<v8::Object> manifest;
      if (ReadJsonObject(context, directory / SAKO_PATH_LITERAL("package.json"),
                         &manifest)) {
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
    if (request.starts_with("sako:")) {
      *error = "unknown Sako library: " + request;
      return false;
    }
    if (request.starts_with("file:")) {
      std::filesystem::path direct;
      std::error_code status_error;
      if (FileUrlToPath(request, &direct) &&
          IsRegularFile(direct, status_error)) {
        std::error_code canonical_error;
        *output = CanonicalPath(direct, canonical_error);
        if (!canonical_error) return true;
      }
      *error = "module not found: " + request + " imported from " + referrer;
      return false;
    }
    const PathString request_wide = Utf8ToPathString(request);
    const PathString referrer_wide = Utf8ToPathString(referrer);
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
      candidates.push_back(candidate.native() + PathString(SAKO_PATH_LITERAL(".js")));
      candidates.push_back(candidate.native() + PathString(SAKO_PATH_LITERAL(".mjs")));
      candidates.push_back(candidate.native() + PathString(SAKO_PATH_LITERAL(".ts")));
      candidates.push_back(candidate.native() + PathString(SAKO_PATH_LITERAL(".mts")));
      candidates.push_back(candidate.native() + PathString(SAKO_PATH_LITERAL(".tsx")));
      candidates.push_back(candidate / SAKO_PATH_LITERAL("index.js"));
      candidates.push_back(candidate / SAKO_PATH_LITERAL("index.ts"));
      candidates.push_back(candidate / SAKO_PATH_LITERAL("index.mts"));
      candidates.push_back(candidate / SAKO_PATH_LITERAL("index.tsx"));
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
        package_root / Utf8ToPathString(target.substr(2));
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
        std::filesystem::path(Utf8ToPathString(referrer)).parent_path();
    while (!directory.empty()) {
      const std::filesystem::path manifest_path =
          directory / SAKO_PATH_LITERAL("package.json");
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
        std::filesystem::path(Utf8ToPathString(referrer)).parent_path();
    while (!directory.empty()) {
      const std::filesystem::path package_root =
          directory / SAKO_PATH_LITERAL("node_modules") /
          Utf8ToPathString(package_name);
      std::error_code directory_error;
      if (IsDirectory(package_root, directory_error)) {
        v8::Local<v8::Object> manifest;
        if (ReadJsonObject(context, package_root / SAKO_PATH_LITERAL("package.json"),
                           &manifest)) {
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
                : package_root / Utf8ToPathString(package_subpath);
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

    // A .node file is a compiled Node-API addon, not JavaScript: it is loaded
    // as a shared library and asked to build its own exports. Reaching the
    // parser with it would fail as "Invalid or unexpected token", which says
    // nothing about what the file actually is.
    if (path.extension() == SAKO_PATH_LITERAL(".node")) {
      v8::Local<v8::Value> addon_exports;
      if (!sako_napi::LoadAddon(context, path, &addon_exports, error)) {
        return false;
      }
      v8::Local<v8::Object> addon_module = v8::Object::New(isolate_);
      if (!Set(context, addon_module, "exports", addon_exports) ||
          !Set(context, addon_module, "loaded", v8::True(isolate_))) {
        *error = "failed to initialize native addon module: " + canonical_path;
        return false;
      }
      commonjs_modules_.emplace(
          canonical_path, v8::Global<v8::Object>(isolate_, addon_module));
      *output = addon_exports;
      return true;
    }

    std::string source_text;
    if (!ReadFile(path, &source_text)) {
      *error = "cannot read CommonJS module: " + canonical_path;
      return false;
    }
    SAKO_PERF_MARK("module.read");
    StripShebang(&source_text);
    if (!TranspileTypeScript(path, true, &source_text, error)) return false;
    SAKO_PERF_MARK("module.transpile");
    if (module_source_bytes_ + source_text.size() > kMaximumModuleBytes) {
      *error = "module source cache byte limit exceeded";
      return false;
    }

    v8::Local<v8::Object> module = v8::Object::New(isolate_);
    if (path.extension() == SAKO_PATH_LITERAL(".json")) {
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
    SAKO_PERF_MARK("module.compile");

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
    SAKO_PERF_MARK("module.instantiate");
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

  /// Populates `import.meta` for an ES module.
  ///
  /// Without this the object is empty, and `import.meta.url` -- which nearly
  /// every ES module written for Node uses to locate itself -- is undefined.
  /// `dirname` and `filename` mirror the properties Node added in 20.11 and
  /// save callers a fileURLToPath round trip.
  static void InitializeImportMeta(v8::Local<v8::Context> context,
                                   v8::Local<v8::Module> module,
                                   v8::Local<v8::Object> meta) {
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    Runtime* runtime = static_cast<Runtime*>(isolate->GetData(0));
    if (runtime == nullptr) return;
    auto found = runtime->module_paths_.find(module->GetIdentityHash());
    if (found == runtime->module_paths_.end()) return;
    const std::filesystem::path path(Utf8ToPathString(found->second));

    const std::string url = PathToFileUrl(path);
    const std::string filename = PathToUtf8(path);
    const std::string dirname = PathToUtf8(path.parent_path());
    for (const auto& [name, value] :
         {std::pair{"url", url}, std::pair{"filename", filename},
          std::pair{"dirname", dirname}}) {
      v8::Local<v8::String> text;
      if (!v8::String::NewFromUtf8(isolate, value.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(value.size()))
               .ToLocal(&text)) {
        continue;
      }
      (void)meta->Set(context,
                      v8::String::NewFromUtf8(isolate, name).ToLocalChecked(),
                      text);
    }
  }

  /// Turns a `file:` URL back into a path.
  ///
  /// `import(pathToFileURL(file).href)` is how ES module code loads an
  /// absolute path it computed at run time, and it is what Vite does with the
  /// bundle it writes for a TypeScript config -- so the resolver has to accept
  /// the URL spelling as readily as the path one.
  static bool FileUrlToPath(const std::string& url,
                            std::filesystem::path* output) {
    if (!url.starts_with("file:")) return false;
    std::string rest = url.substr(5);
    std::string host;
    if (rest.starts_with("//")) {
      rest.erase(0, 2);
      const size_t slash = rest.find('/');
      host = slash == std::string::npos ? rest : rest.substr(0, slash);
      rest = slash == std::string::npos ? std::string() : rest.substr(slash);
      // An empty host and "localhost" both mean this machine; anything else is
      // a UNC share and keeps its leading separators.
      if (host == "localhost") host.clear();
    }
    // A cache-busting query or a fragment is part of the URL, not the file.
    const size_t cut = rest.find_first_of("?#");
    if (cut != std::string::npos) rest.resize(cut);

    std::string decoded;
    decoded.reserve(rest.size());
    for (size_t index = 0; index < rest.size(); ++index) {
      if (rest[index] == '%' && index + 2 < rest.size() &&
          std::isxdigit(static_cast<unsigned char>(rest[index + 1])) != 0 &&
          std::isxdigit(static_cast<unsigned char>(rest[index + 2])) != 0) {
        decoded.push_back(static_cast<char>(
            std::stoi(rest.substr(index + 1, 2), nullptr, 16)));
        index += 2;
        continue;
      }
      decoded.push_back(rest[index]);
    }
#if defined(_WIN32)
    // "/C:/x" is a URL path; the filesystem wants "C:/x".
    if (decoded.size() >= 3 && decoded[0] == '/' &&
        std::isalpha(static_cast<unsigned char>(decoded[1])) != 0 &&
        decoded[1 + 1] == ':') {
      decoded.erase(0, 1);
    }
#endif
    if (!host.empty()) decoded = "//" + host + decoded;
    if (decoded.empty()) return false;
    const PathString wide = Utf8ToPathString(decoded);
    if (wide.empty()) return false;
    *output = std::filesystem::path(wide);
    return true;
  }

  /// Percent-encodes a filesystem path into a `file:` URL.
  ///
  /// Only the characters that would change how the URL parses are escaped;
  /// encoding more (spaces aside) would make the round trip through
  /// fileURLToPath lossy for ordinary Windows paths.
  static std::string PathToFileUrl(const std::filesystem::path& path) {
    std::string text = PathToUtf8(path);
    for (char& character : text) {
      if (character == '\\') character = '/';
    }
    std::string encoded;
    encoded.reserve(text.size() + 8);
    for (const unsigned char character : text) {
      switch (character) {
        case ' ': encoded += "%20"; break;
        case '#': encoded += "%23"; break;
        case '?': encoded += "%3F"; break;
        case '%': encoded += "%25"; break;
        default: encoded.push_back(static_cast<char>(character));
      }
    }
    // A drive-letter path has no leading slash of its own.
    return encoded.starts_with("/") ? "file://" + encoded
                                    : "file:///" + encoded;
  }

  /// Backs `createRequire` from node:module.
  ///
  /// The CommonJS `require` a module receives is already built by
  /// `CreateRequire`; this only exposes the same construction to JavaScript so
  /// an ES module can obtain one for an arbitrary referrer. Tools reach for it
  /// constantly -- reading their own package.json, resolving a peer -- so
  /// without it most real-world CLIs fail on their first import.
  static void CreateRequireCallback(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (info.Length() == 0 || !info[0]->IsString()) {
      ThrowTypeError(isolate, "createRequire needs a path or file URL");
      return;
    }
    // Installed by InstallStaticGlobals, so the callback data is `undefined`
    // rather than an External: a heap address cannot survive the build-time
    // snapshot. FromCallback falls back to the isolate slot, which is what
    // every other snapshotted binding uses.
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr) {
      ThrowTypeError(isolate, "createRequire is unavailable");
      return;
    }
    // The referrer is a filename, and require() resolves relative specifiers
    // against its directory. A file: URL is normalized on the JavaScript side
    // before it reaches here.
    const std::string referrer = ToUtf8(isolate, info[0]);
    v8::Local<v8::Function> require;
    if (!runtime->CreateRequire(context, referrer, &require)) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(isolate, "cannot create require")));
      return;
    }
    info.GetReturnValue().Set(require);
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

  /// Compiles and evaluates one embedded Sako library, and hands back its
  /// namespace.
  ///
  /// The TypeScript is embedded as written and transpiled here, on the first
  /// import in a run, so a library nobody uses costs nothing but its bytes in
  /// the executable. The compiled module is cached under its specifier like
  /// any other, so the second import is a map lookup.
  bool LoadSakoLibrary(v8::Local<v8::Context> context,
                       const std::string& request,
                       v8::Local<v8::Value>* output, std::string* error) {
    auto cached = modules_.find(request);
    if (cached == modules_.end()) {
      const SakoLibrarySource* found = nullptr;
      for (const SakoLibrarySource& library : kSakoLibraries) {
        if (library.specifier != nullptr && request == library.specifier) {
          found = &library;
          break;
        }
      }
      if (found == nullptr) {
        *error = "unknown Sako library: " + request;
        return false;
      }
      if (modules_.size() >= kMaximumModules) {
        *error = "module cache capacity exceeded";
        return false;
      }
      // The path is only ever a name in a diagnostic -- nothing reads it off
      // disk -- but it has to end in .ts for the transpiler to recognize the
      // language, and be absolute for it to form a module URL at all.
      const std::string origin = request.substr(request.find(':') + 1);
      std::error_code absolute_error;
      std::filesystem::path source_path = std::filesystem::absolute(
          std::filesystem::path(Utf8ToPathString("libs/" + origin +
                                                 "/src/index.ts")),
          absolute_error);
      if (absolute_error) source_path = Utf8ToPathString(request);
      std::string source_text(reinterpret_cast<const char*>(found->source),
                              found->length);
      if (!TranspileTypeScript(source_path, false, &source_text, error)) {
        return false;
      }
      v8::Local<v8::String> source;
      v8::Local<v8::String> resource_name;
      if (!v8::String::NewFromUtf8(isolate_, source_text.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(source_text.size()))
               .ToLocal(&source) ||
          !v8::String::NewFromUtf8(isolate_, request.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(request.size()))
               .ToLocal(&resource_name)) {
        *error = "library source exceeds V8 string limits";
        return false;
      }
      v8::ScriptOrigin origin_info(resource_name, 0, 0, false, -1,
                                   v8::Local<v8::Value>(), false, false, true);
      v8::ScriptCompiler::Source compiler_source(source, origin_info);
      v8::Local<v8::Module> module;
      if (!v8::ScriptCompiler::CompileModule(isolate_, &compiler_source)
               .ToLocal(&module)) {
        *error = "failed to compile the library: " + request;
        return false;
      }
      module_paths_[module->GetIdentityHash()] = request;
      modules_.emplace(request, v8::Global<v8::Module>(isolate_, module));
      module_source_bytes_ += source_text.size();
      cached = modules_.find(request);
    }

    v8::Local<v8::Module> module = cached->second.Get(isolate_);
    if (module->GetStatus() == v8::Module::kUninstantiated &&
        !module->InstantiateModule(context, ResolveModule).FromMaybe(false)) {
      *error = "failed to instantiate the library: " + request;
      return false;
    }
    if (module->GetStatus() == v8::Module::kInstantiated) {
      v8::Local<v8::Value> evaluation;
      if (!module->Evaluate(context).ToLocal(&evaluation)) {
        *error = "failed to evaluate the library: " + request;
        return false;
      }
      isolate_->PerformMicrotaskCheckpoint();
    }
    if (module->GetStatus() == v8::Module::kErrored) {
      isolate_->ThrowException(module->GetException());
      return false;
    }
    *output = module->GetModuleNamespace();
    return true;
  }

  bool LoadBuiltin(v8::Local<v8::Context> context,
                   const std::string& request,
                   v8::Local<v8::Value>* output) {
    // Sako's own libraries keep their scheme: "sako:http" is the whole name,
    // not a bare specifier to be canonicalized into the node: namespace.
    if (request.starts_with("sako:")) {
      std::string error;
      if (LoadSakoLibrary(context, request, output, &error)) return true;
      if (!isolate_->HasPendingException()) {
        isolate_->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8(isolate_, error.data(),
                                    v8::NewStringType::kNormal,
                                    static_cast<int>(error.size()))
                .ToLocalChecked()));
      }
      return false;
    }
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
    if (request.starts_with("sako:")) {
      *error = "unknown Sako library: " + request;
      return false;
    }
    if (request.starts_with('#')) {
      return ResolvePackageImport(context, request, referrer, "require",
                                  output, error);
    }
    const PathString request_wide = Utf8ToPathString(request);
    const std::filesystem::path referrer_path(Utf8ToPathString(referrer));
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
        candidate.native() + PathString(SAKO_PATH_LITERAL(".js")),
        candidate.native() + PathString(SAKO_PATH_LITERAL(".cjs")),
        candidate.native() + PathString(SAKO_PATH_LITERAL(".ts")),
        candidate.native() + PathString(SAKO_PATH_LITERAL(".cts")),
        candidate.native() + PathString(SAKO_PATH_LITERAL(".mts")),
        candidate.native() + PathString(SAKO_PATH_LITERAL(".tsx")),
        candidate.native() + PathString(SAKO_PATH_LITERAL(".json")),
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
    const std::filesystem::path manifest_path =
        candidate / SAKO_PATH_LITERAL("package.json");
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
          const std::filesystem::path main_path =
              candidate / Utf8ToPathString(ToUtf8(isolate_, main));
          const std::vector<std::filesystem::path> main_files = {
              main_path,
              main_path.native() + PathString(SAKO_PATH_LITERAL(".js")),
              main_path.native() + PathString(SAKO_PATH_LITERAL(".cjs")),
              main_path.native() + PathString(SAKO_PATH_LITERAL(".ts")),
              main_path.native() + PathString(SAKO_PATH_LITERAL(".cts")),
              main_path.native() + PathString(SAKO_PATH_LITERAL(".mts")),
              main_path.native() + PathString(SAKO_PATH_LITERAL(".tsx")),
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
    return ResolveCommonJsCandidate(context, candidate / SAKO_PATH_LITERAL("index"),
                                    output);
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
    v8::Local<v8::Object> release = v8::Object::New(isolate_);
    if (!Set(context, release, "name",
             v8::String::NewFromUtf8Literal(isolate_, "sako"))) {
      return false;
    }
    v8::Local<v8::Object> stdout_stream = v8::Object::New(isolate_);
    v8::Local<v8::Object> stderr_stream = v8::Object::New(isolate_);
    v8::Local<v8::Function> stdout_write;
    v8::Local<v8::Function> stderr_write;
#if defined(_WIN32)
    const int32_t stdout_selector = static_cast<int32_t>(STD_OUTPUT_HANDLE);
    const int32_t stderr_selector = static_cast<int32_t>(STD_ERROR_HANDLE);
#else
    const int32_t stdout_selector = STDOUT_FILENO;
    const int32_t stderr_selector = STDERR_FILENO;
#endif
    if (!v8::Function::New(context, WriteStream,
                           v8::Integer::New(isolate_, stdout_selector))
             .ToLocal(&stdout_write) ||
        !v8::Function::New(context, WriteStream,
                           v8::Integer::New(isolate_, stderr_selector))
             .ToLocal(&stderr_write) ||
        !Set(context, stdout_stream, "fd", v8::Integer::New(isolate_, 1)) ||
        !Set(context, stdout_stream, "write", stdout_write) ||
        !Set(context, stderr_stream, "fd", v8::Integer::New(isolate_, 2)) ||
        !Set(context, stderr_stream, "write", stderr_write) ||
        // Tools check isTTY to decide whether to emit colour or progress
        // redraws; undefined reads as "not a terminal" and loses both.
        !Set(context, stdout_stream, "isTTY",
             v8::Boolean::New(isolate_, IsTerminal(1))) ||
        !Set(context, stderr_stream, "isTTY",
             v8::Boolean::New(isolate_, IsTerminal(2)))) {
      return false;
    }
    v8::Local<v8::Function> cwd;
    v8::Local<v8::Function> chdir;
    v8::Local<v8::Value> next_tick;
    if (!v8::Function::New(context, ProcessCwd).ToLocal(&cwd) ||
        !v8::Function::New(context, ProcessChdir).ToLocal(&chdir) ||
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
    // Reading the environment block builds one JavaScript string per variable,
    // which on a developer machine is a hundred strings a script that never
    // looks at process.env would never touch. The property materializes the
    // object on its first read and becomes an ordinary data property after,
    // so writes, deletes, and enumeration behave as they did.
    if (!process
             ->SetLazyDataProperty(
                 context, v8::String::NewFromUtf8Literal(isolate_, "env"),
                 ProcessEnvironment)
             .FromMaybe(false)) {
      return false;
    }
    // This execution's wall-clock start, for process.uptime and
    // performance.timeOrigin. Set here rather than in the bootstrap because
    // the bootstrap runs once -- and in a snapshotted build, at compile time.
    const auto epoch = std::chrono::duration_cast<std::chrono::milliseconds>(
                           std::chrono::system_clock::now().time_since_epoch())
                           .count();
    if (!Set(context, context->Global(), "__sakoEpochMs",
             v8::Number::New(isolate_, static_cast<double>(epoch)))) {
      return false;
    }

    const bool installed = Set(context, process, "argv", arguments) &&
           Set(context, process, "execPath", executable) &&
           Set(context, process, "cwd", cwd) &&
           Set(context, process, "chdir", chdir) &&
           Set(context, process, "nextTick", next_tick) &&
           Set(context, process, "stdout", stdout_stream) &&
           Set(context, process, "stderr", stderr_stream) &&
           Set(context, process, "exitCode", v8::Integer::New(isolate_, 0)) &&
           Set(context, process, "version",
               v8::String::NewFromUtf8Literal(isolate_, "v0.1.0")) &&
           Set(context, process, "platform",
               v8::String::NewFromUtf8Literal(isolate_,
#if defined(_WIN32)
                                              "win32"
#else
                                              "linux"
#endif
                                              )) &&
           Set(context, process, "arch",
               v8::String::NewFromUtf8Literal(isolate_, "x64")) &&
           Set(context, versions, "sako",
               v8::String::NewFromUtf8Literal(isolate_, "0.1.0")) &&
           // The Node API level this runtime targets, not a claim to be Node
           // -- `versions.sako` is where the truth lives, and `process.release`
           // says "sako". Ecosystem code reads this to pick a code path
           // (`process.versions.node.split(".")` is the standard spelling of
           // "which Node am I on"), and leaving it undefined does not make
           // those packages fall back gracefully; it makes them crash on the
           // read.
           Set(context, versions, "node",
               v8::String::NewFromUtf8(isolate_, kNodeApiLevel)
                   .ToLocalChecked()) &&
           Set(context, versions, "v8",
               v8::String::NewFromUtf8(isolate_, v8::V8::GetVersion())
                   .ToLocalChecked()) &&
           Set(context, process, "versions", versions) &&
           Set(context, process, "release", release) &&
           Set(context, context->Global(), "process", process) &&
           Set(context, context->Global(), "global", context->Global());
    if (!installed) return false;

    // Members the bootstrap contributes to `process` -- stdin, the stdout and
    // stderr streams, exit, hrtime and the rest -- are far easier to express in
    // JavaScript than here. The bootstrap runs once at context initialization
    // while this runs per execution, so they are parked on the global and
    // copied on each time; otherwise every execution would build a fresh
    // `process` and drop them.
    //
    // Copied last, deliberately: the JavaScript stdout and stderr are real
    // EventEmitters and must replace the plain objects set above, which have
    // only write and fd. Terminal UIs attach listeners to them.
    SAKO_PERF_MARK("runtime.process-object");
    v8::Local<v8::Value> extras;
    if (!context->Global()
             ->Get(context, v8::String::NewFromUtf8Literal(
                                isolate_, "__sakoProcessExtras"))
             .ToLocal(&extras) ||
        !extras->IsObject()) {
      return true;
    }
    v8::Local<v8::Object> source = extras.As<v8::Object>();
    v8::Local<v8::Array> names;
    if (!source->GetOwnPropertyNames(context).ToLocal(&names)) return true;
    for (uint32_t index = 0; index < names->Length(); ++index) {
      v8::Local<v8::Value> key;
      v8::Local<v8::Value> value;
      if (!names->Get(context, index).ToLocal(&key) ||
          !source->Get(context, key).ToLocal(&value) ||
          !process->Set(context, key, value).FromMaybe(false)) {
        return false;
      }
    }
    return true;
  }

  // Materializes process.env from the live environment block on first read.
  static void ProcessEnvironment(
      v8::Local<v8::Name> name,
      const v8::PropertyCallbackInfo<v8::Value>& info) {
    (void)name;
    v8::Isolate* isolate = info.GetIsolate();
    Runtime* runtime = static_cast<Runtime*>(isolate->GetData(0));
    if (runtime == nullptr) return;
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    v8::Local<v8::Object> environment = v8::Object::New(isolate);
    bool environment_ok = true;
#if defined(_WIN32)
    LPWCH environment_block = GetEnvironmentStringsW();
    if (environment_block == nullptr) return;
    for (const wchar_t* entry = environment_block; *entry != L'\0';) {
      const std::wstring item(entry);
      entry += item.size() + 1;
      // Windows hides per-drive current directories as entries whose name is
      // empty; they are not environment variables.
      if (item.starts_with(L'=')) continue;
      const size_t equals = item.find(L'=');
      if (equals == std::wstring::npos) continue;
      const std::string variable = WideToUtf8(item.substr(0, equals));
      const std::string value = WideToUtf8(item.substr(equals + 1));
      v8::Local<v8::String> text;
      if (variable.empty() ||
          !v8::String::NewFromUtf8(isolate, value.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(value.size()))
               .ToLocal(&text) ||
          !runtime->Set(context, environment, variable, text)) {
        environment_ok = false;
        break;
      }
    }
    FreeEnvironmentStringsW(environment_block);
#else
    for (char** entry = environ; *entry != nullptr; ++entry) {
      const std::string item(*entry);
      const size_t equals = item.find('=');
      if (equals == std::string::npos) continue;
      const std::string variable = item.substr(0, equals);
      const std::string value = item.substr(equals + 1);
      v8::Local<v8::String> text;
      if (variable.empty() ||
          !v8::String::NewFromUtf8(isolate, value.data(),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(value.size()))
               .ToLocal(&text) ||
          !runtime->Set(context, environment, variable, text)) {
        environment_ok = false;
        break;
      }
    }
#endif
    if (environment_ok) info.GetReturnValue().Set(environment);
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

  /// Recovers the Runtime for a callback installed on the global object.
  ///
  /// Reads it from the isolate rather than from `info.Data()`, which used to
  /// carry a `v8::External` wrapping `this`. That worked, but it put a
  /// per-run heap address inside the context -- and a context containing one
  /// cannot be serialized into a startup snapshot, because the address is
  /// meaningless the next time the process runs. The isolate slot is set
  /// during construction and is correct for whichever Runtime is executing.
  static Runtime* FromCallback(
      const v8::FunctionCallbackInfo<v8::Value>& info) {
    // Still honoured where a callback genuinely carries its own External --
    // `require` binds one per module, and those are built at run time, never
    // snapshotted.
    if (info.Data()->IsExternal()) {
      return static_cast<Runtime*>(info.Data().As<v8::External>()->Value(
          v8::kExternalPointerTypeTagDefault));
    }
    return static_cast<Runtime*>(info.GetIsolate()->GetData(0));
  }

  static void SetTimeout(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime != nullptr) runtime->ScheduleTimer(info, false);
  }

  static void SetInterval(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime != nullptr) runtime->ScheduleTimer(info, true);
  }

  /// Backs `Timeout.ref()` and `Timeout.unref()`.
  ///
  /// A program arms a timer it does not want to wait for -- a cache sweep, a
  /// deadline that only matters if something else is still running -- and says
  /// so with `unref()`. Without this the process outlives its own work.
  static void TimerRef(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr || info.Length() < 2) return;
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    const uint64_t id = info[0]->IntegerValue(context).FromMaybe(0);
    auto timer = runtime->timers_.find(id);
    if (timer == runtime->timers_.end()) return;
    timer->second.referenced = info[1]->BooleanValue(isolate);
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
    bool append = false;
#if defined(_WIN32)
    DWORD access = 0;
    DWORD creation = OPEN_EXISTING;
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
    NativeFile handle = CreateFileW(
        ExtendedPath(path).native().c_str(), access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr, creation,
        FILE_ATTRIBUTE_NORMAL, nullptr);
#else
    int posix_flags = 0;
    if (flags == "r") {
      posix_flags = O_RDONLY;
    } else if (flags == "r+") {
      posix_flags = O_RDWR;
    } else if (flags == "w" || flags == "wx") {
      posix_flags = O_WRONLY | O_CREAT | O_TRUNC | (flags == "wx" ? O_EXCL : 0);
    } else if (flags == "w+" || flags == "wx+") {
      posix_flags = O_RDWR | O_CREAT | O_TRUNC | (flags == "wx+" ? O_EXCL : 0);
    } else if (flags == "a" || flags == "ax") {
      posix_flags = O_WRONLY | O_CREAT | O_APPEND | (flags == "ax" ? O_EXCL : 0);
      append = true;
    } else if (flags == "a+" || flags == "ax+") {
      posix_flags = O_RDWR | O_CREAT | O_APPEND | (flags == "ax+" ? O_EXCL : 0);
      append = true;
    } else {
      ThrowTypeError(isolate, "unsupported file open flags");
      return;
    }
    NativeFile handle = open(ExtendedPath(path).c_str(), posix_flags, 0644);
#endif
    if (handle == kInvalidFile) {
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
    const NativeFile handle = file->handle;
    runtime->file_descriptors_.erase(descriptor);
#if defined(_WIN32)
    const bool closed = CloseHandle(handle) != 0;
#else
    const bool closed = close(handle) == 0;
#endif
    if (!closed) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "close failed")));
    }
  }

  static bool DescriptorTransferArguments(
      const v8::FunctionCallbackInfo<v8::Value>& info, uint8_t** bytes,
      size_t* length, bool* positioned, int64_t* position) {
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
    if (offset < 0 || requested < 0 ||
        requested > static_cast<int64_t>(std::numeric_limits<uint32_t>::max()) ||
        static_cast<uint64_t>(offset) > byte_length ||
        static_cast<uint64_t>(requested) > byte_length - offset) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate, "descriptor I/O range is invalid")));
      return false;
    }
    *bytes = const_cast<uint8_t*>(data) + offset;
    *length = static_cast<size_t>(requested);
    *positioned = info.Length() > 4 && !info[4]->IsNullOrUndefined();
    *position = 0;
    if (*positioned) {
      const int64_t value = info[4]->IntegerValue(context).FromMaybe(-1);
      if (value < 0) {
        isolate->ThrowException(v8::Exception::RangeError(
            v8::String::NewFromUtf8Literal(isolate, "file position is invalid")));
        return false;
      }
      *position = value;
    }
    return true;
  }

  static void ReadSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
    FileDescriptor* file = FindFileDescriptor(FromCallback(info), info);
    if (file == nullptr) return;
    uint8_t* bytes = nullptr;
    size_t length = 0;
    bool positioned = false;
    int64_t position = 0;
    if (!DescriptorTransferArguments(info, &bytes, &length, &positioned,
                                     &position)) {
      return;
    }
#if defined(_WIN32)
    LARGE_INTEGER win_position{};
    win_position.QuadPart = position;
    LARGE_INTEGER saved{};
    LARGE_INTEGER zero{};
    if (positioned &&
        (!SetFilePointerEx(file->handle, zero, &saved, FILE_CURRENT) ||
         !SetFilePointerEx(file->handle, win_position, nullptr, FILE_BEGIN))) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "read seek failed")));
      return;
    }
    DWORD transferred = 0;
    const BOOL succeeded = ::ReadFile(file->handle, bytes,
                                      static_cast<DWORD>(length), &transferred,
                                      nullptr);
    if (positioned) SetFilePointerEx(file->handle, saved, nullptr, FILE_BEGIN);
    if (!succeeded) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "read failed")));
      return;
    }
    info.GetReturnValue().Set(v8::Integer::NewFromUnsigned(info.GetIsolate(), transferred));
#else
    const ssize_t transferred =
        positioned ? pread(file->handle, bytes, length,
                          static_cast<off_t>(position))
                   : read(file->handle, bytes, length);
    if (transferred < 0) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "read failed")));
      return;
    }
    info.GetReturnValue().Set(v8::Integer::NewFromUnsigned(
        info.GetIsolate(), static_cast<uint32_t>(transferred)));
#endif
  }

  static void WriteSync(const v8::FunctionCallbackInfo<v8::Value>& info) {
    FileDescriptor* file = FindFileDescriptor(FromCallback(info), info);
    if (file == nullptr) return;
    uint8_t* bytes = nullptr;
    size_t length = 0;
    bool positioned = false;
    int64_t position = 0;
    if (!DescriptorTransferArguments(info, &bytes, &length, &positioned,
                                     &position)) {
      return;
    }
#if defined(_WIN32)
    LARGE_INTEGER win_position{};
    LARGE_INTEGER saved{};
    LARGE_INTEGER zero{};
    if (file->append) {
      win_position.QuadPart = 0;
      if (!SetFilePointerEx(file->handle, win_position, nullptr, FILE_END)) {
        info.GetIsolate()->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8Literal(info.GetIsolate(), "append seek failed")));
        return;
      }
      positioned = false;
    } else if (positioned) {
      win_position.QuadPart = position;
      if (!SetFilePointerEx(file->handle, zero, &saved, FILE_CURRENT) ||
          !SetFilePointerEx(file->handle, win_position, nullptr, FILE_BEGIN)) {
        info.GetIsolate()->ThrowException(v8::Exception::Error(
            v8::String::NewFromUtf8Literal(info.GetIsolate(), "write seek failed")));
        return;
      }
    }
    DWORD transferred = 0;
    const BOOL succeeded =
        ::WriteFile(file->handle, bytes, static_cast<DWORD>(length),
                   &transferred, nullptr);
    if (positioned) SetFilePointerEx(file->handle, saved, nullptr, FILE_BEGIN);
    if (!succeeded) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "write failed")));
      return;
    }
    info.GetReturnValue().Set(v8::Integer::NewFromUnsigned(info.GetIsolate(), transferred));
#else
    // O_APPEND (set on the descriptor when it was opened with an append
    // flag) makes the kernel append atomically on every write, so there is
    // no separate seek-to-end step the way Windows needs one.
    const ssize_t transferred =
        (positioned && !file->append)
            ? pwrite(file->handle, bytes, length, static_cast<off_t>(position))
            : write(file->handle, bytes, length);
    if (transferred < 0) {
      info.GetIsolate()->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(info.GetIsolate(), "write failed")));
      return;
    }
    info.GetReturnValue().Set(v8::Integer::NewFromUnsigned(
        info.GetIsolate(), static_cast<uint32_t>(transferred)));
#endif
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
    binding->raw = info.Length() > 2 && info[2]->BooleanValue(isolate);
    uint64_t id = runtime->next_http_server_id_++;
    if (id == 0) id = runtime->next_http_server_id_++;
    binding->id = id;
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
    binding->raw = info.Length() > 4 && info[4]->BooleanValue(isolate);
    uint64_t id = runtime->next_http_server_id_++;
    if (id == 0) id = runtime->next_http_server_id_++;
    binding->id = id;
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

  /// Delivers a response for a request whose handler had not finished by the
  /// time the dispatch returned. JavaScript calls this from `res.end()`.
  static void HttpRespond(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr || info.Length() < 6) return;
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    const uint64_t id = info[0]->IntegerValue(context).FromMaybe(0);
    auto found = runtime->http_servers_.find(id);
    // A server closed while a handler was still working simply drops the
    // answer; so does a client that hung up, one layer further down.
    if (found == runtime->http_servers_.end()) return;
    const uint64_t ticket = info[1]->IntegerValue(context).FromMaybe(0);
    const int64_t status_code = info[2]->IntegerValue(context).FromMaybe(0);
    if (status_code < 100 || status_code > 999) {
      runtime->async_error_ = "HTTP response has an invalid status";
      return;
    }
    std::string reason;
    ToUtf8Into(isolate, info[3], &reason);
    std::string body;
    if (info[5]->IsString()) {
      ToUtf8Into(isolate, info[5], &body);
    } else {
      const uint8_t* bytes = nullptr;
      size_t length = 0;
      if (!ReadBytes(info[5], &bytes, &length)) {
        runtime->async_error_ =
            "HTTP response body must be a string or byte array";
        return;
      }
      body.assign(reinterpret_cast<const char*>(bytes), length);
    }
    if (!info[4]->IsArray()) {
      runtime->async_error_ = "HTTP response headers must be an array";
      return;
    }
    v8::Local<v8::Array> pairs = info[4].As<v8::Array>();
    if (pairs->Length() % 2 != 0 || pairs->Length() / 2 > 128) {
      runtime->async_error_ = "HTTP response has invalid headers";
      return;
    }
    const size_t pair_count = pairs->Length() / 2;
    // Deferred responses are the slow path by definition, so these are plain
    // locals rather than the per-connection buffers the synchronous path
    // reuses: two answers can be in flight in the same turn.
    std::vector<std::string> names(pair_count);
    std::vector<std::string> values(pair_count);
    for (uint32_t index = 0; index < pairs->Length(); index += 2) {
      v8::Local<v8::Value> name;
      v8::Local<v8::Value> value;
      if (!pairs->Get(context, index).ToLocal(&name) ||
          !pairs->Get(context, index + 1).ToLocal(&value)) {
        runtime->async_error_ = "cannot read HTTP response headers";
        return;
      }
      ToUtf8Into(isolate, name, &names[index / 2]);
      ToUtf8Into(isolate, value, &values[index / 2]);
    }
    std::vector<SakoNativeHeader> headers;
    headers.reserve(pair_count);
    for (size_t index = 0; index < pair_count; ++index) {
      headers.push_back(
          {{reinterpret_cast<const uint8_t*>(names[index].data()),
            names[index].size()},
           {reinterpret_cast<const uint8_t*>(values[index].data()),
            values[index].size()}});
    }
    const int outcome = sako_http_server_respond(
        found->second->server, ticket, static_cast<uint16_t>(status_code),
        {reinterpret_cast<const uint8_t*>(reason.data()), reason.size()},
        headers.data(), headers.size(),
        {reinterpret_cast<const uint8_t*>(body.data()), body.size()});
    if (outcome < 0) {
      runtime->async_error_ = "HTTP response could not be delivered";
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

  /// Starts a child that keeps running, and returns `[id, pid]`.
  ///
  /// `spawnSync` answers one question -- what did this command print before
  /// it ended -- and answers it by waiting. This answers nothing at once:
  /// output arrives through the event loop as the child produces it, and the
  /// caller can write to the child's stdin while it runs. That is the only
  /// shape a long-lived child can be used in. esbuild's JavaScript API starts
  /// its binary once and then exchanges packets with it over those pipes for
  /// the life of the build; run through a `spawn` that waits, it deadlocks on
  /// the first request it sends.
  static void ChildSpawn(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (runtime == nullptr || info.Length() < 2 || !info[0]->IsString() ||
        !info[1]->IsArray()) {
      ThrowTypeError(isolate, "spawn needs an executable and argument array");
      return;
    }
    if (runtime->children_.size() >= kMaximumChildren) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate,
                                         "child process capacity exceeded")));
      return;
    }
    const std::string executable = ToUtf8(isolate, info[0]);
    v8::Local<v8::Array> values = info[1].As<v8::Array>();
    if (values->Length() > kMaximumChildArguments) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(isolate,
                                         "child argument limit exceeded")));
      return;
    }
    std::vector<std::string> argument_storage;
    argument_storage.reserve(values->Length());
    for (uint32_t index = 0; index < values->Length(); ++index) {
      v8::Local<v8::Value> value;
      if (!values->Get(context, index).ToLocal(&value)) return;
      argument_storage.push_back(ToUtf8(isolate, value));
    }
    std::vector<SakoNativeBytes> arguments;
    arguments.reserve(argument_storage.size());
    for (const std::string& argument : argument_storage) {
      arguments.push_back({reinterpret_cast<const uint8_t*>(argument.data()),
                           argument.size()});
    }

    const std::string cwd =
        info.Length() > 2 && info[2]->IsString() ? ToUtf8(isolate, info[2])
                                                 : std::string();
    // The caller has already built the command line the way the child expects.
    const int verbatim =
        info.Length() > 3 && info[3]->BooleanValue(isolate) ? 1 : 0;

    // A flat [name, value, ...] array, which is what an environment is once
    // the caller has decided what to keep from its own.
    // An `env` option replaces the child's environment rather than adding to
    // this process's, which is what Node means by it.
    const int replace_environment =
        info.Length() > 4 && info[4]->IsArray() ? 1 : 0;
    std::vector<std::string> environment_storage;
    if (info.Length() > 4 && info[4]->IsArray()) {
      v8::Local<v8::Array> pairs = info[4].As<v8::Array>();
      if (pairs->Length() % 2 != 0 ||
          pairs->Length() / 2 > kMaximumChildEnvironmentVariables) {
        isolate->ThrowException(v8::Exception::RangeError(
            v8::String::NewFromUtf8Literal(
                isolate, "child environment limit exceeded")));
        return;
      }
      environment_storage.reserve(pairs->Length());
      for (uint32_t index = 0; index < pairs->Length(); ++index) {
        v8::Local<v8::Value> value;
        if (!pairs->Get(context, index).ToLocal(&value)) return;
        environment_storage.push_back(ToUtf8(isolate, value));
      }
    }
    std::vector<SakoNativeBytes> environment;
    environment.reserve(environment_storage.size());
    for (const std::string& value : environment_storage) {
      environment.push_back(
          {reinterpret_cast<const uint8_t*>(value.data()), value.size()});
    }
    const int piped_stdin =
        info.Length() > 5 && info[5]->BooleanValue(isolate) ? 1 : 0;

    char error[1024] = {};
    void* child = sako_child_spawn(
        {reinterpret_cast<const uint8_t*>(executable.data()),
         executable.size()},
        arguments.data(), arguments.size(),
        {reinterpret_cast<const uint8_t*>(cwd.data()), cwd.size()}, verbatim,
        environment.data(), environment.size(), replace_environment,
        piped_stdin, error, sizeof(error));
    if (child == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
      return;
    }
    auto binding = std::make_unique<ChildBinding>();
    binding->child = child;
    const uint32_t pid = sako_child_pid(child);
    uint64_t id = runtime->next_child_id_++;
    if (id == 0) id = runtime->next_child_id_++;
    runtime->children_.emplace(id, std::move(binding));

    v8::Local<v8::Array> result = v8::Array::New(isolate, 2);
    if (!result->Set(context, 0, v8::Number::New(isolate,
                                                 static_cast<double>(id)))
             .FromMaybe(false) ||
        !result->Set(context, 1,
                     v8::Number::New(isolate, static_cast<double>(pid)))
             .FromMaybe(false)) {
      return;
    }
    info.GetReturnValue().Set(result);
  }

  /// Looks up a live child, or null when it has already been reaped. A caller
  /// holding on to a finished child is ordinary -- JavaScript learns it ended
  /// one turn after the runtime did.
  ChildBinding* FindChild(const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Local<v8::Context> context = info.GetIsolate()->GetCurrentContext();
    if (info.Length() == 0) return nullptr;
    const uint64_t id = static_cast<uint64_t>(
        info[0]->IntegerValue(context).FromMaybe(0));
    auto found = children_.find(id);
    return found == children_.end() ? nullptr : found->second.get();
  }

  /// Queues bytes for the child's stdin. Reports false when the queue is full
  /// enough that the caller should wait, which is what a writable stream's
  /// `write()` means by the same answer.
  static void ChildWrite(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    const uint8_t* bytes = nullptr;
    size_t length = 0;
    if (runtime == nullptr || info.Length() < 2 ||
        !ReadBytes(info[1], &bytes, &length)) {
      ThrowTypeError(isolate, "child write needs an id and a byte array");
      return;
    }
    ChildBinding* binding = runtime->FindChild(info);
    if (binding == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(isolate, "child is not running")));
      return;
    }
    char error[1024] = {};
    const int accepted = sako_child_write(binding->child, {bytes, length},
                                          error, sizeof(error));
    if (accepted < 0) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
      return;
    }
    info.GetReturnValue().Set(accepted != 0);
  }

  /// Closes the child's stdin once everything queued has reached it. A child
  /// that reads to end of input -- which is most of them -- only finishes
  /// when this happens.
  static void ChildEndStdin(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr) return;
    ChildBinding* binding = runtime->FindChild(info);
    if (binding != nullptr) sako_child_close_stdin(binding->child);
  }

  static void ChildKill(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr) return;
    ChildBinding* binding = runtime->FindChild(info);
    if (binding == nullptr) {
      info.GetReturnValue().Set(false);
      return;
    }
    const bool force =
        info.Length() > 1 && info[1]->BooleanValue(info.GetIsolate());
    info.GetReturnValue().Set(sako_child_kill(binding->child, force ? 1 : 0) ==
                              0);
  }

  /// Whether this child still holds the event loop open.
  static void ChildRef(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr) return;
    ChildBinding* binding = runtime->FindChild(info);
    if (binding == nullptr) return;
    binding->referenced =
        info.Length() < 2 || info[1]->BooleanValue(info.GetIsolate());
  }

  /// Hands one event from a child to the bootstrap's dispatcher.
  ///
  /// Called from inside `sako_child_drain`, once per queued event, with the
  /// bytes borrowed for the duration of the call -- so anything kept has to
  /// be copied into the heap here.
  static void DeliverChildEvent(void* user, int kind, SakoNativeBytes bytes,
                                int status) {
    auto* pump = static_cast<ChildPump*>(user);
    if (pump == nullptr || pump->failed) return;
    Runtime* runtime = pump->runtime;
    v8::Isolate* isolate = runtime->isolate_;
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> context = pump->context;

    switch (kind) {
      case kSakoChildStdoutEnd:
        pump->binding->saw_stdout_end = true;
        break;
      case kSakoChildStderrEnd:
        pump->binding->saw_stderr_end = true;
        break;
      case kSakoChildExited:
        pump->binding->saw_exit = true;
        break;
      default:
        break;
    }

    if (runtime->child_dispatcher_.IsEmpty()) return;
    v8::Local<v8::Function> dispatcher = runtime->child_dispatcher_.Get(isolate);

    v8::Local<v8::Value> payload = v8::Undefined(isolate);
    if (kind == kSakoChildStdout || kind == kSakoChildStderr) {
      std::unique_ptr<v8::BackingStore> backing =
          v8::ArrayBuffer::NewBackingStore(isolate, bytes.length);
      if (bytes.length != 0) {
        std::memcpy(backing->Data(), bytes.data, bytes.length);
      }
      v8::Local<v8::ArrayBuffer> buffer =
          v8::ArrayBuffer::New(isolate, std::move(backing));
      payload = v8::Uint8Array::New(buffer, 0, bytes.length);
    } else if (kind == kSakoChildExited) {
      payload = v8::Integer::New(isolate, status);
    } else if (kind == kSakoChildFailed || kind == kSakoChildStdinFailed) {
      v8::Local<v8::String> message;
      if (v8::String::NewFromUtf8(isolate,
                                  reinterpret_cast<const char*>(bytes.data),
                                  v8::NewStringType::kNormal,
                                  static_cast<int>(bytes.length))
              .ToLocal(&message)) {
        payload = message;
      }
    }

    v8::TryCatch try_catch(isolate);
    v8::Local<v8::Value> arguments[] = {
        v8::Number::New(isolate, static_cast<double>(pump->id)),
        v8::Integer::New(isolate, kind),
        payload,
    };
    v8::Local<v8::Value> result;
    if (!dispatcher
             ->Call(context, v8::Undefined(isolate),
                    static_cast<int>(std::size(arguments)), arguments)
             .ToLocal(&result)) {
      // The rest of this child's queue is abandoned deliberately: the
      // execution is over, and running more of its handlers would report
      // events against a failure already being unwound.
      *pump->error = FormatException(isolate, context, try_catch);
      pump->failed = true;
      return;
    }
    isolate->PerformMicrotaskCheckpoint();
  }

  /// Delivers everything every live child has said since the last turn, and
  /// reaps the ones that have nothing left to say.
  bool TickChildren(v8::Local<v8::Context> context, bool* handled,
                    std::string* error) {
    if (children_.empty()) return true;
    if (child_dispatcher_.IsEmpty() && !ResolveChildDispatcher(context)) {
      *error = "the child process dispatcher is unavailable";
      return false;
    }
    // A handler may spawn or kill children, so the set is snapshotted by id
    // and every entry re-looked-up before it is touched.
    std::vector<uint64_t> ids;
    ids.reserve(children_.size());
    for (const auto& [id, binding] : children_) {
      (void)binding;
      ids.push_back(id);
    }
    std::vector<uint64_t> finished;
    for (uint64_t id : ids) {
      auto found = children_.find(id);
      if (found == children_.end()) continue;
      ChildPump pump;
      pump.runtime = this;
      pump.context = context;
      pump.id = id;
      pump.binding = found->second.get();
      pump.error = error;
      const int delivered =
          sako_child_drain(found->second->child, DeliverChildEvent, &pump);
      if (pump.failed) return false;
      if (delivered > 0) *handled = true;
      // The lookup is repeated because a handler may have removed this child
      // while its own events were being delivered.
      found = children_.find(id);
      if (found != children_.end() && found->second->finished()) {
        finished.push_back(id);
      }
    }
    for (uint64_t id : finished) children_.erase(id);
    return true;
  }

  /// Looks up the bootstrap's child dispatcher once per runtime.
  bool ResolveChildDispatcher(v8::Local<v8::Context> context) {
    v8::Local<v8::Value> dispatcher;
    if (!context->Global()
             ->Get(context, v8::String::NewFromUtf8Literal(
                                isolate_, "__sakoDispatchChildEvent"))
             .ToLocal(&dispatcher) ||
        !dispatcher->IsFunction()) {
      return false;
    }
    child_dispatcher_.Reset(isolate_, dispatcher.As<v8::Function>());
    return true;
  }

  /// Opens one PostgreSQL connection and returns the id JavaScript knows it
  /// by. Blocks until the server has authenticated the session.
  static void PostgresConnect(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    if (runtime == nullptr || info.Length() == 0 || !info[0]->IsString()) {
      ThrowTypeError(isolate, "connect needs a postgres:// URL");
      return;
    }
    if (runtime->databases_.size() >= kMaximumDatabaseConnections) {
      isolate->ThrowException(v8::Exception::RangeError(
          v8::String::NewFromUtf8Literal(
              isolate, "database connection capacity exceeded")));
      return;
    }
    const std::string url = ToUtf8(isolate, info[0]);
    char error[4096] = {};
    void* connection = sako_postgres_connect(
        {reinterpret_cast<const uint8_t*>(url.data()), url.size()}, error,
        sizeof(error));
    if (connection == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
      return;
    }
    uint64_t id = runtime->next_database_id_++;
    if (id == 0) id = runtime->next_database_id_++;
    runtime->databases_.emplace(id, connection);
    info.GetReturnValue().Set(v8::Number::New(isolate, static_cast<double>(id)));
  }

  /// Runs one statement and materializes the whole result.
  ///
  /// Values arrive as the text the server printed and leave as JavaScript
  /// strings; the column type OIDs go back alongside them so the library can
  /// decide what each column's text means. Turning a numeric column into a
  /// number here would mean this layer owning a type table it has no business
  /// owning.
  static void PostgresQuery(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    v8::Local<v8::Context> context = isolate->GetCurrentContext();
    if (runtime == nullptr || info.Length() < 2 || !info[1]->IsString()) {
      ThrowTypeError(isolate, "query needs a connection and a statement");
      return;
    }
    void* connection = runtime->FindDatabase(info);
    if (connection == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8Literal(isolate, "the connection is closed")));
      return;
    }
    const std::string sql = ToUtf8(isolate, info[1]);

    std::vector<std::string> parameter_storage;
    std::vector<SakoNativeBytes> parameters;
    std::vector<uint8_t> nulls;
    if (info.Length() > 2 && info[2]->IsArray()) {
      v8::Local<v8::Array> given = info[2].As<v8::Array>();
      if (given->Length() > kMaximumDatabaseParameters) {
        isolate->ThrowException(v8::Exception::RangeError(
            v8::String::NewFromUtf8Literal(isolate,
                                           "query parameter limit exceeded")));
        return;
      }
      parameter_storage.reserve(given->Length());
      nulls.reserve(given->Length());
      for (uint32_t index = 0; index < given->Length(); ++index) {
        v8::Local<v8::Value> value;
        if (!given->Get(context, index).ToLocal(&value)) return;
        if (value->IsNullOrUndefined()) {
          parameter_storage.emplace_back();
          nulls.push_back(1);
          continue;
        }
        parameter_storage.push_back(ToUtf8(isolate, value));
        nulls.push_back(0);
      }
      parameters.reserve(parameter_storage.size());
      for (const std::string& parameter : parameter_storage) {
        parameters.push_back(
            {reinterpret_cast<const uint8_t*>(parameter.data()), parameter.size()});
      }
    }

    char error[4096] = {};
    void* result = sako_postgres_query(
        connection, {reinterpret_cast<const uint8_t*>(sql.data()), sql.size()},
        parameters.data(), nulls.data(), parameters.size(), error,
        sizeof(error));
    if (result == nullptr) {
      isolate->ThrowException(v8::Exception::Error(
          v8::String::NewFromUtf8(isolate, error).ToLocalChecked()));
      return;
    }
    std::unique_ptr<void, void (*)(void*)> owned(result,
                                                 sako_postgres_result_delete);

    const size_t column_count = sako_postgres_result_column_count(result);
    const size_t row_count = sako_postgres_result_row_count(result);
    v8::Local<v8::Array> names = v8::Array::New(isolate, static_cast<int>(column_count));
    v8::Local<v8::Array> types = v8::Array::New(isolate, static_cast<int>(column_count));
    for (size_t index = 0; index < column_count; ++index) {
      v8::Local<v8::String> name;
      if (!MakeString(isolate, sako_postgres_result_column_name(result, index))
               .ToLocal(&name) ||
          !names->Set(context, static_cast<uint32_t>(index), name).FromMaybe(false) ||
          !types
               ->Set(context, static_cast<uint32_t>(index),
                     v8::Integer::NewFromUnsigned(
                         isolate, sako_postgres_result_column_type(result, index)))
               .FromMaybe(false)) {
        return;
      }
    }

    v8::Local<v8::Array> rows = v8::Array::New(isolate, static_cast<int>(row_count));
    for (size_t row = 0; row < row_count; ++row) {
      v8::Local<v8::Array> values =
          v8::Array::New(isolate, static_cast<int>(column_count));
      for (size_t column = 0; column < column_count; ++column) {
        int is_null = 1;
        const SakoNativeBytes value =
            sako_postgres_result_value(result, row, column, &is_null);
        v8::Local<v8::Value> entry;
        if (is_null != 0) {
          entry = v8::Null(isolate);
        } else {
          v8::Local<v8::String> text;
          if (!MakeString(isolate, value).ToLocal(&text)) return;
          entry = text;
        }
        if (!values->Set(context, static_cast<uint32_t>(column), entry)
                 .FromMaybe(false)) {
          return;
        }
      }
      if (!rows->Set(context, static_cast<uint32_t>(row), values).FromMaybe(false)) {
        return;
      }
    }

    v8::Local<v8::String> command;
    if (!MakeString(isolate, sako_postgres_result_command(result)).ToLocal(&command)) {
      return;
    }
    v8::Local<v8::Object> answer = v8::Object::New(isolate);
    if (!runtime->Set(context, answer, "names", names) ||
        !runtime->Set(context, answer, "types", types) ||
        !runtime->Set(context, answer, "rows", rows) ||
        !runtime->Set(context, answer, "command", command) ||
        !runtime->Set(context, answer, "affected",
                      v8::Number::New(
                          isolate,
                          static_cast<double>(sako_postgres_result_affected(result))))) {
      return;
    }
    info.GetReturnValue().Set(answer);
  }

  /// A server parameter such as `server_version`, or null.
  static void PostgresParameter(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    v8::Isolate* isolate = info.GetIsolate();
    if (runtime == nullptr || info.Length() < 2 || !info[1]->IsString()) {
      ThrowTypeError(isolate, "a server parameter is read by name");
      return;
    }
    void* connection = runtime->FindDatabase(info);
    if (connection == nullptr) {
      info.GetReturnValue().SetNull();
      return;
    }
    const std::string name = ToUtf8(isolate, info[1]);
    const SakoNativeBytes value = sako_postgres_parameter(
        connection, {reinterpret_cast<const uint8_t*>(name.data()), name.size()});
    if (value.length == 0) {
      info.GetReturnValue().SetNull();
      return;
    }
    v8::Local<v8::String> text;
    if (!MakeString(isolate, value).ToLocal(&text)) return;
    info.GetReturnValue().Set(text);
  }

  /// Ends the session and releases the handle.
  static void PostgresClose(const v8::FunctionCallbackInfo<v8::Value>& info) {
    Runtime* runtime = FromCallback(info);
    if (runtime == nullptr || info.Length() == 0) return;
    v8::Local<v8::Context> context = info.GetIsolate()->GetCurrentContext();
    const uint64_t id =
        static_cast<uint64_t>(info[0]->IntegerValue(context).FromMaybe(0));
    auto found = runtime->databases_.find(id);
    if (found == runtime->databases_.end()) return;
    sako_postgres_delete(found->second);
    runtime->databases_.erase(found);
  }

  void* FindDatabase(const v8::FunctionCallbackInfo<v8::Value>& info) {
    v8::Local<v8::Context> context = info.GetIsolate()->GetCurrentContext();
    if (info.Length() == 0) return nullptr;
    const uint64_t id =
        static_cast<uint64_t>(info[0]->IntegerValue(context).FromMaybe(0));
    auto found = databases_.find(id);
    return found == databases_.end() ? nullptr : found->second;
  }

  static v8::MaybeLocal<v8::String> MakeString(v8::Isolate* isolate,
                                               SakoNativeBytes bytes) {
    if (bytes.length > static_cast<size_t>(std::numeric_limits<int>::max())) {
      return {};
    }
    return v8::String::NewFromUtf8(isolate,
                                   reinterpret_cast<const char*>(bytes.data),
                                   v8::NewStringType::kNormal,
                                   static_cast<int>(bytes.length));
  }

  static int DispatchHttp(void* context, uint64_t ticket,
                          SakoNativeBytes method,
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
    if (!runtime->ResolveHttpDispatchers(js_context)) {
      runtime->async_error_ = "HTTP JavaScript dispatcher is unavailable";
      return 1;
    }
    v8::Local<v8::Function> dispatcher =
        binding->raw ? runtime->raw_http_dispatcher_.Get(isolate)
                     : runtime->http_dispatcher_.Get(isolate);
    auto make_string = [isolate](SakoNativeBytes bytes) {
      return v8::String::NewFromUtf8(
          isolate, reinterpret_cast<const char*>(bytes.data),
          v8::NewStringType::kNormal, static_cast<int>(bytes.length));
    };
    std::optional<sako_perf::Span> marshal_request;
    if (sako_perf::g_enabled) {
      marshal_request.emplace(sako_perf::kBucketHttpMarshalRequest);
    }
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
    // Every ArrayBuffer costs an allocation, an extension record, and sweeper
    // bookkeeping, so a request carries at most one: an empty body reuses a
    // shared view, and the header ranges share the header buffer's storage.
    v8::Local<v8::Uint8Array> body_value;
    if (body.length == 0) {
      body_value = runtime->EmptyBytes(isolate);
    } else {
      std::unique_ptr<v8::BackingStore> body_backing =
          v8::ArrayBuffer::NewBackingStore(
              isolate, body.length,
              v8::BackingStoreInitializationMode::kUninitialized);
      std::memcpy(body_backing->Data(), body.data, body.length);
      body_buffer = v8::ArrayBuffer::New(isolate, std::move(body_backing));
      body_value = v8::Uint8Array::New(body_buffer, 0, body.length);
    }
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
    const size_t range_count = header_count * 4;
    const size_t range_bytes = range_count * sizeof(uint32_t);
    v8::Local<v8::Uint8Array> header_values;
    v8::Local<v8::Uint32Array> header_ranges;
    if (header_count == 0) {
      header_values = runtime->EmptyBytes(isolate);
      header_ranges = runtime->EmptyRanges(isolate);
    } else {
      std::unique_ptr<v8::BackingStore> header_backing =
          v8::ArrayBuffer::NewBackingStore(
              isolate, range_bytes + header_bytes_length,
              v8::BackingStoreInitializationMode::kUninitialized);
      auto* ranges = static_cast<uint32_t*>(header_backing->Data());
      auto* header_output =
          static_cast<uint8_t*>(header_backing->Data()) + range_bytes;
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
      header_ranges = v8::Uint32Array::New(header_buffer, 0, range_count);
      header_values =
          v8::Uint8Array::New(header_buffer, range_bytes, header_bytes_length);
    }
    v8::Local<v8::Value> arguments[] = {
        binding->handler.Get(isolate),
        method_value,
        target_value,
        header_values,
        header_ranges,
        body_value,
        v8::Boolean::New(isolate, binding->secure),
        v8::Number::New(isolate, static_cast<double>(binding->id)),
        v8::Number::New(isolate, static_cast<double>(ticket))};
    marshal_request.reset();
    v8::Local<v8::Value> result;
    {
      std::optional<sako_perf::Span> handler_span;
      if (sako_perf::g_enabled) {
        handler_span.emplace(sako_perf::kBucketHttpHandler);
      }
      if (!dispatcher->Call(js_context, v8::Undefined(isolate), 9, arguments)
               .ToLocal(&result) ||
          !result->IsObject()) {
        runtime->async_error_ = FormatException(isolate, js_context, try_catch);
        return 1;
      }
    }
    // A raw dispatch that already has the answer returns it as the wire tuple,
    // so the ordinary request costs one call into JavaScript rather than two.
    // Anything else is a handler that answered with a promise, and needs the
    // second call for the same reason the node-shaped path does: whether it
    // settled during the checkpoint below is only knowable afterwards.
    const bool answered_inline = binding->raw && result->IsArray();
    {
      std::optional<sako_perf::Span> microtask_span;
      if (sako_perf::g_enabled) {
        microtask_span.emplace(sako_perf::kBucketHttpMicrotasks);
      }
      isolate->PerformMicrotaskCheckpoint();
    }
    v8::Local<v8::Value> finalized = result;
    if (!answered_inline) {
      std::optional<sako_perf::Span> finalize_span;
      if (sako_perf::g_enabled) {
        finalize_span.emplace(sako_perf::kBucketHttpFinalize);
      }
      v8::Local<v8::Function> finalizer =
          binding->raw ? runtime->raw_http_finalizer_.Get(isolate)
                       : runtime->http_finalizer_.Get(isolate);
      if (!finalizer->Call(js_context, v8::Undefined(isolate), 1, &result)
               .ToLocal(&finalized)) {
        runtime->async_error_ = FormatException(isolate, js_context, try_catch);
        return 1;
      }
    }
    std::optional<sako_perf::Span> marshal_response;
    if (sako_perf::g_enabled) {
      marshal_response.emplace(sako_perf::kBucketHttpMarshalResponse);
    }
    // The finalizer returns [status, reason, headers, body]: reading four
    // array elements avoids creating and looking up four property names on
    // every request.
    // Null means the handler has not finished: it kept the response object and
    // will call back with the ticket once it has one. Everything the request
    // borrowed from the native side is copied by then, so returning here is
    // safe -- the connection simply goes quiet until the answer arrives.
    if (finalized->IsNull()) return kNativeHttpDeferred;

    v8::Local<v8::Value> status;
    v8::Local<v8::Value> reason;
    v8::Local<v8::Value> response_body;
    v8::Local<v8::Value> response_headers;
    if (!finalized->IsArray()) {
      runtime->async_error_ = "HTTP dispatcher returned an invalid response";
      return 1;
    }
    v8::Local<v8::Array> response_fields = finalized.As<v8::Array>();
    if (response_fields->Length() != 4 ||
        !response_fields->Get(js_context, 0).ToLocal(&status) ||
        !response_fields->Get(js_context, 1).ToLocal(&reason) ||
        !response_fields->Get(js_context, 2).ToLocal(&response_headers) ||
        !response_fields->Get(js_context, 3).ToLocal(&response_body) ||
        !reason->IsString() || !response_headers->IsArray()) {
      runtime->async_error_ = "HTTP dispatcher returned an invalid response";
      return 1;
    }
    const int64_t status_code = status->IntegerValue(js_context).FromMaybe(0);
    if (status_code < 100 || status_code > 999) {
      runtime->async_error_ = "HTTP dispatcher returned an invalid status";
      return 1;
    }
    ToUtf8Into(isolate, reason, &binding->response_reason);
    if (response_body->IsString()) {
      ToUtf8Into(isolate, response_body, &binding->response_body);
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
    // The name and value vectors are never shrunk, so a steady stream of
    // responses with the same header set reuses every string's storage
    // instead of allocating one per header per response.
    if (binding->response_header_names.size() < pair_count) {
      binding->response_header_names.resize(pair_count);
      binding->response_header_values.resize(pair_count);
    }
    for (uint32_t index = 0; index < pairs->Length(); index += 2) {
      v8::Local<v8::Value> name;
      v8::Local<v8::Value> value;
      if (!pairs->Get(js_context, index).ToLocal(&name) ||
          !pairs->Get(js_context, index + 1).ToLocal(&value)) {
        runtime->async_error_ = "cannot read HTTP response headers";
        return 1;
      }
      ToUtf8Into(isolate, name, &binding->response_header_names[index / 2]);
      ToUtf8Into(isolate, value, &binding->response_header_values[index / 2]);
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

  // Zero-length views handed to every request that carries no body or no
  // headers. They expose no bytes, so sharing them cannot leak request data.
  v8::Local<v8::Uint8Array> EmptyBytes(v8::Isolate* isolate) {
    if (empty_bytes_.IsEmpty()) {
      v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(isolate, 0);
      v8::Local<v8::Uint8Array> view = v8::Uint8Array::New(buffer, 0, 0);
      empty_bytes_.Reset(isolate, view);
      return view;
    }
    return empty_bytes_.Get(isolate);
  }

  v8::Local<v8::Uint32Array> EmptyRanges(v8::Isolate* isolate) {
    if (empty_ranges_.IsEmpty()) {
      v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(isolate, 0);
      v8::Local<v8::Uint32Array> view = v8::Uint32Array::New(buffer, 0, 0);
      empty_ranges_.Reset(isolate, view);
      return view;
    }
    return empty_ranges_.Get(isolate);
  }

  // Looks up the bootstrap's HTTP dispatch functions once per runtime; the
  // request path then calls them without a global property lookup each time.
  bool ResolveHttpDispatchers(v8::Local<v8::Context> context) {
    if (!http_dispatcher_.IsEmpty()) return true;
    struct Pair {
      const char* name;
      v8::Global<v8::Function>* slot;
    };
    v8::Global<v8::Function> dispatcher;
    v8::Global<v8::Function> finalizer;
    v8::Global<v8::Function> raw_dispatcher;
    v8::Global<v8::Function> raw_finalizer;
    const Pair pairs[] = {
        {"__sakoDispatchHttpRequest", &dispatcher},
        {"__sakoFinalizeHttpResponse", &finalizer},
        {"__sakoDispatchRawHttpRequest", &raw_dispatcher},
        {"__sakoFinalizeRawHttpResponse", &raw_finalizer},
    };
    for (const Pair& pair : pairs) {
      v8::Local<v8::Value> value;
      v8::Local<v8::String> name;
      if (!v8::String::NewFromUtf8(isolate_, pair.name).ToLocal(&name) ||
          !context->Global()->Get(context, name).ToLocal(&value) ||
          !value->IsFunction()) {
        return false;
      }
      pair.slot->Reset(isolate_, value.As<v8::Function>());
    }
    http_dispatcher_ = std::move(dispatcher);
    http_finalizer_ = std::move(finalizer);
    raw_http_dispatcher_ = std::move(raw_dispatcher);
    raw_http_finalizer_ = std::move(raw_finalizer);
    return true;
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

  // Blocks in each server's completion port until one reports activity. With
  // a single server this is one wait; with several, each gets a slice so no
  // server can hold the loop while another has work.
  void WaitForHttpServers(uint32_t timeout_milliseconds) {
    if (http_servers_.empty() || timeout_milliseconds == 0) return;
    const uint32_t slice = static_cast<uint32_t>(std::max<size_t>(
        1, timeout_milliseconds / http_servers_.size()));
    for (auto& [id, binding] : http_servers_) {
      (void)id;
      if (binding->server == nullptr) continue;
      if (sako_http_server_wait(binding->server, slice) != 0) return;
    }
  }

  /// Tracks promise rejections that nothing is handling.
  ///
  /// Without this a rejected promise vanished: the process printed nothing and
  /// exited 0, so an async failure looked exactly like success. Anything whose
  /// entry point returns a floating promise -- most CLI tools -- failed
  /// invisibly.
  static void HandlePromiseRejection(v8::PromiseRejectMessage message) {
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    Runtime* runtime = static_cast<Runtime*>(isolate->GetData(0));
    if (runtime == nullptr) return;
    const int identity = message.GetPromise()->GetIdentityHash();
    switch (message.GetEvent()) {
      case v8::kPromiseRejectWithNoHandler:
        runtime->pending_rejections_.insert_or_assign(
            identity, v8::Global<v8::Value>(isolate, message.GetValue()));
        break;
      // A handler attached after the fact makes the rejection handled, which
      // is ordinary for a promise stored now and awaited later.
      case v8::kPromiseHandlerAddedAfterReject:
        runtime->pending_rejections_.erase(identity);
        break;
      default:
        break;
    }
  }

  /// Turns any still-unhandled rejection into the execution's error.
  bool ReportPendingRejection(v8::Local<v8::Context> context,
                              std::string* error) {
    if (pending_rejections_.empty()) return true;
    auto entry = pending_rejections_.begin();
    v8::Local<v8::Value> reason = entry->second.Get(isolate_);
    std::string text = "unhandled promise rejection";
    if (!reason.IsEmpty()) {
      // Prefer the stack, which carries the message and the frames; fall back
      // to the plain string form for a non-Error rejection value.
      v8::Local<v8::Value> stack;
      if (reason->IsObject() &&
          GetProperty(context, reason.As<v8::Object>(), "stack", &stack) &&
          stack->IsString()) {
        text = ToUtf8(isolate_, stack);
      } else {
        v8::Local<v8::String> description;
        if (reason->ToString(context).ToLocal(&description)) {
          text = ToUtf8(isolate_, description);
        }
      }
      AppendCauses(isolate_, context, reason, &text);
    }
    pending_rejections_.clear();
    *error = text;
    return false;
  }

  bool DrainEventLoop(v8::Local<v8::Context> context, std::string* error) {
    while (true) {
      // Read before draining, so activity that lands while this turn runs
      // still counts as new when the loop decides how long to block.
      const uint64_t child_tick = sako_child_activity_tick();
      while (v8::platform::PumpMessageLoop(platform_, isolate_)) {
        isolate_->PerformMicrotaskCheckpoint();
      }

      bool handled_child = false;
      if (!TickChildren(context, &handled_child, error)) return false;

      // Finalizers the last collection released, completions from async work,
      // and threadsafe calls posted by an addon's own threads.
      bool handled_napi = false;
      if (!sako_napi::RunTasks(context, &handled_napi, error)) return false;
      const bool napi_active = sako_napi::HasPendingWork(isolate_);

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
      // Nothing left to run: any rejection still unhandled at this point never
      // will be, so surface it as the execution's failure. Servers and addons
      // count as work, and so do timers -- but only the referenced ones, which
      // is the whole point of `unref()`.
      const bool referenced_timer =
          std::any_of(timers_.begin(), timers_.end(), [](const auto& entry) {
            return entry.second.referenced;
          });
      const bool referenced_child =
          std::any_of(children_.begin(), children_.end(),
                      [](const auto& entry) {
                        return entry.second->referenced;
                      });
      if (!referenced_timer && http_servers_.empty() && !napi_active &&
          !referenced_child) {
        return ReportPendingRejection(context, error);
      }
      if (timers_.empty()) {
        // Nothing ran this turn, so block rather than spin: a request landing
        // in the completion port, an addon posting from a worker thread, or a
        // child answering on its pipe wakes the loop at once instead of
        // waiting out a timer tick.
        if (!handled_http && !handled_napi && !handled_child) {
          if (!children_.empty() && http_servers_.empty() && !napi_active) {
            sako_child_wait_activity(child_tick, kIdleHttpWaitMilliseconds);
          } else if (http_servers_.empty()) {
            sako_napi::WaitForWork(isolate_, kIdleHttpWaitMilliseconds);
          } else {
            WaitForHttpServers(kIdleHttpWaitMilliseconds);
          }
        }
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
        const int64_t wait_milliseconds = std::min<int64_t>(
            remaining.count(), std::numeric_limits<uint32_t>::max());
        if (http_servers_.empty()) {
          if (napi_active) {
            sako_napi::WaitForWork(isolate_,
                                   static_cast<uint32_t>(wait_milliseconds));
          } else if (!children_.empty()) {
            // A timer caps the wait, but a child answering on its pipe still
            // ends it early: sleeping out the timer would hold a reply the
            // runtime already has.
            if (!handled_child) {
              sako_child_wait_activity(child_tick,
                                       static_cast<uint32_t>(wait_milliseconds));
            }
          } else {
            std::this_thread::sleep_for(
                std::chrono::milliseconds(wait_milliseconds));
          }
        } else if (!handled_http) {
          // A pending timer caps the wait, but socket activity still ends it
          // early because the completion port is what the loop blocks on.
          WaitForHttpServers(static_cast<uint32_t>(
              std::min<int64_t>(wait_milliseconds, kIdleHttpWaitMilliseconds)));
        }
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
  // Whether this isolate was created from the build-time snapshot, and so
  // whether Context::New already returns a fully bootstrapped context.
  bool from_snapshot_ = false;
  v8::Global<v8::Context> context_;
  std::unordered_map<uint64_t, Timer> timers_;
  std::unordered_map<std::string, v8::Global<v8::Module>> modules_;
  std::unordered_map<int, std::string> module_paths_;
  // Rejections with no handler, keyed by promise identity so a handler
  // attached later can remove the entry again.
  std::unordered_map<int, v8::Global<v8::Value>> pending_rejections_;
  std::unordered_map<std::string, v8::Global<v8::Object>> commonjs_modules_;
  std::unordered_map<int, SyntheticCommonJs> synthetic_commonjs_;
  std::unordered_map<uint64_t, std::unique_ptr<HttpBinding>> http_servers_;
  std::unordered_map<uint64_t, std::unique_ptr<ChildBinding>> children_;
  /// Owned PostgreSQL connections, deleted with the runtime.
  std::unordered_map<uint64_t, void*> databases_;
  std::unordered_map<int, FileDescriptor> file_descriptors_;
  std::vector<uint64_t> closing_http_servers_;
  v8::Global<v8::Function> http_dispatcher_;
  v8::Global<v8::Function> http_finalizer_;
  v8::Global<v8::Function> raw_http_dispatcher_;
  v8::Global<v8::Function> raw_http_finalizer_;
  v8::Global<v8::Function> child_dispatcher_;
  v8::Global<v8::Uint8Array> empty_bytes_;
  v8::Global<v8::Uint32Array> empty_ranges_;
  std::string async_error_;
  size_t module_source_bytes_ = 0;
  uint64_t next_timer_id_ = 1;
  uint64_t next_http_server_id_ = 1;
  uint64_t next_child_id_ = 1;
  uint64_t next_database_id_ = 1;
  int next_file_descriptor_ = 100;
};

// Defined out of line because it takes the addresses of Runtime's own private
// static callbacks alongside the file-scope ones.
const intptr_t* Runtime::ExternalReferences() {
  static const intptr_t references[] = {
      // Timers and scheduling.
      reinterpret_cast<intptr_t>(&Runtime::SetTimeout),
      reinterpret_cast<intptr_t>(&Runtime::SetInterval),
      reinterpret_cast<intptr_t>(&Runtime::ClearTimer),
      reinterpret_cast<intptr_t>(&Runtime::QueueMicrotask),
      // Module system.
      reinterpret_cast<intptr_t>(&Runtime::CreateRequireCallback),
      reinterpret_cast<intptr_t>(&Runtime::TimerRef),
      reinterpret_cast<intptr_t>(&Runtime::HttpRespond),
      // Console and standard streams.
      reinterpret_cast<intptr_t>(&ConsoleLog),
      reinterpret_cast<intptr_t>(&WriteStandard),
      reinterpret_cast<intptr_t>(&TerminalSize),
      reinterpret_cast<intptr_t>(&IsTty),
      reinterpret_cast<intptr_t>(&StdinRead),
      reinterpret_cast<intptr_t>(&StdinSetRawMode),
      reinterpret_cast<intptr_t>(&ProcessExit),
      // Text encoding.
      reinterpret_cast<intptr_t>(&EncodeUtf8),
      reinterpret_cast<intptr_t>(&DecodeUtf8),
      // Filesystem.
      reinterpret_cast<intptr_t>(&ReadFileSync),
      reinterpret_cast<intptr_t>(&WriteFileSync),
      reinterpret_cast<intptr_t>(&ExistsSync),
      reinterpret_cast<intptr_t>(&StatSync),
      reinterpret_cast<intptr_t>(&ReadDirectorySync),
      reinterpret_cast<intptr_t>(&MakeDirectorySync),
      reinterpret_cast<intptr_t>(&RemovePathSync),
      reinterpret_cast<intptr_t>(&RenamePathSync),
      reinterpret_cast<intptr_t>(&LinkPathSync),
      reinterpret_cast<intptr_t>(&SymlinkPathSync),
      reinterpret_cast<intptr_t>(&ReadLinkSync),
      reinterpret_cast<intptr_t>(&RealPathSync),
      reinterpret_cast<intptr_t>(&Runtime::OpenSync),
      reinterpret_cast<intptr_t>(&Runtime::CloseSync),
      reinterpret_cast<intptr_t>(&Runtime::ReadSync),
      reinterpret_cast<intptr_t>(&Runtime::WriteSync),
      // Network and process.
      reinterpret_cast<intptr_t>(&FetchSync),
      reinterpret_cast<intptr_t>(&ResolveHost),
      reinterpret_cast<intptr_t>(&SpawnSync),
      reinterpret_cast<intptr_t>(&Runtime::ChildSpawn),
      reinterpret_cast<intptr_t>(&Runtime::ChildWrite),
      reinterpret_cast<intptr_t>(&Runtime::ChildEndStdin),
      reinterpret_cast<intptr_t>(&Runtime::ChildKill),
      reinterpret_cast<intptr_t>(&Runtime::ChildRef),
      reinterpret_cast<intptr_t>(&Runtime::PostgresConnect),
      reinterpret_cast<intptr_t>(&Runtime::PostgresQuery),
      reinterpret_cast<intptr_t>(&Runtime::PostgresParameter),
      reinterpret_cast<intptr_t>(&Runtime::PostgresClose),
      reinterpret_cast<intptr_t>(&Hash),
      // HTTP server bindings.
      reinterpret_cast<intptr_t>(&Runtime::HttpListen),
      reinterpret_cast<intptr_t>(&Runtime::HttpsListen),
      reinterpret_cast<intptr_t>(&Runtime::HttpClose),
      reinterpret_cast<intptr_t>(&Runtime::HttpAddress),
      // The table must end with a zero sentinel.
      0,
  };
  return references;
}

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

extern "C" void sako_v8_runtime_abandon(void* runtime) {
  if (runtime == nullptr) return;
  // Deliberately not deleted. See Runtime::abandon on the Rust side: the heap
  // walk that disposing costs produces only memory the kernel is about to
  // unmap. The engine has to be told, or its own process-exit teardown trips
  // over the isolate this just left alive.
  GetEngine().Abandon();
}

extern "C" void sako_v8_runtime_delete(void* runtime) {
  delete static_cast<Runtime*>(runtime);
}

/// Puts the terminal back before the process ends.
///
/// `process.exit` already does this on its own way out, but a script that
/// simply returns leaves through the CLI instead, and that path terminates
/// too -- no destructor between a prompt in raw mode and a shell that has lost
/// its echo. Safe to call when raw mode was never entered.
extern "C" void sako_v8_restore_terminal() { RestoreTerminalMode(); }

/// Records the console output code page the CLI switched away from, so the
/// exit paths that never return can put it back.
extern "C" void sako_v8_remember_output_code_page(unsigned int code_page) {
#if defined(_WIN32)
  g_saved_output_code_page = code_page;
#else
  (void)code_page;
#endif
}

#if defined(SAKO_SNAPSHOT_GENERATOR)

// --- Build-time context snapshot generator ---------------------------------
//
// Compiled from this same translation unit, so the context it serializes is
// built by exactly the code a run would otherwise execute: there is one
// bootstrap sequence in the tree, not two that can drift apart.
//
// The Rust-side bindings the bridge calls at run time are not linked in here.
// Nothing the bootstrap does reaches them today, and if that ever changes the
// build has to stop rather than quietly snapshot a context built on stub
// behaviour -- so every stub aborts instead of returning something plausible.

namespace {

[[noreturn]] void SnapshotStubReached(const char* name) {
  fprintf(stderr,
          "sako snapshot: the bootstrap reached %s, which exists only in the "
          "linked runtime; its result must not be serialized\n",
          name);
  fflush(stderr);
  std::abort();
}

}  // namespace

#define SAKO_SNAPSHOT_STUB(name) SnapshotStubReached(#name)

// The snapshot generator is this file compiled on its own, without napi.cc.
// It never loads an addon -- it only builds a context -- so the seam is
// satisfied with definitions that say so.
namespace sako_napi {
bool LoadAddon(v8::Local<v8::Context>, const std::filesystem::path&,
               v8::Local<v8::Value>*, std::string* error) {
  *error = "native addons are unavailable while generating the snapshot";
  return false;
}
bool RunTasks(v8::Local<v8::Context>, bool* ran, std::string*) {
  if (ran != nullptr) *ran = false;
  return true;
}
bool HasPendingWork(v8::Isolate*) { return false; }
void WaitForWork(v8::Isolate*, uint32_t) {}
void Shutdown(v8::Isolate*) {}
}  // namespace sako_napi

extern "C" {

void* sako_http_server_new(uint16_t, uint16_t*, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_http_server_new);
}
void* sako_https_server_new(uint16_t, SakoNativeBytes, SakoNativeBytes,
                            uint16_t*, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_https_server_new);
}
int sako_http_server_tick(void*, SakoNativeHttpHandler, void*, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_http_server_tick);
}
int sako_http_server_wait(void*, uint32_t) {
  SAKO_SNAPSHOT_STUB(sako_http_server_wait);
}
void sako_http_server_delete(void*) {
  SAKO_SNAPSHOT_STUB(sako_http_server_delete);
}
int sako_http_server_respond(void*, uint64_t, uint16_t, SakoNativeBytes,
                             const SakoNativeHeader*, size_t,
                             SakoNativeBytes) {
  SAKO_SNAPSHOT_STUB(sako_http_server_respond);
}
int sako_http_server_close(void*) {
  SAKO_SNAPSHOT_STUB(sako_http_server_close);
}
int sako_http_server_stats(void*, uint64_t*, uint64_t*) {
  SAKO_SNAPSHOT_STUB(sako_http_server_stats);
}
int sako_dns_resolve(SakoNativeBytes, int, char*, size_t, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_dns_resolve);
}
void* sako_process_spawn_sync(SakoNativeBytes, const SakoNativeBytes*, size_t,
                              SakoNativeBytes, int, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_process_spawn_sync);
}
int sako_process_output_status(const void*) {
  SAKO_SNAPSHOT_STUB(sako_process_output_status);
}
SakoNativeBytes sako_process_output_stdout(const void*) {
  SAKO_SNAPSHOT_STUB(sako_process_output_stdout);
}
SakoNativeBytes sako_process_output_stderr(const void*) {
  SAKO_SNAPSHOT_STUB(sako_process_output_stderr);
}
void sako_process_output_delete(void*) {
  SAKO_SNAPSHOT_STUB(sako_process_output_delete);
}
void* sako_child_spawn(SakoNativeBytes, const SakoNativeBytes*, size_t,
                       SakoNativeBytes, int, const SakoNativeBytes*, size_t,
                       int, int, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_child_spawn);
}
uint32_t sako_child_pid(const void*) { SAKO_SNAPSHOT_STUB(sako_child_pid); }
int sako_child_drain(const void*, SakoNativeChildEvent, void*) {
  SAKO_SNAPSHOT_STUB(sako_child_drain);
}
int sako_child_write(const void*, SakoNativeBytes, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_child_write);
}
int sako_child_close_stdin(const void*) {
  SAKO_SNAPSHOT_STUB(sako_child_close_stdin);
}
int sako_child_kill(const void*, int) { SAKO_SNAPSHOT_STUB(sako_child_kill); }
void sako_child_delete(void*) { SAKO_SNAPSHOT_STUB(sako_child_delete); }
uint64_t sako_child_activity_tick() {
  SAKO_SNAPSHOT_STUB(sako_child_activity_tick);
}
uint64_t sako_child_wait_activity(uint64_t, uint32_t) {
  SAKO_SNAPSHOT_STUB(sako_child_wait_activity);
}
void* sako_postgres_connect(SakoNativeBytes, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_postgres_connect);
}
void* sako_postgres_query(void*, SakoNativeBytes, const SakoNativeBytes*,
                          const uint8_t*, size_t, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_postgres_query);
}
void sako_postgres_close(void*) { SAKO_SNAPSHOT_STUB(sako_postgres_close); }
void sako_postgres_delete(void*) { SAKO_SNAPSHOT_STUB(sako_postgres_delete); }
SakoNativeBytes sako_postgres_parameter(const void*, SakoNativeBytes) {
  SAKO_SNAPSHOT_STUB(sako_postgres_parameter);
}
size_t sako_postgres_result_column_count(const void*) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_column_count);
}
SakoNativeBytes sako_postgres_result_column_name(const void*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_column_name);
}
uint32_t sako_postgres_result_column_type(const void*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_column_type);
}
size_t sako_postgres_result_row_count(const void*) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_row_count);
}
SakoNativeBytes sako_postgres_result_value(const void*, size_t, size_t, int*) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_value);
}
SakoNativeBytes sako_postgres_result_command(const void*) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_command);
}
uint64_t sako_postgres_result_affected(const void*) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_affected);
}
void sako_postgres_result_delete(void*) {
  SAKO_SNAPSHOT_STUB(sako_postgres_result_delete);
}
void* sako_fetch_sync(SakoNativeBytes, SakoNativeBytes, const SakoNativeHeader*,
                      size_t, SakoNativeBytes, char*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_fetch_sync);
}
uint16_t sako_fetch_output_status(const void*) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_status);
}
SakoNativeBytes sako_fetch_output_status_text(const void*) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_status_text);
}
SakoNativeBytes sako_fetch_output_url(const void*) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_url);
}
size_t sako_fetch_output_header_count(const void*) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_header_count);
}
SakoNativeBytes sako_fetch_output_header_name(const void*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_header_name);
}
SakoNativeBytes sako_fetch_output_header_value(const void*, size_t) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_header_value);
}
SakoNativeBytes sako_fetch_output_body(const void*) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_body);
}
void sako_fetch_output_delete(void*) {
  SAKO_SNAPSHOT_STUB(sako_fetch_output_delete);
}
void* sako_typescript_transpile(SakoNativeBytes, SakoNativeBytes, int, char*,
                                size_t) {
  SAKO_SNAPSHOT_STUB(sako_typescript_transpile);
}
SakoNativeBytes sako_typescript_output_source(const void*) {
  SAKO_SNAPSHOT_STUB(sako_typescript_output_source);
}
void sako_typescript_output_delete(void*) {
  SAKO_SNAPSHOT_STUB(sako_typescript_output_delete);
}
size_t sako_hash(uint32_t, SakoNativeBytes, uint8_t*) {
  SAKO_SNAPSHOT_STUB(sako_hash);
}

}  // extern "C"

namespace {

// Names the snapshot has to carry. A context missing any of them deserializes
// into something the runtime cannot use, and failing the build beats shipping
// it.
constexpr const char* kRequiredGlobals[] = {
    "console",       "setTimeout",          "setInterval",
    "queueMicrotask", "setImmediate",       "Buffer",
    "TextEncoder",   "TextDecoder",         "URL",
    "fetch",         "Headers",             "Response",
    "AbortController", "performance",       "__sakoPlatform",
    "__sakoBuiltins", "__sakoProcessExtras", "__sakoReadFileSync",
    "__sakoIsTty",   "__sakoWriteStandard", "__sakoCreateRequire",
    "__sakoHttpListen", "__sakoStdinRead",    "__sakoStdinSetRawMode",
    "__sakoChildSpawn", "__sakoDispatchChildEvent",
};

// Names the snapshot must not carry. Each describes one execution rather than
// the build, so a value frozen here would be wrong on every run afterwards.
constexpr const char* kForbiddenGlobals[] = {
    "__sakoCwd", "__sakoEpochMs", "process", "global",
};

bool VerifySnapshotContext(v8::Isolate* isolate, v8::Local<v8::Context> context,
                           std::string* error) {
  v8::Local<v8::Object> global = context->Global();
  for (const char* name : kRequiredGlobals) {
    v8::Local<v8::String> key;
    v8::Local<v8::Value> value;
    if (!v8::String::NewFromUtf8(isolate, name).ToLocal(&key) ||
        !global->Get(context, key).ToLocal(&value) || value->IsUndefined()) {
      *error = std::string("the bootstrapped context is missing ") + name;
      return false;
    }
  }
  for (const char* name : kForbiddenGlobals) {
    v8::Local<v8::String> key;
    bool present = false;
    if (!v8::String::NewFromUtf8(isolate, name).ToLocal(&key) ||
        !global->Has(context, key).To(&present)) {
      *error = "cannot inspect the bootstrapped context";
      return false;
    }
    if (present) {
      *error = std::string("per-run state reached the snapshot: ") + name;
      return false;
    }
  }
  return true;
}

bool WriteSnapshotHeader(const char* path, const uint8_t* data, size_t length) {
  std::string header =
      "// Generated by bridge.cc compiled as the snapshot generator.\n"
      "static constexpr unsigned char kSakoSnapshot[] = {\n";
  header.reserve(length * 4 + 128);
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
    fprintf(stderr,
            "usage: sako_snapshot <icudtl.dat|empty> <output header>\n");
    return 1;
  }
  // An empty path means this V8 build carries its ICU data compiled in
  // (icu_use_data_file=false), so there is no external file to point at.
  if (argv[1][0] != '\0' &&
      !v8::V8::InitializeICUDefaultLocation(argv[0], argv[1])) {
    fprintf(stderr, "sako snapshot: cannot initialize ICU from %s\n", argv[1]);
    return 1;
  }
  // The same baseline tuning a run applies, so the flag hash V8 stamps into
  // the blob matches the one it checks on deserialization. SAKO_V8_FLAGS is
  // deliberately not read here: a run that sets it skips the snapshot.
  if (kSakoV8Flags[0] != '\0') v8::V8::SetFlagsFromString(kSakoV8Flags);
  std::unique_ptr<v8::Platform> platform = v8::platform::NewDefaultPlatform();
  v8::V8::InitializePlatform(platform.get());
  if (!v8::V8::Initialize()) {
    fprintf(stderr, "sako snapshot: cannot initialize V8\n");
    return 1;
  }

  int status = 0;
  v8::StartupData blob = {nullptr, 0};
  {
    std::unique_ptr<v8::ArrayBuffer::Allocator> allocator(
        v8::ArrayBuffer::Allocator::NewDefaultAllocator());
    v8::Isolate::CreateParams params;
    params.array_buffer_allocator = allocator.get();
    // Must be the same table, in the same order, that Isolate::New is given
    // on the consuming side: the blob stores indices into it.
    params.external_references = Runtime::ExternalReferences();
    v8::SnapshotCreator creator(params);
    {
      v8::Isolate* isolate = creator.GetIsolate();
      v8::HandleScope handle_scope(isolate);
      v8::Local<v8::Context> context = v8::Context::New(isolate);
      std::string error;
      {
        v8::Context::Scope context_scope(context);
        if (!Runtime::BuildContext(isolate, context, &error) ||
            !VerifySnapshotContext(isolate, context, &error)) {
          fprintf(stderr, "sako snapshot: %s\n", error.c_str());
          status = 1;
        }
      }
      if (status == 0) creator.SetDefaultContext(context);
    }
    // CreateBlob must not run inside a handle scope. kKeep keeps whatever
    // code V8 has already compiled; measured against kClear that is almost
    // nothing, because the bootstrap's functions are compiled lazily and only
    // its top-level body has run. The two blobs differ by 0.2% in size and
    // restore in the same time, so this keeps the safer of the two.
    if (status == 0) {
      blob = creator.CreateBlob(v8::SnapshotCreator::FunctionCodeHandling::kKeep);
    }
  }

  if (status == 0 && (blob.data == nullptr || blob.raw_size <= 0)) {
    fprintf(stderr, "sako snapshot: V8 produced no snapshot data\n");
    status = 1;
  }
  if (status == 0 &&
      !WriteSnapshotHeader(argv[2],
                           reinterpret_cast<const uint8_t*>(blob.data),
                           static_cast<size_t>(blob.raw_size))) {
    fprintf(stderr, "sako snapshot: cannot write %s\n", argv[2]);
    status = 1;
  }
  if (status == 0) {
    fprintf(stderr, "sako snapshot: %d bytes\n", blob.raw_size);
  }
  delete[] blob.data;
  v8::V8::Dispose();
  v8::V8::DisposePlatform();
  return status;
}

#endif  // SAKO_SNAPSHOT_GENERATOR
