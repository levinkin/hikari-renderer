#include <filesystem>
#include "Motion/CubismMotion.hpp"
#include "Motion/CubismMotionInternal.hpp"
#include "Motion/CubismMotionJson.hpp"
#include "Motion/CubismMotionQueueEntry.hpp"

static const float kBackgroundStepMs = 2000.0f / 24.0f;

class engine_allocator : public ICubismAllocator
{
    void* Allocate(const csmSizeType size)
    {
        return malloc(size);
    }
    void Deallocate(void* memory)
    {
        free(memory);
    }
    void* AllocateAligned(const csmSizeType size, const csmUint32 alignment)
    {
        size_t align = alignment < sizeof(void*) ? sizeof(void*) : alignment;
#ifdef _WIN32
        return _aligned_malloc(size, align);
#else
        void* memory = NULL;
        posix_memalign(&memory, align, size);
        return memory;
#endif
    }
    void DeallocateAligned(void* memory)
    {
#ifdef _WIN32
        _aligned_free(memory);
#else
        free(memory);
#endif
    }
};

struct physics_scale
{
    csmInt32 index;
    float strength;
    float wind_multiplier;
};

struct shared_moc
{
    CubismMoc* moc = NULL;
    int users = 0;
};

static std::map<std::string, shared_moc> moc_cache;
static std::mutex moc_lock;

struct engine : public CubismUserModel, public engine_gpu
{
    using CubismUserModel::_moc;
    using CubismUserModel::_model;
    using CubismUserModel::_modelMatrix;
    using CubismUserModel::_motionManager;
    using CubismUserModel::_expressionManager;
    using CubismUserModel::_eyeBlink;
    using CubismUserModel::_breath;
    using CubismUserModel::_pose;
    using CubismUserModel::_physics;
    ICubismModelSetting* setting = NULL;
    std::map<std::string, ACubismMotion*> expressions;
    csmVector<CubismBreath::BreathParameterData> breath_parameters;
    CubismMatrix44 mvp;
    int width = 0;
    int height = 0;
    std::vector<uint8_t> rgba;
    std::vector<float> shown_params;
    std::vector<float> shown_parts;
    bool update_pending = false;
    std::vector<float> carry_params;
    std::vector<float> carry_parts;
    float carry_left_ms = 0.0f;
    float carry_total_ms = 0.0f;
    std::vector<std::vector<uint8_t>> background_frames;
    std::vector<float> background_ms;
    float background_total_ms = 0;
    float view_zoom = 1;
    float view_x = 0;
    float view_y = 0;
    float view_rot = 0;
    float physics_weight = 1;
    float wind = 0;
    double wind_clock = 0;
    float wind_phase[4] = {0, 0, 0, 0};
    std::vector<physics_scale> physics_scales;
    std::vector<float> pre_physics;
    std::vector<std::unique_ptr<CubismExpressionMotionManager>> kept_expressions;
    std::unique_ptr<CubismExpressionMotionManager> pose_expressions;
    std::string moc_path;
    std::map<std::string, std::vector<ACubismMotion*>> motions;
    std::vector<ACubismMotion*>* idle = NULL;
    bool idle_paused = false;
    uint32_t motion_seed = 0;
    unsigned int blink_seed = 1;
    csmVector<CubismIdHandle> blink_ids;
    csmVector<CubismIdHandle> lip_ids;
    std::vector<float> blink_open;
    std::map<ACubismMotion*, std::vector<csmInt32>> motion_params;
    std::vector<char> rest_marks;

    ~engine()
    {
        for (auto& group : motions)
        {
            for (ACubismMotion* motion : group.second)
            {
                ACubismMotion::Delete(motion);
            }
        }
        if (_moc == NULL)
        {
            return;
        }
        std::lock_guard<std::mutex> guard(moc_lock);
        if (_model != NULL)
        {
            _moc->DeleteModel(_model);
            _model = NULL;
        }
        shared_moc& shared = moc_cache[moc_path];
        if (--shared.users == 0)
        {
            CubismMoc::Delete(shared.moc);
            moc_cache.erase(moc_path);
        }
        _moc = NULL;
    }
};

