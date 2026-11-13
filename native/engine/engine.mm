#import <Metal/Metal.h>
#import <Foundation/Foundation.h>
#import <ImageIO/ImageIO.h>
#import <CoreGraphics/CoreGraphics.h>
#if TARGET_OS_OSX
#import <AppKit/AppKit.h>
#import <QuartzCore/CAMetalLayer.h>
#import "SyphonServerBase.h"
#endif
#include <algorithm>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <map>
#include <mutex>
#include <string>
#include <vector>
#include "engine.h"
#include "texture_bc7.inc"
#include "CubismFramework.hpp"
#include "CubismModelSettingJson.hpp"
#include "Model/CubismUserModel.hpp"
#include "Motion/CubismMotionManager.hpp"
#include "Motion/CubismExpressionMotionManager.hpp"
#include "Effect/CubismBreath.hpp"
#include "Effect/CubismEyeBlink.hpp"
#include "Effect/CubismPose.hpp"
#include "Physics/CubismPhysics.hpp"
#include "Id/CubismIdManager.hpp"
#include "Math/CubismMatrix44.hpp"

using namespace Live2D::Cubism::Framework;

static const char metal_shader_types[] = {
#embed "Rendering/Metal/Shaders/MetalShaderTypes.h"
    , 0};
static const char metal_shaders[] = {
#embed "Rendering/Metal/Shaders/MetalShaders.metal"
    , 0};

struct metal_renderer;
bool metal_renderer_start(id<MTLDevice> device, const std::string& shader_source);
metal_renderer* metal_renderer_create(CubismModel* model, const std::vector<id<MTLTexture>>& textures, const char* name);
void metal_renderer_destroy(metal_renderer* r);
void metal_renderer_draw(metal_renderer* r, id<MTLCommandBuffer> command_buffer, id<MTLTexture> target, const CubismMatrix44& mvp, id<MTLTexture> back, bool opaque);
bool metal_renderer_share(id<MTLCommandBuffer> command_buffer, id<MTLTexture> model, id<MTLTexture> back, id<MTLTexture> window_target, id<MTLTexture> share_target);

struct engine_gpu
{
    metal_renderer* own = NULL;
    id<MTLTexture> color_texture = nil;
    uint64_t background_serial = 0;
};

#include "engine_common.inc"

static id<MTLDevice> device = nil;
static id<MTLCommandQueue> queue = nil;
static std::map<std::string, std::vector<id<MTLTexture>>> texture_cache;
static std::mutex texture_lock;

static bool start_metal()
{
    if (device != nil)
    {
        return true;
    }
    device = MTLCreateSystemDefaultDevice();
    if (device == nil)
    {
        return false;
    }
    queue = [device newCommandQueue];
    gpu_name = std::string([[device name] UTF8String]);
    std::string source(metal_shaders);
    const std::string include_line = "#include \"MetalShaderTypes.h\"";
    source.replace(source.find(include_line), include_line.size(), metal_shader_types);
    if (!metal_renderer_start(device, source))
    {
        return false;
    }
    static CubismFramework::Option option = {};
    option.LoggingLevel = CubismFramework::Option::LogLevel_Off;
    CubismFramework::StartUp(&allocator, &option);
    CubismFramework::Initialize();
    return true;
}

static void upload_level(id<MTLTexture> texture, id<MTLBuffer> staging, const uint8_t* data, uint32_t level, size_t row_bytes, uint32_t rows, uint32_t row_height)
{
    uint32_t w = std::max((uint32_t)texture.width >> level, 1u);
    uint32_t h = std::max((uint32_t)texture.height >> level, 1u);
    uint32_t chunk_rows = (uint32_t)std::max(staging.length / row_bytes, (NSUInteger)1);
    for (uint32_t row = 0; row < rows; row += chunk_rows)
    {
        uint32_t count = std::min(chunk_rows, rows - row);
        memcpy([staging contents], data + row * row_bytes, count * row_bytes);
        uint32_t y = row * row_height;
        id<MTLCommandBuffer> command_buffer = [queue commandBuffer];
        id<MTLBlitCommandEncoder> blit = [command_buffer blitCommandEncoder];
        [blit copyFromBuffer:staging sourceOffset:0 sourceBytesPerRow:row_bytes sourceBytesPerImage:count * row_bytes sourceSize:MTLSizeMake(w, std::min(count * row_height, h - y), 1) toTexture:texture destinationSlice:0 destinationLevel:level destinationOrigin:MTLOriginMake(0, y, 0)];
        [blit endEncoding];
        [command_buffer commit];
        [command_buffer waitUntilCompleted];
    }
}

