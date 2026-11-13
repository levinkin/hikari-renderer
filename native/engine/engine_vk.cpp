#define _USE_MATH_DEFINES
#define STB_IMAGE_IMPLEMENTATION
#define STBI_ONLY_PNG
#define STBI_ONLY_JPEG
#define STBI_ONLY_GIF
#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define VK_USE_PLATFORM_WIN32_KHR
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#endif
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <vector>
#include "stb_image.h"
#include "engine.h"
#include "texture_bc7.inc"
#include "window_vk.h"
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
#include "Rendering/Vulkan/CubismClass_Vulkan.hpp"
#include "renderer_vk.h"

using namespace Live2D::Cubism::Framework;

static const VkFormat color_format = VK_FORMAT_B8G8R8A8_UNORM;
static const VkFormat texture_format = VK_FORMAT_R8G8B8A8_UNORM;
static bool supports_bc7 = false;

struct engine_gpu
{
    renderer* gpu = NULL;
    VkBuffer readback = VK_NULL_HANDLE;
    VkDeviceMemory readback_memory = VK_NULL_HANDLE;
    void* readback_mapped = NULL;
    uint64_t background_serial = 0;
};

#include "engine_common.inc"

static VkInstance instance = VK_NULL_HANDLE;
static VkPhysicalDevice physical_device = VK_NULL_HANDLE;
static VkDevice device = VK_NULL_HANDLE;
static VkQueue queue = VK_NULL_HANDLE;
static VkCommandPool command_pool = VK_NULL_HANDLE;
static bool can_present = false;
#ifdef _WIN32
static bool can_share = false;
#endif
static std::mutex queue_lock;
static std::map<std::string, std::vector<CubismImageVulkan>> texture_cache;
static std::mutex texture_lock;

static int graphics_family(VkPhysicalDevice candidate)
{
    uint32_t count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(candidate, &count, NULL);
    std::vector<VkQueueFamilyProperties> families(count);
    vkGetPhysicalDeviceQueueFamilyProperties(candidate, &count, families.data());
    for (uint32_t i = 0; i < count; i++)
    {
        if (families[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)
        {
            return (int)i;
        }
    }
    return -1;
}

static bool has_extension(VkPhysicalDevice candidate, const char* name)
{
    uint32_t count = 0;
    vkEnumerateDeviceExtensionProperties(candidate, NULL, &count, NULL);
    std::vector<VkExtensionProperties> extensions(count);
    vkEnumerateDeviceExtensionProperties(candidate, NULL, &count, extensions.data());
    for (const VkExtensionProperties& extension : extensions)
    {
        if (strcmp(extension.extensionName, name) == 0)
        {
            return true;
        }
    }
    return false;
}

static void wait_idle()
{
    std::lock_guard<std::mutex> guard(queue_lock);
    vkDeviceWaitIdle(device);
}

static bool start_vulkan()
{
    if (device != VK_NULL_HANDLE)
    {
        return true;
    }
    VkApplicationInfo app{};
    app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
    app.pApplicationName = "hikari";
    app.apiVersion = VK_API_VERSION_1_3;
    uint32_t available_count = 0;
    vkEnumerateInstanceExtensionProperties(NULL, &available_count, NULL);
    std::vector<VkExtensionProperties> available(available_count);
    vkEnumerateInstanceExtensionProperties(NULL, &available_count, available.data());
    std::vector<const char*> instance_extensions;
    for (const char* name : {VK_KHR_SURFACE_EXTENSION_NAME, window_surface_extension()})
    {
        for (const VkExtensionProperties& extension : available)
        {
            if (strcmp(extension.extensionName, name) == 0)
            {
                instance_extensions.push_back(name);
            }
        }
    }
    VkInstanceCreateInfo instance_info{};
    instance_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    instance_info.pApplicationInfo = &app;
    instance_info.enabledExtensionCount = (uint32_t)instance_extensions.size();
    instance_info.ppEnabledExtensionNames = instance_extensions.data();
    if (vkCreateInstance(&instance_info, NULL, &instance) != VK_SUCCESS)
    {
        return false;
    }
    uint32_t count = 0;
    vkEnumeratePhysicalDevices(instance, &count, NULL);
    std::vector<VkPhysicalDevice> candidates(count);
    vkEnumeratePhysicalDevices(instance, &count, candidates.data());
    static const int rank[] = {0, 3, 4, 2, 1};
    int best_rank = -1;
    VkPhysicalDeviceProperties properties{};
    for (VkPhysicalDevice candidate : candidates)
    {
        VkPhysicalDeviceProperties candidate_properties;
        vkGetPhysicalDeviceProperties(candidate, &candidate_properties);
        int candidate_rank = candidate_properties.deviceType <= VK_PHYSICAL_DEVICE_TYPE_CPU ? rank[candidate_properties.deviceType] : 0;
        if (candidate_properties.apiVersion >= VK_API_VERSION_1_3 && graphics_family(candidate) >= 0 && candidate_rank > best_rank)
        {
            best_rank = candidate_rank;
            physical_device = candidate;
            properties = candidate_properties;
        }
    }
    if (physical_device == VK_NULL_HANDLE)
    {
        return false;
    }
    gpu_name = properties.deviceName;
    float priority = 1.0f;
    uint32_t family = (uint32_t)graphics_family(physical_device);
    VkDeviceQueueCreateInfo queue_info{};
    queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    queue_info.queueFamilyIndex = family;
    queue_info.queueCount = 1;
    queue_info.pQueuePriorities = &priority;
    VkPhysicalDeviceVulkan13Features vulkan13{};
    vulkan13.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES;
    VkPhysicalDeviceFeatures2 features{};
    features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
    features.pNext = &vulkan13;
    vkGetPhysicalDeviceFeatures2(physical_device, &features);
    if (!vulkan13.dynamicRendering || !features.features.sampleRateShading || !features.features.shaderSampledImageArrayDynamicIndexing)
    {
        return false;
    }
    VkPhysicalDeviceVulkan13Features enabled{};
    enabled.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES;
    enabled.dynamicRendering = VK_TRUE;
    VkPhysicalDeviceFeatures2 enabled_features{};
    enabled_features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
    enabled_features.pNext = &enabled;
    enabled_features.features.samplerAnisotropy = features.features.samplerAnisotropy;
    enabled_features.features.textureCompressionBC = features.features.textureCompressionBC;
    supports_bc7 = features.features.textureCompressionBC == VK_TRUE;
    enabled_features.features.sampleRateShading = VK_TRUE;
    enabled_features.features.shaderSampledImageArrayDynamicIndexing = VK_TRUE;
    can_present = instance_extensions.size() == 2 && has_extension(physical_device, VK_KHR_SWAPCHAIN_EXTENSION_NAME);
    std::vector<const char*> extensions;
    if (can_present)
    {
        extensions.push_back(VK_KHR_SWAPCHAIN_EXTENSION_NAME);
    }
#ifdef _WIN32
    can_share = has_extension(physical_device, VK_KHR_EXTERNAL_MEMORY_WIN32_EXTENSION_NAME);
    if (can_share)
    {
        extensions.push_back(VK_KHR_EXTERNAL_MEMORY_WIN32_EXTENSION_NAME);
    }
#endif
    VkDeviceCreateInfo device_info{};
    device_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    device_info.pNext = &enabled_features;
    device_info.queueCreateInfoCount = 1;
    device_info.pQueueCreateInfos = &queue_info;
    device_info.enabledExtensionCount = (uint32_t)extensions.size();
    device_info.ppEnabledExtensionNames = extensions.data();
    if (vkCreateDevice(physical_device, &device_info, NULL, &device) != VK_SUCCESS)
    {
        device = VK_NULL_HANDLE;
        return false;
    }
    vkGetDeviceQueue(device, family, 0, &queue);
    VkCommandPoolCreateInfo pool_info{};
    pool_info.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
    pool_info.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pool_info.queueFamilyIndex = family;
    vkCreateCommandPool(device, &pool_info, NULL, &command_pool);
    static CubismFramework::Option option = {};
    option.LoggingLevel = CubismFramework::Option::LogLevel_Off;
    CubismFramework::StartUp(&allocator, &option);
    CubismFramework::Initialize();
    renderer_start(physical_device, device, queue, family, command_pool, color_format, &queue_lock);
    return true;
}

static void upload_level(CubismBufferVulkan& staging, size_t staging_bytes, CubismImageVulkan& texture, const uint8_t* data, uint32_t level, uint32_t w, uint32_t h, size_t row_bytes, uint32_t rows, uint32_t row_height)
{
    uint32_t level_w = std::max(w >> level, 1u);
    uint32_t level_h = std::max(h >> level, 1u);
    uint32_t chunk_rows = (uint32_t)std::max(staging_bytes / row_bytes, (size_t)1);
    for (uint32_t row = 0; row < rows; row += chunk_rows)
    {
        uint32_t count = std::min(chunk_rows, rows - row);
        staging.MemCpy(data + row * row_bytes, count * row_bytes);
        uint32_t y = row * row_height;
        VkBufferImageCopy region{};
        region.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, level, 0, 1};
        region.imageOffset = {0, (int32_t)y, 0};
        region.imageExtent = {level_w, std::min(count * row_height, level_h - y), 1};
        VkCommandBuffer commands = renderer_once_begin();
        vkCmdCopyBufferToImage(commands, staging.GetBuffer(), texture.GetImage(), VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);
        renderer_once_end(commands);
    }
}

