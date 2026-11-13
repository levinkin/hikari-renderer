#include <atomic>
#include <chrono>
#include <cstdio>
#include <filesystem>
#include <thread>
#include "bc7enc/bc7enc.c"
#include "bc7enc/bc7decomp.cpp"

static const uint32_t bc7_version = 2;
static const double bc7_max_mae = 1.0;
static const int bc7_max_error = 160;
static const size_t staging_size = 4 << 20;

struct bc7_header
{
    char magic[4];
    uint32_t version;
    uint32_t width;
    uint32_t height;
    uint32_t levels;
    uint64_t checksum;
};

struct bc7_job
{
    const uint8_t* pixels;
    uint32_t width;
    uint32_t height;
    uint8_t* blocks;
    std::atomic<uint32_t>* next_row;
    uint64_t error_sum;
    uint64_t channels;
    int error_max;
};

static uint32_t mip_levels(uint32_t w, uint32_t h)
{
    uint32_t levels = 1;
    for (uint32_t size = w > h ? w : h; size > 1; size /= 2)
    {
        levels++;
    }
    return levels;
}

static size_t bc7_level_bytes(uint32_t w, uint32_t h, uint32_t level)
{
    size_t blocks_w = (std::max(w >> level, 1u) + 3) / 4;
    size_t blocks_h = (std::max(h >> level, 1u) + 3) / 4;
    return blocks_w * blocks_h * 16;
}

static uint64_t bc7_hash(uint64_t hash, const void* data, size_t size)
{
    const uint8_t* bytes = (const uint8_t*)data;
    for (size_t i = 0; i < size; i++)
    {
        hash = (hash ^ bytes[i]) * 1099511628211ull;
    }
    return hash;
}

static uint64_t bc7_header_hash(const std::string& path, const bc7_header& header)
{
    std::string key = std::filesystem::path(path).filename().string();
    uint32_t fields[4] = {header.version, header.width, header.height, header.levels};
    return bc7_hash(bc7_hash(14695981039346656037ull, key.data(), key.size()), fields, sizeof(fields));
}

static std::string bc7_cache_path(const std::vector<uint8_t>& bytes)
{
    uint64_t hash = bc7_hash(14695981039346656037ull, bytes.data(), bytes.size());
    hash = (hash ^ bc7_version) * 1099511628211ull;
#if defined(_WIN32)
    const char* base = getenv("LOCALAPPDATA");
    std::string dir = base == NULL ? "" : std::string(base) + "/hikari";
#elif defined(__APPLE__)
    const char* base = getenv("HOME");
    std::string dir = base == NULL ? "" : std::string(base) + "/Library/Caches/hikari";
#else
    const char* xdg = getenv("XDG_CACHE_HOME");
    const char* base = getenv("HOME");
    std::string dir = xdg != NULL && xdg[0] == '/' ? std::string(xdg) + "/hikari" : base == NULL ? "" : std::string(base) + "/.cache/hikari";
#endif
    std::error_code error;
    if (dir.empty() || (!std::filesystem::create_directories(dir, error) && error))
    {
        return "";
    }
    char name[32];
    snprintf(name, sizeof(name), "%016llx.bc7", (unsigned long long)hash);
    return dir + "/" + name;
}

static FILE* bc7_open(const std::string& path, bc7_header& header)
{
    FILE* file = path.empty() ? NULL : fopen(path.c_str(), "rb");
    if (file == NULL)
    {
        return NULL;
    }
    size_t expected = sizeof(header);
    bool valid = fread(&header, sizeof(header), 1, file) == 1 && memcmp(header.magic, "HBC7", 4) == 0 && header.version == bc7_version && header.width > 0 && header.height > 0 && header.width <= 16384 && header.height <= 16384 && (header.levels == 0 || header.levels == mip_levels(header.width, header.height));
    for (uint32_t level = 0; valid && level < header.levels; level++)
    {
        expected += bc7_level_bytes(header.width, header.height, level);
    }
    valid = valid && fseek(file, 0, SEEK_END) == 0 && (size_t)ftell(file) == expected && fseek(file, sizeof(header), SEEK_SET) == 0;
    uint64_t checksum = valid ? bc7_header_hash(path, header) : 0;
    std::vector<uint8_t> chunk(staging_size);
    for (size_t left = expected - sizeof(header); valid && left > 0; left -= chunk.size())
    {
        chunk.resize(std::min(left, staging_size));
        valid = fread(chunk.data(), 1, chunk.size(), file) == chunk.size();
        checksum = bc7_hash(checksum, chunk.data(), chunk.size());
    }
    if (!valid || checksum != header.checksum || fseek(file, sizeof(header), SEEK_SET) != 0)
    {
        fclose(file);
        return NULL;
    }
    return file;
}