static id<MTLTexture> load_bc7(FILE* file, const bc7_header& header)
{
    MTLTextureDescriptor* descriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBC7_RGBAUnorm width:header.width height:header.height mipmapped:YES];
    descriptor.storageMode = MTLStorageModePrivate;
    descriptor.usage = MTLTextureUsageShaderRead;
    id<MTLTexture> texture = [device newTextureWithDescriptor:descriptor];
    id<MTLBuffer> staging = [device newBufferWithLength:std::min(bc7_level_bytes(header.width, header.height, 0), staging_size) options:MTLResourceStorageModeShared];
    std::vector<uint8_t> level_bytes;
    bool ok = texture != nil && staging != nil;
    for (uint32_t level = 0; ok && level < header.levels; level++)
    {
        level_bytes.resize(bc7_level_bytes(header.width, header.height, level));
        ok = fread(level_bytes.data(), 1, level_bytes.size(), file) == level_bytes.size();
        uint32_t blocks_w = (std::max(header.width >> level, 1u) + 3) / 4;
        if (ok)
        {
            upload_level(texture, staging, level_bytes.data(), level, (size_t)blocks_w * 16, (uint32_t)(level_bytes.size() / (blocks_w * 16)), 4);
        }
    }
    fclose(file);
    return ok ? texture : nil;
}

static id<MTLTexture> load_texture(const std::string& path)
{
    std::vector<uint8_t> bytes = read_file(path);
    bool use_bc7 = !bytes.empty() && [device supportsBCTextureCompression];
    std::string cache_path = use_bc7 ? bc7_cache_path(bytes) : "";
    bc7_header header;
    FILE* cached = bc7_open(cache_path, header);
    if (cached != NULL && header.levels > 0)
    {
        return load_bc7(cached, header);
    }
    if (cached != NULL)
    {
        fclose(cached);
        use_bc7 = false;
    }
    NSData* data = [NSData dataWithBytesNoCopy:bytes.data() length:bytes.size() freeWhenDone:NO];
    CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)data, NULL);
    if (source == NULL)
    {
        return nil;
    }
    CGImageRef image = CGImageSourceCreateImageAtIndex(source, 0, NULL);
    CFRelease(source);
    if (image == NULL)
    {
        return nil;
    }
    size_t w = CGImageGetWidth(image);
    size_t h = CGImageGetHeight(image);
    std::vector<uint8_t> pixels(w * h * 4);
    CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    CGContextRef context = CGBitmapContextCreate(pixels.data(), w, h, 8, w * 4, space, kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
    CGContextSetBlendMode(context, kCGBlendModeCopy);
    CGContextDrawImage(context, CGRectMake(0, 0, w, h), image);
    CGContextRelease(context);
    CGColorSpaceRelease(space);
    CGImageRelease(image);
    if (use_bc7 && bc7_encode(pixels.data(), (uint32_t)w, (uint32_t)h, cache_path) && (cached = bc7_open(cache_path, header)) != NULL)
    {
        return load_bc7(cached, header);
    }
    MTLTextureDescriptor* descriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:w height:h mipmapped:YES];
    descriptor.storageMode = MTLStorageModePrivate;
    descriptor.usage = MTLTextureUsageShaderRead;
    id<MTLTexture> texture = [device newTextureWithDescriptor:descriptor];
    id<MTLBuffer> staging = [device newBufferWithLength:std::min(pixels.size(), staging_size) options:MTLResourceStorageModeShared];
    upload_level(texture, staging, pixels.data(), 0, w * 4, (uint32_t)h, 1);
    id<MTLCommandBuffer> command_buffer = [queue commandBuffer];
    id<MTLBlitCommandEncoder> blit = [command_buffer blitCommandEncoder];
    [blit generateMipmapsForTexture:texture];
    [blit endEncoding];
    [command_buffer commit];
    [command_buffer waitUntilCompleted];
    return texture;
}