static void fill_texture(CubismBufferVulkan& staging, size_t staging_bytes, const uint8_t* pixels, int w, int h, uint32_t levels, CubismImageVulkan& texture)
{
    VkCommandBuffer commands = renderer_once_begin();
    texture.SetImageLayout(commands, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, levels, VK_IMAGE_ASPECT_COLOR_BIT);
    renderer_once_end(commands);
    upload_level(staging, staging_bytes, texture, pixels, 0, w, h, (size_t)w * 4, h, 1);
    commands = renderer_once_begin();
    VkImageMemoryBarrier barrier{};
    barrier.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    barrier.image = texture.GetImage();
    barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    int32_t mip_w = w;
    int32_t mip_h = h;
    for (uint32_t level = 1; level <= levels; level++)
    {
        barrier.subresourceRange.baseMipLevel = level - 1;
        barrier.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
        barrier.newLayout = level < levels ? VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL : VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
        barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
        barrier.dstAccessMask = level < levels ? VK_ACCESS_TRANSFER_READ_BIT : VK_ACCESS_SHADER_READ_BIT;
        vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TRANSFER_BIT, level < levels ? VK_PIPELINE_STAGE_TRANSFER_BIT : VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0, NULL, 1, &barrier);
        if (level == levels)
        {
            break;
        }
        VkImageBlit blit{};
        blit.srcOffsets[1] = {mip_w, mip_h, 1};
        blit.srcSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, level - 1, 0, 1};
        mip_w = mip_w > 1 ? mip_w / 2 : 1;
        mip_h = mip_h > 1 ? mip_h / 2 : 1;
        blit.dstOffsets[1] = {mip_w, mip_h, 1};
        blit.dstSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, level, 0, 1};
        vkCmdBlitImage(commands, texture.GetImage(), VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, texture.GetImage(), VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &blit, VK_FILTER_LINEAR);
        barrier.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
        barrier.newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
        barrier.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
        barrier.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
        vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0, NULL, 1, &barrier);
    }
    renderer_once_end(commands);
    texture.SetCurrentLayout(VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
}

static void upload_texture(CubismBufferVulkan& staging, size_t staging_bytes, const uint8_t* pixels, int w, int h, uint32_t levels, CubismImageVulkan& texture)
{
    texture.CreateImage(device, physical_device, w, h, levels, texture_format, VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT);
    fill_texture(staging, staging_bytes, pixels, w, h, levels, texture);
    texture.CreateView(device, texture_format, VK_IMAGE_ASPECT_COLOR_BIT, levels);
    texture.CreateSampler(device, VK_SAMPLER_ADDRESS_MODE_REPEAT, VK_FILTER_LINEAR, VK_FILTER_LINEAR, VK_SAMPLER_MIPMAP_MODE_LINEAR, 0.0f, levels);
}

static bool load_bc7(FILE* file, const bc7_header& header, CubismImageVulkan& texture)
{
    size_t staging_bytes = std::min(bc7_level_bytes(header.width, header.height, 0), staging_size);
    CubismBufferVulkan staging;
    staging.CreateBuffer(device, physical_device, staging_bytes, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    staging.Map(device, staging_bytes);
    texture.CreateImage(device, physical_device, header.width, header.height, header.levels, VK_FORMAT_BC7_UNORM_BLOCK, VK_IMAGE_TILING_OPTIMAL, VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT);
    VkCommandBuffer commands = renderer_once_begin();
    texture.SetImageLayout(commands, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, header.levels, VK_IMAGE_ASPECT_COLOR_BIT);
    renderer_once_end(commands);
    std::vector<uint8_t> level_bytes;
    bool ok = true;
    for (uint32_t level = 0; ok && level < header.levels; level++)
    {
        level_bytes.resize(bc7_level_bytes(header.width, header.height, level));
        ok = fread(level_bytes.data(), 1, level_bytes.size(), file) == level_bytes.size();
        uint32_t blocks_w = (std::max(header.width >> level, 1u) + 3) / 4;
        if (ok)
        {
            upload_level(staging, staging_bytes, texture, level_bytes.data(), level, header.width, header.height, (size_t)blocks_w * 16, (uint32_t)(level_bytes.size() / (blocks_w * 16)), 4);
        }
    }
    fclose(file);
    commands = renderer_once_begin();
    VkImageMemoryBarrier barrier{};
    barrier.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    barrier.image = texture.GetImage();
    barrier.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    barrier.newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    barrier.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, header.levels, 0, 1};
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0, NULL, 1, &barrier);
    renderer_once_end(commands);
    texture.SetCurrentLayout(VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL);
    staging.UnMap(device);
    staging.Destroy(device);
    texture.CreateView(device, VK_FORMAT_BC7_UNORM_BLOCK, VK_IMAGE_ASPECT_COLOR_BIT, header.levels);
    texture.CreateSampler(device, VK_SAMPLER_ADDRESS_MODE_REPEAT, VK_FILTER_LINEAR, VK_FILTER_LINEAR, VK_SAMPLER_MIPMAP_MODE_LINEAR, 0.0f, header.levels);
    return ok;
}

static bool load_texture(const std::string& path, CubismImageVulkan& texture)
{
    std::vector<uint8_t> bytes = read_file(path);
    bool use_bc7 = !bytes.empty() && supports_bc7;
    std::string cache_path = use_bc7 ? bc7_cache_path(bytes) : "";
    bc7_header header;
    FILE* cached = bc7_open(cache_path, header);
    if (cached != NULL && header.levels > 0)
    {
        return load_bc7(cached, header, texture);
    }
    if (cached != NULL)
    {
        fclose(cached);
        use_bc7 = false;
    }
    int w = 0;
    int h = 0;
    int channels = 0;
    stbi_uc* pixels = bytes.empty() ? NULL : stbi_load_from_memory(bytes.data(), (int)bytes.size(), &w, &h, &channels, 4);
    if (pixels == NULL)
    {
        return false;
    }
    size_t size = (size_t)w * h * 4;
    for (size_t i = 0; i < size; i += 4)
    {
        for (int c = 0; c < 3; c++)
        {
            pixels[i + c] = (stbi_uc)((pixels[i + c] * pixels[i + 3] + 127) / 255);
        }
    }
    if (use_bc7 && bc7_encode(pixels, (uint32_t)w, (uint32_t)h, cache_path) && (cached = bc7_open(cache_path, header)) != NULL)
    {
        stbi_image_free(pixels);
        return load_bc7(cached, header, texture);
    }
    size_t staging_bytes = std::min(size, staging_size);
    CubismBufferVulkan staging;
    staging.CreateBuffer(device, physical_device, staging_bytes, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    staging.Map(device, staging_bytes);
    upload_texture(staging, staging_bytes, pixels, w, h, mip_levels((uint32_t)w, (uint32_t)h), texture);
    staging.UnMap(device);
    staging.Destroy(device);
    stbi_image_free(pixels);
    return true;
}

static void create_readback(engine* e, VkDeviceSize size)
{
    VkBufferCreateInfo info{};
    info.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO;
    info.size = size;
    info.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
    info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    vkCreateBuffer(device, &info, NULL, &e->readback);
    VkMemoryRequirements requirements;
    vkGetBufferMemoryRequirements(device, e->readback, &requirements);
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical_device, &memory);
    VkMemoryPropertyFlags visible = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    uint32_t chosen = UINT32_MAX;
    for (VkMemoryPropertyFlags wanted : {visible | VK_MEMORY_PROPERTY_HOST_CACHED_BIT, visible})
    {
        for (uint32_t i = 0; i < memory.memoryTypeCount; i++)
        {
            if (chosen == UINT32_MAX && (requirements.memoryTypeBits & (1u << i)) && (memory.memoryTypes[i].propertyFlags & wanted) == wanted)
            {
                chosen = i;
            }
        }
    }
    VkMemoryAllocateInfo allocate{};
    allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    allocate.allocationSize = requirements.size;
    allocate.memoryTypeIndex = chosen;
    vkAllocateMemory(device, &allocate, NULL, &e->readback_memory);
    vkBindBufferMemory(device, e->readback, e->readback_memory, 0);
    vkMapMemory(device, e->readback_memory, 0, size, 0, &e->readback_mapped);
}

engine* engine_create(const char* model3_json_path, int width, int height)
{
    if (!start_vulkan())
    {
        return NULL;
    }
    std::string path(model3_json_path);
    std::string dir = path.substr(0, path.find_last_of("/\\") + 1);
    engine* e = load_model(path, dir, width, height);
    if (e == NULL)
    {
        return NULL;
    }
    std::lock_guard<std::mutex> guard(texture_lock);
    std::vector<CubismImageVulkan>& textures = texture_cache[path];
    for (csmInt32 i = (csmInt32)textures.size(); i < e->setting->GetTextureCount(); i++)
    {
        CubismImageVulkan texture;
        if (!load_texture(dir + e->setting->GetTextureFileName(i), texture))
        {
            engine_destroy(e);
            return NULL;
        }
        textures.push_back(texture);
    }
    std::vector<VkDescriptorImageInfo> images;
    for (CubismImageVulkan& texture : textures)
    {
        images.push_back({texture.GetSampler(), texture.GetView(), VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL});
    }
    if (images.empty())
    {
        engine_destroy(e);
        return NULL;
    }
    e->gpu = renderer_create(e->_model, images);
    if (e->gpu == NULL)
    {
        engine_destroy(e);
        return NULL;
    }
    view_matrix(e);
    return e;
}

