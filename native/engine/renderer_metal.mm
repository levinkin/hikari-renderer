#import <Metal/Metal.h>
#include <simd/simd.h>
#include <algorithm>
#include <cmath>
#include <string>
#include <unordered_map>
#include <vector>
#include "CubismFramework.hpp"
#include "Model/CubismModel.hpp"
#include "Math/CubismMatrix44.hpp"
#include "Rendering/CubismRenderer.hpp"
#include "Rendering/CubismClippingManager.hpp"
#include "Rendering/Metal/Shaders/MetalShaderTypes.h"

using namespace Live2D::Cubism::Framework;
using namespace Live2D::Cubism::Framework::Rendering;

static const int kSamples = 4;

static const char under_shader[] = R"(
struct under_out
{
    float4 position [[position]];
};
vertex under_out under_vertex(uint id [[vertex_id]])
{
    under_out out;
    out.position = float4(float2((id << 1) & 2, id & 2) * 2.0 - 1.0, 0.0, 1.0);
    return out;
}
float3 cover(texture2d<float> back, float2 position, float4 size)
{
    constexpr sampler linear(filter::linear, address::clamp_to_edge);
    if (size.z <= 0.0)
    {
        return float3(0.0);
    }
    float2 back_size = float2(back.get_width(), back.get_height());
    float scale = max(size.x / back_size.x, size.y / back_size.y);
    return back.sample(linear, ((position - size.xy * 0.5) / scale + back_size * 0.5) / back_size).rgb;
}
fragment float4 under_fragment(under_out in [[stage_in]], texture2d<float> back [[texture(0)]], constant float4& size [[buffer(0)]])
{
    return float4(cover(back, in.position.xy, size), 1.0);
}
struct share_out
{
    float4 window [[color(0)]];
    float4 share [[color(1)]];
};
fragment share_out share_fragment(under_out in [[stage_in]], texture2d<float> model [[texture(0)]], texture2d<float> back [[texture(1)]], constant float4& size [[buffer(0)]])
{
    float4 color = model.read(uint2(in.position.xy));
    share_out out;
    out.window = float4(color.rgb + cover(back, in.position.xy, size) * (1.0 - color.a), 1.0);
    out.share = color.a > 0.0 ? float4(color.rgb / color.a, color.a) : float4(0.0);
    return out;
}
)";

static const char blend_functions[] = {
#embed "blend_shaders.metal"
    , 0};

static const char layer_shader[] = R"(
struct layer_uniforms
{
    float4x4 matrix;
    float4x4 clip_matrix;
    float4 channel;
    float4 base;
    float4 multiply;
    float4 screen;
    float4 tile;
};
struct layer_in
{
    float4 position [[position]];
    float2 uv;
    float4 clip;
};
struct layer_out
{
    float4 color [[color(TARGET)]];
};
vertex layer_in layer_vertex(uint id [[vertex_id]], constant float2* positions [[buffer(0)]], constant float2* uvs [[buffer(1)]], constant layer_uniforms& u [[buffer(2)]])
{
    layer_in out;
    float4 position = float4(positions[id], 0.0, 1.0);
    out.position = u.matrix * position;
    out.uv = float2(uvs[id].x, 1.0 - uvs[id].y);
    out.clip = u.clip_matrix * position;
    return out;
}
fragment layer_out clear_fragment()
{
    layer_out out;
    out.color = float4(0.0);
    return out;
}
fragment layer_out layer_fragment(layer_in in [[stage_in]], texture2d<float> texture [[texture(0)]], texture2d<float> mask [[texture(1)]], sampler texture_sampler [[sampler(0)]], sampler mask_sampler [[sampler(1)]], constant layer_uniforms& u [[buffer(2)]]
#if SOURCE > 0
    , float4 source [[color(SOURCE)]]
#endif
#if BLEND
    , float4 destination [[color(TARGET)]]
#endif
    )
{
#if SOURCE > 0
    float4 color = source;
#else
    float4 color = texture.sample(texture_sampler, in.uv);
#endif
    color.rgb = color.rgb * u.multiply.rgb;
    color.rgb = (color.rgb + u.screen.rgb * color.a) - (color.rgb * u.screen.rgb);
    color = color * u.base;
#if MASK > 0
    float2 at = in.clip.xy / in.clip.w;
    float4 covered = (1.0 - mask.sample(mask_sampler, float2(at.x, (u.tile.x + 1.0 - at.y) / u.tile.y))) * u.channel;
    float value = covered.r + covered.g + covered.b + covered.a;
    color = color * (MASK == 2 ? 1.0 - value : value);
#endif
    layer_out out;
#if BLEND
    float4 straight = ConvertPremultipliedToStraight(color);
    float4 behind = ConvertPremultipliedToStraight(destination);
    out.color = AlphaBlend(ColorBlend(straight.rgb, behind.rgb), straight, behind);
#else
    out.color = color;
#endif
    return out;
}
)";

struct layer_uniforms
{
    CubismShaderUniforms shared;
    simd::float4 tile;
};