engine* engine_create(const char* model3_json_path, int width, int height)
{
    @autoreleasepool
    {
        if (!start_metal())
        {
            return NULL;
        }
        std::string path(model3_json_path);
        std::string dir = path.substr(0, path.rfind('/') + 1);
        engine* e = load_model(path, dir, width, height);
        if (e == NULL)
        {
            return NULL;
        }
        std::lock_guard<std::mutex> guard(texture_lock);
        std::vector<id<MTLTexture>>& textures = texture_cache[path];
        if (textures.empty())
        {
            for (csmInt32 i = 0; i < e->setting->GetTextureCount(); i++)
            {
                textures.push_back(load_texture(dir + e->setting->GetTextureFileName(i)));
            }
        }
        e->own = metal_renderer_create(e->_model, textures, model3_json_path);
        if (e->own == NULL)
        {
            engine_destroy(e);
            return NULL;
        }
        view_matrix(e);
        return e;
    }
}

void engine_destroy(engine* e)
{
    @autoreleasepool
    {
        for (auto& item : e->expressions)
        {
            ACubismMotion::Delete(item.second);
        }
        metal_renderer_destroy(e->own);
        delete e->setting;
        delete e;
    }
}

void engine_textures_release(const char* model3_json_path)
{
    std::lock_guard<std::mutex> guard(texture_lock);
    texture_cache.erase(model3_json_path);
}

int engine_background_load(engine* e, const uint8_t* bytes, size_t length, const char* expected_type)
{
    @autoreleasepool
    {
        NSDictionary* formats = @{
            @"image/png": @[@"public.png", [NSData dataWithBytes:"IEND\xae\x42\x60\x82" length:8]],
            @"image/jpeg": @[@"public.jpeg", [NSData dataWithBytes:"\xff\xd9" length:2]],
            @"image/gif": @[@"com.compuserve.gif", [NSData dataWithBytes:";" length:1]],
        };
        NSArray* format = formats[[NSString stringWithUTF8String:expected_type]];
        NSData* trailer = format[1];
        NSData* data = [NSData dataWithBytes:bytes length:length];
        CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)data, NULL);
        if (source == NULL)
        {
            return 0;
        }
        size_t count = CGImageSourceGetCount(source);
        bool ok = count > 0 && format != nil && [(__bridge NSString*)CGImageSourceGetType(source) isEqualToString:format[0]] && CGImageSourceGetStatus(source) == kCGImageStatusComplete && length >= trailer.length && memcmp(bytes + length - trailer.length, trailer.bytes, trailer.length) == 0;
        std::vector<std::vector<uint8_t>> frames;
        std::vector<float> ms;
        float total_ms = 0;
        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        for (size_t i = 0; ok && i < count; i++)
        {
            CGImageRef image = CGImageSourceCreateImageAtIndex(source, i, NULL);
            if (image == NULL)
            {
                ok = false;
                break;
            }
            NSDictionary* properties = (__bridge_transfer NSDictionary*)CGImageSourceCopyPropertiesAtIndex(source, i, NULL);
            NSNumber* delay = nil;
            NSDictionary* gif = properties[(__bridge NSString*)kCGImagePropertyGIFDictionary];
            NSDictionary* png = properties[(__bridge NSString*)kCGImagePropertyPNGDictionary];
            NSDictionary* webp = properties[(__bridge NSString*)kCGImagePropertyWebPDictionary];
            if (gif != nil)
            {
                delay = gif[(__bridge NSString*)kCGImagePropertyGIFUnclampedDelayTime] ?: gif[(__bridge NSString*)kCGImagePropertyGIFDelayTime];
            }
            if (png != nil)
            {
                delay = png[(__bridge NSString*)kCGImagePropertyAPNGUnclampedDelayTime] ?: png[(__bridge NSString*)kCGImagePropertyAPNGDelayTime];
            }
            if (webp != nil)
            {
                delay = webp[(__bridge NSString*)kCGImagePropertyWebPUnclampedDelayTime] ?: webp[(__bridge NSString*)kCGImagePropertyWebPDelayTime];
            }
            float frame_ms = delay != nil ? [delay floatValue] * 1000.0f : 100.0f;
            if (frame_ms < 20.0f)
            {
                frame_ms = 100.0f;
            }
            double iw = CGImageGetWidth(image);
            double ih = CGImageGetHeight(image);
            double scale = fmax(e->width / iw, e->height / ih);
            double bw = ceil(iw * scale);
            double bh = ceil(ih * scale);
            std::vector<uint8_t> frame((size_t)e->width * e->height * 4);
            CGContextRef context = CGBitmapContextCreate(frame.data(), e->width, e->height, 8, e->width * 4, space, kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
            CGContextSetRGBFillColor(context, 0, 0, 0, 1);
            CGContextFillRect(context, CGRectMake(0, 0, e->width, e->height));
            CGContextSetInterpolationQuality(context, kCGInterpolationHigh);
            CGContextDrawImage(context, CGRectMake((e->width - bw) / 2, (e->height - bh) / 2, bw, bh), image);
            CGContextRelease(context);
            CGImageRelease(image);
            frames.push_back(std::move(frame));
            ms.push_back(frame_ms);
            total_ms += frame_ms;
        }
        CGColorSpaceRelease(space);
        CFRelease(source);
        if (!ok)
        {
            return 0;
        }
        e->background_frames = std::move(frames);
        e->background_ms = std::move(ms);
        e->background_total_ms = total_ms;
        e->background_serial++;
        return (int)e->background_frames.size();
    }
}