void engine_destroy(engine* e)
{
    wait_idle();
    for (auto& item : e->expressions)
    {
        ACubismMotion::Delete(item.second);
    }
    if (e->gpu != NULL)
    {
        renderer_destroy(e->gpu);
    }
    if (e->readback != VK_NULL_HANDLE)
    {
        vkUnmapMemory(device, e->readback_memory);
        vkDestroyBuffer(device, e->readback, NULL);
        vkFreeMemory(device, e->readback_memory, NULL);
    }
    delete e->setting;
    delete e;
}

void engine_textures_release(const char* model3_json_path)
{
    std::lock_guard<std::mutex> guard(texture_lock);
    auto found = texture_cache.find(model3_json_path);
    if (found == texture_cache.end())
    {
        return;
    }
    wait_idle();
    for (CubismImageVulkan& texture : found->second)
    {
        texture.Destroy(device);
    }
    texture_cache.erase(found);
}

static std::vector<std::vector<std::pair<int, float>>> axis_weights(int src, int dst, int span)
{
    std::vector<std::vector<std::pair<int, float>>> weights(dst);
    double offset = (dst - span) / 2.0;
    double step = (double)src / span;
    double radius = step > 1.0 ? step : 1.0;
    for (int i = 0; i < dst; i++)
    {
        double center = (i + 0.5 - offset) * step - 0.5;
        if (i + 0.5 < offset || i + 0.5 > offset + span)
        {
            continue;
        }
        float total = 0;
        for (int s = (int)std::ceil(center - radius); s <= (int)std::floor(center + radius); s++)
        {
            float w = (float)(1.0 - std::fabs(s - center) / radius);
            if (w <= 0)
            {
                continue;
            }
            weights[i].push_back({s < 0 ? 0 : (s >= src ? src - 1 : s), w});
            total += w;
        }
        for (auto& item : weights[i])
        {
            item.second /= total;
        }
    }
    return weights;
}

static std::vector<uint8_t> cover_frame(const stbi_uc* pixels, int iw, int ih, const std::vector<std::vector<std::pair<int, float>>>& columns, const std::vector<std::vector<std::pair<int, float>>>& rows)
{
    int width = (int)columns.size();
    int height = (int)rows.size();
    std::vector<float> premultiplied((size_t)iw * ih * 3);
    for (size_t p = 0; p < (size_t)iw * ih; p++)
    {
        for (int c = 0; c < 3; c++)
        {
            premultiplied[p * 3 + c] = pixels[p * 4 + c] * pixels[p * 4 + 3] / 255.0f;
        }
    }
    std::vector<float> wide((size_t)ih * width * 3);
    for (int y = 0; y < ih; y++)
    {
        for (int x = 0; x < width; x++)
        {
            for (auto& item : columns[x])
            {
                for (int c = 0; c < 3; c++)
                {
                    wide[((size_t)y * width + x) * 3 + c] += premultiplied[((size_t)y * iw + item.first) * 3 + c] * item.second;
                }
            }
        }
    }
    std::vector<uint8_t> frame((size_t)width * height * 4);
    for (int y = 0; y < height; y++)
    {
        for (int x = 0; x < width; x++)
        {
            float sum[3] = {0, 0, 0};
            for (auto& item : rows[y])
            {
                for (int c = 0; c < 3; c++)
                {
                    sum[c] += wide[((size_t)item.first * width + x) * 3 + c] * item.second;
                }
            }
            for (int c = 0; c < 3; c++)
            {
                frame[((size_t)y * width + x) * 4 + c] = (uint8_t)std::fmin(255.0f, std::fmax(0.0f, sum[c] + 0.5f));
            }
            frame[((size_t)y * width + x) * 4 + 3] = 255;
        }
    }
    return frame;
}

static uint32_t read_be(const uint8_t* bytes, int count)
{
    uint32_t value = 0;
    for (int i = 0; i < count; i++)
    {
        value = value << 8 | bytes[i];
    }
    return value;
}

static void append_chunk(std::string& png, const char* type, const uint8_t* data, size_t size)
{
    for (int shift = 24; shift >= 0; shift -= 8)
    {
        png.push_back((char)(size >> shift));
    }
    png.append(type, 4);
    png.append((const char*)data, size);
    png.append(4, '\0');
}

struct apng_frame
{
    uint32_t x = 0;
    uint32_t y = 0;
    uint32_t width = 0;
    uint32_t height = 0;
    float ms = 0;
    uint8_t dispose = 0;
    uint8_t blend = 0;
    std::string data;
};

static bool decode_apng(const uint8_t* bytes, size_t length, int& iw, int& ih, std::vector<uint8_t>& canvases, std::vector<float>& delays)
{
    const uint8_t* header = NULL;
    std::string palette;
    std::vector<apng_frame> frames;
    bool animated = false;
    bool default_frame = false;
    uint32_t declared_frames = 0;
    uint32_t sequence = 0;
    for (size_t at = 8; at + 12 <= length;)
    {
        size_t size = read_be(bytes + at, 4);
        if (size > length - at - 12)
        {
            return false;
        }
        std::string type((const char*)bytes + at + 4, 4);
        const uint8_t* data = bytes + at + 8;
        if ((type == "fcTL" || type == "fdAT") && (size < 4 || read_be(data, 4) != sequence++))
        {
            return false;
        }
        if (type == "IHDR" && size == 13)
        {
            header = data;
        }
        else if (type == "PLTE" || type == "tRNS")
        {
            palette.append((const char*)bytes + at, size + 12);
        }
        else if (type == "acTL")
        {
            if (size != 8 || animated)
            {
                return false;
            }
            animated = true;
            declared_frames = read_be(data, 4);
        }
        else if (type == "fcTL")
        {
            if (size != 26 || data[24] > 2 || data[25] > 1)
            {
                return false;
            }
            apng_frame frame;
            frame.width = read_be(data + 4, 4);
            frame.height = read_be(data + 8, 4);
            frame.x = read_be(data + 12, 4);
            frame.y = read_be(data + 16, 4);
            uint32_t denominator = read_be(data + 22, 2);
            frame.ms = read_be(data + 20, 2) * 1000.0f / (denominator == 0 ? 100 : denominator);
            frame.dispose = data[24];
            frame.blend = data[25];
            frames.push_back(frame);
        }
        else if (type == "IDAT" && !frames.empty())
        {
            default_frame = true;
            frames.back().data.append((const char*)data, size);
        }
        else if (type == "fdAT" && !frames.empty())
        {
            frames.back().data.append((const char*)data + 4, size - 4);
        }
        at += size + 12;
    }
    if (!animated)
    {
        return true;
    }
    if (header == NULL || frames.empty() || declared_frames != frames.size() || (uint64_t)read_be(header, 4) * read_be(header + 4, 4) > (1u << 26) || (uint64_t)read_be(header, 4) * read_be(header + 4, 4) * 4 * frames.size() > (1ull << 28))
    {
        return false;
    }
    iw = (int)read_be(header, 4);
    ih = (int)read_be(header + 4, 4);
    std::vector<uint8_t> canvas((size_t)iw * ih * 4);
    for (apng_frame& frame : frames)
    {
        if ((uint64_t)frame.x + frame.width > (uint64_t)iw || (uint64_t)frame.y + frame.height > (uint64_t)ih)
        {
            return false;
        }
        uint8_t frame_header[13];
        memcpy(frame_header, header, 13);
        for (int i = 0; i < 4; i++)
        {
            frame_header[i] = (uint8_t)(frame.width >> (24 - 8 * i));
            frame_header[4 + i] = (uint8_t)(frame.height >> (24 - 8 * i));
        }
        std::string png("\x89PNG\r\n\x1a\n", 8);
        append_chunk(png, "IHDR", frame_header, 13);
        png += palette;
        append_chunk(png, "IDAT", (const uint8_t*)frame.data.data(), frame.data.size());
        append_chunk(png, "IEND", frame_header, 0);
        int fw = 0;
        int fh = 0;
        int channels = 0;
        stbi_uc* pixels = stbi_load_from_memory((const stbi_uc*)png.data(), (int)png.size(), &fw, &fh, &channels, 4);
        bool fits = (uint32_t)fw == frame.width && (uint32_t)fh == frame.height;
        bool full = frame.x == 0 && frame.y == 0 && fw == iw && fh == ih;
        if (pixels == NULL || !fits || (default_frame && &frame == &frames[0] && !full))
        {
            stbi_image_free(pixels);
            return false;
        }
        std::vector<uint8_t> prior = frame.dispose == 2 ? canvas : std::vector<uint8_t>();
        for (int row = 0; row < fh; row++)
        {
            for (int col = 0; col < fw; col++)
            {
                const uint8_t* source = pixels + ((size_t)row * fw + col) * 4;
                uint8_t* target = &canvas[(((size_t)frame.y + row) * iw + frame.x + col) * 4];
                if (frame.blend == 0 || source[3] == 255)
                {
                    memcpy(target, source, 4);
                }
                else if (source[3] != 0)
                {
                    float source_alpha = source[3] / 255.0f;
                    float target_alpha = target[3] / 255.0f * (1.0f - source_alpha);
                    float alpha = source_alpha + target_alpha;
                    for (int c = 0; c < 3; c++)
                    {
                        target[c] = (uint8_t)((source[c] * source_alpha + target[c] * target_alpha) / alpha + 0.5f);
                    }
                    target[3] = (uint8_t)(alpha * 255.0f + 0.5f);
                }
            }
        }
        stbi_image_free(pixels);
        canvases.insert(canvases.end(), canvas.begin(), canvas.end());
        delays.push_back(frame.ms);
        if (frame.dispose == 1)
        {
            for (int row = 0; row < fh; row++)
            {
                memset(&canvas[(((size_t)frame.y + row) * iw + frame.x) * 4], 0, (size_t)fw * 4);
            }
        }
        if (frame.dispose == 2)
        {
            canvas = std::move(prior);
        }
    }
    return true;
}