static const float quad[8] = {-1.0f, -1.0f, 1.0f, -1.0f, -1.0f, 1.0f, 1.0f, 1.0f};
static const MTLBlendFactor fixed_blends[5][4] = {
    {MTLBlendFactorOne, MTLBlendFactorOneMinusSourceAlpha, MTLBlendFactorOne, MTLBlendFactorOneMinusSourceAlpha},
    {MTLBlendFactorOne, MTLBlendFactorOne, MTLBlendFactorZero, MTLBlendFactorOne},
    {MTLBlendFactorDestinationColor, MTLBlendFactorOneMinusSourceAlpha, MTLBlendFactorZero, MTLBlendFactorOne},
    {MTLBlendFactorOneMinusDestinationAlpha, MTLBlendFactorOne, MTLBlendFactorOneMinusDestinationAlpha, MTLBlendFactorOne},
    {MTLBlendFactorZero, MTLBlendFactorOneMinusSourceColor, MTLBlendFactorZero, MTLBlendFactorOneMinusSourceAlpha},
};

namespace Live2D { namespace Cubism { namespace Framework { namespace Rendering {

CubismRenderer* CubismRenderer::Create(csmUint32 width, csmUint32 height)
{
    return NULL;
}

void CubismRenderer::StaticRelease()
{
}

}}}}

struct mask_target
{
};

struct mask_context;

typedef CubismClippingManager<mask_context, mask_target> mask_manager_base;

struct mask_context : public CubismClippingContext
{
    mask_manager_base* owner;
    uint32_t tile = 0;
    mask_context(mask_manager_base* manager, CubismModel& model, const csmInt32* indices, csmInt32 count) : CubismClippingContext(indices, count), owner(manager)
    {
    }
    mask_manager_base* GetClippingManager()
    {
        return owner;
    }
};

struct mask_manager : public mask_manager_base
{
    int layout(CubismModel& model, CubismRenderer::DrawableObjectType type, bool high_precision, const CubismMatrix44& mvp)
    {
        if (high_precision)
        {
            SetupMatrixForHighPrecision(model, false, type, mvp);
            return 1;
        }
        int using_count = 0;
        for (csmUint32 i = 0; i < _clippingContextListForMask.GetSize(); i++)
        {
            CalcClippedTotalBounds(model, _clippingContextListForMask[i], type);
            using_count += _clippingContextListForMask[i]->_isUsing ? 1 : 0;
        }
        if (using_count == 0)
        {
            return 0;
        }
        SetupLayoutBounds(using_count);
        for (csmUint32 i = 0; i < _clippingContextListForMask.GetSize(); i++)
        {
            mask_context* context = _clippingContextListForMask[i];
            csmRectF* all = context->_allClippedDrawRect;
            _tmpBoundsOnModel.SetRect(all);
            _tmpBoundsOnModel.Expand(all->Width * 0.05f, all->Height * 0.05f);
            CreateMatrixForMask(false, context->_layoutBounds, context->_layoutBounds->Width / _tmpBoundsOnModel.Width, context->_layoutBounds->Height / _tmpBoundsOnModel.Height);
            context->_matrixForMask.SetMatrix(_tmpMatrixForMask.GetArray());
            context->_matrixForDraw.SetMatrix(_tmpMatrixForDraw.GetArray());
        }
        return using_count;
    }
    csmVector<mask_context*>& mask_list()
    {
        return _clippingContextListForMask;
    }
};

struct metal_renderer
{
    CubismModel* model = NULL;
    std::vector<id<MTLTexture>> textures;
    id<MTLBuffer> uvs = nil;
    id<MTLBuffer> indices = nil;
    id<MTLBuffer> positions = nil;
    std::vector<size_t> vertex_offset;
    std::vector<size_t> index_offset;
    std::vector<int> sorted;
    mask_manager* masks = NULL;
    mask_manager* offscreen_masks = NULL;
    id<MTLTexture> mask_texture = nil;
    id<MTLTexture> msaa = nil;
    bool blend = false;
    uint32_t mask_tiles = 1;
    uint32_t mask_side = 0;
    std::vector<id<MTLTexture>> layers;
    std::vector<int> offscreen_parent;
    std::vector<int> offscreen_layer;
    std::vector<int> drawable_owner;
    std::vector<id<MTLRenderPipelineState>> drawable_state;
    std::vector<id<MTLRenderPipelineState>> offscreen_state;
    std::vector<id<MTLRenderPipelineState>> offscreen_clear;
    id<MTLRenderPipelineState> under = nil;
    MTLRenderPassDescriptor* mask_pass = nil;
    MTLRenderPassDescriptor* pass = nil;
    int current = -1;
};

static id<MTLDevice> renderer_device = nil;
static id<MTLLibrary> renderer_library = nil;
static id<MTLRenderPipelineState> draw_pipelines[9];
static id<MTLRenderPipelineState> mask_pipeline = nil;
static id<MTLRenderPipelineState> share_pipelines[2];
static id<MTLSamplerState> texture_sampler = nil;
static id<MTLSamplerState> mask_sampler = nil;
static bool fetch_supported = false;
static uint32_t max_texture_side = 8192;
static const int max_attachments = 8;

static simd::float4x4 simd_matrix(CubismMatrix44 matrix)
{
    const float* m = matrix.GetArray();
    return simd::float4x4(simd::float4{m[0], m[1], m[2], m[3]}, simd::float4{m[4], m[5], m[6], m[7]}, simd::float4{m[8], m[9], m[10], m[11]}, simd::float4{m[12], m[13], m[14], m[15]});
}