const uint8_t* engine_render(engine* e, float background_clock_ms)
{
    @autoreleasepool
    {
        if (e->color_texture == nil || (int)e->color_texture.width != e->width || (int)e->color_texture.height != e->height)
        {
            MTLTextureDescriptor* color = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:e->width height:e->height mipmapped:NO];
            color.storageMode = MTLStorageModeShared;
            color.usage = MTLTextureUsageRenderTarget;
            e->color_texture = [device newTextureWithDescriptor:color];
            e->rgba.resize((size_t)e->width * e->height * 4);
        }
        update_shown(e);
        id<MTLCommandBuffer> command_buffer = [queue commandBuffer];
        metal_renderer_draw(e->own, command_buffer, e->color_texture, e->mvp, nil, false);
        [command_buffer commit];
        [command_buffer waitUntilCompleted];
        [e->color_texture getBytes:e->rgba.data() bytesPerRow:e->width * 4 fromRegion:MTLRegionMake2D(0, 0, e->width, e->height) mipmapLevel:0];
    }
    const uint8_t* back = background_at(e, background_clock_ms);
    uint8_t* out = e->rgba.data();
    size_t count = (size_t)e->width * e->height;
    for (size_t p = 0; p < count; p++, out += 4)
    {
        int keep = 255 - out[3];
        int blue = out[0];
        int back_red = back != NULL ? back[p * 4] : 0;
        int back_green = back != NULL ? back[p * 4 + 1] : 0;
        int back_blue = back != NULL ? back[p * 4 + 2] : 0;
        out[0] = (uint8_t)((out[2] * 255 + back_red * keep + 127) / 255);
        out[1] = (uint8_t)((out[1] * 255 + back_green * keep + 127) / 255);
        out[2] = (uint8_t)((blue * 255 + back_blue * keep + 127) / 255);
        out[3] = 255;
    }
    return e->rgba.data();
}

#if TARGET_OS_OSX
@interface SyphonServerBase (SyphonSubclassing)
- (IOSurfaceRef)newSurfaceForWidth:(size_t)width height:(size_t)height options:(NSDictionary*)options;
- (void)publish;
@end

@interface hikari_window : NSWindow
@property(nonatomic) BOOL close_requested;
@end

@implementation hikari_window
- (void)performClose:(id)sender
{
    self.close_requested = YES;
}
- (BOOL)canBecomeKeyWindow
{
    return YES;
}
- (BOOL)canBecomeMainWindow
{
    return YES;
}
@end