int engine_background_load(engine* e, const uint8_t* bytes, size_t length, const char* expected_type)
{
    static const std::map<std::string, std::pair<std::string, std::string>> formats = {
        {"image/png", {std::string("\x89PNG", 4), std::string("IEND\xae\x42\x60\x82", 8)}},
        {"image/jpeg", {std::string("\xff\xd8", 2), std::string("\xff\xd9", 2)}},
        {"image/gif", {std::string("GIF8", 4), std::string(";", 1)}},
    };
    auto format = formats.find(expected_type);
    if (format == formats.end())
    {
        return 0;
    }
    const std::string& magic = format->second.first;
    const std::string& trailer = format->second.second;
    if (length < magic.size() + trailer.size() || memcmp(bytes, magic.data(), magic.size()) != 0 || memcmp(bytes + length - trailer.size(), trailer.data(), trailer.size()) != 0)
    {
        return 0;
    }
    int iw = 0;
    int ih = 0;
    int channels = 0;
    std::vector<uint8_t> apng;
    std::vector<float> delays_ms;
    if (format->first == "image/png" && !decode_apng(bytes, length, iw, ih, apng, delays_ms))
    {
        return 0;
    }
    stbi_uc* pixels = NULL;
    if (delays_ms.empty())
    {
        int count = 1;
        int* delays = NULL;
        pixels = format->first == "image/gif" ? stbi_load_gif_from_memory(bytes, (int)length, &delays, &iw, &ih, &count, &channels, 4) : stbi_load_from_memory(bytes, (int)length, &iw, &ih, &channels, 4);
        for (int i = 0; pixels != NULL && i < count; i++)
        {
            delays_ms.push_back(delays != NULL ? (float)delays[i] : 100.0f);
        }
        free(delays);
        if (pixels == NULL)
        {
            return 0;
        }
    }
    const uint8_t* source = pixels != NULL ? pixels : apng.data();
    double scale = std::fmax((double)e->width / iw, (double)e->height / ih);
    std::vector<std::vector<std::pair<int, float>>> columns = axis_weights(iw, e->width, (int)std::ceil(iw * scale));
    std::vector<std::vector<std::pair<int, float>>> rows = axis_weights(ih, e->height, (int)std::ceil(ih * scale));
    std::vector<std::vector<uint8_t>> frames;
    std::vector<float> ms;
    float total_ms = 0;
    for (size_t i = 0; i < delays_ms.size(); i++)
    {
        float frame_ms = delays_ms[i];
        if (frame_ms < 20.0f)
        {
            frame_ms = 100.0f;
        }
        frames.push_back(cover_frame(source + i * iw * ih * 4, iw, ih, columns, rows));
        ms.push_back(frame_ms);
        total_ms += frame_ms;
    }
    stbi_image_free(pixels);
    e->background_frames = std::move(frames);
    e->background_ms = std::move(ms);
    e->background_total_ms = total_ms;
    e->background_serial++;
    return (int)e->background_frames.size();
}

const uint8_t* engine_render(engine* e, float background_clock_ms)
{
    size_t pixels = (size_t)e->width * e->height;
    if (e->readback == VK_NULL_HANDLE)
    {
        create_readback(e, pixels * 4);
        e->rgba.resize(pixels * 4);
    }
    update_shown(e);
    VkCommandBuffer commands = renderer_begin(e->gpu);
    renderer_draw(e->gpu, commands, VkExtent2D{(uint32_t)e->width, (uint32_t)e->height}, e->mvp.GetArray(), VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_ACCESS_TRANSFER_READ_BIT);
    VkBufferImageCopy region{};
    region.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    region.imageExtent = {(uint32_t)e->width, (uint32_t)e->height, 1};
    vkCmdCopyImageToBuffer(commands, e->gpu->resolved.image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, e->readback, 1, &region);
    VkBufferMemoryBarrier host{};
    host.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER;
    host.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    host.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
    host.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    host.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    host.buffer = e->readback;
    host.size = VK_WHOLE_SIZE;
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 0, NULL, 1, &host, 0, NULL);
    renderer_submit(e->gpu, VK_NULL_HANDLE, 0, VK_NULL_HANDLE);
    vkWaitForFences(device, 2, e->gpu->fences, VK_TRUE, UINT64_MAX);
    const uint8_t* raw = (const uint8_t*)e->readback_mapped;
    const uint8_t* background = background_at(e, background_clock_ms);
    for (size_t i = 0; i < pixels * 4; i += 4)
    {
        int keep = 255 - raw[i + 3];
        for (int c = 0; c < 3; c++)
        {
            int back = background != NULL ? background[i + c] : 0;
            e->rgba[i + c] = (uint8_t)((raw[i + 2 - c] * 255 + back * keep + 127) / 255);
        }
        e->rgba[i + 3] = 255;
    }
    return e->rgba.data();
}

static const uint32_t present_vert[] = {
    0x07230203, 0x00010000, 0x000d000b, 0x0000002c, 0x00000000, 0x00020011, 0x00000001, 0x0006000b,
    0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e, 0x00000000, 0x00000001,
    0x0008000f, 0x00000000, 0x00000004, 0x6e69616d, 0x00000000, 0x00000009, 0x0000000c, 0x0000001d,
    0x00040047, 0x00000009, 0x0000001e, 0x00000000, 0x00040047, 0x0000000c, 0x0000000b, 0x0000002a,
    0x00030047, 0x0000001b, 0x00000002, 0x00050048, 0x0000001b, 0x00000000, 0x0000000b, 0x00000000,
    0x00050048, 0x0000001b, 0x00000001, 0x0000000b, 0x00000001, 0x00050048, 0x0000001b, 0x00000002,
    0x0000000b, 0x00000003, 0x00050048, 0x0000001b, 0x00000003, 0x0000000b, 0x00000004, 0x00020013,
    0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016, 0x00000006, 0x00000020, 0x00040017,
    0x00000007, 0x00000006, 0x00000002, 0x00040020, 0x00000008, 0x00000003, 0x00000007, 0x0004003b,
    0x00000008, 0x00000009, 0x00000003, 0x00040015, 0x0000000a, 0x00000020, 0x00000001, 0x00040020,
    0x0000000b, 0x00000001, 0x0000000a, 0x0004003b, 0x0000000b, 0x0000000c, 0x00000001, 0x0004002b,
    0x0000000a, 0x0000000e, 0x00000001, 0x0004002b, 0x0000000a, 0x00000010, 0x00000002, 0x00040017,
    0x00000017, 0x00000006, 0x00000004, 0x00040015, 0x00000018, 0x00000020, 0x00000000, 0x0004002b,
    0x00000018, 0x00000019, 0x00000001, 0x0004001c, 0x0000001a, 0x00000006, 0x00000019, 0x0006001e,
    0x0000001b, 0x00000017, 0x00000006, 0x0000001a, 0x0000001a, 0x00040020, 0x0000001c, 0x00000003,
    0x0000001b, 0x0004003b, 0x0000001c, 0x0000001d, 0x00000003, 0x0004002b, 0x0000000a, 0x0000001e,
    0x00000000, 0x0004002b, 0x00000006, 0x00000020, 0x40000000, 0x0004002b, 0x00000006, 0x00000022,
    0x3f800000, 0x0004002b, 0x00000006, 0x00000025, 0x00000000, 0x00040020, 0x00000029, 0x00000003,
    0x00000017, 0x0005002c, 0x00000007, 0x0000002b, 0x00000022, 0x00000022, 0x00050036, 0x00000002,
    0x00000004, 0x00000000, 0x00000003, 0x000200f8, 0x00000005, 0x0004003d, 0x0000000a, 0x0000000d,
    0x0000000c, 0x000500c4, 0x0000000a, 0x0000000f, 0x0000000d, 0x0000000e, 0x000500c7, 0x0000000a,
    0x00000011, 0x0000000f, 0x00000010, 0x0004006f, 0x00000006, 0x00000012, 0x00000011, 0x000500c7,
    0x0000000a, 0x00000014, 0x0000000d, 0x00000010, 0x0004006f, 0x00000006, 0x00000015, 0x00000014,
    0x00050050, 0x00000007, 0x00000016, 0x00000012, 0x00000015, 0x0003003e, 0x00000009, 0x00000016,
    0x0004003d, 0x00000007, 0x0000001f, 0x00000009, 0x0005008e, 0x00000007, 0x00000021, 0x0000001f,
    0x00000020, 0x00050083, 0x00000007, 0x00000024, 0x00000021, 0x0000002b, 0x00050051, 0x00000006,
    0x00000026, 0x00000024, 0x00000000, 0x00050051, 0x00000006, 0x00000027, 0x00000024, 0x00000001,
    0x00070050, 0x00000017, 0x00000028, 0x00000026, 0x00000027, 0x00000025, 0x00000022, 0x00050041,
    0x00000029, 0x0000002a, 0x0000001d, 0x0000001e, 0x0003003e, 0x0000002a, 0x00000028, 0x000100fd,
    0x00010038};
