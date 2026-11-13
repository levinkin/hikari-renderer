#pragma once
#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>
#ifdef __cplusplus
extern "C" {
#endif
typedef struct engine engine;
typedef void (*engine_hook)(void* context);
typedef struct window window;
void engine_core_load(void);
engine* engine_create(const char* model3_json_path, int width, int height);
void engine_destroy(engine* e);
void engine_textures_release(const char* model3_json_path);
const char* engine_gpu_name(engine* e);
bool engine_has_param(engine* e, const char* id);
float engine_param_get(engine* e, const char* id);
const char* engine_param_at(engine* e, int index, float* minimum, float* maximum);
void engine_param_set(engine* e, const char* id, float value);
int engine_param_index(engine* e, const char* id);
void engine_param_set_at(engine* e, int index, float value);
void engine_params_ahead(engine* e, float ms, const char* const* ids, float* values, int count);
bool engine_expression(engine* e, int layer, const char* name);
bool engine_expression_add(engine* e, const char* name, const char* path);
bool engine_expression_keep(engine* e, const char* name);
void engine_expression_fade(engine* e, const char* name, float seconds);
void engine_physics_set(engine* e, float strength, float wind, float wind_degrees, float gravity_degrees);
void engine_physics_scale(engine* e, const char* param_id, float strength, float wind);
void engine_expressions_stop(engine* e);
void engine_breath_enable(engine* e, bool on);
bool engine_idle_motions_enable(engine* e, bool on);
bool engine_motion_add(engine* e, const char* group, const char* path);
bool engine_motion_idle(engine* e, const char* group);
float engine_motion_play(engine* e, const char* group, bool looped);
void engine_update(engine* e, float dt_ms, engine_hook after_motion_update, engine_hook before_model_update, void* context);
void engine_mirror(engine* e, engine* source);
void engine_carry(engine* e, engine* source, float ms);
int engine_background_load(engine* e, const uint8_t* bytes, size_t length, const char* expected_type);
void engine_background_clear(engine* e);
bool engine_view_set(engine* e, float zoom, float x, float y, float rot);
const uint8_t* engine_render(engine* e, float background_clock_ms);
window* window_open(const char* title, int width, int height);
bool window_poll(window* w, int* width, int* height, engine_hook frame, void* context);
void window_close(window* w);
int window_ask_save(window* w, const char* model);
bool window_input(window* w, float* pan_x, float* pan_y, float* zoom, float* cursor_x, float* cursor_y, float* turn, int* keys);
bool engine_present(engine* e, window* w, float background_clock_ms, float frame_seconds, bool share);
#ifdef __cplusplus
}
#endif