struct window
{
    hikari_window* ns = nil;
    NSView* view = nil;
    CAMetalLayer* layer = nil;
    id monitor = nil;
    id close_observer = nil;
    id resign_observer = nil;
    NSTimer* timer = nil;
    engine_hook frame = NULL;
    void* context = NULL;
    int* poll_width = NULL;
    int* poll_height = NULL;
    bool closed = false;
    bool fullscreen = false;
    NSRect saved_frame = NSZeroRect;
    NSWindowStyleMask saved_style = 0;
    int width = 0;
    int height = 0;
    bool panning = false;
    float pan_x = 0;
    float pan_y = 0;
    float zoom = 1;
    float cursor_x = 0;
    float cursor_y = 0;
    bool turn_ccw = false;
    bool turn_cw = false;
    int keys = 0;
    id<MTLTexture> background_texture = nil;
    const uint8_t* background_uploaded = NULL;
    uint64_t background_uploaded_load = 0;
    id<MTLCommandBuffer> last = nil;
    SyphonServerBase* share = nil;
    id<NSObject> share_activity = nil;
    id<MTLTexture> model_texture = nil;
    id<MTLTexture> share_texture = nil;
    dispatch_source_t share_tick = nil;
    dispatch_semaphore_t share_wake = nil;
    bool share_off = false;
};

static void backing_size(window* w, int* width, int* height)
{
    NSSize size = [w->view convertSizeToBacking:w->view.bounds.size];
    *width = (int)size.width;
    *height = (int)size.height;
}

static void window_mouse(window* w, NSEvent* event)
{
    NSSize size = w->view.bounds.size;
    NSPoint at = [w->view convertPoint:event.locationInWindow fromView:nil];
    if (size.width <= 0 || size.height <= 0)
    {
        return;
    }
    if (event.type == NSEventTypeLeftMouseDown || event.type == NSEventTypeLeftMouseUp)
    {
        w->panning = event.type == NSEventTypeLeftMouseDown && NSPointInRect(at, w->view.bounds);
        return;
    }
    if (event.type == NSEventTypeLeftMouseDragged)
    {
        if (w->panning)
        {
            w->pan_x += event.deltaX / size.width;
            w->pan_y -= event.deltaY / size.height;
        }
        return;
    }
    if (event.type == NSEventTypeScrollWheel)
    {
        w->zoom *= expf(event.scrollingDeltaY * (event.hasPreciseScrollingDeltas ? 0.005f : 0.1f));
    }
    else
    {
        w->zoom *= 1 + event.magnification;
    }
    w->cursor_x = 2 * at.x / size.width - 1;
    w->cursor_y = 2 * at.y / size.height - 1;
}

#include "window_input.inc"

static void toggle_fullscreen(window* w)
{
    if ((w->ns.styleMask & NSWindowStyleMaskFullScreen) != 0)
    {
        [w->ns toggleFullScreen:nil];
        return;
    }
    if (w->fullscreen)
    {
        [w->ns setStyleMask:w->saved_style];
        [w->ns setFrame:w->saved_frame display:YES];
        [NSApp setPresentationOptions:NSApplicationPresentationDefault];
        [w->ns makeKeyAndOrderFront:nil];
        w->fullscreen = false;
        return;
    }
    NSScreen* screen = w->ns.screen != nil ? w->ns.screen : [NSScreen mainScreen];
    w->saved_style = w->ns.styleMask;
    w->saved_frame = w->ns.frame;
    [NSApp setPresentationOptions:NSApplicationPresentationHideDock | NSApplicationPresentationHideMenuBar];
    [w->ns setStyleMask:NSWindowStyleMaskBorderless];
    [w->ns setFrame:screen.frame display:YES];
    [w->ns makeKeyAndOrderFront:nil];
    w->fullscreen = true;
}

