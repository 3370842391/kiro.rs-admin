"""AWS signin browser identity and encrypted fingerprint generation."""

from __future__ import annotations

import base64
import hashlib
import json
import random
import re
import struct
import time
from collections import OrderedDict
from dataclasses import dataclass
from typing import Any, Mapping, MutableMapping, Sequence


UINT32 = 0xFFFFFFFF
DELTA = 0x9E3779B9


@dataclass(frozen=True)
class AppJsConfig:
    key: tuple[int, int, int, int]
    identifier: str
    version: str


FALLBACK_CONFIG = AppJsConfig(
    key=(1888420705, 2576816180, 2347232058, 874813317),
    identifier="ECdITeCs",
    version="4.0.0",
)


def _js_code_units(value: str) -> list[int]:
    raw = value.encode("utf-16-le", "surrogatepass")
    return [raw[index] | (raw[index + 1] << 8) for index in range(0, len(raw), 2)]


def _crc32_units(units: Sequence[int]) -> int:
    crc = UINT32
    for value in units:
        crc ^= value & 0xFF
        for _ in range(8):
            crc = ((crc >> 1) ^ 0xEDB88320) & UINT32 if crc & 1 else crc >> 1
    return (crc ^ UINT32) & UINT32


def crc32(value: str) -> int:
    """Return the IEEE CRC32 produced by the AWS JavaScript collector."""

    return _crc32_units(_js_code_units(value))


def xxtea_encrypt(
    plaintext: str, key: tuple[int, int, int, int]
) -> bytes:
    """Port of the collector's unsigned 32-bit XXTEA implementation."""

    units = _js_code_units(plaintext)
    if not units:
        return b""
    size = (len(units) + 3) // 4
    values = [0] * size
    for index in range(size):
        word = 0
        for offset in range(4):
            position = index * 4 + offset
            if position < len(units):
                word |= units[position] << (offset * 8)
        values[index] = word & UINT32

    rounds = 6 + 52 // size
    z = values[-1]
    total = 0
    for _ in range(rounds):
        total = (total + DELTA) & UINT32
        e = (total >> 2) & 3
        for position in range(size):
            y = values[(position + 1) % size]
            part1 = ((z >> 5) ^ ((y << 2) & UINT32)) & UINT32
            part2 = ((y >> 3) ^ ((z << 4) & UINT32)) & UINT32
            group1 = (part1 + part2) & UINT32
            group2 = ((total ^ y) + (key[(position & 3) ^ e] ^ z)) & UINT32
            mix = (group1 ^ group2) & UINT32
            values[position] = (values[position] + mix) & UINT32
            z = values[position]
    return b"".join(struct.pack("<I", value) for value in values)


def extract_app_js_config(source: str) -> AppJsConfig:
    """Extract the runtime key, identifier and version from AWS ``app.js``."""

    key_match = re.search(
        r'var\s+\w+\s*=\s*\[(\d+),\s*"([A-Za-z0-9]+)",\s*(\d+),\s*(\d+),\s*(\d+)\]',
        source,
    )
    version_match = re.search(r'FWCIM_VERSION\s*=\s*"(\d+\.\d+\.\d+)"', source)
    if key_match is None or version_match is None:
        raise ValueError("AWS app.js fingerprint config is incomplete")
    numbers = tuple(int(key_match.group(index)) for index in (1, 3, 4, 5))
    return AppJsConfig(
        key=(numbers[2], numbers[0], numbers[3], numbers[1]),
        identifier=key_match.group(2),
        version=version_match.group(1),
    )


def encrypt_fingerprint(json_text: str, config: AppJsConfig) -> str:
    prefix = f"{crc32(json_text):08X}#{json_text}"
    encrypted = xxtea_encrypt(prefix, config.key)
    return f"{config.identifier}:{base64.b64encode(encrypted).decode('ascii')}"


@dataclass(frozen=True)
class ScreenInfo:
    width: int
    height: int
    avail_width: int
    avail_height: int
    color_depth: int


@dataclass(frozen=True)
class BrowserPlugin:
    name: str
    filename: str
    description: str


