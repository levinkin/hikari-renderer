# getting started

this page walks through installing the hikari renderer, running it in a window or headless, and the http api. the [readme](../README.md) has the numbers and licensing.

## install

extract the zip for your os from the [releases page](https://github.com/levinkin/hikari-renderer/releases). it holds the source code plus a small `hikari` launcher binary built from `native/launcher`. 

download the [cubism sdk for native](https://www.live2d.com/en/sdk/download/native/) from live2d and drop the zip next to the `hikari` launcher (no need to extract it).


building needs rust and a compiler (xcode for mac, visual studio build tools for windows, build-essential and libx11-dev for linux). first run will get them for you if you're missing them.

## running it

double-click `hikari` in the folder (`hikari.exe` on windows, on mac finder opens it in terminal), or run it from inside the folder:

```
./hikari
```

first run grabs missing dependencies, builds hikari (~1 minute), and prompts whether to clean up the build files. builds only happen if hikari is uncompiled (or with `--build`). if the sdk is missing, it offers to open live2d's download page for you. 

running it will create a window view (for window capture/game capture) and an http server on 127.0.0.1:8793. `./hikari --headless` runs without a window for streaming frames directly to outputs. your own models go in `models/` (one folder/model). default model is the first model by name if unspecified. set `model` in `settings.json` to pick another, or switch live with `PUT /settings {"model": "<name>"}`. 

a model that fails to load returns an error instead of falling back. if youve already set a model up with vtube studio, drop the model's folder into `models/` with its `.vtube.json` and the renderer picks up its parameter mapping, hotkeys, expressions, physics, wind and framing.

## window

hikari opens the model in a window by default.

| control | what it does |
|---|---|
| drag | moves the model |
| scroll / pinch | zooms around the cursor |
| q / e (hold) | rotates clockwise/counter-clockwise while the window is focused |
| space | resets to the model's default framing |
| ctrl+s (cmd+s on mac) | saves the current framing for that model (`views.json`) |
| alt+enter (option+return on mac) | fullscreen |
| esc | leaves fullscreen, or quits |
| cmd+q (mac) | quits |

framing doesn't save on its own. everything you do in the window also moves the http streams for that model.

## headless

`./hikari --headless` skips the window and starts an http server on 127.0.0.1:8793 so you can pull your own frames as a stream. `GET /listen` holds the connection open and sends `fps=` frames a second (60 by default) as a chunked `application/octet-stream` with one frame per chunk.

frame contents:
- 4-byte timestamp in ms
- 4-byte data length (counts codec byte + picture)
- 4 bytes of head pose (yaw and pitch as signed 16-bit numbers for if you have servos)
- codec byte and the picture

numbers are all big-endian for faster comparisons/debugging. 

## actions

| endpoint | params (type, default) | what it does | returns | example |
|---|---|---|---|---|
| `GET /listen` | `codec` (string, `jpeg`), `fps` (integer, `60`), `size` (WxH, `size` setting) | streams idling until you close the connection. every listener sees the same picture | frame stream | `GET /listen?codec=jpeg&fps=30` |
| `POST /talk` | `codec`, `fps`, `size` (same as `/listen`). body: wav | lip syncs to the wav audio | frame stream | `POST /talk?codec=qoi&size=320x240` with a wav body |
| `POST /state` | `face` (string), `pose` (string). leave one out to keep it | sets a face/pose and holds it | `{"face", "pose", "asleep"}`, `400` for an unknown state | `POST /state?face=laughing` |
| `GET /state` | none | the current face/pose, and every state this model has | `{"face", "pose", "asleep", "states": [...]}` | - |
| `POST /action` | `action` (string, required) | plays an action then returns to idle | `{"action", "motion", "expression"}`, `400` for an unknown action | `POST /action?action=yawn` |
| `GET /action` | none | every action this model has, and the idle ones | `{"actions": [...], "idle": [...]}` | - |
| `POST /params` | none. body: json like `{"ParamAngleX": 10.0, "ParamMouthOpenY": 0.5}` | holds model params on top of idle and actions until changed, clamped to the model's range. `null` lets one go | every held param, `400` naming unknown params | `{"ParamAngleX": 10.0}` |
| `POST /physics` | none. body: json with any of `strength` (number, `50`), `wind` (object with `strength`, number, `0` = off, `100` = vts max, and `direction`, degrees, `90`), `gravity` (object with `direction`, degrees, `0`). missing keys keep their value | changes physics live, in memory only. a model switch resets them to that model's values, then the `physics` setting | `{"strength", "wind": {"strength", "direction"}, "gravity": {"direction"}}`, `400` for an unknown key, a `wind`/`gravity` that isn't an object, or a non-number | `{"wind": {"strength": 100, "direction": 90}}` |

## misc endpoints

| endpoint | params (type, default) | what it does | returns | example |
|---|---|---|---|---|
| `GET /models` | none | lists the models it found and the current one | `{"model": ..., "models": [...]}` | - |
| `GET /view` | none | the model's current framing, its saved one and its default | `{"zoom", "x", "y", "rot", "saved", "default"}` (`saved` is `null` if never saved) | - |
| `PUT /view` | `zoom`, `x`, `y`, `rot` (number, current value), `save` (bool, `false`) | moves the framing live. `save=true` also writes it to `views.json` | `204`, `500` if saving fails | `PUT /view?zoom=1.5&save=true` |
| `GET /settings` | none | the current settings | json | - |
| `PUT /settings` | json body with any of `model`, `face`, `idle_actions`, `action_every`, `transition_seconds`, `physics` | changes them live and saves them to `settings.json` | the new settings, `400` for a bad value or a restart-only key, `500` if `settings.json` can't be read or written | `{"face": "neutral", "action_every": [10, 30], "transition_seconds": 0.5}` |
| `GET /background` | none | describes the current background | `{"kind", "name", "updated", ...}` | - |
| `GET /background/file` | none | the background image itself | the image, or `404` | - |
| `PUT /background` | none. body: a png, jpeg or gif | sets/switches a background from the body (5 MB max, animated gifs play) | 200, `400` if it isn't png/jpeg/gif or can't decode, `413` if too big | a png body |
| `DELETE /background` | none | removes the background | 200 | - |

## params
| param | type | accepts | default |
|---|---|---|---|
| `codec` | string | `jpeg`, `rgb565` (raw 16-bit pixels), `pal256` (256-color palette), `split` (top/bottom halves), `qoi` (lossless), `mjpeg` (plain multipart jpeg for a browser/obs/vlc, no frame header) | `jpeg` |
| `fps` | integer | 1 to 240 | `60` |
| `size` | WxH | e.g. `160x120`, max `3840x2160`. ignored for `split` | the `size` setting |
| `zoom` | number | > 0. `PUT /view` only | the model's current value |
| `x`, `y` | number | offset from center. `PUT /view` only | the model's current value |
| `rot` | number | degrees. `PUT /view` only | the model's current value |
| `face` | string | any state from `GET /state` | the `face` setting |
| `pose` | string | any state from `GET /state`, `none` to clear | `none` |
| `action` | string | any action from `GET /action` | required |

bools take `true` or `false`. anything else, or the same param twice, returns `400`.

## states, actions and loops

- states hold until you change them. they're the model's own expressions (`.exp3.json`, named by their vtube studio hotkey or file name), plus some default shipped ones in `default.model3.json` (`neutral`, `happy`, `laughing`, `sad`, `crying`, `angry`, `doubtful`, `excited`, `grumpy`, `confused`, `thinking`, `listening`). drop a `<name>.exp3.json` in the model folder to add your own.
- actions play once: `look_left`, `look_right`, `look_up`, `look_down`, any motion group in `model3.json` except `Idle`, any vtube studio animation hotkey, plus some default shipped ones in `default.model3.json` (`look_left`, `look_right`, `look_up`, `look_down`, `look_ahead`, `dance`, `sleep` and the idle actions). `sleep` holds until the next action, `/talk` or `/state`.
- loops run underneath everything: the model's `Idle` motions or its vtube studio idle animation. `idle_loop` picks one.

the `/talk` body is a 16-bit pcm wav (mono or stereo, 8000 to 96000 hz), sent with `content-length` or chunked. a wav with an unknown length (size `0xFFFFFFFF`) works. she starts moving as the audio arrives so you can stream it as you play it.

in the frame stream, the picture after the codec byte depends on the codec:
- `jpeg` (byte 0): a plain jpeg
- `pal256` (byte 1): a 256-entry rgb565 palette (512 bytes), then one palette index per pixel as a raw lz4 block
- `rgb565` (byte 2): 16-bit little-endian rgb565 pixels as a raw lz4 block
- `split` (byte 3): top and bottom halves as two jpegs, with a 4-byte big-endian length of the top one in front
- `qoi` (byte 4): a plain qoi image

## command line

`./hikari` with no args opens the window with defaults.

| arg | type | what it does | default |
|---|---|---|---|
| `--window <model>` | string | opens the window with that model (`--window` is optional, `./hikari <model>` does the same) | `model` setting |
| `--headless` | bool | no window, http server only | `false` |
| `--build` | bool | rebuilds hikari (after pulling new code for example), then runs it | `false` |
| `--fps N` | integer | window frame rate, 1 to 240 | `60` |
| `--no-share` | bool | stops sharing frames with obs (spout2 on windows, syphon on mac) | `false` |
| `--port N` | integer | serves on that port for this run, 1 to 65535 | `port` setting |
| `--settings` | bool | opens the settings screen for the hikari already running on `port` (it opens in its own terminal window on every launch) | `false` |
| `--no-settings` | bool | skips opening the settings screen on launch | `false` |

## settings.json

| key | type | accepts | default |
|---|---|---|---|
| `model` | string | a model name. switches live | the first model by name |
| `face` | string | a state name | `neutral` |
| `port` | integer | 1 to 65535 | `8793` |
| `size` | `[int, int]` | `[width, height]` the window opens at, and for streams without `size=`. up to `[3840, 2160]` | `[960, 720]` |
| `idle_actions` | list of strings | actions to pick from at random, `[]` for none | `["look_around", "glance_left", "glance_right", "double_blink", "yawn", "tilt_smile", "look_up_think"]` |
| `action_every` | integer or `[min, max]` | whole seconds between idle actions, > 0. a pair waits a random time between the two | `15` |
| `idle_loop` | string | an `Idle` motion name | the vtube studio idle animation, else the first `Idle` motion |
| `transition_seconds` | number | 0 to 3 seconds for expression fade in and out, including faces, poses and vtube studio hotkey expressions. adjustable live with `PUT /settings` or the settings screen's `transition` row | `0.5` |
| `physics` | object | same shape as the `POST /physics` body, any subset, e.g. `{"wind": {"strength": 40, "direction": 270}, "gravity": {"direction": 0}}`. used for models whose `.vtube.json` has no physics of its own (a model's own strength and wind win). directions always come from here | `{"strength": 50, "wind": {"strength": 0, "direction": 90}, "gravity": {"direction": 0}}` |

actions are model3.json motion groups and vtube studio animation hotkeys, plus some default shipped ones in `default.model3.json` next to the launcher. edit that file to change them.

states are `.exp3.json` expressions named by the model's vtube studio hotkey or file name, plus some default shipped ones in the same file.

`port`, `size` and `idle_loop` need a restart.

wind and gravity directions are compass degrees on the screen, top is north: the number says where the force comes from. `0` = from the top, flowing down. `90` = from the right, flowing left. `180` = from the bottom, flowing up. `270` = from the left, flowing right.

one renderer runs one model. for more models, run a copy per model on its own port (`--port`).