window* window_open(const char* title, int width, int height)
{
    @autoreleasepool
    {
        if (![NSThread isMainThread] || width <= 0 || height <= 0)
        {
            return NULL;
        }
        static bool app_started = false;
        if (!app_started)
        {
            [NSApplication sharedApplication];
            [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
            [NSApp finishLaunching];
            app_started = true;
        }
        window* w = new window();
        w->ns = [[hikari_window alloc] initWithContentRect:NSMakeRect(0, 0, width, height) styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskMiniaturizable | NSWindowStyleMaskResizable backing:NSBackingStoreBuffered defer:NO];
        if (w->ns == nil)
        {
            delete w;
            return NULL;
        }
        w->ns.releasedWhenClosed = NO;
        w->ns.title = [NSString stringWithUTF8String:title];
        w->ns.collectionBehavior = NSWindowCollectionBehaviorFullScreenPrimary;
        w->layer = [CAMetalLayer layer];
        w->layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
        w->layer.framebufferOnly = YES;
        w->layer.maximumDrawableCount = 2;
        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        w->layer.colorspace = space;
        CGColorSpaceRelease(space);
        w->view = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, width, height)];
        w->view.layer = w->layer;
        w->view.wantsLayer = YES;
        w->ns.contentView = w->view;
        [w->ns center];
        [w->ns makeKeyAndOrderFront:nil];
        [NSApp activateIgnoringOtherApps:YES];
        hikari_window* ns = w->ns;
        w->close_observer = [[NSNotificationCenter defaultCenter] addObserverForName:NSWindowWillCloseNotification object:ns queue:nil usingBlock:^(NSNotification* note) {
            w->closed = true;
        }];
        w->resign_observer = [[NSNotificationCenter defaultCenter] addObserverForName:NSWindowDidResignKeyNotification object:ns queue:nil usingBlock:^(NSNotification* note) {
            w->turn_ccw = false;
            w->turn_cw = false;
        }];
        NSEventMask mouse = NSEventMaskLeftMouseDown | NSEventMaskLeftMouseUp | NSEventMaskLeftMouseDragged | NSEventMaskScrollWheel | NSEventMaskMagnify;
        w->monitor = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskKeyDown | NSEventMaskKeyUp | mouse handler:^NSEvent*(NSEvent* event) {
            if (event.window != ns)
            {
                return event;
            }
            if (event.type != NSEventTypeKeyDown && event.type != NSEventTypeKeyUp)
            {
                window_mouse(w, event);
                return event;
            }
            if (event.type == NSEventTypeKeyUp)
            {
                if (event.keyCode == 12 || event.keyCode == 14)
                {
                    w->keys |= event.keyCode == 12 ? 4 : 8;
                    return nil;
                }
                return event;
            }
            if (event.keyCode == 12 && (event.modifierFlags & NSEventModifierFlagCommand) != 0)
            {
                ns.close_requested = YES;
                return nil;
            }
            if (event.keyCode == 12 || event.keyCode == 14)
            {
                (event.keyCode == 12 ? w->turn_ccw : w->turn_cw) = true;
                w->keys &= event.keyCode == 12 ? ~4 : ~8;
                return nil;
            }
            if (event.type == NSEventTypeKeyDown && (event.keyCode == 49 || (event.keyCode == 1 && (event.modifierFlags & (NSEventModifierFlagCommand | NSEventModifierFlagControl)) != 0)))
            {
                w->keys |= event.keyCode == 49 ? 1 : 2;
                return nil;
            }
            bool option = (event.modifierFlags & NSEventModifierFlagOption) != 0;
            if ((event.keyCode == 36 || event.keyCode == 76) && option)
            {
                toggle_fullscreen(w);
                return nil;
            }
            if (event.keyCode == 53)
            {
                if (w->fullscreen || (ns.styleMask & NSWindowStyleMaskFullScreen) != 0)
                {
                    toggle_fullscreen(w);
                }
                else
                {
                    ns.close_requested = YES;
                }
                return nil;
            }
            return event;
        }];
        w->timer = [NSTimer timerWithTimeInterval:0.001 repeats:YES block:^(NSTimer* timer) {
            if (w->frame != NULL)
            {
                backing_size(w, w->poll_width, w->poll_height);
                w->frame(w->context);
            }
        }];
        [[NSRunLoop currentRunLoop] addTimer:w->timer forMode:NSEventTrackingRunLoopMode];
        return w;
    }
}

bool window_poll(window* w, int* width, int* height, engine_hook frame, void* context)
{
    @autoreleasepool
    {
        w->frame = frame;
        w->context = context;
        w->poll_width = width;
        w->poll_height = height;
        NSEvent* event = nil;
        while ((event = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:nil inMode:NSDefaultRunLoopMode dequeue:YES]) != nil)
        {
            [NSApp sendEvent:event];
        }
        w->frame = NULL;
        backing_size(w, width, height);
        return !w->closed && !w->ns.close_requested;
    }
}