@dataclass(frozen=True)
class BrowserIdentity:
    chrome_version: str
    user_agent: str
    gpu_vendor: str
    gpu_model: str
    webgl_extensions: list[str]
    canvas_hash: int
    histogram_base: list[int]
    math_tan: str
    math_sin: str
    math_cos: str
    plugins: list[BrowserPlugin]
    screen: ScreenInfo
    lsubid_prefix_signin: str
    lsubid_prefix_profile: str
    webpack_hash: str


@dataclass
class FingerprintContext:
    identity: BrowserIdentity
    canvas_hash: int
    histogram_bins: list[int]
    ls_ubid_signin: str
    ls_ubid_profile: str = ""
    performance_timing: dict[str, int] | None = None
    start_time: int | None = None


class OrderedFingerprint:
    def __init__(self):
        self._values: MutableMapping[str, Any] = OrderedDict()

    def set(self, key: str, value: Any) -> None:
        self._values[key] = value

    def to_json(self) -> str:
        return json.dumps(self._values, separators=(",", ":"), ensure_ascii=False)

    def as_dict(self) -> Mapping[str, Any]:
        return self._values.copy()


LSUBID_PREFIXES = ("X10", "X19", "X42", "X55", "X73", "X81", "X96")
GPU_CONFIGS = (
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) Iris(R) Xe Graphics (0x000046A6) Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) UHD Graphics 630 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) UHD Graphics 770 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) UHD Graphics 730 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) HD Graphics 620 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) HD Graphics 530 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (Intel)", "ANGLE (Intel, Intel(R) Iris(R) Plus Graphics Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1060 6GB Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 3060 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1650 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 2060 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 3070 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 4060 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1080 Ti Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 3080 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce GTX 1070 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (NVIDIA)", "ANGLE (NVIDIA, NVIDIA GeForce RTX 4070 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 580 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 6600 XT Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 5700 XT Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 6700 XT Direct3D11 vs_5_0 ps_5_0, D3D11)"),
    ("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 570 Direct3D11 vs_5_0 ps_5_0, D3D11)"),
)
SCREEN_CONFIGS = (
    (1920, 1080, 1920, 1040, 24), (2560, 1440, 2560, 1400, 24),
    (1920, 1200, 1920, 1160, 24), (1366, 768, 1366, 728, 24),
    (1536, 864, 1536, 824, 24), (1680, 1050, 1680, 1010, 24),
    (1440, 900, 1440, 860, 24), (1600, 900, 1600, 860, 24),
    (2560, 1080, 2560, 1040, 24), (3440, 1440, 3440, 1400, 24),
    (3840, 2160, 3840, 2120, 24), (1280, 1024, 1280, 984, 24),
)
MATH_POOL = (
    ("-1.4214488238747245", "0.8178819121159085", "-0.5753861119575491"),
    ("-1.4214488238747245", "0.8178819121159085", "-0.5765775004286854"),
    ("-1.4214488238747243", "0.8178819121159083", "-0.5753861119575489"),
    ("-1.4214488238747247", "0.8178819121159087", "-0.5753861119575493"),
    ("-1.4214488238747244", "0.8178819121159084", "-0.5765775004286855"),
    ("-1.4214488238747246", "0.8178819121159086", "-0.5753861119575490"),
    ("-1.4214488238747242", "0.8178819121159082", "-0.5765775004286853"),
    ("-1.4214488238747248", "0.8178819121159088", "-0.5753861119575492"),
    ("-1.4214488238747241", "0.8178819121159081", "-0.5765775004286852"),
    ("-1.4214488238747249", "0.8178819121159089", "-0.5753861119575494"),
)
WEBGL_EXT_CORE = (
    "ANGLE_instanced_arrays", "EXT_blend_minmax", "EXT_color_buffer_half_float",
    "EXT_float_blend", "EXT_frag_depth", "EXT_shader_texture_lod",
    "EXT_texture_filter_anisotropic", "EXT_sRGB", "KHR_parallel_shader_compile",
    "OES_element_index_uint", "OES_fbo_render_mipmap", "OES_standard_derivatives",
    "OES_texture_float", "OES_texture_float_linear", "OES_texture_half_float",
    "OES_texture_half_float_linear", "OES_vertex_array_object",
    "WEBGL_color_buffer_float", "WEBGL_compressed_texture_s3tc",
    "WEBGL_compressed_texture_s3tc_srgb", "WEBGL_debug_renderer_info",
    "WEBGL_debug_shaders", "WEBGL_depth_texture", "WEBGL_draw_buffers",
    "WEBGL_lose_context", "WEBGL_multi_draw",
)
WEBGL_EXT_OPTIONAL = (
    "EXT_disjoint_timer_query", "EXT_texture_compression_bptc",
    "EXT_texture_compression_rgtc", "WEBGL_compressed_texture_astc",
    "WEBGL_compressed_texture_etc", "OES_draw_buffers_indexed",
    "EXT_color_buffer_float",
)
PLUGINS_POOL = tuple(
    BrowserPlugin(name, "internal-pdf-viewer", "Portable Document Format")
    for name in (
        "PDF Viewer", "Chrome PDF Viewer", "Chromium PDF Viewer",
        "Microsoft Edge PDF Viewer", "WebKit built-in PDF",
    )
)
CHROME_VERSIONS = (
    (137, 7151, 7160), (138, 7204, 7213), (139, 7259, 7268),
    (140, 7316, 7325), (141, 7371, 7380), (142, 7430, 7439),
    (143, 7485, 7494), (144, 7544, 7553), (145, 7601, 7610),
    (146, 7660, 7669),
)