static const uint32_t present_frag[] = {
    0x07230203, 0x00010000, 0x000d000b, 0x0000002b, 0x00000000, 0x00020011, 0x00000001, 0x0006000b,
    0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e, 0x00000000, 0x0003000e, 0x00000000, 0x00000001,
    0x0007000f, 0x00000004, 0x00000004, 0x6e69616d, 0x00000000, 0x00000011, 0x00000015, 0x00030010,
    0x00000004, 0x00000007, 0x00040047, 0x0000000d, 0x00000021, 0x00000000, 0x00040047, 0x0000000d,
    0x00000022, 0x00000000, 0x00040047, 0x00000011, 0x0000001e, 0x00000000, 0x00040047, 0x00000015,
    0x0000001e, 0x00000000, 0x00040047, 0x00000019, 0x00000021, 0x00000001, 0x00040047, 0x00000019,
    0x00000022, 0x00000000, 0x00020013, 0x00000002, 0x00030021, 0x00000003, 0x00000002, 0x00030016,
    0x00000006, 0x00000020, 0x00040017, 0x00000007, 0x00000006, 0x00000004, 0x00090019, 0x0000000a,
    0x00000006, 0x00000001, 0x00000000, 0x00000000, 0x00000000, 0x00000001, 0x00000000, 0x0003001b,
    0x0000000b, 0x0000000a, 0x00040020, 0x0000000c, 0x00000000, 0x0000000b, 0x0004003b, 0x0000000c,
    0x0000000d, 0x00000000, 0x00040017, 0x0000000f, 0x00000006, 0x00000002, 0x00040020, 0x00000010,
    0x00000001, 0x0000000f, 0x0004003b, 0x00000010, 0x00000011, 0x00000001, 0x00040020, 0x00000014,
    0x00000003, 0x00000007, 0x0004003b, 0x00000014, 0x00000015, 0x00000003, 0x00040017, 0x00000016,
    0x00000006, 0x00000003, 0x0004003b, 0x0000000c, 0x00000019, 0x00000000, 0x0004002b, 0x00000006,
    0x0000001e, 0x3f800000, 0x00050036, 0x00000002, 0x00000004, 0x00000000, 0x00000003, 0x000200f8,
    0x00000005, 0x0004003d, 0x0000000b, 0x0000000e, 0x0000000d, 0x0004003d, 0x0000000f, 0x00000012,
    0x00000011, 0x00050057, 0x00000007, 0x00000013, 0x0000000e, 0x00000012, 0x0008004f, 0x00000016,
    0x00000018, 0x00000013, 0x00000013, 0x00000000, 0x00000001, 0x00000002, 0x0004003d, 0x0000000b,
    0x0000001a, 0x00000019, 0x00050057, 0x00000007, 0x0000001c, 0x0000001a, 0x00000012, 0x0008004f,
    0x00000016, 0x0000001d, 0x0000001c, 0x0000001c, 0x00000000, 0x00000001, 0x00000002, 0x00050051,
    0x00000006, 0x00000023, 0x00000013, 0x00000003, 0x00050083, 0x00000006, 0x00000024, 0x0000001e,
    0x00000023, 0x0005008e, 0x00000016, 0x00000025, 0x0000001d, 0x00000024, 0x00050081, 0x00000016,
    0x00000026, 0x00000018, 0x00000025, 0x00050051, 0x00000006, 0x00000027, 0x00000026, 0x00000000,
    0x00050051, 0x00000006, 0x00000028, 0x00000026, 0x00000001, 0x00050051, 0x00000006, 0x00000029,
    0x00000026, 0x00000002, 0x00070050, 0x00000007, 0x0000002a, 0x00000027, 0x00000028, 0x00000029,
    0x0000001e, 0x0003003e, 0x00000015, 0x0000002a, 0x000100fd, 0x00010038};

struct present_target
{
    window* owner = NULL;
    VkSurfaceKHR surface = VK_NULL_HANDLE;
    VkSurfaceFormatKHR format{};
    VkSwapchainKHR swapchain = VK_NULL_HANDLE;
    VkExtent2D extent{};
    int polled[2] = {-1, -1};
    std::chrono::steady_clock::time_point deadline{};
    std::vector<VkImageView> views;
    std::vector<VkImage> images;
    std::vector<VkSemaphore> rendered;
    VkSemaphore acquired[2] = {};
    VkSampler sampler = VK_NULL_HANDLE;
    VkDescriptorSetLayout set_layout = VK_NULL_HANDLE;
    VkDescriptorPool pool = VK_NULL_HANDLE;
    VkDescriptorSet set = VK_NULL_HANDLE;
    VkPipelineLayout pipeline_layout = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
    CubismImageVulkan background;
    CubismBufferVulkan background_staging;
    int background_size[2] = {0, 0};
    uint32_t background_levels = 0;
    const uint8_t* shown_source = NULL;
    uint64_t shown_serial = 0;
    VkExtent2D shown_extent{};
    uint64_t bound_generation = 0;
};

static present_target target;

#ifdef _WIN32
struct spout_info
{
    uint32_t share_handle;
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint32_t usage;
    uint8_t description[256];
    uint32_t partner_id;
};

struct spout_sender
{
    HANDLE names = NULL;
    HANDLE names_mutex = NULL;
    HANDLE info = NULL;
    HANDLE info_mutex = NULL;
    HANDLE active = NULL;
    HANDLE active_mutex = NULL;
    HANDLE access = NULL;
    char* names_view = NULL;
    spout_info* info_view = NULL;
    char* active_view = NULL;
    size_t slots = 0;
    uint32_t family = 0;
    ID3D11Device* d3d = NULL;
    ID3D11Texture2D* texture = NULL;
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory memory = VK_NULL_HANDLE;
    VkExtent2D extent{};
    bool registered = false;
    bool released = false;
    bool failed = false;
};

static spout_sender spout;
static const char spout_name[] = "hikari";

static bool spout_lock(HANDLE mutex, DWORD timeout_ms)
{
    DWORD result = WaitForSingleObject(mutex, timeout_ms);
    return result == WAIT_OBJECT_0 || result == WAIT_ABANDONED;
}

static bool spout_register(bool add)
{
    if (spout.names_view == NULL || !spout_lock(spout.names_mutex, 67))
    {
        return false;
    }
    size_t used = 0;
    size_t found = spout.slots;
    while (used < spout.slots && spout.names_view[used * 256] != 0)
    {
        if (strncmp(spout.names_view + used * 256, spout_name, 256) == 0)
        {
            found = used;
        }
        used++;
    }
    bool added = add && found == spout.slots && used < spout.slots;
    if (added)
    {
        memcpy(spout.names_view + used * 256, spout_name, sizeof(spout_name));
        if (used + 1 < spout.slots)
        {
            spout.names_view[(used + 1) * 256] = 0;
        }
    }
    if (!add && found < spout.slots)
    {
        memmove(spout.names_view + found * 256, spout.names_view + (found + 1) * 256, (used - found - 1) * 256);
        memset(spout.names_view + (used - 1) * 256, 0, 256);
    }
    if ((added || !add) && spout_lock(spout.active_mutex, 67))
    {
        if (add && spout.active_view[0] == 0)
        {
            memcpy(spout.active_view, spout_name, sizeof(spout_name));
        }
        if (!add && strncmp(spout.active_view, spout_name, 256) == 0)
        {
            memcpy(spout.active_view, spout.names_view, 256);
        }
        ReleaseMutex(spout.active_mutex);
    }
    ReleaseMutex(spout.names_mutex);
    return added;
}