int window_ask_save(window* w, const char* model)
{
    @autoreleasepool
    {
        if (w->closed)
        {
            return 0;
        }
        NSAlert* alert = [[NSAlert alloc] init];
        alert.messageText = [NSString stringWithFormat:@"Save changes to %s's view?", model];
        [alert addButtonWithTitle:@"Save"];
        [alert addButtonWithTitle:@"Don't Save"];
        [alert addButtonWithTitle:@"Cancel"];
        NSModalResponse response = [alert runModal];
        w->ns.close_requested = NO;
        [w->ns makeKeyAndOrderFront:nil];
        if (response == NSAlertFirstButtonReturn)
        {
            return 1;
        }
        if (response == NSAlertSecondButtonReturn)
        {
            return 0;
        }
        return -1;
    }
}

static void share_stop(window* w)
{
    if (w->share_tick != nil)
    {
        dispatch_source_cancel(w->share_tick);
        w->share_tick = nil;
    }
    if (w->share != nil)
    {
        [w->share stop];
        w->share = nil;
    }
    if (w->share_activity != nil)
    {
        [[NSProcessInfo processInfo] endActivity:w->share_activity];
        w->share_activity = nil;
    }
    w->model_texture = nil;
    w->share_texture = nil;
    w->share_off = true;
}

static bool share_ready(window* w, int width, int height)
{
    if (w->share == nil)
    {
        w->share = [[SyphonServerBase alloc] initWithName:@"hikari" options:nil];
        if (w->share != nil)
        {
            w->share_activity = [[NSProcessInfo processInfo] beginActivityWithOptions:NSActivityUserInitiatedAllowingIdleSystemSleep reason:@"hikari syphon"];
        }
    }
    if (w->share != nil && (w->model_texture == nil || (int)w->model_texture.width != width || (int)w->model_texture.height != height))
    {
        MTLTextureDescriptor* descriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:width height:height mipmapped:NO];
        descriptor.usage = MTLTextureUsageRenderTarget;
        IOSurfaceRef surface = [w->share newSurfaceForWidth:width height:height options:nil];
        w->share_texture = surface != NULL ? [device newTextureWithDescriptor:descriptor iosurface:surface plane:0] : nil;
        if (surface != NULL)
        {
            CFRelease(surface);
        }
        descriptor.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead;
        descriptor.storageMode = MTLStorageModePrivate;
        w->model_texture = [device newTextureWithDescriptor:descriptor];
    }
    if (w->share == nil || w->share_texture == nil || w->model_texture == nil)
    {
        NSLog(@"engine: syphon share failed, drawing the window only");
        share_stop(w);
        return false;
    }
    return true;
}

void window_close(window* w)
{
    @autoreleasepool
    {
        if (w->last != nil)
        {
            [w->last waitUntilCompleted];
        }
        [w->timer invalidate];
        share_stop(w);
        [NSEvent removeMonitor:w->monitor];
        [[NSNotificationCenter defaultCenter] removeObserver:w->close_observer];
        [[NSNotificationCenter defaultCenter] removeObserver:w->resign_observer];
        if (w->fullscreen)
        {
            [NSApp setPresentationOptions:NSApplicationPresentationDefault];
        }
        if (!w->closed)
        {
            [w->ns close];
        }
        delete w;
        NSEvent* event = nil;
        while ((event = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:nil inMode:NSDefaultRunLoopMode dequeue:YES]) != nil)
        {
            [NSApp sendEvent:event];
        }
    }
}

