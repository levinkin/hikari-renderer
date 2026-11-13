#if defined(__APPLE__)
#include <TargetConditionals.h>
#endif
#include "Live2DCubismCore.h"
#if TARGET_OS_IOS
extern "C" void engine_core_load(void)
{
}
#else
#include <stdio.h>
#include <stdlib.h>
#include <string>
#if defined(_WIN32)
#include <windows.h>
#else
#include <dlfcn.h>
#include <limits.h>
#include <unistd.h>
#endif
#if defined(__APPLE__)
#include <mach-o/dyld.h>
#endif

#define CORE_FUNCTIONS(X) \
    X(csmVersion, csmGetVersion, (), ()) \
    X(csmMocVersion, csmGetLatestMocVersion, (), ()) \
    X(csmMocVersion, csmGetMocVersion, (const void* address, const unsigned int size), (address, size)) \
    X(int, csmHasMocConsistency, (void* address, const unsigned int size), (address, size)) \
    X(csmLogFunction, csmGetLogFunction, (), ()) \
    X(void, csmSetLogFunction, (csmLogFunction handler), (handler)) \
    X(csmMoc*, csmReviveMocInPlace, (void* address, const unsigned int size), (address, size)) \
    X(unsigned int, csmGetSizeofModel, (const csmMoc* moc), (moc)) \
    X(csmModel*, csmInitializeModelInPlace, (const csmMoc* moc, void* address, const unsigned int size), (moc, address, size)) \
    X(void, csmUpdateModel, (csmModel* model), (model)) \
    X(const int*, csmGetRenderOrders, (const csmModel* model), (model)) \
    X(void, csmReadCanvasInfo, (const csmModel* model, csmVector2* outSizeInPixels, csmVector2* outOriginInPixels, float* outPixelsPerUnit), (model, outSizeInPixels, outOriginInPixels, outPixelsPerUnit)) \
    X(int, csmGetParameterCount, (const csmModel* model), (model)) \
    X(const char**, csmGetParameterIds, (const csmModel* model), (model)) \
    X(const csmParameterType*, csmGetParameterTypes, (const csmModel* model), (model)) \
    X(const float*, csmGetParameterMinimumValues, (const csmModel* model), (model)) \
    X(const float*, csmGetParameterMaximumValues, (const csmModel* model), (model)) \
    X(const float*, csmGetParameterDefaultValues, (const csmModel* model), (model)) \
    X(float*, csmGetParameterValues, (csmModel* model), (model)) \
    X(const int*, csmGetParameterRepeats, (const csmModel* model), (model)) \
    X(int, csmGetPartCount, (const csmModel* model), (model)) \
    X(const char**, csmGetPartIds, (const csmModel* model), (model)) \
    X(float*, csmGetPartOpacities, (csmModel* model), (model)) \
    X(const int*, csmGetPartParentPartIndices, (const csmModel* model), (model)) \
    X(const int*, csmGetPartOffscreenIndices, (const csmModel* model), (model)) \
    X(int, csmGetDrawableCount, (const csmModel* model), (model)) \
    X(const char**, csmGetDrawableIds, (const csmModel* model), (model)) \
    X(const csmFlags*, csmGetDrawableConstantFlags, (const csmModel* model), (model)) \
    X(const csmFlags*, csmGetDrawableDynamicFlags, (const csmModel* model), (model)) \
    X(const int*, csmGetDrawableBlendModes, (const csmModel* model), (model)) \
    X(const int*, csmGetDrawableTextureIndices, (const csmModel* model), (model)) \
    X(const float*, csmGetDrawableOpacities, (const csmModel* model), (model)) \
    X(const int*, csmGetDrawableMaskCounts, (const csmModel* model), (model)) \
    X(const int**, csmGetDrawableMasks, (const csmModel* model), (model)) \
    X(const int*, csmGetDrawableVertexCounts, (const csmModel* model), (model)) \
    X(const csmVector2**, csmGetDrawableVertexPositions, (const csmModel* model), (model)) \
    X(const csmVector2**, csmGetDrawableVertexUvs, (const csmModel* model), (model)) \
    X(const int*, csmGetDrawableIndexCounts, (const csmModel* model), (model)) \
    X(const unsigned short**, csmGetDrawableIndices, (const csmModel* model), (model)) \
    X(const csmVector4*, csmGetDrawableMultiplyColors, (const csmModel* model), (model)) \
    X(const csmVector4*, csmGetDrawableScreenColors, (const csmModel* model), (model)) \
    X(const int*, csmGetDrawableParentPartIndices, (const csmModel* model), (model)) \
    X(void, csmResetDrawableDynamicFlags, (csmModel* model), (model)) \
    X(int, csmGetOffscreenCount, (const csmModel* model), (model)) \
    X(const int*, csmGetOffscreenBlendModes, (const csmModel* model), (model)) \
    X(const float*, csmGetOffscreenOpacities, (const csmModel* model), (model)) \
    X(const int*, csmGetOffscreenOwnerIndices, (const csmModel* model), (model)) \
    X(const csmVector4*, csmGetOffscreenMultiplyColors, (const csmModel* model), (model)) \
    X(const csmVector4*, csmGetOffscreenScreenColors, (const csmModel* model), (model)) \
    X(const int*, csmGetOffscreenMaskCounts, (const csmModel* model), (model)) \
    X(const int**, csmGetOffscreenMasks, (const csmModel* model), (model)) \
    X(const csmFlags*, csmGetOffscreenConstantFlags, (const csmModel* model), (model))