static const char* spout_open()
{
    spout.info = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, sizeof(spout_info), spout_name);
    spout.info_mutex = CreateMutexA(NULL, FALSE, "hikari_mutex");
    spout.names = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 64 * 256, "SpoutSenderNames");
    spout.names_mutex = CreateMutexA(NULL, FALSE, "SpoutSenderNames_mutex");
    spout.active = CreateFileMappingA(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 256, "ActiveSenderName");
    spout.active_mutex = CreateMutexA(NULL, FALSE, "ActiveSenderName_mutex");
    spout.access = CreateMutexA(NULL, FALSE, "hikari_SpoutAccessMutex");
    for (HANDLE handle : {spout.info, spout.info_mutex, spout.names, spout.names_mutex, spout.active, spout.active_mutex, spout.access})
    {
        if (handle == NULL)
        {
            return "shared memory";
        }
    }
    spout.info_view = (spout_info*)MapViewOfFile(spout.info, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    spout.names_view = (char*)MapViewOfFile(spout.names, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    spout.active_view = (char*)MapViewOfFile(spout.active, FILE_MAP_ALL_ACCESS, 0, 0, 0);
    if (spout.info_view == NULL || spout.names_view == NULL || spout.active_view == NULL)
    {
        return "shared memory view";
    }
    MEMORY_BASIC_INFORMATION region{};
    VirtualQuery(spout.names_view, &region, sizeof(region));
    spout.slots = region.RegionSize / 256;
    spout.family = (uint32_t)graphics_family(physical_device);
    spout.registered = spout_register(true);
    return spout.registered ? NULL : "sender hikari already exists or the sender list is busy";
}

static void spout_drop_image()
{
    if (spout.image != VK_NULL_HANDLE)
    {
        wait_idle();
    }
    vkDestroyImage(device, spout.image, NULL);
    vkFreeMemory(device, spout.memory, NULL);
    spout.image = VK_NULL_HANDLE;
    spout.memory = VK_NULL_HANDLE;
    spout.released = false;
    if (spout.texture != NULL)
    {
        spout.texture->Release();
        spout.texture = NULL;
    }
}

static const char* spout_device()
{
    VkPhysicalDeviceExternalImageFormatInfo external_format{};
    external_format.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO;
    external_format.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_KMT_BIT;
    VkPhysicalDeviceImageFormatInfo2 format_info{};
    format_info.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2;
    format_info.pNext = &external_format;
    format_info.format = color_format;
    format_info.type = VK_IMAGE_TYPE_2D;
    format_info.tiling = VK_IMAGE_TILING_OPTIMAL;
    format_info.usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    VkExternalImageFormatProperties external_properties{};
    external_properties.sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES;
    VkImageFormatProperties2 format_properties{};
    format_properties.sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2;
    format_properties.pNext = &external_properties;
    if (vkGetPhysicalDeviceImageFormatProperties2(physical_device, &format_info, &format_properties) != VK_SUCCESS || !(external_properties.externalMemoryProperties.externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT))
    {
        return "vulkan cannot import d3d11 textures";
    }
    VkPhysicalDeviceIDProperties id{};
    id.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;
    VkPhysicalDeviceProperties2 properties{};
    properties.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    properties.pNext = &id;
    vkGetPhysicalDeviceProperties2(physical_device, &properties);
    IDXGIFactory1* factory = NULL;
    if (!id.deviceLUIDValid || FAILED(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void**)&factory)))
    {
        return "no dxgi adapter";
    }
    IDXGIAdapter1* adapter = NULL;
    for (UINT i = 0; spout.d3d == NULL && factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; i++)
    {
        DXGI_ADAPTER_DESC1 desc{};
        adapter->GetDesc1(&desc);
        if (memcmp(&desc.AdapterLuid, id.deviceLUID, VK_LUID_SIZE) == 0)
        {
            D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, NULL, 0, NULL, 0, D3D11_SDK_VERSION, &spout.d3d, NULL, NULL);
        }
        adapter->Release();
    }
    factory->Release();
    return spout.d3d == NULL ? "no d3d11 device on the vulkan adapter" : NULL;
}

static const char* spout_texture(VkExtent2D extent)
{
    spout_drop_image();
    D3D11_TEXTURE2D_DESC desc{};
    desc.Width = extent.width;
    desc.Height = extent.height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
    desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
    if (FAILED(spout.d3d->CreateTexture2D(&desc, NULL, &spout.texture)))
    {
        return "d3d11 texture";
    }
    HANDLE handle = NULL;
    IDXGIResource* resource = NULL;
    if (SUCCEEDED(spout.texture->QueryInterface(__uuidof(IDXGIResource), (void**)&resource)))
    {
        resource->GetSharedHandle(&handle);
        resource->Release();
    }
    if (handle == NULL)
    {
        return "d3d11 share handle";
    }
    VkExternalMemoryImageCreateInfo external{};
    external.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO;
    external.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_KMT_BIT;
    VkImageCreateInfo info{};
    info.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
    info.pNext = &external;
    info.imageType = VK_IMAGE_TYPE_2D;
    info.format = color_format;
    info.extent = {extent.width, extent.height, 1};
    info.mipLevels = 1;
    info.arrayLayers = 1;
    info.samples = VK_SAMPLE_COUNT_1_BIT;
    info.tiling = VK_IMAGE_TILING_OPTIMAL;
    info.usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    info.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    if (vkCreateImage(device, &info, NULL, &spout.image) != VK_SUCCESS)
    {
        spout.image = VK_NULL_HANDLE;
        return "vulkan image";
    }
    VkMemoryRequirements requirements;
    vkGetImageMemoryRequirements(device, spout.image, &requirements);
    uint32_t bits = requirements.memoryTypeBits;
    VkMemoryWin32HandlePropertiesKHR handle_properties{};
    handle_properties.sType = VK_STRUCTURE_TYPE_MEMORY_WIN32_HANDLE_PROPERTIES_KHR;
    PFN_vkGetMemoryWin32HandlePropertiesKHR handle_query = (PFN_vkGetMemoryWin32HandlePropertiesKHR)vkGetDeviceProcAddr(device, "vkGetMemoryWin32HandlePropertiesKHR");
    if (handle_query != NULL && handle_query(device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_KMT_BIT, handle, &handle_properties) == VK_SUCCESS)
    {
        bits &= handle_properties.memoryTypeBits;
    }
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical_device, &memory);
    uint32_t chosen = UINT32_MAX;
    for (VkMemoryPropertyFlags wanted : {(VkMemoryPropertyFlags)VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, (VkMemoryPropertyFlags)0})
    {
        for (uint32_t i = 0; i < memory.memoryTypeCount; i++)
        {
            if (chosen == UINT32_MAX && (bits & (1u << i)) && (memory.memoryTypes[i].propertyFlags & wanted) == wanted)
            {
                chosen = i;
            }
        }
    }
    VkMemoryDedicatedAllocateInfo dedicated{};
    dedicated.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO;
    dedicated.image = spout.image;
    VkImportMemoryWin32HandleInfoKHR imported{};
    imported.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_WIN32_HANDLE_INFO_KHR;
    imported.pNext = &dedicated;
    imported.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D11_TEXTURE_KMT_BIT;
    imported.handle = handle;
    VkMemoryAllocateInfo allocate{};
    allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    allocate.pNext = &imported;
    allocate.allocationSize = requirements.size;
    allocate.memoryTypeIndex = chosen;
    if (chosen == UINT32_MAX || vkAllocateMemory(device, &allocate, NULL, &spout.memory) != VK_SUCCESS)
    {
        spout.memory = VK_NULL_HANDLE;
        return "vulkan memory import";
    }
    if (vkBindImageMemory(device, spout.image, spout.memory, 0) != VK_SUCCESS)
    {
        return "vulkan memory bind";
    }
    spout.extent = extent;
    spout_info shared{};
    shared.share_handle = (uint32_t)(uintptr_t)handle;
    shared.width = extent.width;
    shared.height = extent.height;
    shared.format = DXGI_FORMAT_B8G8R8A8_UNORM;
    if (!spout_lock(spout.info_mutex, 67))
    {
        return "sender info busy";
    }
    *spout.info_view = shared;
    ReleaseMutex(spout.info_mutex);
    return NULL;
}

static void spout_release()
{
    if (spout.registered)
    {
        spout_register(false);
    }
    spout_drop_image();
    if (spout.d3d != NULL)
    {
        spout.d3d->Release();
    }
    for (void* view : {(void*)spout.names_view, (void*)spout.info_view, (void*)spout.active_view})
    {
        if (view != NULL)
        {
            UnmapViewOfFile(view);
        }
    }
    for (HANDLE handle : {spout.info, spout.info_mutex, spout.names, spout.names_mutex, spout.active, spout.active_mutex, spout.access})
    {
        if (handle != NULL)
        {
            CloseHandle(handle);
        }
    }
    spout = spout_sender();
}

static bool spout_ready(VkExtent2D extent)
{
    if (spout.failed)
    {
        return false;
    }
    if (spout.image != VK_NULL_HANDLE && spout.extent.width == extent.width && spout.extent.height == extent.height)
    {
        return true;
    }
    const char* reason = can_share ? NULL : "VK_KHR_external_memory_win32 unsupported";
    if (reason == NULL && spout.info == NULL)
    {
        reason = spout_open();
    }
    if (reason == NULL && spout.d3d == NULL)
    {
        reason = spout_device();
    }
    if (reason == NULL)
    {
        reason = spout_texture(extent);
    }
    if (reason != NULL)
    {
        fprintf(stderr, "hikari: spout share unavailable (%s)\n", reason);
        spout_release();
        spout.failed = true;
        return false;
    }
    return true;
}

static void spout_copy(VkCommandBuffer commands, VkImage resolved)
{
    VkImageMemoryBarrier barriers[2] = {};
    for (VkImageMemoryBarrier& barrier : barriers)
    {
        barrier.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
        barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
        barrier.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    }
    barriers[0].image = resolved;
    barriers[0].dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    barriers[0].oldLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    barriers[0].newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    barriers[1].image = spout.image;
    barriers[1].dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    barriers[1].oldLayout = spout.released ? VK_IMAGE_LAYOUT_GENERAL : VK_IMAGE_LAYOUT_UNDEFINED;
    barriers[1].newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    barriers[1].srcQueueFamilyIndex = spout.released ? VK_QUEUE_FAMILY_EXTERNAL : VK_QUEUE_FAMILY_IGNORED;
    barriers[1].dstQueueFamilyIndex = spout.released ? spout.family : VK_QUEUE_FAMILY_IGNORED;
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 2, barriers);
    VkImageCopy region{};
    region.srcSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    region.dstSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    region.extent = {spout.extent.width, spout.extent.height, 1};
    vkCmdCopyImage(commands, resolved, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, spout.image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);
    barriers[0].srcAccessMask = 0;
    barriers[0].dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    barriers[0].oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    barriers[0].newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    barriers[1].srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    barriers[1].dstAccessMask = 0;
    barriers[1].oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    barriers[1].newLayout = VK_IMAGE_LAYOUT_GENERAL;
    barriers[1].srcQueueFamilyIndex = spout.family;
    barriers[1].dstQueueFamilyIndex = VK_QUEUE_FAMILY_EXTERNAL;
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT | VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 2, barriers);
    spout.released = true;
}
#endif