static void bc7_encode_rows(bc7_job* job)
{
    bc7enc_compress_block_params params;
    bc7enc_compress_block_params_init(&params);
    bc7enc_compress_block_params_init_linear_weights(&params);
    uint32_t blocks_w = (job->width + 3) / 4;
    uint32_t blocks_h = (job->height + 3) / 4;
    for (uint32_t by = job->next_row->fetch_add(1); by < blocks_h; by = job->next_row->fetch_add(1))
    {
        for (uint32_t bx = 0; bx < blocks_w; bx++)
        {
            uint8_t source[64];
            for (uint32_t i = 0; i < 16; i++)
            {
                uint32_t x = std::min(bx * 4 + i % 4, job->width - 1);
                uint32_t y = std::min(by * 4 + i / 4, job->height - 1);
                memcpy(source + i * 4, job->pixels + ((size_t)y * job->width + x) * 4, 4);
            }
            uint8_t* block = job->blocks + ((size_t)by * blocks_w + bx) * 16;
            bc7enc_compress_block(block, source, &params);
            bc7decomp::color_rgba decoded[16];
            bc7decomp::unpack_bc7(block, decoded);
            for (uint32_t i = 0; i < 16; i++)
            {
                if (bx * 4 + i % 4 >= job->width || by * 4 + i / 4 >= job->height || source[i * 4 + 3] == 0)
                {
                    continue;
                }
                for (int c = 0; c < 4; c++)
                {
                    int error = std::abs((int)decoded[i].m_comps[c] - (int)source[i * 4 + c]);
                    job->error_sum += error;
                    job->error_max = std::max(job->error_max, error);
                }
                job->channels += 4;
            }
        }
    }
}

static bool bc7_encode(const uint8_t* pixels, uint32_t w, uint32_t h, const std::string& path)
{
    if (path.empty())
    {
        return false;
    }
    bc7enc_compress_block_init();
    bc7_header header = {{'H', 'B', 'C', '7'}, bc7_version, w, h, mip_levels(w, h), 0};
    std::vector<uint8_t> blocks;
    std::vector<uint8_t> mip;
    const uint8_t* level_pixels = pixels;
    uint64_t error_sum = 0;
    uint64_t channels = 0;
    int error_max = 0;
    uint32_t thread_count = std::max(1u, std::thread::hardware_concurrency());
    for (uint32_t level = 0; level < header.levels; level++)
    {
        uint32_t lw = std::max(w >> level, 1u);
        uint32_t lh = std::max(h >> level, 1u);
        if (level > 0)
        {
            std::vector<uint8_t> next((size_t)lw * lh * 4);
            uint32_t pw = std::max(w >> (level - 1), 1u);
            uint32_t ph = std::max(h >> (level - 1), 1u);
            for (uint32_t y = 0; y < lh; y++)
            {
                for (uint32_t x = 0; x < lw; x++)
                {
                    uint32_t x0 = std::min(x * 2, pw - 1);
                    uint32_t x1 = std::min(x * 2 + 1, pw - 1);
                    uint32_t y0 = std::min(y * 2, ph - 1);
                    uint32_t y1 = std::min(y * 2 + 1, ph - 1);
                    for (int c = 0; c < 4; c++)
                    {
                        uint32_t sum = level_pixels[((size_t)y0 * pw + x0) * 4 + c] + level_pixels[((size_t)y0 * pw + x1) * 4 + c] + level_pixels[((size_t)y1 * pw + x0) * 4 + c] + level_pixels[((size_t)y1 * pw + x1) * 4 + c];
                        next[((size_t)y * lw + x) * 4 + c] = (uint8_t)((sum + 2) / 4);
                    }
                }
            }
            mip.swap(next);
            level_pixels = mip.data();
        }
        size_t offset = blocks.size();
        blocks.resize(offset + bc7_level_bytes(w, h, level));
        std::atomic<uint32_t> next_row(0);
        uint32_t level_threads = std::min(thread_count, (lh + 3) / 4);
        std::vector<bc7_job> jobs(level_threads, bc7_job{level_pixels, lw, lh, blocks.data() + offset, &next_row, 0, 0, 0});
        std::vector<std::thread> threads;
        for (uint32_t i = 1; i < level_threads; i++)
        {
            threads.emplace_back(bc7_encode_rows, &jobs[i]);
        }
        bc7_encode_rows(&jobs[0]);
        for (std::thread& thread : threads)
        {
            thread.join();
        }
        for (bc7_job& job : jobs)
        {
            error_sum += job.error_sum;
            channels += job.channels;
            error_max = std::max(error_max, job.error_max);
        }
    }
    double mae = channels == 0 ? 0.0 : (double)error_sum / channels;
    bool accepted = mae <= bc7_max_mae && error_max <= bc7_max_error;
    fprintf(stderr, "engine: bc7 %ux%u mae=%.3f max=%d %s\n", w, h, mae, error_max, accepted ? "compressed" : "kept uncompressed");
    if (!accepted)
    {
        header.levels = 0;
    }
    header.checksum = bc7_hash(bc7_header_hash(path, header), blocks.data(), accepted ? blocks.size() : 0);
    std::string temporary = path + "." + std::to_string(std::chrono::steady_clock::now().time_since_epoch().count());
    FILE* file = fopen(temporary.c_str(), "wb");
    if (file == NULL)
    {
        return false;
    }
    bool written = fwrite(&header, sizeof(header), 1, file) == 1 && (!accepted || fwrite(blocks.data(), 1, blocks.size(), file) == blocks.size());
    written = fclose(file) == 0 && written;
    std::error_code error;
    if (written)
    {
        std::filesystem::rename(temporary, path, error);
    }
    if (!written || error)
    {
        std::filesystem::remove(temporary, error);
        return false;
    }
    return accepted;
}