static id<MTLRenderPipelineState> make_pipeline(id<MTLLibrary> library, NSString* vertex, NSString* fragment, int samples, int attachments, int target, const MTLBlendFactor* factors)
{
    MTLRenderPipelineDescriptor* descriptor = [MTLRenderPipelineDescriptor new];
    descriptor.vertexFunction = [library newFunctionWithName:vertex];
    descriptor.fragmentFunction = [library newFunctionWithName:fragment];
    descriptor.rasterSampleCount = samples;
    for (int i = 0; i < attachments; i++)
    {
        descriptor.colorAttachments[i].pixelFormat = MTLPixelFormatBGRA8Unorm;
        descriptor.colorAttachments[i].writeMask = i == target ? MTLColorWriteMaskAll : MTLColorWriteMaskNone;
    }
    descriptor.colorAttachments[target].blendingEnabled = factors != NULL;
    if (factors != NULL)
    {
        descriptor.colorAttachments[target].sourceRGBBlendFactor = factors[0];
        descriptor.colorAttachments[target].destinationRGBBlendFactor = factors[1];
        descriptor.colorAttachments[target].sourceAlphaBlendFactor = factors[2];
        descriptor.colorAttachments[target].destinationAlphaBlendFactor = factors[3];
    }
    NSError* error = nil;
    id<MTLRenderPipelineState> state = [renderer_device newRenderPipelineStateWithDescriptor:descriptor error:&error];
    if (state == nil)
    {
        NSLog(@"engine: renderer pipeline %@ failed %@", fragment, error);
    }
    return state;
}

static id<MTLRenderPipelineState> layer_pipeline(std::unordered_map<uint64_t, id<MTLRenderPipelineState>>& cache, int attachments, int target, int source, int mask, int color, int alpha, int fixed)
{
    color = std::clamp(color, 0, 15);
    alpha = std::clamp(alpha, 0, 4);
    uint64_t key = (((((uint64_t)attachments * 8 + target) * 8 + source) * 3 + mask) * 16 + color) * 5 + alpha;
    key = key * 6 + (uint64_t)(fixed + 1);
    __strong id<MTLRenderPipelineState>& state = cache[key];
    if (state != nil)
    {
        return state;
    }
    if (fixed == 3)
    {
        state = make_pipeline(renderer_library, @"under_vertex", @"under_fragment", kSamples, attachments, 0, fixed_blends[3]);
        return state;
    }
    MTLCompileOptions* options = [MTLCompileOptions new];
    options.preprocessorMacros = @{@"CSM_COLOR_BLEND_MODE": @(color), @"CSM_ALPHA_BLEND_MODE": @(alpha), @"TARGET": @(target), @"SOURCE": @(source), @"MASK": @(mask), @"BLEND": @(fixed < 0 ? 1 : 0)};
    NSError* error = nil;
    id<MTLLibrary> library = [renderer_device newLibraryWithSource:[NSString stringWithUTF8String:(std::string(blend_functions) + layer_shader).c_str()] options:options error:&error];
    if (library == nil)
    {
        NSLog(@"engine: layer shader compile failed %@", error);
        return nil;
    }
    state = make_pipeline(library, @"layer_vertex", fixed == 4 ? @"clear_fragment" : @"layer_fragment", kSamples, attachments, target, fixed >= 0 && fixed < 3 ? fixed_blends[fixed] : NULL);
    return state;
}

bool metal_renderer_start(id<MTLDevice> device, const std::string& shader_source)
{
    if (renderer_device != nil)
    {
        return true;
    }
    NSError* error = nil;
    id<MTLLibrary> library = [device newLibraryWithSource:[NSString stringWithUTF8String:(shader_source + under_shader).c_str()] options:nil error:&error];
    if (library == nil)
    {
        NSLog(@"engine: renderer shader compile failed %@", error);
        return false;
    }
    renderer_device = device;
    renderer_library = library;
    fetch_supported = [device supportsFamily:MTLGPUFamilyApple1];
    max_texture_side = [device supportsFamily:MTLGPUFamilyApple3] || [device supportsFamily:MTLGPUFamilyMac2] ? 16384 : 8192;
    NSString* fragments[3] = {@"FragShaderSrcPremultipliedAlpha", @"FragShaderSrcMaskPremultipliedAlpha", @"FragShaderSrcMaskInvertedPremultipliedAlpha"};
    for (int blend = 0; blend < 3; blend++)
    {
        for (int masked = 0; masked < 3; masked++)
        {
            draw_pipelines[blend * 3 + masked] = make_pipeline(library, masked == 0 ? @"VertShaderSrc" : @"VertShaderSrcMasked", fragments[masked], kSamples, 1, 0, fixed_blends[blend]);
        }
    }
    mask_pipeline = make_pipeline(library, @"VertShaderSrcSetupMask", @"FragShaderSrcSetupMask", 1, 1, 0, fixed_blends[4]);
    MTLSamplerDescriptor* sampler = [MTLSamplerDescriptor new];
    sampler.rAddressMode = MTLSamplerAddressModeRepeat;
    sampler.sAddressMode = MTLSamplerAddressModeRepeat;
    sampler.tAddressMode = MTLSamplerAddressModeRepeat;
    sampler.minFilter = MTLSamplerMinMagFilterLinear;
    sampler.magFilter = MTLSamplerMinMagFilterLinear;
    sampler.mipFilter = MTLSamplerMipFilterLinear;
    texture_sampler = [device newSamplerStateWithDescriptor:sampler];
    sampler.minFilter = MTLSamplerMinMagFilterNearest;
    sampler.magFilter = MTLSamplerMinMagFilterNearest;
    sampler.mipFilter = MTLSamplerMipFilterNotMipmapped;
    mask_sampler = [device newSamplerStateWithDescriptor:sampler];
    for (id<MTLRenderPipelineState> state : draw_pipelines)
    {
        if (state == nil)
        {
            return false;
        }
    }
    return mask_pipeline != nil;
}