#define CORE_POINTER(ret, name, params, args) static decltype(&name) name##_ptr;
#define CORE_FORWARD(ret, name, params, args) \
    ret name params \
    { \
        return name##_ptr args; \
    }
#define CORE_ENTRY(ret, name, params, args) {#name, (void**)&name##_ptr},

CORE_FUNCTIONS(CORE_POINTER)

extern "C"
{
    CORE_FUNCTIONS(CORE_FORWARD)
}

struct core_entry
{
    const char* name;
    void** slot;
};

static const core_entry core_entries[] = {CORE_FUNCTIONS(CORE_ENTRY)};

#if defined(_WIN32)
static const char core_file[] = "Live2DCubismCore.dll";
static const char core_sdk_path[] = "Core/dll/windows/x86_64/Live2DCubismCore.dll";

static std::string exe_dir(void)
{
    wchar_t path[MAX_PATH];
    DWORD length = GetModuleFileNameW(NULL, path, MAX_PATH);
    std::wstring wide(path, length);
    wide.resize(wide.find_last_of(L"\\/"));
    int size = WideCharToMultiByte(CP_UTF8, 0, wide.c_str(), (int)wide.size(), NULL, 0, NULL, NULL);
    std::string utf8(size, '\0');
    WideCharToMultiByte(CP_UTF8, 0, wide.c_str(), (int)wide.size(), &utf8[0], size, NULL, NULL);
    return utf8;
}

static void* core_open(const std::string& path)
{
    int size = MultiByteToWideChar(CP_UTF8, 0, path.c_str(), -1, NULL, 0);
    std::wstring wide(size, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, path.c_str(), -1, &wide[0], size);
    return (void*)LoadLibraryW(wide.c_str());
}

static void* core_symbol(void* library, const char* name)
{
    return (void*)GetProcAddress((HMODULE)library, name);
}
#else
#if defined(__APPLE__)
static const char core_file[] = "libLive2DCubismCore.dylib";
static const char core_sdk_path[] = "Core/dll/macos/libLive2DCubismCore.dylib";
#else
static const char core_file[] = "libLive2DCubismCore.so";
#if defined(__aarch64__)
static const char core_sdk_path[] = "Core/dll/experimental/linux/arm64/libLive2DCubismCore.so";
#else
static const char core_sdk_path[] = "Core/dll/linux/x86_64/libLive2DCubismCore.so";
#endif
#endif

static std::string exe_dir(void)
{
    char raw[PATH_MAX];
    char path[PATH_MAX];
#if defined(__APPLE__)
    uint32_t size = sizeof(raw);
    _NSGetExecutablePath(raw, &size);
#else
    ssize_t length = readlink("/proc/self/exe", raw, sizeof(raw) - 1);
    raw[length < 0 ? 0 : length] = 0;
#endif
    std::string resolved = realpath(raw, path) ? path : raw;
    return resolved.substr(0, resolved.find_last_of('/'));
}

static void* core_open(const std::string& path)
{
    return dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
}

static void* core_symbol(void* library, const char* name)
{
    return dlsym(library, name);
}
#endif

extern "C" void engine_core_load(void)
{
    std::string dir = exe_dir();
#if defined(_WIN32)
    std::string files[4] = {dir + "\\" + core_file, dir + "\\core\\" + core_file, dir + "\\sdk\\" + core_sdk_path, std::string(HIKARI_SDK "/") + core_sdk_path};
#else
    std::string files[4] = {dir + "/" + core_file, dir + "/core/" + core_file, dir + "/sdk/" + core_sdk_path, std::string(HIKARI_SDK "/") + core_sdk_path};
#endif
    bool complete = false;
    for (const std::string& file : files)
    {
        if (complete)
        {
            break;
        }
        void* library = core_open(file);
        complete = library != NULL;
        for (const core_entry& entry : core_entries)
        {
            *entry.slot = complete ? core_symbol(library, entry.name) : NULL;
            complete = complete && *entry.slot != NULL;
        }
        if (library != NULL && !complete)
        {
#if defined(_WIN32)
            FreeLibrary((HMODULE)library);
#else
            dlclose(library);
#endif
        }
    }
    if (!complete)
    {
        fprintf(stderr, "hikari: no usable Cubism Core at %s, %s, %s or %s; download the Cubism SDK for Native zip from https://www.live2d.com/en/sdk/download/native/ and drop it next to hikari before building, or copy %s next to hikari\n", files[0].c_str(), files[1].c_str(), files[2].c_str(), files[3].c_str(), core_sdk_path);
        exit(1);
    }
}
#endif
