// SPDX-License-Identifier: BSD-3-Clause

//! Everything the runtime needs from the Node-API layer, and nothing more.
//!
//! `napi.cc` is a large translation unit with its own state; keeping the seam
//! this narrow is what lets it stay out of `bridge.cc`, which is already the
//! biggest file in the tree.

#ifndef SAKO_NAPI_H_
#define SAKO_NAPI_H_

#include <cstdint>
#include <filesystem>
#include <string>

#include "v8.h"

namespace sako_napi {

/// Loads a compiled Node-API addon and hands back the value its registration
/// function returned, which becomes the CommonJS `module.exports`.
///
/// The library stays loaded for the life of the process: an addon's static
/// state, its threads, and the JavaScript objects still pointing into it all
/// outlive any sensible unload point.
bool LoadAddon(v8::Local<v8::Context> context, const std::filesystem::path& path,
               v8::Local<v8::Value>* exports, std::string* error);

/// Runs whatever an addon has queued for the loop thread: finalizers released
/// by the last GC, completions from async work, and threadsafe-function calls
/// posted from other threads.
///
/// `ran` reports whether anything happened, so the caller can tell a busy turn
/// from an idle one. Returns false with `error` filled when a callback threw.
bool RunTasks(v8::Local<v8::Context> context, bool* ran, std::string* error);

/// Whether an addon still holds the loop open -- queued work, or a
/// threadsafe function that has not been unreferenced.
bool HasPendingWork(v8::Isolate* isolate);

/// Blocks until an addon posts work or `milliseconds` elapse. Called instead
/// of sleeping so a completion arriving from a worker thread wakes the loop at
/// once rather than at the end of the current tick.
void WaitForWork(v8::Isolate* isolate, uint32_t milliseconds);

/// Releases everything this isolate's addons own. Runs the environment cleanup
/// hooks they registered, then stops the worker threads.
void Shutdown(v8::Isolate* isolate);

}  // namespace sako_napi

#endif  // SAKO_NAPI_H_