static int owning_offscreen(CubismModel* model, csmInt32 part)
{
    for (; part != CubismModel::CubismNoIndex_Parent; part = model->GetPartParentPartIndex(part))
    {
        for (csmInt32 j = 0; j < model->GetOffscreenCount(); j++)
        {
            if (model->GetOffscreenOwnerIndices()[j] == part)
            {
                return j;
            }
        }
    }
    return -1;
}

static int fixed_blend(const csmBlendMode& mode)
{
    if (mode.GetColorBlendType() == Live2D::Cubism::Core::csmColorBlendType_AddCompatible)
    {
        return 1;
    }
    if (mode.GetColorBlendType() == Live2D::Cubism::Core::csmColorBlendType_MultiplyCompatible)
    {
        return 2;
    }
    return mode.GetColorBlendType() == Live2D::Cubism::Core::csmColorBlendType_Normal && mode.GetAlphaBlendType() == Live2D::Cubism::Core::csmAlphaBlendType_Over ? 0 : -1;
}

static id<MTLRenderPipelineState> mode_pipeline(std::unordered_map<uint64_t, id<MTLRenderPipelineState>>& cache, metal_renderer* r, int owner, int source, int mask, const csmBlendMode& mode)
{
    int target = owner >= 0 ? r->offscreen_layer[owner] : 0;
    return layer_pipeline(cache, (int)r->layers.size() + 1, target, source, mask, mode.GetColorBlendType() - 2, mode.GetAlphaBlendType(), fixed_blend(mode));
}

void metal_renderer_destroy(metal_renderer* r);

static bool compile_layers(metal_renderer* r)
{
    CubismModel* model = r->model;
    std::unordered_map<uint64_t, id<MTLRenderPipelineState>> cache;
    int attachments = (int)r->layers.size() + 1;
    r->under = layer_pipeline(cache, attachments, 0, 0, 0, 0, 0, 3);
    bool ok = r->under != nil;
    for (csmInt32 i = 0; r->blend && i < model->GetDrawableCount(); i++)
    {
        mask_context* clip = r->masks != NULL ? (*r->masks->GetClippingContextListForDraw())[i] : NULL;
        r->drawable_state.push_back(mode_pipeline(cache, r, r->drawable_owner[i], 0, clip == NULL ? 0 : (model->GetDrawableInvertedMask(i) ? 2 : 1), model->GetDrawableBlendModeType(i)));
        ok = r->drawable_state.back() != nil && ok;
    }
    for (csmInt32 i = 0; r->blend && i < model->GetOffscreenCount(); i++)
    {
        mask_context* clip = r->offscreen_masks != NULL ? (*r->offscreen_masks->GetClippingContextListForOffscreen())[i] : NULL;
        r->offscreen_state.push_back(mode_pipeline(cache, r, r->offscreen_parent[i], r->offscreen_layer[i], clip == NULL ? 0 : (model->GetOffscreenInvertedMask(i) ? 2 : 1), model->GetOffscreenBlendModeType(i)));
        r->offscreen_clear.push_back(layer_pipeline(cache, attachments, r->offscreen_layer[i], 0, 0, 0, 0, 4));
        ok = r->offscreen_state.back() != nil && r->offscreen_clear.back() != nil && ok;
    }
    return ok;
}