def _rng(rng):
    return rng if rng is not None else random.SystemRandom()


def _randint(rng, maximum: int) -> int:
    return rng.randrange(maximum)


def _generate_canvas_data(rng) -> tuple[int, list[int]]:
    bins = [0] * 256
    bins[0] = 10_000 + _randint(rng, 5_001)
    bins[255] = 12_000 + _randint(rng, 4_001)
    for index, value in (
        (255, 400 + _randint(rng, 301)), (165, 200 + _randint(rng, 201)),
        (0, 300 + _randint(rng, 301)), (128, 100 + _randint(rng, 201)),
        (64, 50 + _randint(rng, 101)), (192, 80 + _randint(rng, 121)),
        (32, 30 + _randint(rng, 71)), (224, 60 + _randint(rng, 121)),
    ):
        bins[index] = value
    remaining = 36_000 - sum(bins)
    for index in range(1, 255):
        if bins[index] == 0 and remaining > 0:
            value = min(4 + _randint(rng, 97), remaining)
            bins[index] = value
            remaining -= value
    bins[0] += remaining
    digest = hashlib.sha256(b"".join(struct.pack("<I", item) for item in bins)).digest()
    return struct.unpack("<i", digest[:4])[0], bins


def random_identity(rng=None) -> BrowserIdentity:
    rng = _rng(rng)
    major, build_min, build_max = rng.choice(CHROME_VERSIONS)
    chrome_version = f"{major}.0.{rng.randint(build_min, build_max)}.{_randint(rng, 150)}"
    gpu_vendor, gpu_model = rng.choice(GPU_CONFIGS)
    screen = rng.choice(SCREEN_CONFIGS)
    math_values = rng.choice(MATH_POOL)
    canvas_hash, histogram = _generate_canvas_data(rng)
    extensions = list(WEBGL_EXT_CORE)
    optional = list(WEBGL_EXT_OPTIONAL)
    rng.shuffle(optional)
    extensions.extend(optional[:_randint(rng, 5)])
    extensions.sort()
    plugins = list(PLUGINS_POOL)
    rng.shuffle(plugins)
    return BrowserIdentity(
        chrome_version=chrome_version,
        user_agent=("Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
                    f"AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{chrome_version} Safari/537.36"),
        gpu_vendor=gpu_vendor,
        gpu_model=gpu_model,
        webgl_extensions=extensions,
        canvas_hash=canvas_hash,
        histogram_base=histogram,
        math_tan=math_values[0], math_sin=math_values[1], math_cos=math_values[2],
        plugins=plugins,
        screen=ScreenInfo(*screen),
        lsubid_prefix_signin=rng.choice(LSUBID_PREFIXES),
        lsubid_prefix_profile=rng.choice(LSUBID_PREFIXES),
        webpack_hash=f"{_randint(rng, 0x7FFFFFFF):010x}"[-10:],
    )