static engine_allocator allocator;
static std::string gpu_name;

static std::vector<uint8_t> read_file(const std::string& path)
{
    std::ifstream file(path, std::ios::binary);
    return std::vector<uint8_t>(std::istreambuf_iterator<char>(file), std::istreambuf_iterator<char>());
}

static bool motion_segments_ok(const std::vector<uint8_t>& bytes)
{
    Utils::CubismJson* json = Utils::CubismJson::Create(bytes.data(), (csmSizeInt)bytes.size());
    if (json == NULL)
    {
        return false;
    }
    Utils::Value& curves = json->GetRoot()["Curves"];
    bool ok = curves.IsArray();
    for (csmInt32 c = 0; ok && c < curves.GetSize(); c++)
    {
        Utils::Value& segments = curves[c]["Segments"];
        ok = segments.IsArray() && segments.GetSize() >= 2;
        for (csmInt32 i = 2; ok && i < segments.GetSize();)
        {
            csmInt32 tag = (csmInt32)segments[i].ToFloat();
            csmInt32 length = tag == CubismMotionSegmentType_Bezier ? 7 : 3;
            ok = tag >= CubismMotionSegmentType_Linear && tag <= CubismMotionSegmentType_InverseStepped && i + length <= segments.GetSize();
            i += length;
        }
    }
    Utils::CubismJson::Delete(json);
    return ok;
}

static void motion_part_opacities(engine* e, const std::string& dir)
{
    std::vector<std::string> files;
    for (csmInt32 g = 0; g < e->setting->GetMotionGroupCount(); g++)
    {
        const csmChar* group = e->setting->GetMotionGroupName(g);
        for (csmInt32 i = 0; i < e->setting->GetMotionCount(group); i++)
        {
            files.push_back(dir + e->setting->GetMotionFileName(group, i));
        }
    }
    if (files.empty())
    {
        std::error_code error;
        for (std::filesystem::recursive_directory_iterator it(dir, error), end; !error && it != end; it.increment(error))
        {
            std::string file = it->path().string();
            if (file.size() > 13 && file.compare(file.size() - 13, 13, ".motion3.json") == 0)
            {
                files.push_back(file);
            }
        }
        std::sort(files.begin(), files.end());
    }
    std::map<csmInt32, std::pair<float, bool>> parts;
    for (const std::string& file : files)
    {
        std::vector<uint8_t> bytes = read_file(file);
        if (bytes.empty())
        {
            continue;
        }
        CubismMotionJson json(bytes.data(), (csmSizeInt)bytes.size());
        if (!motion_segments_ok(bytes) || !json.HasConsistency())
        {
            continue;
        }
        for (csmInt32 c = 0; c < json.GetMotionCurveCount(); c++)
        {
            if (strcmp(json.GetMotionCurveTarget(c), "PartOpacity") != 0 || json.GetMotionCurveSegmentCount(c) < 2)
            {
                continue;
            }
            csmInt32 index = e->_model->GetPartIndex(json.GetMotionCurveId(c));
            if (index >= e->_model->GetPartCount())
            {
                continue;
            }
            float value = json.GetMotionCurveSegment(c, 1);
            auto found = parts.emplace(index, std::make_pair(value, false));
            found.first->second.second |= found.first->second.first != value;
        }
    }
    for (const auto& part : parts)
    {
        if (part.second.second)
        {
            e->_model->SetPartOpacity(part.first, part.second.first);
        }
    }
}