metal_renderer* metal_renderer_create(CubismModel* model, const std::vector<id<MTLTexture>>& textures, const char* name)
{
    metal_renderer* r = new metal_renderer();
    r->model = model;
    r->textures = textures;
    csmInt32 count = model->GetDrawableCount();
    size_t vertices = 0;
    size_t index_total = 0;
    for (csmInt32 i = 0; i < count; i++)
    {
        r->vertex_offset.push_back(vertices);
        r->index_offset.push_back(index_total);
        r->drawable_owner.push_back(owning_offscreen(model, model->GetDrawableParentPartIndex(i)));
        vertices += model->GetDrawableVertexCount(i);
        index_total += model->GetDrawableVertexIndexCount(i);
    }
    r->uvs = [renderer_device newBufferWithLength:std::max<size_t>(vertices, 1) * 8 options:MTLResourceStorageModeShared];
    r->indices = [renderer_device newBufferWithLength:std::max<size_t>(index_total, 1) * 2 options:MTLResourceStorageModeShared];
    for (csmInt32 i = 0; i < count; i++)
    {
        memcpy((uint8_t*)r->uvs.contents + r->vertex_offset[i] * 8, model->GetDrawableVertexUvs(i), model->GetDrawableVertexCount(i) * 8);
        memcpy((uint8_t*)r->indices.contents + r->index_offset[i] * 2, model->GetDrawableVertexIndices(i), model->GetDrawableVertexIndexCount(i) * 2);
    }
    r->positions = [renderer_device newBufferWithLength:std::max<size_t>(vertices, 1) * 8 options:MTLResourceStorageModeShared | MTLResourceCPUCacheModeWriteCombined];
    r->sorted.assign(count + model->GetOffscreenCount(), 0);
    r->blend = model->IsBlendModeEnabled();
    if (model->IsUsingMasking())
    {
        r->masks = new mask_manager();
        r->masks->Initialize(*model, 1, CubismRenderer::DrawableObjectType_Drawable);
    }
    if (model->IsUsingMaskingForOffscreen())
    {
        r->offscreen_masks = new mask_manager();
        r->offscreen_masks->Initialize(*model, 1, CubismRenderer::DrawableObjectType_Offscreen);
    }
    uint32_t tiles = 0;
    for (mask_manager* manager : {r->masks, r->offscreen_masks})
    {
        for (csmUint32 i = 0; manager != NULL && i < manager->mask_list().GetSize(); i++)
        {
            manager->mask_list()[i]->tile = r->blend ? tiles + i : tiles;
        }
        tiles += manager == NULL ? 0 : (r->blend ? manager->mask_list().GetSize() : 1);
    }
    r->mask_tiles = std::max<uint32_t>(tiles, 1);
    csmInt32 offscreens = model->GetOffscreenCount();
    for (csmInt32 i = 0; i < offscreens; i++)
    {
        r->offscreen_parent.push_back(owning_offscreen(model, model->GetPartParentPartIndex(model->GetOffscreenOwnerIndices()[i])));
    }
    int depth = 0;
    for (csmInt32 i = 0; i < offscreens; i++)
    {
        int layer = 1;
        for (int parent = r->offscreen_parent[i]; parent >= 0; parent = r->offscreen_parent[parent])
        {
            layer++;
        }
        r->offscreen_layer.push_back(layer);
        depth = std::max(depth, layer);
    }
    r->layers.resize(r->blend ? depth : 0);
    const char* reason = NULL;
    if (r->blend && !fetch_supported)
    {
        reason = "blend modes need framebuffer fetch, which this gpu lacks";
    }
    else if (r->blend && depth + 1 > max_attachments)
    {
        reason = "offscreen nesting exceeds the color attachment limit";
    }
    else if (r->blend && r->mask_tiles * 256 > max_texture_side)
    {
        reason = "mask atlas exceeds the texture size limit";
    }
    else if (!compile_layers(r))
    {
        reason = "layer pipelines failed to compile";
    }
    if (reason != NULL)
    {
        fprintf(stderr, "engine: model %s unsupported: %s (offscreen depth %d, attachments %d of %d)\n", name, reason, depth, depth + 1, max_attachments);
        metal_renderer_destroy(r);
        return NULL;
    }
    r->mask_pass = [MTLRenderPassDescriptor renderPassDescriptor];
    r->mask_pass.colorAttachments[0].loadAction = MTLLoadActionClear;
    r->mask_pass.colorAttachments[0].storeAction = MTLStoreActionStore;
    r->mask_pass.colorAttachments[0].clearColor = MTLClearColorMake(1, 1, 1, 1);
    r->pass = [MTLRenderPassDescriptor renderPassDescriptor];
    for (int i = 0; i < (int)r->layers.size() + 1; i++)
    {
        r->pass.colorAttachments[i].loadAction = MTLLoadActionClear;
        r->pass.colorAttachments[i].storeAction = i == 0 ? MTLStoreActionMultisampleResolve : MTLStoreActionDontCare;
        r->pass.colorAttachments[i].clearColor = MTLClearColorMake(0, 0, 0, 0);
    }
    return r;
}

void metal_renderer_destroy(metal_renderer* r)
{
    if (r == NULL)
    {
        return;
    }
    delete r->masks;
    delete r->offscreen_masks;
    delete r;
}

static void encode_mesh(metal_renderer* r, id<MTLRenderCommandEncoder> encoder, id<MTLBuffer> positions, csmInt32 index, const void* uniforms, size_t length)
{
    [encoder setCullMode:r->model->GetDrawableCulling(index) != 0 ? MTLCullModeBack : MTLCullModeNone];
    [encoder setVertexBuffer:positions offset:r->vertex_offset[index] * 8 atIndex:MetalVertexInputIndexVertices];
    [encoder setVertexBuffer:r->uvs offset:r->vertex_offset[index] * 8 atIndex:MetalVertexInputUVs];
    [encoder setVertexBytes:uniforms length:length atIndex:MetalVertexInputIndexUniforms];
    [encoder setFragmentBytes:uniforms length:length atIndex:MetalVertexInputIndexUniforms];
    [encoder setFragmentTexture:r->textures[r->model->GetDrawableTextureIndex(index)] atIndex:0];
    [encoder drawIndexedPrimitives:MTLPrimitiveTypeTriangle indexCount:r->model->GetDrawableVertexIndexCount(index) indexType:MTLIndexTypeUInt16 indexBuffer:r->indices indexBufferOffset:r->index_offset[index] * 2];
}

static void set_channel(mask_manager* manager, mask_context* context, CubismShaderUniforms& uniforms)
{
    CubismRenderer::CubismTextureColor* channel = manager->GetChannelFlagAsColor(context->_layoutChannelIndex);
    uniforms.channelFlag = (vector_float4){channel->R, channel->G, channel->B, channel->A};
}

static void set_colors(CubismShaderUniforms& uniforms, float opacity, const CubismRenderer::CubismTextureColor& multiply, const CubismRenderer::CubismTextureColor& screen)
{
    uniforms.baseColor = (vector_float4){opacity, opacity, opacity, opacity};
    uniforms.multiplyColor = (vector_float4){multiply.R, multiply.G, multiply.B, multiply.A};
    uniforms.screenColor = (vector_float4){screen.R, screen.G, screen.B, screen.A};
}