def new_fingerprint_context(
    identity: BrowserIdentity, *, now_seconds: int | None = None, rng=None
) -> FingerprintContext:
    rng = _rng(rng)
    timestamp = int(time.time()) if now_seconds is None else now_seconds
    ubid = (f"{identity.lsubid_prefix_signin}-{_randint(rng, 10_000_000):07d}-"
            f"{_randint(rng, 10_000_000):07d}:{timestamp}")
    return FingerprintContext(identity, identity.canvas_hash, list(identity.histogram_base), ubid)


def reset_performance_timing(context: FingerprintContext) -> None:
    context.performance_timing = None


def _performance_timing(now_ms: int, rng) -> dict[str, int]:
    load_end = now_ms - (500 + _randint(rng, 1001))
    duration = 2000 + _randint(rng, 2001)
    base = load_end - duration
    dns = 2 + _randint(rng, 8)
    connect = 300 + _randint(rng, 300)
    response = connect + 200 + _randint(rng, 400)
    interactive = duration - (5 + _randint(rng, 11))
    content_start = interactive + _randint(rng, 3)
    return {
        "connectStart": base + dns + 1 + _randint(rng, 3),
        "secureConnectionStart": base + dns + 3 + _randint(rng, 5),
        "unloadEventEnd": 0, "domainLookupStart": base + dns,
        "domainLookupEnd": base + dns + _randint(rng, 2),
        "responseStart": base + response, "connectEnd": base + connect,
        "responseEnd": base + response + _randint(rng, 5),
        "requestStart": base + connect,
        "domLoading": base + response + 2 + _randint(rng, 5),
        "redirectStart": 0, "loadEventEnd": load_end, "domComplete": load_end,
        "navigationStart": base, "loadEventStart": load_end,
        "domContentLoadedEventEnd": load_end, "unloadEventStart": 0,
        "redirectEnd": 0, "domInteractive": base + interactive,
        "fetchStart": base + dns,
        "domContentLoadedEventStart": base + content_start,
    }


def _metrics(first_load: bool, page_type: str, rng) -> dict[str, int]:
    values = {key: 0 for key in (
        "el script h batt perf auto tz fp2 lsubid browser capabilities gpu dnt "
        "math tts input canvas captchainput pow"
    ).split()}
    if not first_load:
        values["perf"] = _randint(rng, 3)
    elif page_type == "profile":
        values.update(batt=5 + _randint(rng, 21), fp2=1 + _randint(rng, 8),
                      browser=_randint(rng, 4), capabilities=1 + _randint(rng, 8),
                      dnt=_randint(rng, 4), input=8 + _randint(rng, 23),
                      canvas=5 + _randint(rng, 16))
    elif page_type == "signup":
        values.update(script=_randint(rng, 3), batt=_randint(rng, 6),
                      fp2=_randint(rng, 4), gpu=3 + _randint(rng, 6))
    else:
        values.update(script=_randint(rng, 3), auto=_randint(rng, 3),
                      browser=_randint(rng, 3), gpu=3 + _randint(rng, 6))
    return values