static void release_swapchain_images()
{
    for (size_t i = 0; i < target.views.size(); i++)
    {
        vkDestroyImageView(device, target.views[i], NULL);
        vkDestroySemaphore(device, target.rendered[i], NULL);
    }
    target.views.clear();
    target.rendered.clear();
    target.images.clear();
}

static void release_background()
{
    target.background.Destroy(device);
    target.background = CubismImageVulkan();
    if (target.background_staging.GetBuffer() != VK_NULL_HANDLE)
    {
        target.background_staging.UnMap(device);
        target.background_staging.Destroy(device);
    }
    target.background_staging = CubismBufferVulkan();
}

static void release_target()
{
    wait_idle();
    release_swapchain_images();
    vkDestroySwapchainKHR(device, target.swapchain, NULL);
    vkDestroyPipeline(device, target.pipeline, NULL);
    vkDestroyPipelineLayout(device, target.pipeline_layout, NULL);
    vkDestroyDescriptorPool(device, target.pool, NULL);
    vkDestroyDescriptorSetLayout(device, target.set_layout, NULL);
    for (VkSemaphore semaphore : target.acquired)
    {
        vkDestroySemaphore(device, semaphore, NULL);
    }
    vkDestroySampler(device, target.sampler, NULL);
    release_background();
    vkDestroySurfaceKHR(instance, target.surface, NULL);
    target = present_target();
#ifdef _WIN32
    spout_release();
#endif
}

void window_release_target(window* w)
{
    if (target.owner == w)
    {
        release_target();
    }
}

static void create_pipeline()
{
    VkDescriptorSetLayoutBinding bindings[2] = {};
    for (uint32_t i = 0; i < 2; i++)
    {
        bindings[i].binding = i;
        bindings[i].descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        bindings[i].descriptorCount = 1;
        bindings[i].stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT;
    }
    VkDescriptorSetLayoutCreateInfo set_info{};
    set_info.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO;
    set_info.bindingCount = 2;
    set_info.pBindings = bindings;
    vkCreateDescriptorSetLayout(device, &set_info, NULL, &target.set_layout);
    VkDescriptorPoolSize pool_size = {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 2};
    VkDescriptorPoolCreateInfo pool_info{};
    pool_info.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO;
    pool_info.maxSets = 1;
    pool_info.poolSizeCount = 1;
    pool_info.pPoolSizes = &pool_size;
    vkCreateDescriptorPool(device, &pool_info, NULL, &target.pool);
    VkDescriptorSetAllocateInfo allocate{};
    allocate.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO;
    allocate.descriptorPool = target.pool;
    allocate.descriptorSetCount = 1;
    allocate.pSetLayouts = &target.set_layout;
    vkAllocateDescriptorSets(device, &allocate, &target.set);
    VkPipelineLayoutCreateInfo layout_info{};
    layout_info.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO;
    layout_info.setLayoutCount = 1;
    layout_info.pSetLayouts = &target.set_layout;
    vkCreatePipelineLayout(device, &layout_info, NULL, &target.pipeline_layout);
    VkPipelineShaderStageCreateInfo stages[2] = {};
    stages[0].sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
    stages[0].stage = VK_SHADER_STAGE_VERTEX_BIT;
    stages[0].module = shader_module(present_vert, sizeof(present_vert));
    stages[0].pName = "main";
    stages[1].sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
    stages[1].stage = VK_SHADER_STAGE_FRAGMENT_BIT;
    stages[1].module = shader_module(present_frag, sizeof(present_frag));
    stages[1].pName = "main";
    VkPipelineVertexInputStateCreateInfo vertex_input{};
    vertex_input.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO;
    VkPipelineInputAssemblyStateCreateInfo assembly{};
    assembly.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO;
    assembly.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
    VkPipelineViewportStateCreateInfo viewport{};
    viewport.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO;
    viewport.viewportCount = 1;
    viewport.scissorCount = 1;
    VkPipelineRasterizationStateCreateInfo raster{};
    raster.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO;
    raster.polygonMode = VK_POLYGON_MODE_FILL;
    raster.cullMode = VK_CULL_MODE_NONE;
    raster.lineWidth = 1.0f;
    VkPipelineMultisampleStateCreateInfo multisample{};
    multisample.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO;
    multisample.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
    VkPipelineColorBlendAttachmentState blend_attachment{};
    blend_attachment.colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT | VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT;
    VkPipelineColorBlendStateCreateInfo blend{};
    blend.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO;
    blend.attachmentCount = 1;
    blend.pAttachments = &blend_attachment;
    VkDynamicState dynamic_states[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
    VkPipelineDynamicStateCreateInfo dynamic{};
    dynamic.sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO;
    dynamic.dynamicStateCount = 2;
    dynamic.pDynamicStates = dynamic_states;
    VkPipelineRenderingCreateInfo rendering{};
    rendering.sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO;
    rendering.colorAttachmentCount = 1;
    rendering.pColorAttachmentFormats = &target.format.format;
    VkGraphicsPipelineCreateInfo info{};
    info.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO;
    info.pNext = &rendering;
    info.stageCount = 2;
    info.pStages = stages;
    info.pVertexInputState = &vertex_input;
    info.pInputAssemblyState = &assembly;
    info.pViewportState = &viewport;
    info.pRasterizationState = &raster;
    info.pMultisampleState = &multisample;
    info.pColorBlendState = &blend;
    info.pDynamicState = &dynamic;
    info.layout = target.pipeline_layout;
    vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &info, NULL, &target.pipeline);
    vkDestroyShaderModule(device, stages[0].module, NULL);
    vkDestroyShaderModule(device, stages[1].module, NULL);
}

static bool open_target(window* w)
{
    target.surface = window_surface(w, instance);
    VkBool32 supported = VK_FALSE;
    if (target.surface != VK_NULL_HANDLE)
    {
        vkGetPhysicalDeviceSurfaceSupportKHR(physical_device, (uint32_t)graphics_family(physical_device), target.surface, &supported);
    }
    uint32_t count = 0;
    if (supported)
    {
        vkGetPhysicalDeviceSurfaceFormatsKHR(physical_device, target.surface, &count, NULL);
    }
    if (count == 0)
    {
        vkDestroySurfaceKHR(instance, target.surface, NULL);
        target = present_target();
        return false;
    }
    std::vector<VkSurfaceFormatKHR> formats(count);
    vkGetPhysicalDeviceSurfaceFormatsKHR(physical_device, target.surface, &count, formats.data());
    target.format = formats[0];
    for (const VkSurfaceFormatKHR& format : formats)
    {
        if ((format.format == VK_FORMAT_B8G8R8A8_UNORM || format.format == VK_FORMAT_R8G8B8A8_UNORM) && format.colorSpace == VK_COLOR_SPACE_SRGB_NONLINEAR_KHR)
        {
            target.format = format;
        }
    }
    VkSemaphoreCreateInfo semaphore_info{};
    semaphore_info.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO;
    for (VkSemaphore& semaphore : target.acquired)
    {
        vkCreateSemaphore(device, &semaphore_info, NULL, &semaphore);
    }
    VkSamplerCreateInfo sampler{};
    sampler.sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO;
    sampler.magFilter = VK_FILTER_LINEAR;
    sampler.minFilter = VK_FILTER_LINEAR;
    sampler.addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
    sampler.addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
    sampler.addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
    vkCreateSampler(device, &sampler, NULL, &target.sampler);
    create_pipeline();
    target.owner = w;
    return true;
}

static bool build_swapchain(const VkSurfaceCapabilitiesKHR& capabilities)
{
    wait_idle();
    uint32_t count = 0;
    uint32_t images = capabilities.minImageCount + 1;
    if (capabilities.maxImageCount > 0 && images > capabilities.maxImageCount)
    {
        images = capabilities.maxImageCount;
    }
    VkSwapchainCreateInfoKHR info{};
    info.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR;
    info.surface = target.surface;
    info.minImageCount = images;
    info.imageFormat = target.format.format;
    info.imageColorSpace = target.format.colorSpace;
    info.imageExtent = capabilities.currentExtent;
    info.imageArrayLayers = 1;
    info.imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    info.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
    info.preTransform = capabilities.currentTransform;
    info.compositeAlpha = (capabilities.supportedCompositeAlpha & VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR) ? VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR : (VkCompositeAlphaFlagBitsKHR)(capabilities.supportedCompositeAlpha & (~capabilities.supportedCompositeAlpha + 1));
    info.presentMode = VK_PRESENT_MODE_FIFO_KHR;
    info.clipped = VK_TRUE;
    info.oldSwapchain = target.swapchain;
    VkSwapchainKHR swapchain = VK_NULL_HANDLE;
    VkResult result = vkCreateSwapchainKHR(device, &info, NULL, &swapchain);
    release_swapchain_images();
    vkDestroySwapchainKHR(device, target.swapchain, NULL);
    target.swapchain = swapchain;
    if (result != VK_SUCCESS)
    {
        target.swapchain = VK_NULL_HANDLE;
        return false;
    }
    vkGetSwapchainImagesKHR(device, swapchain, &count, NULL);
    target.images.resize(count);
    vkGetSwapchainImagesKHR(device, swapchain, &count, target.images.data());
    VkSemaphoreCreateInfo semaphore_info{};
    semaphore_info.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO;
    for (VkImage image : target.images)
    {
        VkImageViewCreateInfo view_info{};
        view_info.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
        view_info.image = image;
        view_info.viewType = VK_IMAGE_VIEW_TYPE_2D;
        view_info.format = target.format.format;
        view_info.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VkImageView view = VK_NULL_HANDLE;
        vkCreateImageView(device, &view_info, NULL, &view);
        target.views.push_back(view);
        VkSemaphore semaphore = VK_NULL_HANDLE;
        vkCreateSemaphore(device, &semaphore_info, NULL, &semaphore);
        target.rendered.push_back(semaphore);
    }
    target.extent = capabilities.currentExtent;
    return true;
}