static void size_masks(metal_renderer* renderer, id<MTLTexture> target, CubismMatrix44 projection)
{
    if (renderer->masks == NULL && renderer->offscreen_masks == NULL)
    {
        return;
    }
    CubismModel* model = renderer->model;
    const float* mvp = projection.GetArray();
    uint32_t side = 256;
    if (renderer->blend)
    {
        while (side < std::max(target.width, target.height) && side < 2048)
        {
            side *= 2;
        }
        while (side > 256 && ((uint64_t)renderer->mask_tiles * side * side * 4 > (64ull << 20) || renderer->mask_tiles * side > max_texture_side))
        {
            side /= 2;
        }
    }
    else
    {
        float half_width = target.width * 0.5f;
        float half_height = target.height * 0.5f;
        float canvas_side = std::min(std::max(std::hypot(mvp[0] * half_width, mvp[1] * half_height) * model->GetCanvasWidth(), std::hypot(mvp[4] * half_width, mvp[5] * half_height) * model->GetCanvasHeight()), 4.0f * std::max(half_width, half_height));
        while (side < std::ceil(canvas_side) && side < 4096)
        {
            side *= 2;
        }
    }
    if (side == renderer->mask_side)
    {
        return;
    }
    renderer->mask_side = side;
    for (mask_manager* manager : {renderer->masks, renderer->offscreen_masks})
    {
        if (manager != NULL && !renderer->blend)
        {
            manager->SetClippingMaskBufferSize(side, side);
        }
    }
    MTLTextureDescriptor* descriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:side height:renderer->blend ? side * renderer->mask_tiles : side mipmapped:NO];
    descriptor.storageMode = MTLStorageModePrivate;
    descriptor.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead;
    renderer->mask_texture = [renderer_device newTextureWithDescriptor:descriptor];
    renderer->mask_pass.colorAttachments[0].texture = renderer->mask_texture;
}

static void draw_masks(metal_renderer* r, id<MTLCommandBuffer> command_buffer, id<MTLBuffer> positions, const CubismMatrix44& mvp)
{
    id<MTLRenderCommandEncoder> encoder = nil;
    for (mask_manager* manager : {r->masks, r->offscreen_masks})
    {
        CubismRenderer::DrawableObjectType type = manager == r->offscreen_masks ? CubismRenderer::DrawableObjectType_Offscreen : CubismRenderer::DrawableObjectType_Drawable;
        if (manager == NULL || manager->layout(*r->model, type, r->blend, type == CubismRenderer::DrawableObjectType_Offscreen ? mvp : CubismMatrix44()) == 0)
        {
            continue;
        }
        if (encoder == nil)
        {
            encoder = [command_buffer renderCommandEncoderWithDescriptor:r->mask_pass];
            [encoder setRenderPipelineState:mask_pipeline];
            [encoder setFrontFacingWinding:MTLWindingCounterClockwise];
            [encoder setFragmentSamplerState:texture_sampler atIndex:0];
        }
        for (csmUint32 c = 0; c < manager->mask_list().GetSize(); c++)
        {
            mask_context* context = manager->mask_list()[c];
            if (r->blend && !context->_isUsing)
            {
                continue;
            }
            if (r->blend)
            {
                [encoder setViewport:(MTLViewport){0, (double)(context->tile * r->mask_side), (double)r->mask_side, (double)r->mask_side, 0, 1}];
            }
            CubismShaderUniforms uniforms = {};
            set_channel(manager, context, uniforms);
            uniforms.clipMatrix = simd_matrix(context->_matrixForMask);
            csmRectF* rect = context->_layoutBounds;
            uniforms.baseColor = (vector_float4){rect->X * 2.0f - 1.0f, rect->Y * 2.0f - 1.0f, rect->GetRight() * 2.0f - 1.0f, rect->GetBottom() * 2.0f - 1.0f};
            for (csmInt32 i = 0; i < context->_clippingIdCount; i++)
            {
                csmInt32 index = context->_clippingIdList[i];
                if (!r->model->GetDrawableDynamicFlagVertexPositionsDidChange(index) || r->model->GetDrawableVertexIndexCount(index) <= 0 || r->model->GetDrawableVertexCount(index) <= 0 || r->textures[r->model->GetDrawableTextureIndex(index)] == nil)
                {
                    continue;
                }
                encode_mesh(r, encoder, positions, index, &uniforms, sizeof(uniforms));
            }
        }
    }
    [encoder endEncoding];
}

id<MTLTexture> metal_renderer_msaa(metal_renderer* r, int width, int height)
{
    if (r->msaa == nil || (int)r->msaa.width != width || (int)r->msaa.height != height)
    {
        MTLTextureDescriptor* descriptor = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:width height:height mipmapped:NO];
        descriptor.textureType = MTLTextureType2DMultisample;
        descriptor.sampleCount = kSamples;
        descriptor.storageMode = fetch_supported ? MTLStorageModeMemoryless : MTLStorageModePrivate;
        descriptor.usage = MTLTextureUsageRenderTarget;
        r->msaa = [renderer_device newTextureWithDescriptor:descriptor];
        for (__strong id<MTLTexture>& layer : r->layers)
        {
            layer = [renderer_device newTextureWithDescriptor:descriptor];
        }
    }
    return r->msaa;
}