bool engine_present(engine* e, window* w, float background_clock_ms, float frame_seconds, bool share)
{
    @autoreleasepool
    {
        NSSize size = [w->view convertSizeToBacking:w->view.bounds.size];
        int width = (int)size.width;
        int height = (int)size.height;
        if (w->closed || width <= 0 || height <= 0 || device == nil)
        {
            return false;
        }
        share = share && !w->share_off;
        bool hidden = w->ns.miniaturized || (w->ns.occlusionState & NSWindowOcclusionStateVisible) == 0;
        if (hidden && !share)
        {
            [NSThread sleepForTimeInterval:frame_seconds];
            return false;
        }
        if (hidden && w->share_tick == nil)
        {
            dispatch_semaphore_t wake = dispatch_semaphore_create(0);
            w->share_wake = wake;
            w->share_tick = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, DISPATCH_TIMER_STRICT, dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0));
            dispatch_source_set_event_handler(w->share_tick, ^{
                dispatch_semaphore_signal(wake);
            });
            dispatch_source_set_timer(w->share_tick, DISPATCH_TIME_NOW, (uint64_t)(frame_seconds * 1e9), 0);
            dispatch_resume(w->share_tick);
        }
        if (!hidden && w->share_tick != nil)
        {
            dispatch_source_cancel(w->share_tick);
            w->share_tick = nil;
        }
        if (hidden)
        {
            dispatch_semaphore_wait(w->share_wake, DISPATCH_TIME_FOREVER);
        }
        if (w->last != nil)
        {
            [w->last waitUntilCompleted];
            w->last = nil;
        }
        w->layer.device = device;
        w->layer.displaySyncEnabled = YES;
        if (w->width != width || w->height != height)
        {
            w->layer.drawableSize = CGSizeMake(width, height);
            w->width = width;
            w->height = height;
        }
        int stream_width = e->width;
        int stream_height = e->height;
        e->width = width;
        e->height = height;
        view_matrix(e);
        CubismMatrix44 mvp = e->mvp;
        e->width = stream_width;
        e->height = stream_height;
        view_matrix(e);
        const uint8_t* back = background_at(e, background_clock_ms);
        if (back != NULL)
        {
            if (w->background_texture == nil || (int)w->background_texture.width != e->width || (int)w->background_texture.height != e->height)
            {
                MTLTextureDescriptor* descriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatRGBA8Unorm width:e->width height:e->height mipmapped:NO];
                descriptor.storageMode = MTLStorageModeShared;
                descriptor.usage = MTLTextureUsageShaderRead;
                w->background_texture = [device newTextureWithDescriptor:descriptor];
                w->background_uploaded = NULL;
            }
            if (back != w->background_uploaded || e->background_serial != w->background_uploaded_load)
            {
                [w->background_texture replaceRegion:MTLRegionMake2D(0, 0, e->width, e->height) mipmapLevel:0 withBytes:back bytesPerRow:e->width * 4];
                w->background_uploaded = back;
                w->background_uploaded_load = e->background_serial;
            }
        }
        update_shown(e);
        id<MTLCommandBuffer> command_buffer = [queue commandBuffer];
        id<CAMetalDrawable> drawable = hidden ? nil : [w->layer nextDrawable];
        if (share && share_ready(w, width, height))
        {
            metal_renderer_draw(e->own, command_buffer, w->model_texture, mvp, nil, false);
            if (metal_renderer_share(command_buffer, w->model_texture, back != NULL ? w->background_texture : nil, drawable.texture, w->share_texture))
            {
                SyphonServerBase* server = w->share;
                [command_buffer addCompletedHandler:^(id<MTLCommandBuffer> done) {
                    [server publish];
                }];
                if (drawable != nil)
                {
                    [command_buffer presentDrawable:drawable afterMinimumDuration:frame_seconds];
                }
                [command_buffer commit];
                w->last = command_buffer;
                return true;
            }
            share_stop(w);
        }
        if (drawable == nil)
        {
            [command_buffer commit];
            w->last = command_buffer;
            return false;
        }
        metal_renderer_draw(e->own, command_buffer, drawable.texture, mvp, back != NULL ? w->background_texture : nil, true);
        [command_buffer presentDrawable:drawable afterMinimumDuration:frame_seconds];
        [command_buffer commit];
        w->last = command_buffer;
        return true;
    }
}
#else
window* window_open(const char* title, int width, int height)
{
    return NULL;
}

bool window_poll(window* w, int* width, int* height, engine_hook frame, void* context)
{
    return false;
}

void window_close(window* w)
{
}

bool engine_present(engine* e, window* w, float background_clock_ms, float frame_seconds, bool share)
{
    return false;
}
#endif