static void show_background(engine* e, float background_clock_ms)
{
    static const uint8_t black[4] = {0, 0, 0, 255};
    const uint8_t* source = background_at(e, background_clock_ms);
    if (target.background.GetImage() != VK_NULL_HANDLE && source == target.shown_source && e->background_serial == target.shown_serial && target.extent.width == target.shown_extent.width && target.extent.height == target.shown_extent.height)
    {
        return;
    }
    const uint8_t* pixels = source != NULL ? source : black;
    int w = source != NULL ? e->width : 1;
    int h = source != NULL ? e->height : 1;
    double scale = std::max((double)target.extent.width / w, (double)target.extent.height / h);
    int crop_width = std::min(w, std::max(1, (int)lround(target.extent.width / scale)));
    int crop_height = std::min(h, std::max(1, (int)lround(target.extent.height / scale)));
    std::vector<uint8_t> crop((size_t)crop_width * crop_height * 4);
    for (int y = 0; y < crop_height; y++)
    {
        memcpy(&crop[(size_t)y * crop_width * 4], pixels + ((size_t)(y + (h - crop_height) / 2) * w + (w - crop_width) / 2) * 4, (size_t)crop_width * 4);
    }
    uint32_t levels = scale < 1.0 ? mip_levels((uint32_t)crop_width, (uint32_t)crop_height) : 1;
    size_t staging_bytes = std::min(crop.size(), staging_size);
    vkWaitForFences(device, 2, e->gpu->fences, VK_TRUE, UINT64_MAX);
    if (target.background.GetImage() != VK_NULL_HANDLE && crop_width == target.background_size[0] && crop_height == target.background_size[1] && levels == target.background_levels)
    {
        fill_texture(target.background_staging, staging_bytes, crop.data(), crop_width, crop_height, levels, target.background);
    }
    else
    {
        release_background();
        target.background_staging.CreateBuffer(device, physical_device, staging_bytes, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
        target.background_staging.Map(device, staging_bytes);
        upload_texture(target.background_staging, staging_bytes, crop.data(), crop_width, crop_height, levels, target.background);
        target.background_size[0] = crop_width;
        target.background_size[1] = crop_height;
        target.background_levels = levels;
        target.bound_generation = 0;
    }
    target.shown_source = source;
    target.shown_serial = e->background_serial;
    target.shown_extent = target.extent;
}

bool engine_present(engine* e, window* w, float background_clock_ms, float frame_seconds, bool share)
{
    if (!can_present || w == NULL)
    {
        return false;
    }
    target.deadline += std::chrono::duration_cast<std::chrono::steady_clock::duration>(std::chrono::duration<double>(frame_seconds));
    std::chrono::steady_clock::time_point now = std::chrono::steady_clock::now();
    if (target.deadline > now)
    {
        std::this_thread::sleep_until(target.deadline);
    }
    else
    {
        target.deadline = now;
    }
    if (target.owner != w)
    {
        if (target.owner != NULL)
        {
            release_target();
        }
        if (!open_target(w))
        {
            return false;
        }
    }
    int polled[2] = {0, 0};
    window_size(w, &polled[0], &polled[1]);
    if (target.swapchain == VK_NULL_HANDLE || target.extent.width == 0 || polled[0] != target.polled[0] || polled[1] != target.polled[1])
    {
        VkSurfaceCapabilitiesKHR capabilities;
        if (vkGetPhysicalDeviceSurfaceCapabilitiesKHR(physical_device, target.surface, &capabilities) != VK_SUCCESS || capabilities.currentExtent.width == 0 || capabilities.currentExtent.height == 0 || capabilities.currentExtent.width == UINT32_MAX)
        {
            return false;
        }
        if (target.swapchain == VK_NULL_HANDLE || capabilities.currentExtent.width != target.extent.width || capabilities.currentExtent.height != target.extent.height)
        {
            if (!build_swapchain(capabilities))
            {
                return false;
            }
        }
        target.polled[0] = polled[0];
        target.polled[1] = polled[1];
    }
    VkCommandBuffer commands = renderer_begin(e->gpu);
    VkSemaphore acquire = target.acquired[e->gpu->slot];
    uint32_t index = 0;
    VkResult acquired = vkAcquireNextImageKHR(device, target.swapchain, UINT64_MAX, acquire, VK_NULL_HANDLE, &index);
    if (acquired != VK_SUCCESS && acquired != VK_SUBOPTIMAL_KHR)
    {
        target.extent = VkExtent2D{};
        return false;
    }
    show_background(e, background_clock_ms);
    int width = e->width;
    int height = e->height;
    e->width = (int)target.extent.width;
    e->height = (int)target.extent.height;
    view_matrix(e);
    update_shown(e);
    renderer_draw(e->gpu, commands, target.extent, e->mvp.GetArray(), VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, VK_ACCESS_SHADER_READ_BIT);
    e->width = width;
    e->height = height;
    view_matrix(e);
#ifdef _WIN32
    bool shared = share && spout_ready(target.extent) && spout_lock(spout.access, 2);
    if (shared)
    {
        spout_copy(commands, e->gpu->resolved.image);
    }
    if (!share && spout.info != NULL)
    {
        spout_release();
    }
#endif
    if (target.bound_generation != e->gpu->generation)
    {
        vkWaitForFences(device, 2, e->gpu->fences, VK_TRUE, UINT64_MAX);
        VkDescriptorImageInfo images[2] = {
            {target.sampler, e->gpu->resolved.view, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL},
            {target.background.GetSampler(), target.background.GetView(), VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL}};
        VkWriteDescriptorSet write{};
        write.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        write.dstSet = target.set;
        write.descriptorCount = 2;
        write.descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
        write.pImageInfo = images;
        vkUpdateDescriptorSets(device, 1, &write, 0, NULL);
        target.bound_generation = e->gpu->generation;
    }
    VkImageMemoryBarrier barrier{};
    barrier.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    barrier.dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    barrier.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    barrier.newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.image = target.images[index];
    barrier.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0, 0, NULL, 0, NULL, 1, &barrier);
    VkRenderingAttachmentInfo attachment{};
    attachment.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO;
    attachment.imageView = target.views[index];
    attachment.imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    attachment.loadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
    attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
    VkRenderingInfo rendering{};
    rendering.sType = VK_STRUCTURE_TYPE_RENDERING_INFO;
    rendering.renderArea = {{0, 0}, target.extent};
    rendering.layerCount = 1;
    rendering.colorAttachmentCount = 1;
    rendering.pColorAttachments = &attachment;
    vkCmdBeginRendering(commands, &rendering);
    VkViewport viewport = {0, 0, (float)target.extent.width, (float)target.extent.height, 0, 1};
    VkRect2D scissor = {{0, 0}, target.extent};
    vkCmdSetViewport(commands, 0, 1, &viewport);
    vkCmdSetScissor(commands, 0, 1, &scissor);
    vkCmdBindPipeline(commands, VK_PIPELINE_BIND_POINT_GRAPHICS, target.pipeline);
    vkCmdBindDescriptorSets(commands, VK_PIPELINE_BIND_POINT_GRAPHICS, target.pipeline_layout, 0, 1, &target.set, 0, NULL);
    vkCmdDraw(commands, 3, 1, 0, 0);
    vkCmdEndRendering(commands);
    barrier.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    barrier.dstAccessMask = 0;
    barrier.oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    barrier.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &barrier);
    renderer_submit(e->gpu, acquire, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, target.rendered[index]);
#ifdef _WIN32
    if (shared)
    {
        vkWaitForFences(device, 1, &e->gpu->fences[e->gpu->slot ^ 1], VK_TRUE, UINT64_MAX);
        ReleaseMutex(spout.access);
    }
#endif
    VkPresentInfoKHR present{};
    present.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR;
    present.waitSemaphoreCount = 1;
    present.pWaitSemaphores = &target.rendered[index];
    present.swapchainCount = 1;
    present.pSwapchains = &target.swapchain;
    present.pImageIndices = &index;
    queue_lock.lock();
    VkResult presented = vkQueuePresentKHR(queue, &present);
    queue_lock.unlock();
    if (presented != VK_SUCCESS || acquired == VK_SUBOPTIMAL_KHR)
    {
        target.extent = VkExtent2D{};
    }
    return presented == VK_SUCCESS || presented == VK_SUBOPTIMAL_KHR;
}