static void draw_quad(id<MTLRenderCommandEncoder> encoder, const layer_uniforms& uniforms)
{
    [encoder setVertexBytes:quad length:sizeof(quad) atIndex:MetalVertexInputIndexVertices];
    [encoder setVertexBytes:quad length:sizeof(quad) atIndex:MetalVertexInputUVs];
    [encoder setVertexBytes:&uniforms length:sizeof(uniforms) atIndex:MetalVertexInputIndexUniforms];
    [encoder setFragmentBytes:&uniforms length:sizeof(uniforms) atIndex:MetalVertexInputIndexUniforms];
    [encoder drawPrimitives:MTLPrimitiveTypeTriangleStrip vertexStart:0 vertexCount:4];
}

static void set_clip(metal_renderer* r, layer_uniforms& uniforms, mask_manager* manager, mask_context* clip)
{
    set_channel(manager, clip, uniforms.shared);
    uniforms.shared.clipMatrix = simd_matrix(clip->_matrixForDraw);
    uniforms.tile = (simd::float4){(float)clip->tile, (float)r->mask_tiles, 0.0f, 0.0f};
}

static void draw_offscreen(metal_renderer* r, id<MTLRenderCommandEncoder> encoder, int offscreen)
{
    CubismModel* model = r->model;
    r->current = r->offscreen_parent[offscreen];
    mask_context* clip = r->offscreen_masks != NULL ? (*r->offscreen_masks->GetClippingContextListForOffscreen())[offscreen] : NULL;
    layer_uniforms uniforms = {};
    uniforms.shared.matrix = matrix_identity_float4x4;
    set_colors(uniforms.shared, model->GetOffscreenOpacity(offscreen), model->GetOverrideMultiplyAndScreenColor().GetOffscreenMultiplyColor(offscreen), model->GetOverrideMultiplyAndScreenColor().GetOffscreenScreenColor(offscreen));
    if (clip != NULL)
    {
        set_clip(r, uniforms, r->offscreen_masks, clip);
    }
    [encoder setRenderPipelineState:r->offscreen_state[offscreen]];
    [encoder setCullMode:model->GetOffscreenCulling(offscreen) != 0 ? MTLCullModeBack : MTLCullModeNone];
    draw_quad(encoder, uniforms);
}

static void submit_to_parent(metal_renderer* r, id<MTLRenderCommandEncoder> encoder, int owner)
{
    while (r->current >= 0)
    {
        for (int parent = owner; parent >= 0; parent = r->offscreen_parent[parent])
        {
            if (parent == r->current)
            {
                return;
            }
        }
        draw_offscreen(r, encoder, r->current);
    }
}

static void add_offscreen(metal_renderer* r, id<MTLRenderCommandEncoder> encoder, int offscreen)
{
    if (r->current != offscreen)
    {
        submit_to_parent(r, encoder, r->offscreen_parent[offscreen]);
    }
    layer_uniforms uniforms = {};
    uniforms.shared.matrix = matrix_identity_float4x4;
    r->current = offscreen;
    [encoder setRenderPipelineState:r->offscreen_clear[offscreen]];
    [encoder setCullMode:MTLCullModeNone];
    draw_quad(encoder, uniforms);
}

static void draw_layer_drawable(metal_renderer* r, id<MTLRenderCommandEncoder> encoder, id<MTLBuffer> positions, simd::float4x4 matrix, csmInt32 index)
{
    CubismModel* model = r->model;
    if (!model->GetDrawableDynamicFlagIsVisible(index))
    {
        return;
    }
    submit_to_parent(r, encoder, r->drawable_owner[index]);
    mask_context* clip = r->masks != NULL ? (*r->masks->GetClippingContextListForDraw())[index] : NULL;
    if (model->GetDrawableVertexIndexCount(index) <= 0 || model->GetDrawableOpacity(index) <= 0.0f || r->textures[model->GetDrawableTextureIndex(index)] == nil)
    {
        return;
    }
    layer_uniforms uniforms = {};
    uniforms.shared.matrix = matrix;
    set_colors(uniforms.shared, model->GetDrawableOpacity(index), model->GetOverrideMultiplyAndScreenColor().GetDrawableMultiplyColor(index), model->GetOverrideMultiplyAndScreenColor().GetDrawableScreenColor(index));
    if (clip != NULL)
    {
        set_clip(r, uniforms, r->masks, clip);
    }
    [encoder setRenderPipelineState:r->drawable_state[index]];
    encode_mesh(r, encoder, positions, index, &uniforms, sizeof(uniforms));
}