def _interaction(event_type: str, rng) -> dict[str, Any]:
    empty = {"clicks": 0, "touches": 0, "keyPresses": 0, "cuts": 0,
             "copies": 0, "pastes": 0, "keyPressTimeIntervals": [],
             "mouseClickPositions": [], "keyCycles": [], "mouseCycles": [],
             "touchCycles": []}
    if event_type in ("PageLoad", "first_load"):
        return empty
    clicks, keys = 1 + _randint(rng, 3), 3 + _randint(rng, 8)
    intervals = max(1, keys // 3) + _randint(rng, max(1, keys // 2 - keys // 3 + 1))
    cycles = max(2, keys // 2) + _randint(rng, max(1, keys * 2 // 3 - keys // 2 + 1))
    empty.update(
        clicks=clicks, keyPresses=keys,
        keyPressTimeIntervals=[80 + _randint(rng, 621) for _ in range(intervals)],
        mouseClickPositions=[f"{400 + _randint(rng, 401)},{300 + _randint(rng, 201)}" for _ in range(clicks)],
        keyCycles=[20 + _randint(rng, 281) for _ in range(cycles)],
        mouseCycles=[50 + _randint(rng, 101) for _ in range(clicks)],
    )
    return empty


def _form_field(now_ms: int, email_length: int, email: str, interaction, rng):
    field_name = f"formField29-{now_ms - (10 + _randint(rng, 41))}-{1000 + _randint(rng, 9000)}"
    key_count = max(3, email_length // 3 + _randint(rng, 5) - 2)
    intervals = [80 + _randint(rng, 621) for _ in range(min(key_count - 1, 5))]
    cycles = [20 + _randint(rng, 231) for _ in range(min(key_count, 6))]
    if interaction["keyPresses"] > 0:
        key_count = interaction["keyPresses"]
    checksum_value = email or f"user{1000 + _randint(rng, 9000)}@example.com"
    return {field_name: {
        "clicks": 1, "touches": 0, "keyPresses": key_count, "cuts": 0,
        "copies": 0, "pastes": 0, "keyPressTimeIntervals": intervals,
        "mouseClickPositions": [f"{100 + _randint(rng, 151)}.5,{10 + _randint(rng, 11)}.5"],
        "keyCycles": cycles, "mouseCycles": [80 + _randint(rng, 71)],
        "touchCycles": [], "width": 180, "height": 32, "totalFocusTime": 0,
        "checksum": f"{crc32(checksum_value):08X}", "autocomplete": False,
        "prefilled": False,
    }}


def build_fingerprint_data(
    *, identity: BrowserIdentity, location_url: str, referrer: str, now_ms: int,
    context: FingerprintContext | None, page_type: str, event_type: str,
    time_on_page: int, email_length: int, email: str, config: AppJsConfig,
    rng=None,
) -> OrderedFingerprint:
    rng = _rng(rng)
    histogram = context.histogram_bins if context else identity.histogram_base
    canvas_hash = context.canvas_hash if context else identity.canvas_hash
    timing = context.performance_timing if context else None
    if timing is None:
        timing = _performance_timing(now_ms, rng)
        if context:
            context.performance_timing = timing
    if context:
        if page_type == "profile":
            if not context.ls_ubid_profile:
                timestamp = timing["loadEventEnd"] // 1000
                context.ls_ubid_profile = (
                    f"{identity.lsubid_prefix_profile}-{_randint(rng, 10_000_000):07d}-"
                    f"{_randint(rng, 10_000_000):07d}:{timestamp}"
                )
            ls_ubid = context.ls_ubid_profile
        else:
            ls_ubid = context.ls_ubid_signin
    else:
        ls_ubid = (f"{identity.lsubid_prefix_signin}-{_randint(rng, 10_000_000):07d}-"
                   f"{_randint(rng, 10_000_000):07d}:{timing['loadEventEnd'] // 1000}")

    if page_type == "profile":
        dynamic_urls, elapsed, history, compatible = [f"/dist/main/app_{identity.webpack_hash}.min.js"], 0, (2 if event_type in ("PageLoad", "first_load") else 3), True
    elif page_type == "signup":
        dynamic_urls, elapsed, history, compatible = ["/assets/js/app.js"], 1, 5, True
    else:
        dynamic_urls, elapsed, history, compatible = ["/assets/js/app.js"], 1, 1, False
    first_load = event_type == "first_load" or (event_type == "PageLoad" and page_type == "profile")
    interaction = _interaction(event_type, rng)
    end_ms = now_ms + _randint(rng, 51)
    if event_type not in ("PageLoad", "first_load") and time_on_page > 0:
        start = end_ms - time_on_page
    elif context:
        if context.start_time is None:
            if event_type == "first_load":
                context.start_time = now_ms - (500 + _randint(rng, 501))
            elif event_type == "PageLoad" and page_type == "profile":
                context.start_time = now_ms - (30 + _randint(rng, 51))
            else:
                context.start_time = now_ms
        start = context.start_time
    else:
        start = now_ms

    plugin_names = " ".join(plugin.name for plugin in identity.plugins)
    screen = identity.screen
    screen_text = f"{screen.width}-{screen.height}-{screen.avail_height}-{screen.color_depth}-*-*-*"
    result = OrderedFingerprint()
    result.set("metrics", _metrics(first_load, page_type, rng))
    result.set("start", start)
    result.set("interaction", interaction)
    result.set("scripts", {"dynamicUrls": dynamic_urls, "inlineHashes": [],
                            "elapsed": elapsed, "dynamicUrlCount": len(dynamic_urls),
                            "inlineHashesCount": 0})
    result.set("history", {"length": history})
    result.set("battery", {})
    result.set("performance", {"timing": timing})
    result.set("automation", {"wd": {"properties": {"document": [], "window": [], "navigator": []}},
                              "phantom": {"properties": {"window": []}}})
    result.set("end", end_ms)
    result.set("timeZone", 8)
    result.set("flashVersion", None)
    result.set("plugins", f"{plugin_names} ||{screen_text}")
    result.set("dupedPlugins", f"{plugin_names} ||{screen_text}")
    result.set("screenInfo", screen_text)
    result.set("lsUbid", ls_ubid)
    result.set("referrer", referrer)
    result.set("userAgent", identity.user_agent)
    result.set("location", location_url)
    result.set("webDriver", False)
    result.set("capabilities", {
        "css": {"textShadow": 1, "WebkitTextStroke": 1, "boxShadow": 1,
                "borderRadius": 1, "borderImage": 1, "opacity": 1,
                "transform": 1, "transition": 1},
        "js": {"audio": True, "geolocation": True, "localStorage": "supported",
               "touch": False, "video": True, "webWorker": True}, "elapsed": 0,
    })
    result.set("gpu", {"vendor": identity.gpu_vendor, "model": identity.gpu_model,
                       "extensions": identity.webgl_extensions})
    result.set("dnt", None)
    result.set("math", {"tan": identity.math_tan, "sin": identity.math_sin,
                        "cos": identity.math_cos})
    if page_type == "profile":
        result.set("timeToSubmit", (1 + _randint(rng, 5)) if event_type in ("PageLoad", "first_load")
                   else (time_on_page if time_on_page > 0 else 2000 + _randint(rng, 4001)))
    if page_type == "profile" and event_type not in ("PageLoad", "first_load") and email_length > 0:
        result.set("form", _form_field(now_ms, email_length, email, interaction, rng))
    else:
        result.set("form", {})
    result.set("canvas", {"hash": canvas_hash, "emailHash": None,
                          "histogramBins": list(histogram)})
    result.set("token", {"isCompatible": compatible, "pageHasCaptcha": 0})
    result.set("auth", {"form": {"method": "get"}})
    result.set("errors", [])
    result.set("version", config.version)
    return result


def generate_fingerprint(
    *, identity: BrowserIdentity, location_url: str, referrer: str,
    context: FingerprintContext | None, page_type: str, event_type: str,
    time_on_page: int, email_length: int, email: str, config: AppJsConfig,
    now_ms: int | None = None, rng=None,
) -> str:
    """Build and encrypt one fingerprint with explicit identity and config."""

    timestamp = int(time.time() * 1000) if now_ms is None else now_ms
    data = build_fingerprint_data(
        identity=identity, location_url=location_url, referrer=referrer,
        now_ms=timestamp, context=context, page_type=page_type,
        event_type=event_type, time_on_page=time_on_page,
        email_length=email_length, email=email, config=config, rng=rng,
    )
    return encrypt_fingerprint(data.to_json(), config)