static ACubismMotion* motion_load(engine* e, const std::string& path, const csmChar* group, csmInt32 index)
{
    std::vector<uint8_t> bytes = read_file(path);
    if (bytes.empty())
    {
        return NULL;
    }
    CubismMotion* motion = motion_segments_ok(bytes) ? static_cast<CubismMotion*>(e->LoadMotion(bytes.data(), (csmSizeInt)bytes.size(), path.c_str(), NULL, NULL, group == NULL ? NULL : e->setting, group, index, true)) : NULL;
    if (motion == NULL)
    {
        fprintf(stderr, "hikari: skipping inconsistent motion %s\n", path.c_str());
        return NULL;
    }
    motion->SetEffectIds(e->blink_ids, e->lip_ids);
    CubismMotionJson json(bytes.data(), (csmSizeInt)bytes.size());
    std::vector<csmInt32>& params = e->motion_params[motion];
    for (csmInt32 curve = 0; curve < json.GetMotionCurveCount(); curve++)
    {
        if (strcmp(json.GetMotionCurveTarget(curve), "Parameter") != 0)
        {
            continue;
        }
        csmInt32 index = e->_model->GetParameterIndex(json.GetMotionCurveId(curve));
        if (index < e->_model->GetParameterCount())
        {
            params.push_back(index);
        }
    }
    return motion;
}

static ACubismMotion* motion_pick(engine* e, const std::vector<ACubismMotion*>& group)
{
    e->motion_seed = e->motion_seed * 1664525u + 1013904223u;
    return group[(e->motion_seed >> 8) % group.size()];
}

static void view_matrix(engine* e)
{
    float canvas_h = e->_model->GetCanvasHeightPixel();
    float ppu = e->_model->GetPixelsPerUnit();
    float base = e->height / (0.315f * canvas_h);
    float scale = ppu * base * e->view_zoom;
    float lift = e->view_zoom * (1.0f - canvas_h * base / e->height) * e->height * 0.5f;
    float c = cosf(e->view_rot * (float)M_PI / 180.0f);
    float s = sinf(e->view_rot * (float)M_PI / 180.0f);
    float matrix[16] = {
        2.0f * c * scale / e->width, 2.0f * s * scale / e->height, 0, 0,
        -2.0f * s * scale / e->width, 2.0f * c * scale / e->height, 0, 0,
        0, 0, 1, 0,
        -2.0f * s * lift / e->width + 2.0f * e->view_x, 2.0f * c * lift / e->height + 2.0f * e->view_y, 0, 1};
    e->mvp.SetMatrix(matrix);
}