void metal_renderer_draw(metal_renderer* r, id<MTLCommandBuffer> command_buffer, id<MTLTexture> target, const CubismMatrix44& mvp, id<MTLTexture> back, bool opaque)
{
    CubismModel* model = r->model;
    csmInt32 count = model->GetDrawableCount();
    csmInt32 total = count + model->GetOffscreenCount();
    id<MTLBuffer> positions = r->positions;
    for (csmInt32 i = 0; i < count; i++)
    {
        memcpy((uint8_t*)positions.contents + r->vertex_offset[i] * 8, model->GetDrawableVertices(i), model->GetDrawableVertexCount(i) * 8);
    }
    const csmInt32* orders = model->GetRenderOrders();
    for (csmInt32 i = 0; i < total; i++)
    {
        r->sorted[orders[i]] = i;
    }
    size_masks(r, target, mvp);
    draw_masks(r, command_buffer, positions, mvp);
    r->pass.colorAttachments[0].texture = metal_renderer_msaa(r, (int)target.width, (int)target.height);
    r->pass.colorAttachments[0].resolveTexture = target;
    for (size_t i = 0; i < r->layers.size(); i++)
    {
        r->pass.colorAttachments[i + 1].texture = r->layers[i];
    }
    id<MTLRenderCommandEncoder> encoder = [command_buffer renderCommandEncoderWithDescriptor:r->pass];
    [encoder setFrontFacingWinding:MTLWindingCounterClockwise];
    [encoder setFragmentSamplerState:texture_sampler atIndex:0];
    [encoder setFragmentSamplerState:mask_sampler atIndex:1];
    if (r->mask_texture != nil)
    {
        [encoder setFragmentTexture:r->mask_texture atIndex:1];
    }
    simd::float4x4 matrix = simd_matrix(mvp);
    if (r->blend)
    {
        r->current = -1;
        for (csmInt32 n = 0; n < total; n++)
        {
            if (r->sorted[n] < count)
            {
                draw_layer_drawable(r, encoder, positions, matrix, r->sorted[n]);
            }
            else
            {
                add_offscreen(r, encoder, r->sorted[n] - count);
            }
        }
        while (r->current >= 0)
        {
            draw_offscreen(r, encoder, r->current);
        }
    }
    else
    {
        csmVector<mask_context*>* draw_contexts = r->masks != NULL ? r->masks->GetClippingContextListForDraw() : NULL;
        const CubismModelMultiplyAndScreenColor& colors = model->GetOverrideMultiplyAndScreenColor();
        for (csmInt32 n = 0; n < count; n++)
        {
            csmInt32 index = r->sorted[n];
            if (!model->GetDrawableDynamicFlagIsVisible(index) || model->GetDrawableVertexIndexCount(index) <= 0 || r->textures[model->GetDrawableTextureIndex(index)] == nil)
            {
                continue;
            }
            mask_context* context = draw_contexts != NULL ? (*draw_contexts)[index] : NULL;
            int blend = std::max(fixed_blend(model->GetDrawableBlendModeType(index)), 0);
            int masked = context == NULL ? 0 : (model->GetDrawableInvertedMask(index) ? 2 : 1);
            CubismShaderUniforms uniforms = {};
            if (context != NULL)
            {
                set_channel(r->masks, context, uniforms);
                uniforms.clipMatrix = simd_matrix(context->_matrixForDraw);
            }
            uniforms.matrix = matrix;
            set_colors(uniforms, model->GetDrawableOpacity(index), colors.GetDrawableMultiplyColor(index), colors.GetDrawableScreenColor(index));
            [encoder setRenderPipelineState:draw_pipelines[blend * 3 + masked]];
            encode_mesh(r, encoder, positions, index, &uniforms, sizeof(uniforms));
        }
    }
    if (opaque)
    {
        vector_float4 size = {(float)target.width, (float)target.height, back != nil ? 1.0f : 0.0f, 0.0f};
        [encoder setRenderPipelineState:r->under];
        [encoder setCullMode:MTLCullModeNone];
        [encoder setFragmentTexture:back != nil ? back : r->textures[0] atIndex:0];
        [encoder setFragmentBytes:&size length:sizeof(size) atIndex:0];
        [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    }
    [encoder endEncoding];
}

bool metal_renderer_share(id<MTLCommandBuffer> command_buffer, id<MTLTexture> model, id<MTLTexture> back, id<MTLTexture> window_target, id<MTLTexture> share_target)
{
    int shown = window_target != nil ? 0 : 1;
    if (share_pipelines[shown] == nil)
    {
        MTLRenderPipelineDescriptor* descriptor = [MTLRenderPipelineDescriptor new];
        descriptor.vertexFunction = [renderer_library newFunctionWithName:@"under_vertex"];
        descriptor.fragmentFunction = [renderer_library newFunctionWithName:@"share_fragment"];
        descriptor.colorAttachments[0].pixelFormat = window_target != nil ? MTLPixelFormatBGRA8Unorm : MTLPixelFormatInvalid;
        descriptor.colorAttachments[1].pixelFormat = MTLPixelFormatBGRA8Unorm;
        NSError* error = nil;
        share_pipelines[shown] = [renderer_device newRenderPipelineStateWithDescriptor:descriptor error:&error];
        if (share_pipelines[shown] == nil)
        {
            NSLog(@"engine: share pipeline failed, drawing the window only %@", error);
            return false;
        }
    }
    MTLRenderPassDescriptor* pass = [MTLRenderPassDescriptor renderPassDescriptor];
    pass.colorAttachments[0].texture = window_target;
    pass.colorAttachments[0].loadAction = MTLLoadActionDontCare;
    pass.colorAttachments[0].storeAction = MTLStoreActionStore;
    pass.colorAttachments[1].texture = share_target;
    pass.colorAttachments[1].loadAction = MTLLoadActionDontCare;
    pass.colorAttachments[1].storeAction = MTLStoreActionStore;
    vector_float4 size = {(float)share_target.width, (float)share_target.height, back != nil ? 1.0f : 0.0f, 0.0f};
    id<MTLRenderCommandEncoder> encoder = [command_buffer renderCommandEncoderWithDescriptor:pass];
    [encoder setRenderPipelineState:share_pipelines[shown]];
    [encoder setFragmentTexture:model atIndex:0];
    [encoder setFragmentTexture:back != nil ? back : model atIndex:1];
    [encoder setFragmentBytes:&size length:sizeof(size) atIndex:0];
    [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    [encoder endEncoding];
    return true;
}
