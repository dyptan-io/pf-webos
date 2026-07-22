//! C wrapper for the webOS `StarfishMediaAPIs` C++ class (`libplayerAPIs.so.1`).
//!
//! `libplayerAPIs.so` ships on every webOS 5+ TV but only exposes a C++ ABI.
//! `starfish.rs` expects C-compatible symbols (resolved via `dlopen`/`dlsym` as
//! `libplayerAPIs_C.so`) — this file IS that wrapper.  `build.rs` compiles it
//! as a shared library and `taskfiles/toolchain.yml` bundles it alongside
//! `libSDL2-2.0.so.0` in the IPK's `lib/` directory, loaded via the binary's
//! `$ORIGIN/../lib` rpath.
//!
//! Naming mirrors the convention used by `mariotaku/ss4s` (`modules/webos/smp`)
//! and `GuiDev1994/aurora-tv`, which also ship this wrapper rather than linking
//! against the C++ class directly.

#include <starfish-media-pipeline/StarfishMediaAPIs.h>
#include <cstring>
#include <cstdint>
#include <cstddef>

extern "C" {

// ── lifecycle ────────────────────────────────────────────────────────────────

void* StarfishMediaAPIs_create(const char* uid)
{
    return new StarfishMediaAPIs(uid);
}

void StarfishMediaAPIs_destroy(void* api)
{
    delete static_cast<StarfishMediaAPIs*>(api);
}

// ── pipeline control ─────────────────────────────────────────────────────────

bool StarfishMediaAPIs_load(
    void*    api,
    const char* payload,
    void  (* cb)(int type, int64_t num_value, const char* str_value, void* data),
    void*    data)
{
    return static_cast<StarfishMediaAPIs*>(api)->Load(payload, cb, data);
}

bool StarfishMediaAPIs_play(void* api)
{
    return static_cast<StarfishMediaAPIs*>(api)->Play();
}

bool StarfishMediaAPIs_unload(void* api)
{
    return static_cast<StarfishMediaAPIs*>(api)->Unload();
}

bool StarfishMediaAPIs_pushEOS(void* api)
{
    return static_cast<StarfishMediaAPIs*>(api)->pushEOS();
}

// ── frame feeding ────────────────────────────────────────────────────────────

// `StarfishMediaAPIs::Feed` returns a std::string.  We copy it into the
// caller-supplied `result_buf` (null-terminated, truncated to `result_size`)
// before the string object destructs.
bool StarfishMediaAPIs_feed(
    void*       api,
    const char* payload,
    char*       result_buf,
    size_t      result_size)
{
    std::string result = static_cast<StarfishMediaAPIs*>(api)->Feed(payload);
    if (result_buf && result_size > 0) {
        std::strncpy(result_buf, result.c_str(), result_size - 1);
        result_buf[result_size - 1] = '\0';
    }
    return true;
}

// ── metadata / misc ──────────────────────────────────────────────────────────

bool StarfishMediaAPIs_notifyForeground(void* api)
{
    return static_cast<StarfishMediaAPIs*>(api)->notifyForeground();
}

bool StarfishMediaAPIs_setHdrInfo(void* api, const char* message)
{
    return static_cast<StarfishMediaAPIs*>(api)->setHdrInfo(message);
}

} // extern "C"