static engine* load_model(const std::string& path, const std::string& dir, int width, int height)
{
    std::vector<uint8_t> json = read_file(path);
    if (json.empty())
    {
        return NULL;
    }
    engine* e = new engine();
    e->setting = new CubismModelSettingJson(json.data(), (csmSizeInt)json.size());
    std::string moc_path = dir + e->setting->GetModelFileName();
    {
        std::lock_guard<std::mutex> guard(moc_lock);
        shared_moc& shared = moc_cache[moc_path];
        if (shared.moc == NULL)
        {
            std::vector<uint8_t> moc = read_file(moc_path);
            shared.moc = CubismMoc::Create(moc.data(), (csmSizeInt)moc.size());
        }
        if (shared.moc == NULL)
        {
            moc_cache.erase(moc_path);
        }
        else
        {
            shared.users++;
            e->_moc = shared.moc;
            e->moc_path = moc_path;
            e->_model = shared.moc->CreateModel();
        }
    }
    if (e->_model != NULL)
    {
        e->_model->SaveParameters();
        e->_modelMatrix = CSM_NEW CubismModelMatrix(e->_model->GetCanvasWidth(), e->_model->GetCanvasHeight());
    }
    if (e->_model == NULL)
    {
        delete e->setting;
        delete e;
        return NULL;
    }
    for (csmInt32 i = 0; i < e->setting->GetExpressionCount(); i++)
    {
        std::string file = e->setting->GetExpressionFileName(i);
        std::string name = file.substr(file.find_last_of("/\\") + 1);
        name = name.substr(0, name.find(".exp3.json"));
        std::vector<uint8_t> bytes = read_file(dir + file);
        ACubismMotion* motion = e->LoadExpression(bytes.data(), (csmSizeInt)bytes.size(), name.c_str());
        if (motion != NULL)
        {
            e->expressions[name] = motion;
        }
    }
    static const char empty_expression[] = "{\"Type\":\"Live2D Expression\",\"FadeInTime\":0.3,\"FadeOutTime\":0.3,\"Parameters\":[]}";
    e->expressions[""] = e->LoadExpression((const csmByte*)empty_expression, (csmSizeInt)strlen(empty_expression), "");
    e->pose_expressions.reset(new CubismExpressionMotionManager());
    if (strcmp(e->setting->GetPhysicsFileName(), "") != 0)
    {
        std::vector<uint8_t> bytes = read_file(dir + e->setting->GetPhysicsFileName());
        e->LoadPhysics(bytes.data(), (csmSizeInt)bytes.size());
    }
    if (strcmp(e->setting->GetPoseFileName(), "") != 0)
    {
        std::vector<uint8_t> bytes = read_file(dir + e->setting->GetPoseFileName());
        e->LoadPose(bytes.data(), (csmSizeInt)bytes.size());
    }
    else
    {
        motion_part_opacities(e, dir);
    }
    if (e->setting->GetEyeBlinkParameterCount() > 0)
    {
        e->_eyeBlink = CubismEyeBlink::Create(e->setting);
    }
    for (csmInt32 i = 0; i < e->setting->GetEyeBlinkParameterCount(); i++)
    {
        e->blink_ids.PushBack(e->setting->GetEyeBlinkParameterId(i));
    }
    for (csmInt32 i = 0; i < e->setting->GetLipSyncParameterCount(); i++)
    {
        e->lip_ids.PushBack(e->setting->GetLipSyncParameterId(i));
    }
    for (csmInt32 g = 0; g < e->setting->GetMotionGroupCount(); g++)
    {
        const csmChar* group = e->setting->GetMotionGroupName(g);
        for (csmInt32 i = 0; i < e->setting->GetMotionCount(group); i++)
        {
            ACubismMotion* motion = motion_load(e, dir + e->setting->GetMotionFileName(group, i), group, i);
            if (motion != NULL)
            {
                e->motions[group].push_back(motion);
            }
        }
    }
    e->_breath = CubismBreath::Create();
    e->breath_parameters.PushBack(CubismBreath::BreathParameterData(CubismFramework::GetIdManager()->GetId("ParamAngleX"), 0.0f, 15.0f, 6.5345f, 0.5f));
    e->breath_parameters.PushBack(CubismBreath::BreathParameterData(CubismFramework::GetIdManager()->GetId("ParamAngleY"), 0.0f, 8.0f, 3.5345f, 0.5f));
    e->breath_parameters.PushBack(CubismBreath::BreathParameterData(CubismFramework::GetIdManager()->GetId("ParamAngleZ"), 0.0f, 10.0f, 5.5345f, 0.5f));
    e->breath_parameters.PushBack(CubismBreath::BreathParameterData(CubismFramework::GetIdManager()->GetId("ParamBodyAngleX"), 0.0f, 4.0f, 15.5345f, 0.5f));
    e->breath_parameters.PushBack(CubismBreath::BreathParameterData(CubismFramework::GetIdManager()->GetId("ParamBreath"), 0.0f, 0.5f, 3.2345f, 0.5f));
    e->_breath->SetParameters(e->breath_parameters);
    e->width = width;
    e->height = height;
    uint32_t seed = 2166136261u;
    for (char c : path)
    {
        seed = (seed ^ (uint8_t)c) * 16777619u;
    }
    for (float& phase : e->wind_phase)
    {
        seed = seed * 1664525u + 1013904223u;
        phase = (seed >> 8) * (6.2831853f / 16777216.0f);
    }
    e->motion_seed = seed;
    e->blink_seed = seed * 1664525u + 1013904223u;
    return e;
}

bool engine_view_set(engine* e, float zoom, float x, float y, float rot)
{
    float before[4] = {e->view_zoom, e->view_x, e->view_y, e->view_rot};
    if (std::isfinite(zoom) && zoom > 0)
    {
        e->view_zoom = zoom;
    }
    if (std::isfinite(x))
    {
        e->view_x = x;
    }
    if (std::isfinite(y))
    {
        e->view_y = y;
    }
    if (std::isfinite(rot))
    {
        e->view_rot = rot;
    }
    if (before[0] == e->view_zoom && before[1] == e->view_x && before[2] == e->view_y && before[3] == e->view_rot)
    {
        return false;
    }
    view_matrix(e);
    return true;
}

const char* engine_gpu_name(engine* e)
{
    return gpu_name.c_str();
}

bool engine_has_param(engine* e, const char* id)
{
    return e->_model->GetParameterIndex(CubismFramework::GetIdManager()->GetId(id)) < (csmInt32)e->_model->GetParameterCount();
}

float engine_param_get(engine* e, const char* id)
{
    return e->_model->GetParameterValue(CubismFramework::GetIdManager()->GetId(id));
}

void engine_param_set(engine* e, const char* id, float value)
{
    e->_model->SetParameterValue(CubismFramework::GetIdManager()->GetId(id), value);
}

bool engine_expression(engine* e, int layer, const char* name)
{
    auto found = e->expressions.find(name);
    if (found == e->expressions.end())
    {
        return false;
    }
    (layer == 0 ? e->_expressionManager : e->pose_expressions.get())->StartMotion(found->second, false);
    return true;
}

bool engine_expression_add(engine* e, const char* name, const char* path)
{
    std::vector<uint8_t> bytes = read_file(path);
    ACubismMotion* motion = bytes.empty() ? NULL : e->LoadExpression(bytes.data(), (csmSizeInt)bytes.size(), name);
    if (motion == NULL)
    {
        return false;
    }
    auto found = e->expressions.find(name);
    if (found != e->expressions.end())
    {
        ACubismMotion::Delete(found->second);
    }
    e->expressions[name] = motion;
    return true;
}

bool engine_expression_keep(engine* e, const char* name)
{
    auto found = e->expressions.find(name);
    if (found == e->expressions.end())
    {
        return false;
    }
    e->kept_expressions.emplace_back(new CubismExpressionMotionManager());
    e->kept_expressions.back()->StartMotion(found->second, false);
    return true;
}

void engine_expression_fade(engine* e, const char* name, float seconds)
{
    auto found = e->expressions.find(name);
    if (found == e->expressions.end())
    {
        return;
    }
    found->second->SetFadeInTime(seconds);
    found->second->SetFadeOutTime(seconds);
}

void engine_physics_set(engine* e, float strength, float wind)
{
    e->physics_weight = std::fmin(std::fmax(strength / 50.0f, 0.0f), 2.0f);
    e->wind = std::isfinite(wind) ? wind / 100.0f : 0.0f;
    if (e->_physics != NULL)
    {
        CubismPhysics::Options options = e->_physics->GetOptions();
        options.Wind.X = 0.0f;
        options.Wind.Y = 0.0f;
        e->_physics->SetOptions(options);
    }
}

void engine_physics_scale(engine* e, const char* id, float strength, float wind)
{
    csmInt32 index = e->_model->GetParameterIndex(CubismFramework::GetIdManager()->GetId(id));
    if (index >= (csmInt32)e->_model->GetParameterCount())
    {
        return;
    }
    for (physics_scale& scale : e->physics_scales)
    {
        if (scale.index == index)
        {
            scale.strength = strength;
            scale.wind_multiplier = wind;
            return;
        }
    }
    e->physics_scales.push_back({index, strength, wind});
}

void engine_expressions_stop(engine* e)
{
    CSM_DELETE(e->_expressionManager);
    e->_expressionManager = CSM_NEW CubismExpressionMotionManager();
    e->pose_expressions.reset(new CubismExpressionMotionManager());
}

void engine_breath_enable(engine* e, bool on)
{
    if (on)
    {
        e->_breath->SetParameters(e->breath_parameters);
        return;
    }
    e->_breath->SetParameters(csmVector<CubismBreath::BreathParameterData>());
}

bool engine_idle_motions_enable(engine* e, bool on)
{
    e->idle_paused = !on;
    if (on)
    {
        return !e->motions.empty();
    }
    if (e->motions.empty())
    {
        e->_motionManager->StopAllMotions();
    }
    else
    {
        csmVector<CubismMotionQueueEntry*>* entries = e->_motionManager->GetCubismMotionQueueEntries();
        for (csmUint32 i = 0; i < entries->GetSize(); i++)
        {
            if ((*entries)[i] != NULL)
            {
                (*entries)[i]->SetFadeout((*entries)[i]->GetCubismMotion()->GetFadeOutTime());
            }
        }
    }
    return !e->motions.empty();
}

bool engine_motion_add(engine* e, const char* group, const char* path)
{
    ACubismMotion* motion = motion_load(e, path, NULL, -1);
    if (motion == NULL)
    {
        return false;
    }
    e->motions[group].push_back(motion);
    return true;
}

bool engine_motion_idle(engine* e, const char* group)
{
    auto found = e->motions.find(group);
    if (found == e->motions.end())
    {
        return false;
    }
    e->idle = &found->second;
    if (found->second.size() == 1)
    {
        found->second[0]->SetLoop(true);
        found->second[0]->SetLoopFadeIn(false);
    }
    return true;
}

float engine_motion_play(engine* e, const char* group, bool looped)
{
    auto found = e->motions.find(group);
    if (found == e->motions.end() || &found->second == e->idle)
    {
        return -1.0f;
    }
    ACubismMotion* motion = motion_pick(e, found->second);
    motion->SetLoop(looped);
    motion->SetLoopFadeIn(!looped);
    e->_motionManager->StartMotionPriority(motion, false, 2);
    return looped ? 0.0f : motion->GetDuration();
}

static void copy_params(engine* e, std::vector<float>& out)
{
    out.resize(e->_model->GetParameterCount());
    for (size_t i = 0; i < out.size(); i++)
    {
        out[i] = e->_model->GetParameterValue((csmInt32)i);
    }
}

void engine_update(engine* e, float dt_ms, engine_hook after_motion_update, engine_hook before_model_update, void* context)
{
    float dt = dt_ms / 1000.0f;
    if (e->idle != NULL && !e->idle_paused && e->_motionManager->IsFinished())
    {
        e->_motionManager->StartMotionPriority(motion_pick(e, *e->idle), false, 1);
    }
    e->rest_marks.assign(e->_model->GetParameterCount(), 0);
    bool eyes_held = false;
    if (e->idle != NULL)
    {
        for (ACubismMotion* motion : *e->idle)
        {
            for (csmInt32 index : e->motion_params[motion])
            {
                e->rest_marks[index] = 2;
            }
        }
    }
    csmVector<CubismMotionQueueEntry*>* entries = e->_motionManager->GetCubismMotionQueueEntries();
    for (csmUint32 i = 0; i < entries->GetSize(); i++)
    {
        CubismMotionQueueEntry* entry = (*entries)[i];
        if (entry == NULL || entry->IsFinished())
        {
            continue;
        }
        ACubismMotion* motion = entry->GetCubismMotion();
        char mark = entry->IsTriggeredFadeOut() || (entry->GetEndTime() >= 0.0f && entry->GetStateTime() >= entry->GetEndTime() - motion->GetFadeOutTime()) ? 1 : 2;
        bool idle_motion = e->idle != NULL && std::find(e->idle->begin(), e->idle->end(), motion) != e->idle->end();
        for (csmUint32 b = 0; mark == 2 && !idle_motion && b < e->blink_ids.GetSize(); b++)
        {
            csmInt32 blink_index = e->_model->GetParameterIndex(e->blink_ids[b]);
            eyes_held = eyes_held || std::find(e->motion_params[motion].begin(), e->motion_params[motion].end(), blink_index) != e->motion_params[motion].end();
        }
        for (csmInt32 index : e->motion_params[motion])
        {
            e->rest_marks[index] = std::max(e->rest_marks[index], mark);
        }
    }
    for (size_t i = 0; i < e->rest_marks.size(); i++)
    {
        if (e->rest_marks[i] == 1)
        {
            e->_model->SetParameterValue((csmInt32)i, e->_model->GetParameterDefaultValue((csmInt32)i));
        }
    }
    bool motion_updated = e->_motionManager->UpdateMotion(e->_model, dt);
    if (!e->motions.empty())
    {
        e->_model->SaveParameters();
    }
    if (after_motion_update != NULL)
    {
        after_motion_update(context);
    }
    if (e->motions.empty())
    {
        e->_model->SaveParameters();
    }
    e->pose_expressions->UpdateMotion(e->_model, dt);
    e->_expressionManager->UpdateMotion(e->_model, dt);
    for (auto& kept : e->kept_expressions)
    {
        kept->UpdateMotion(e->_model, dt);
    }
    if (e->_eyeBlink != NULL && !eyes_held)
    {
        srand(e->blink_seed);
        if (!motion_updated)
        {
            e->_eyeBlink->UpdateParameters(e->_model, dt);
        }
        else
        {
            const csmVector<CubismIdHandle>& ids = e->_eyeBlink->GetParameterIds();
            e->blink_open.resize(ids.GetSize());
            for (csmUint32 i = 0; i < ids.GetSize(); i++)
            {
                e->blink_open[i] = e->_model->GetParameterValue(ids[i]);
            }
            e->_eyeBlink->UpdateParameters(e->_model, dt);
            for (csmUint32 i = 0; i < ids.GetSize(); i++)
            {
                e->_model->SetParameterValue(ids[i], e->blink_open[i] * e->_model->GetParameterValue(ids[i]));
            }
        }
        e->blink_seed = (unsigned int)rand();
    }
    e->_breath->UpdateParameters(e->_model, dt);
    if (e->_physics != NULL)
    {
        float gust = 0.0f;
        if (e->wind != 0.0f)
        {
            e->wind_clock += dt;
            double t = e->wind_clock;
            const float* p = e->wind_phase;
            CubismPhysics::Options options = e->_physics->GetOptions();
            options.Wind.X = e->wind * (float)((0.6 * sin(0.31 * t + p[0]) + 0.4 * sin(0.97 * t + p[1])) * (0.75 + 0.25 * sin(2.3 * t + p[2])));
            options.Wind.Y = e->wind * (float)(0.25 * sin(0.53 * t + p[3]));
            e->_physics->SetOptions(options);
            if (!e->physics_scales.empty())
            {
                gust = std::fmin(std::hypot(options.Wind.X, options.Wind.Y), 1.0f);
            }
        }
        if (e->physics_weight == 1.0f && e->physics_scales.empty())
        {
            e->_physics->Evaluate(e->_model, dt);
        }
        else
        {
            copy_params(e, e->pre_physics);
            e->_physics->Evaluate(e->_model, dt);
            for (const physics_scale& scale : e->physics_scales)
            {
                float before = e->pre_physics[scale.index];
                float v = e->_model->GetParameterValue(scale.index);
                if (v == before)
                {
                    continue;
                }
                float weight = e->physics_weight * scale.strength * (1.0f + (scale.wind_multiplier - 1.0f) * gust);
                e->_model->SetParameterValue(scale.index, before + (v - before) * weight);
                e->pre_physics[scale.index] = e->_model->GetParameterValue(scale.index);
            }
            for (size_t i = 0; i < e->pre_physics.size(); i++)
            {
                float v = e->_model->GetParameterValue((csmInt32)i);
                if (v != e->pre_physics[i])
                {
                    e->_model->SetParameterValue((csmInt32)i, e->pre_physics[i] + (v - e->pre_physics[i]) * e->physics_weight);
                }
            }
        }
    }
    if (e->_pose != NULL)
    {
        e->_pose->UpdateParameters(e->_model, dt);
    }
    if (before_model_update != NULL)
    {
        before_model_update(context);
    }
    if (e->carry_left_ms > 0.0f)
    {
        float u = e->carry_left_ms / e->carry_total_ms;
        float w = u * u * (3.0f - 2.0f * u);
        for (size_t i = 0; i < e->carry_params.size(); i++)
        {
            float v = e->_model->GetParameterValue((csmInt32)i);
            e->_model->SetParameterValue((csmInt32)i, v + (e->carry_params[i] - v) * w);
        }
        for (size_t i = 0; i < e->carry_parts.size(); i++)
        {
            float v = e->_model->GetPartOpacity((csmInt32)i);
            e->_model->SetPartOpacity((csmInt32)i, v + (e->carry_parts[i] - v) * w);
        }
        e->carry_left_ms -= dt_ms;
    }
    copy_params(e, e->shown_params);
    e->shown_parts.resize(e->_model->GetPartCount());
    for (size_t i = 0; i < e->shown_parts.size(); i++)
    {
        e->shown_parts[i] = e->_model->GetPartOpacity((csmInt32)i);
    }
    e->update_pending = true;
    e->_model->LoadParameters();
}

static void update_shown(engine* e)
{
    if (!e->update_pending)
    {
        return;
    }
    e->_model->SaveParameters();
    for (size_t i = 0; i < e->shown_params.size(); i++)
    {
        e->_model->SetParameterValue((csmInt32)i, e->shown_params[i]);
    }
    for (size_t i = 0; i < e->shown_parts.size(); i++)
    {
        e->_model->SetPartOpacity((csmInt32)i, e->shown_parts[i]);
    }
    e->_model->Update();
    e->_model->LoadParameters();
    e->update_pending = false;
}

void engine_carry(engine* e, engine* source, float ms)
{
    e->carry_params = source->shown_params;
    e->carry_parts = source->shown_parts;
    e->carry_left_ms = ms;
    e->carry_total_ms = ms;
}

void engine_mirror(engine* e, engine* source)
{
    for (size_t i = 0; i < source->shown_params.size(); i++)
    {
        e->_model->SetParameterValue((csmInt32)i, source->shown_params[i]);
    }
    for (size_t i = 0; i < source->shown_parts.size(); i++)
    {
        e->_model->SetPartOpacity((csmInt32)i, source->shown_parts[i]);
    }
    e->_model->Update();
    e->update_pending = false;
}

void engine_background_clear(engine* e)
{
    e->background_frames.clear();
    e->background_ms.clear();
    e->background_total_ms = 0;
}

static const uint8_t* background_at(engine* e, float background_clock_ms)
{
    if (e->background_frames.empty())
    {
        return NULL;
    }
    float t = fmodf(floorf(background_clock_ms / kBackgroundStepMs) * kBackgroundStepMs, e->background_total_ms);
    size_t index = 0;
    while (index + 1 < e->background_frames.size() && t >= e->background_ms[index])
    {
        t -= e->background_ms[index];
        index++;
    }
    return e->background_frames[index].data();
}

void engine_params_ahead(engine* e, float ms, const char* const* ids, float* values, int count)
{
    thread_local std::vector<float> saved;
    thread_local std::vector<float> saved_parts;
    copy_params(e, saved);
    saved_parts.resize(e->_model->GetPartCount());
    for (size_t i = 0; i < saved_parts.size(); i++)
    {
        saved_parts[i] = e->_model->GetPartOpacity((csmInt32)i);
    }
    float saved_opacity = e->_model->GetModelOpacity();
    csmVector<CubismMotionQueueEntry*>* entries = e->_motionManager->GetCubismMotionQueueEntries();
    for (csmUint32 i = 0; i < entries->GetSize(); i++)
    {
        CubismMotionQueueEntry* entry = (*entries)[i];
        if (entry == NULL || entry->GetCubismMotion() == NULL)
        {
            continue;
        }
        CubismMotionQueueEntry ahead = *entry;
        entry->GetCubismMotion()->UpdateParameters(e->_model, &ahead, entry->GetStateTime() + ms / 1000.0f);
    }
    for (int i = 0; i < count; i++)
    {
        values[i] = engine_param_get(e, ids[i]);
    }
    for (size_t i = 0; i < saved.size(); i++)
    {
        e->_model->SetParameterValue((csmInt32)i, saved[i]);
    }
    for (size_t i = 0; i < saved_parts.size(); i++)
    {
        e->_model->SetPartOpacity((csmInt32)i, saved_parts[i]);
    }
    e->_model->SetModelOpacity(saved_opacity);
}
