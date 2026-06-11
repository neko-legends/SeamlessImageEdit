use base64::{engine::general_purpose, Engine as _};
use image::{imageops::FilterType, ImageEncoder, ImageFormat, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;
use texture_synthesis as texsynth;

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "webp", "bmp", "tif", "tiff",
];
const SEAM_FEATHER: i32 = 3;
const DEFAULT_AGENT_API_PORT: u16 = 17335;
const AGENT_APP_ID: &str = "seamless-image-edit";
const AGENT_APP_NAME: &str = "Seamless Image Edit";
const AGENT_API_BIND_ADDRESS: &str = "127.0.0.1";
const AGENT_API_REGISTRY_FILE: &str = "agent-api-registry.json";

fn default_mode() -> String {
    "tile".to_string()
}

fn default_output_format() -> String {
    "webp".to_string()
}

fn default_suffix() -> String {
    "_seamless".to_string()
}

fn default_blend_percent() -> f32 {
    20.0
}

fn default_strategy() -> String {
    "seam-cut".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SeamlessOptions {
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_output_format")]
    output_format: String,
    #[serde(default)]
    same_folder: bool,
    #[serde(default)]
    output_dir: String,
    #[serde(default = "default_suffix")]
    suffix: String,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    overwrite: bool,
    #[serde(default = "default_blend_percent")]
    blend_percent: f32,
    #[serde(default = "default_strategy")]
    strategy: String,
    #[serde(default)]
    flatten: f32,
    #[serde(default)]
    snap_period: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessResult {
    input_path: String,
    output_path: Option<String>,
    status: String,
    message: String,
}

struct ProcessedOutput {
    output_path: PathBuf,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentJobRequest {
    paths: Option<Vec<String>>,
    options: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentServerStatus {
    enabled: bool,
    port: u16,
    url: String,
    openapi_url: String,
    busy: bool,
    active_job_id: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AgentApiRegistryEntry {
    app_id: String,
    app_name: String,
    default_port: u16,
    bind_address: String,
    port: u16,
    enabled: bool,
    url: String,
    openapi_url: String,
    busy: bool,
    active_job_id: Option<String>,
    last_seen: Option<String>,
    note: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentApiRegistry {
    updated_at: String,
    apps: Vec<AgentApiRegistryEntry>,
}

#[derive(Clone, Default)]
struct ActiveJobState {
    inner: Arc<Mutex<Option<ActiveJob>>>,
}

struct ActiveJob {
    id: String,
    cancel_requested: bool,
}

impl ActiveJobState {
    fn is_busy(&self) -> Result<bool, String> {
        self.inner
            .lock()
            .map(|job| job.is_some())
            .map_err(|_| "Unable to lock active job state.".to_string())
    }

    fn start(&self, id: String) -> Result<(), String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        if active.is_some() {
            return Err("Another seamless job is already running.".to_string());
        }
        *active = Some(ActiveJob {
            id,
            cancel_requested: false,
        });
        Ok(())
    }

    fn request_cancel(&self) -> Result<(), String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        let Some(job) = active.as_mut() else {
            return Err("No active seamless job to cancel.".to_string());
        };
        job.cancel_requested = true;
        Ok(())
    }

    fn is_canceled(&self, id: &str) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|job| {
                job.as_ref()
                    .filter(|job| job.id == id)
                    .map(|job| job.cancel_requested)
            })
            .unwrap_or(true)
    }

    fn finish(&self, id: &str) -> Result<bool, String> {
        let mut active = self
            .inner
            .lock()
            .map_err(|_| "Unable to lock active job state.".to_string())?;
        let canceled = active
            .as_ref()
            .filter(|job| job.id == id)
            .map(|job| job.cancel_requested)
            .unwrap_or(false);
        if active.as_ref().is_some_and(|job| job.id == id) {
            *active = None;
        }
        Ok(canceled)
    }

    fn active_job_id(&self) -> Result<Option<String>, String> {
        self.inner
            .lock()
            .map(|job| job.as_ref().map(|job| job.id.clone()))
            .map_err(|_| "Unable to lock active job state.".to_string())
    }
}

#[derive(Clone, Default)]
struct AgentServerState {
    inner: Arc<Mutex<AgentServerControl>>,
}

#[derive(Default)]
struct AgentServerControl {
    enabled: bool,
    port: u16,
    stop: Option<Arc<AtomicBool>>,
}

impl AgentServerControl {
    fn port(&self) -> u16 {
        if self.port == 0 {
            read_registered_agent_api_port().unwrap_or(DEFAULT_AGENT_API_PORT)
        } else {
            self.port
        }
    }
}

fn default_seamless_options() -> SeamlessOptions {
    SeamlessOptions {
        mode: default_mode(),
        output_format: default_output_format(),
        same_folder: true,
        output_dir: String::new(),
        suffix: default_suffix(),
        recursive: true,
        overwrite: false,
        blend_percent: default_blend_percent(),
        strategy: default_strategy(),
        flatten: 0.0,
        snap_period: false,
    }
}

fn next_job_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("job-{millis}")
}

pub fn run_headless_cli(args: Vec<String>) -> Result<(), String> {
    let mut options = SeamlessOptions {
        mode: default_mode(),
        output_format: default_output_format(),
        same_folder: false,
        output_dir: String::new(),
        suffix: default_suffix(),
        recursive: false,
        overwrite: false,
        blend_percent: default_blend_percent(),
        strategy: default_strategy(),
        flatten: 0.0,
        snap_period: false,
    };
    let mut input_paths: Vec<String> = Vec::new();
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--headless" => {}
            "--help" | "-h" => {
                print_headless_help();
                return Ok(());
            }
            "--mode" => {
                index += 1;
                options.mode = args
                    .get(index)
                    .ok_or_else(|| "--mode requires horizontal, vertical, or tile".to_string())?
                    .clone();
            }
            "--format" => {
                index += 1;
                options.output_format = args
                    .get(index)
                    .ok_or_else(|| "--format requires webp or png".to_string())?
                    .clone();
            }
            "--strategy" => {
                index += 1;
                options.strategy = args
                    .get(index)
                    .ok_or_else(|| "--strategy requires seam-cut, synthesis, or blend".to_string())?
                    .clone();
            }
            "--flatten" => {
                index += 1;
                let flatten = args
                    .get(index)
                    .ok_or_else(|| "--flatten requires a strength from 0 to 1".to_string())?;
                options.flatten = flatten
                    .parse::<f32>()
                    .map_err(|error| format!("Invalid --flatten value '{flatten}': {error}"))?;
            }
            "--output-dir" => {
                index += 1;
                options.output_dir = args
                    .get(index)
                    .ok_or_else(|| "--output-dir requires a folder path".to_string())?
                    .clone();
                options.same_folder = false;
            }
            "--same-folder" => {
                options.same_folder = true;
            }
            "--suffix" => {
                index += 1;
                options.suffix = args
                    .get(index)
                    .ok_or_else(|| "--suffix requires a filename suffix".to_string())?
                    .clone();
            }
            "--recursive" => {
                options.recursive = true;
            }
            "--overwrite" => {
                options.overwrite = true;
            }
            "--snap-period" => {
                options.snap_period = true;
            }
            "--blend" => {
                index += 1;
                let blend = args
                    .get(index)
                    .ok_or_else(|| "--blend requires a numeric percentage".to_string())?;
                options.blend_percent = blend
                    .parse::<f32>()
                    .map_err(|error| format!("Invalid --blend value '{blend}': {error}"))?;
            }
            value if value.starts_with('-') => {
                return Err(format!("Unknown headless option: {value}"));
            }
            value => {
                input_paths.push(value.to_string());
            }
        }
        index += 1;
    }

    normalized_mode(&options.mode)?;
    normalized_format(&options.output_format)?;
    normalized_strategy(&options.strategy)?;
    if !options.same_folder && options.output_dir.trim().is_empty() {
        return Err("--output-dir is required unless --same-folder is set.".to_string());
    }
    if input_paths.is_empty() {
        return Err("Provide at least one input image or folder.".to_string());
    }

    let mut resolved_paths = Vec::new();
    for path in input_paths {
        collect_images(Path::new(&path), options.recursive, &mut resolved_paths);
    }
    resolved_paths.sort();
    resolved_paths.dedup();
    if resolved_paths.is_empty() {
        return Err("No supported input images found.".to_string());
    }

    let results = process_paths_headless(resolved_paths, options);
    println!(
        "{}",
        serde_json::to_string_pretty(&results)
            .map_err(|error| format!("Unable to serialize results: {error}"))?
    );
    if let Some(error) = results.iter().find(|result| result.status == "error") {
        return Err(error.message.clone());
    }
    Ok(())
}

fn print_headless_help() {
    println!(
        "SeamlessImageEdit headless usage:\n\
         seamless-image-edit --headless [options] <image-or-folder>...\n\n\
         Options:\n\
           --mode horizontal|vertical|tile   Seam direction, default tile\n\
           --strategy seam-cut|synthesis|blend Strategy, default seam-cut\n\
           --flatten <0..1>                  Illumination flattening, default 0\n\
           --format webp|png                 Output format, default webp\n\
           --output-dir <folder>             Output folder\n\
           --same-folder                     Save beside each source image\n\
           --suffix <suffix>                 Output filename suffix, default _seamless\n\
           --recursive                       Recurse through input folders\n\
           --overwrite                       Replace existing outputs\n\
           --blend <percent>                 Seam band, default 20\n\
           --snap-period                     Crop to the detected pattern period before cutting"
    );
}

fn process_paths_headless(paths: Vec<String>, options: SeamlessOptions) -> Vec<ProcessResult> {
    let mut results = Vec::new();
    for path in paths {
        let input_path = PathBuf::from(&path);
        match process_one(&input_path, &options) {
            Ok(output) => results.push(ProcessResult {
                input_path: path,
                output_path: Some(output.output_path.display().to_string()),
                status: "done".to_string(),
                message: output.message,
            }),
            Err(error) => results.push(ProcessResult {
                input_path: path,
                output_path: None,
                status: "error".to_string(),
                message: error,
            }),
        }
    }
    results
}

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

fn is_webp_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("webp"))
        .unwrap_or(false)
}

fn collect_images(path: &Path, recursive: bool, output: &mut Vec<String>) {
    if path.is_file() {
        if is_image_file(path) {
            output.push(path.display().to_string());
        }
        return;
    }

    if !path.is_dir() {
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_file() && is_image_file(&entry_path) {
            output.push(entry_path.display().to_string());
        } else if recursive && entry_path.is_dir() {
            collect_images(&entry_path, recursive, output);
        }
    }
}

fn image_mime_type(path: &Path) -> Result<&'static str, String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg") | Some("jpeg") | Some("jpe") | Some("jfif") => Ok("image/jpeg"),
        Some("webp") => Ok("image/webp"),
        Some("bmp") => Ok("image/bmp"),
        Some("tif") | Some("tiff") => Ok("image/tiff"),
        Some(extension) => Err(format!("Unsupported preview image extension: {extension}")),
        None => Err("Preview image has no file extension.".to_string()),
    }
}

fn rgba_png_data_url(image: &RgbaImage) -> Result<String, String> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ColorType::Rgba8.into(),
        )
        .map_err(|error| format!("Unable to encode temporary PNG preview: {error}"))?;
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(bytes)
    ))
}

fn read_image_rgba(path: &Path) -> Result<RgbaImage, String> {
    match image::open(path) {
        Ok(image) => Ok(image.to_rgba8()),
        Err(error) if is_webp_file(path) => read_webp_rgba_fallback(path).map_err(|fallback_error| {
            format!("Unable to read image: {error}; WebP fallback failed: {fallback_error}")
        }),
        Err(error) => Err(format!("Unable to read image: {error}")),
    }
}

#[cfg(target_os = "windows")]
fn read_webp_rgba_fallback(path: &Path) -> Result<RgbaImage, String> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{GENERIC_READ, RPC_E_CHANGED_MODE},
            Graphics::Imaging::{
                CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
                IWICPalette, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
                WICDecodeMetadataCacheOnLoad,
            },
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
                COINIT_MULTITHREADED,
            },
        },
    };

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();

    unsafe {
        let com_status = CoInitializeEx(None, COINIT_MULTITHREADED);
        let should_uninitialize = com_status.is_ok();
        if com_status.is_err() && com_status != RPC_E_CHANGED_MODE {
            return Err(format!(
                "Windows imaging initialization failed with HRESULT 0x{:08X}",
                com_status.0 as u32
            ));
        }

        let result = (|| {
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .map_err(|error| format!("Unable to create Windows imaging factory: {error}"))?;
            let decoder = factory
                .CreateDecoderFromFilename(
                    PCWSTR::from_raw(wide_path.as_ptr()),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnLoad,
                )
                .map_err(|error| format!("Windows imaging could not open WebP: {error}"))?;
            let frame = decoder
                .GetFrame(0)
                .map_err(|error| format!("Windows imaging could not read first WebP frame: {error}"))?;
            let converter = factory
                .CreateFormatConverter()
                .map_err(|error| format!("Windows imaging could not create format converter: {error}"))?;
            converter
                .Initialize(
                    &frame,
                    &GUID_WICPixelFormat32bppRGBA,
                    WICBitmapDitherTypeNone,
                    None::<&IWICPalette>,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )
                .map_err(|error| format!("Windows imaging could not convert WebP to RGBA: {error}"))?;

            let mut width = 0;
            let mut height = 0;
            converter
                .GetSize(&mut width, &mut height)
                .map_err(|error| format!("Windows imaging could not read WebP size: {error}"))?;
            if width == 0 || height == 0 {
                return Err("Windows imaging decoded an empty WebP.".to_string());
            }

            let stride = width
                .checked_mul(4)
                .ok_or_else(|| "WebP image is too wide to decode safely.".to_string())?;
            let buffer_len = stride
                .checked_mul(height)
                .and_then(|length| usize::try_from(length).ok())
                .ok_or_else(|| "WebP image is too large to decode safely.".to_string())?;
            let mut pixels = vec![0; buffer_len];
            converter
                .CopyPixels(ptr::null(), stride, &mut pixels)
                .map_err(|error| format!("Windows imaging could not copy WebP pixels: {error}"))?;

            RgbaImage::from_raw(width, height, pixels)
                .ok_or_else(|| "Windows imaging returned an invalid RGBA buffer.".to_string())
        })();

        if should_uninitialize {
            CoUninitialize();
        }

        result
    }
}

#[cfg(not(target_os = "windows"))]
fn read_webp_rgba_fallback(_path: &Path) -> Result<RgbaImage, String> {
    Err("no platform WebP fallback is available".to_string())
}

fn sanitized_suffix(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "_seamless".to_string();
    }

    let safe = trimmed
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>();

    if safe.is_empty() {
        "_seamless".to_string()
    } else {
        safe
    }
}

fn normalized_format(value: &str) -> Result<(&'static str, ImageFormat), String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(("png", ImageFormat::Png)),
        "webp" => Ok(("webp", ImageFormat::WebP)),
        other => Err(format!("Unsupported output format: {other}")),
    }
}

fn normalized_mode(value: &str) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "horizontal" => Ok("horizontal"),
        "vertical" => Ok("vertical"),
        "tile" => Ok("tile"),
        other => Err(format!("Unsupported seamless mode: {other}")),
    }
}

fn normalized_strategy(value: &str) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "seam-cut" | "seam_cut" | "seamcut" => Ok("seam-cut"),
        "synthesis" | "content-aware" | "content_aware" => Ok("synthesis"),
        "blend" | "crossfade" => Ok("blend"),
        other => Err(format!("Unsupported seamless strategy: {other}")),
    }
}

fn output_path_for(input_path: &Path, options: &SeamlessOptions) -> Result<PathBuf, String> {
    let (extension, _) = normalized_format(&options.output_format)?;
    let source_dir = input_path
        .parent()
        .ok_or_else(|| "Unable to resolve source folder.".to_string())?;
    let target_dir = if options.same_folder {
        source_dir.to_path_buf()
    } else {
        let output_dir = options.output_dir.trim();
        if output_dir.is_empty() {
            return Err("Choose an output folder or enable same-folder output.".to_string());
        }
        PathBuf::from(output_dir)
    };

    fs::create_dir_all(&target_dir)
        .map_err(|error| format!("Unable to create output folder: {error}"))?;

    let stem = input_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("image");
    let suffix = sanitized_suffix(&options.suffix);
    let base_name = format!("{stem}{suffix}");
    let candidate = target_dir.join(format!("{base_name}.{extension}"));
    if options.overwrite || !candidate.exists() {
        return Ok(candidate);
    }

    for index in 2..10_000 {
        let candidate = target_dir.join(format!("{base_name}-{index}.{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err("Unable to find an unused output filename.".to_string())
}

fn sample_shifted(source: &RgbaImage, x: u32, y: u32, shift_x: u32, shift_y: u32) -> Rgba<u8> {
    let width = source.width();
    let height = source.height();
    *source.get_pixel((x + shift_x) % width, (y + shift_y) % height)
}

fn clamp_channel(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

fn smoothstep(value: f32) -> f32 {
    let t = value.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn seam_cut_band(size: u32, blend_percent: f32) -> u32 {
    if size <= 2 {
        return 1;
    }

    let raw = ((size as f32) * blend_percent.clamp(4.0, 45.0) / 200.0).round() as u32;
    raw.max(4).min((size / 4).max(1))
}

fn seam_weight(index: u32, size: u32, blend_percent: f32) -> f32 {
    if size <= 2 {
        return 0.0;
    }

    let percent = blend_percent.clamp(4.0, 45.0);
    let band = ((size as f32) * percent / 100.0).round().max(2.0);
    let radius = (band / 2.0).max(1.0);
    let center = size as f32 / 2.0;
    let distance = ((index as f32 + 0.5) - center).abs();
    let t = (1.0 - distance / radius).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn edge_band(size: u32, blend_percent: f32) -> u32 {
    if size <= 2 {
        return 1;
    }

    let percent = (blend_percent.clamp(4.0, 45.0) * 0.5).max(2.0);
    (((size as f32) * percent / 100.0).round() as u32)
        .max(1)
        .min((size / 2).max(1))
}

fn synthesis_band(size: u32, blend_percent: f32) -> u32 {
    if size <= 2 {
        return 1;
    }

    let raw = ((size as f32) * blend_percent.clamp(8.0, 42.0) / 100.0).round() as u32;
    raw.max(8).min((size / 3).max(1))
}

fn edge_weight(index: u32, band: u32) -> f32 {
    if band <= 1 {
        return 1.0;
    }
    let t = 1.0 - index as f32 / band.saturating_sub(1) as f32;
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn synthesis_mask(width: u32, height: u32, mode: &str, blend_percent: f32) -> RgbaImage {
    let x_band = synthesis_band(width, blend_percent);
    let y_band = synthesis_band(height, blend_percent);
    let mut mask = RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]));

    for y in 0..height {
        for x in 0..width {
            let in_horizontal_band = x < x_band || x >= width.saturating_sub(x_band);
            let in_vertical_band = y < y_band || y >= height.saturating_sub(y_band);
            let editable = match mode {
                "horizontal" => in_horizontal_band,
                "vertical" => in_vertical_band,
                "tile" => in_horizontal_band || in_vertical_band,
                _ => false,
            };

            if editable {
                mask.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
    }

    mask
}

fn temp_job_dir() -> Result<PathBuf, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock error: {error}"))?
        .as_millis();
    let dir = std::env::temp_dir().join(format!(
        "seamless-image-edit-{}-{millis}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).map_err(|error| format!("Unable to create temp folder: {error}"))?;
    Ok(dir)
}

fn mix2(a: Rgba<u8>, b: Rgba<u8>, weight_b: f32) -> Rgba<u8> {
    let weight_b = weight_b.clamp(0.0, 1.0);
    let weight_a = 1.0 - weight_b;
    let mut out = [0u8; 4];
    for channel in 0..4 {
        out[channel] =
            ((a.0[channel] as f32 * weight_a) + (b.0[channel] as f32 * weight_b)).round() as u8;
    }
    Rgba(out)
}

fn snap_horizontal_edges(image: &mut RgbaImage) {
    let width = image.width();
    if width < 2 {
        return;
    }

    for y in 0..image.height() {
        let matched = mix2(*image.get_pixel(0, y), *image.get_pixel(width - 1, y), 0.5);
        image.put_pixel(0, y, matched);
        image.put_pixel(width - 1, y, matched);
    }
}

fn snap_vertical_edges(image: &mut RgbaImage) {
    let height = image.height();
    if height < 2 {
        return;
    }

    for x in 0..image.width() {
        let matched = mix2(*image.get_pixel(x, 0), *image.get_pixel(x, height - 1), 0.5);
        image.put_pixel(x, 0, matched);
        image.put_pixel(x, height - 1, matched);
    }
}

fn reconcile_horizontal_edges(image: &mut RgbaImage, blend_percent: f32) {
    let width = image.width();
    let height = image.height();
    let band = edge_band(width, blend_percent);
    for y in 0..height {
        for offset in 0..band {
            let left_x = offset;
            let right_x = width - 1 - offset;
            if left_x >= right_x {
                break;
            }

            let left = *image.get_pixel(left_x, y);
            let right = *image.get_pixel(right_x, y);
            let matched = mix2(left, right, 0.5);
            let weight = edge_weight(offset, band);
            image.put_pixel(left_x, y, mix2(left, matched, weight));
            image.put_pixel(right_x, y, mix2(right, matched, weight));
        }
    }
}

fn reconcile_vertical_edges(image: &mut RgbaImage, blend_percent: f32) {
    let width = image.width();
    let height = image.height();
    let band = edge_band(height, blend_percent);
    for x in 0..width {
        for offset in 0..band {
            let top_y = offset;
            let bottom_y = height - 1 - offset;
            if top_y >= bottom_y {
                break;
            }

            let top = *image.get_pixel(x, top_y);
            let bottom = *image.get_pixel(x, bottom_y);
            let matched = mix2(top, bottom, 0.5);
            let weight = edge_weight(offset, band);
            image.put_pixel(x, top_y, mix2(top, matched, weight));
            image.put_pixel(x, bottom_y, mix2(bottom, matched, weight));
        }
    }
}

fn mix_tile(
    base: Rgba<u8>,
    x_ref: Rgba<u8>,
    y_ref: Rgba<u8>,
    original: Rgba<u8>,
    wx: f32,
    wy: f32,
) -> Rgba<u8> {
    let wx = wx.clamp(0.0, 1.0);
    let wy = wy.clamp(0.0, 1.0);
    let weights = [
        (1.0 - wx) * (1.0 - wy),
        wx * (1.0 - wy),
        (1.0 - wx) * wy,
        wx * wy,
    ];
    let pixels = [base, x_ref, y_ref, original];
    let mut out = [0u8; 4];

    for channel in 0..4 {
        let value = pixels
            .iter()
            .zip(weights)
            .map(|(pixel, weight)| pixel.0[channel] as f32 * weight)
            .sum::<f32>();
        out[channel] = value.round().clamp(0.0, 255.0) as u8;
    }

    Rgba(out)
}

fn make_seamless(source: &RgbaImage, mode: &str, blend_percent: f32) -> RgbaImage {
    let width = source.width();
    let height = source.height();
    let shift_x = if matches!(mode, "horizontal" | "tile") {
        width / 2
    } else {
        0
    };
    let shift_y = if matches!(mode, "vertical" | "tile") {
        height / 2
    } else {
        0
    };
    let mut output = RgbaImage::new(width, height);

    for y in 0..height {
        let wy = if matches!(mode, "vertical" | "tile") {
            seam_weight(y, height, blend_percent)
        } else {
            0.0
        };

        for x in 0..width {
            let wx = if matches!(mode, "horizontal" | "tile") {
                seam_weight(x, width, blend_percent)
            } else {
                0.0
            };

            let pixel = match mode {
                "horizontal" => {
                    let wrapped = sample_shifted(source, x, y, shift_x, 0);
                    mix2(wrapped, *source.get_pixel(x, y), wx)
                }
                "vertical" => {
                    let wrapped = sample_shifted(source, x, y, 0, shift_y);
                    mix2(wrapped, *source.get_pixel(x, y), wy)
                }
                "tile" => {
                    let base = sample_shifted(source, x, y, shift_x, shift_y);
                    let x_ref = sample_shifted(source, x, y, 0, shift_y);
                    let y_ref = sample_shifted(source, x, y, shift_x, 0);
                    mix_tile(base, x_ref, y_ref, *source.get_pixel(x, y), wx, wy)
                }
                _ => *source.get_pixel(x, y),
            };

            output.put_pixel(x, y, pixel);
        }
    }

    if matches!(mode, "horizontal" | "tile") {
        reconcile_horizontal_edges(&mut output, blend_percent);
    }
    if matches!(mode, "vertical" | "tile") {
        reconcile_vertical_edges(&mut output, blend_percent);
    }

    output
}

#[derive(Debug, Clone, Copy)]
struct AxisSnap {
    period: u32,
    crop: u32,
}

#[derive(Debug, Clone, Copy)]
struct SnapInfo {
    original_width: u32,
    original_height: u32,
    width: u32,
    height: u32,
    period_x: Option<u32>,
    period_y: Option<u32>,
}

struct SnapResult {
    image: RgbaImage,
    info: SnapInfo,
}

fn channel_luma(pixel: Rgba<u8>) -> f32 {
    0.2126 * pixel.0[0] as f32 + 0.7152 * pixel.0[1] as f32 + 0.0722 * pixel.0[2] as f32
}

fn horizontal_shift_score(source: &RgbaImage, lag: u32) -> f32 {
    let width = source.width();
    let height = source.height();
    if lag == 0 || lag >= width || height == 0 {
        return f32::INFINITY;
    }

    let span = width - lag;
    let x_step = (span / 96).max(1);
    let y_step = (height / 128).max(1);
    let mut total = 0.0f32;
    let mut count = 0u32;
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < span {
            let head = *source.get_pixel(x, y);
            let shifted = *source.get_pixel(x + lag, y);
            for channel in 0..3 {
                total += (head.0[channel] as f32 - shifted.0[channel] as f32).abs();
            }
            count += 3;
            x += x_step;
        }
        y += y_step;
    }

    if count == 0 {
        f32::INFINITY
    } else {
        total / count as f32
    }
}

fn structure_sample(source: &RgbaImage, x: u32, y: u32) -> f32 {
    let width = source.width();
    let height = source.height();
    let left = channel_luma(*source.get_pixel(x.saturating_sub(1), y));
    let right = channel_luma(*source.get_pixel((x + 1).min(width - 1), y));
    let top = channel_luma(*source.get_pixel(x, y.saturating_sub(1)));
    let bottom = channel_luma(*source.get_pixel(x, (y + 1).min(height - 1)));
    let gradient = (right - left).abs() + (bottom - top).abs();
    if gradient >= 24.0 {
        255.0
    } else {
        0.0
    }
}

fn horizontal_structure_shift_score(source: &RgbaImage, lag: u32) -> f32 {
    let width = source.width();
    let height = source.height();
    if lag == 0 || lag >= width || height == 0 {
        return f32::INFINITY;
    }

    let span = width - lag;
    let x_step = (span / 128).max(1);
    let y_step = (height / 160).max(1);
    let mut total = 0.0f32;
    let mut count = 0u32;
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < span {
            total += (structure_sample(source, x, y) - structure_sample(source, x + lag, y)).abs();
            count += 1;
            x += x_step;
        }
        y += y_step;
    }

    if count == 0 {
        f32::INFINITY
    } else {
        total / count as f32
    }
}

fn detect_periodic_axis_by_score(
    source: &RgbaImage,
    score_fn: fn(&RgbaImage, u32) -> f32,
    max_best_score: f32,
    tolerance_floor: f32,
    tolerance_ratio: f32,
    anti_ratio: f32,
) -> Option<AxisSnap> {
    let width = source.width();
    if width < 16 {
        return None;
    }

    let max_lag = width / 2;
    if max_lag < 4 {
        return None;
    }

    let scores = (4..=max_lag)
        .map(|lag| (lag, score_fn(source, lag)))
        .collect::<Vec<_>>();
    let best_score = scores
        .iter()
        .map(|(_, score)| *score)
        .fold(f32::INFINITY, f32::min);
    // The repeat must match closely in absolute terms or the image is not periodic.
    if !best_score.is_finite() || best_score > max_best_score {
        return None;
    }

    // Fundamental period = smallest lag whose score is close to the global best.
    let tolerance = best_score.mul_add(tolerance_ratio, tolerance_floor);
    let period = scores
        .iter()
        .find(|(_, score)| *score <= best_score + tolerance)
        .map(|(lag, _)| *lag)?;

    // A true repeating pattern scores much worse half a period out of phase;
    // smooth gradients score low at every small lag and must not snap.
    let anti_lag = (period + (period / 2).max(1)).min(width - 1);
    let anti_score = score_fn(source, anti_lag);
    if anti_score < anti_ratio * (best_score + 1.0) {
        return None;
    }

    let crop = (width / period) * period;
    if crop == width || crop < 8 {
        None
    } else {
        Some(AxisSnap { period, crop })
    }
}

fn detect_periodic_axis(source: &RgbaImage) -> Option<AxisSnap> {
    let color = detect_periodic_axis_by_score(source, horizontal_shift_score, 26.0, 2.0, 0.25, 4.0);
    let structure = detect_periodic_axis_by_score(
        source,
        horizontal_structure_shift_score,
        38.0,
        3.0,
        0.35,
        2.6,
    );

    match (color, structure) {
        (Some(color), Some(structure)) => {
            if structure.crop >= color.crop {
                Some(structure)
            } else {
                Some(color)
            }
        }
        (Some(color), None) => Some(color),
        (None, Some(structure)) => Some(structure),
        (None, None) => None,
    }
}

#[cfg(test)]
fn detect_periodic_crop_width(source: &RgbaImage) -> Option<u32> {
    detect_periodic_axis(source).map(|snap| snap.crop)
}

fn snap_periodic_crop_with_info(source: &RgbaImage, mode: &str) -> SnapResult {
    let snap_x = matches!(mode, "horizontal" | "tile");
    let snap_y = matches!(mode, "vertical" | "tile");
    let crop_x = if snap_x {
        detect_periodic_axis(source)
    } else {
        None
    };
    let crop_y = if snap_y {
        detect_periodic_axis(&transpose_image(source))
    } else {
        None
    };

    let width = crop_x.map(|snap| snap.crop).unwrap_or(source.width());
    let height = crop_y.map(|snap| snap.crop).unwrap_or(source.height());
    let info = SnapInfo {
        original_width: source.width(),
        original_height: source.height(),
        width,
        height,
        period_x: crop_x.map(|snap| snap.period),
        period_y: crop_y.map(|snap| snap.period),
    };
    if width == source.width() && height == source.height() {
        return SnapResult {
            image: source.clone(),
            info,
        };
    }

    SnapResult {
        image: image::imageops::crop_imm(source, 0, 0, width, height).to_image(),
        info,
    }
}

#[cfg(test)]
fn snap_periodic_crop(source: &RgbaImage, mode: &str) -> RgbaImage {
    snap_periodic_crop_with_info(source, mode).image
}

fn snap_status_message(info: Option<SnapInfo>) -> String {
    let Some(info) = info else {
        return "Saved".to_string();
    };

    if info.width == info.original_width && info.height == info.original_height {
        return "Saved - snap on, no partial repeat found".to_string();
    }

    let mut periods = Vec::new();
    if let Some(period) = info.period_x {
        periods.push(format!("x {period}px"));
    }
    if let Some(period) = info.period_y {
        periods.push(format!("y {period}px"));
    }
    let period_label = if periods.is_empty() {
        String::new()
    } else {
        format!(" ({})", periods.join(", "))
    };
    format!(
        "Saved - snapped {}x{} to {}x{}{}",
        info.original_width, info.original_height, info.width, info.height, period_label
    )
}

fn transpose_image(source: &RgbaImage) -> RgbaImage {
    let mut output = RgbaImage::new(source.height(), source.width());
    for y in 0..source.height() {
        for x in 0..source.width() {
            output.put_pixel(y, x, *source.get_pixel(x, y));
        }
    }
    output
}

fn layer_delta(source: &RgbaImage, x: u32, y: u32, shift_x: u32) -> f32 {
    let rolled = sample_shifted(source, x, y, shift_x, 0);
    let original = *source.get_pixel(x, y);
    (0..3)
        .map(|channel| (rolled.0[channel] as f32 - original.0[channel] as f32).abs())
        .sum::<f32>()
        / 3.0
}

fn layer_gradient_delta(source: &RgbaImage, x: u32, y: u32, shift_x: u32) -> f32 {
    let width = source.width();
    let left_x = x.saturating_sub(1);
    let right_x = (x + 1).min(width - 1);
    let rolled_left = sample_shifted(source, left_x, y, shift_x, 0);
    let rolled_right = sample_shifted(source, right_x, y, shift_x, 0);
    let original_left = *source.get_pixel(left_x, y);
    let original_right = *source.get_pixel(right_x, y);

    (0..3)
        .map(|channel| {
            let rolled_grad = rolled_right.0[channel] as f32 - rolled_left.0[channel] as f32;
            let original_grad = original_right.0[channel] as f32 - original_left.0[channel] as f32;
            (rolled_grad - original_grad).abs()
        })
        .sum::<f32>()
        / 3.0
}

fn seam_cost(source: &RgbaImage, x: u32, y: u32, shift_x: u32) -> f32 {
    layer_delta(source, x, y, shift_x) + 0.5 * layer_gradient_delta(source, x, y, shift_x)
}

fn minimum_vertical_path(source: &RgbaImage, start_x: u32, end_x: u32, shift_x: u32) -> Vec<u32> {
    let height = source.height() as usize;
    let band_width = (end_x - start_x + 1) as usize;
    let mut cumulative = vec![0.0f32; height * band_width];
    let mut backtrack = vec![0usize; height * band_width];

    for y in 0..height {
        for local_x in 0..band_width {
            let x = start_x + local_x as u32;
            let current_cost = seam_cost(source, x, y as u32, shift_x);
            let index = y * band_width + local_x;
            if y == 0 {
                cumulative[index] = current_cost;
                backtrack[index] = local_x;
                continue;
            }

            let mut best_x = local_x;
            let mut best_cost = f32::INFINITY;
            let prev_start = local_x.saturating_sub(1);
            let prev_end = (local_x + 1).min(band_width - 1);
            for prev_x in prev_start..=prev_end {
                let candidate = cumulative[(y - 1) * band_width + prev_x];
                if candidate < best_cost {
                    best_cost = candidate;
                    best_x = prev_x;
                }
            }
            cumulative[index] = current_cost + best_cost;
            backtrack[index] = best_x;
        }
    }

    let last_row = height - 1;
    let mut best_x = 0usize;
    let mut best_cost = f32::INFINITY;
    for local_x in 0..band_width {
        let candidate = cumulative[last_row * band_width + local_x];
        if candidate < best_cost {
            best_cost = candidate;
            best_x = local_x;
        }
    }

    let mut path = vec![start_x; height];
    let mut local_x = best_x;
    for y in (0..height).rev() {
        path[y] = start_x + local_x as u32;
        local_x = backtrack[y * band_width + local_x];
    }
    path
}

fn smooth_path_delta(values: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let radius = 7usize;
    let mut smoothed = vec![[0.0; 3]; values.len()];
    for (index, output) in smoothed.iter_mut().enumerate() {
        let start = index.saturating_sub(radius);
        let end = (index + radius).min(values.len() - 1);
        let count = (end - start + 1) as f32;
        for value in values.iter().take(end + 1).skip(start) {
            for channel in 0..3 {
                output[channel] += value[channel] / count;
            }
        }
    }
    smoothed
}

fn path_delta(source: &RgbaImage, path: &[u32], shift_x: u32) -> Vec<[f32; 3]> {
    let raw = path
        .iter()
        .enumerate()
        .map(|(y, &x)| {
            let rolled = sample_shifted(source, x, y as u32, shift_x, 0);
            let original = *source.get_pixel(x, y as u32);
            [
                rolled.0[0] as f32 - original.0[0] as f32,
                rolled.0[1] as f32 - original.0[1] as f32,
                rolled.0[2] as f32 - original.0[2] as f32,
            ]
        })
        .collect::<Vec<_>>();
    let smoothed = smooth_path_delta(&raw);
    let max_abs = smoothed
        .iter()
        .flat_map(|value| value.iter())
        .map(|value| value.abs())
        .fold(0.0f32, f32::max);

    if !(2.0..=64.0).contains(&max_abs) {
        vec![[0.0; 3]; path.len()]
    } else {
        smoothed
    }
}

fn corrected_mix(
    rolled: Rgba<u8>,
    original: Rgba<u8>,
    weight_original: f32,
    left_side: f32,
    left_delta: [f32; 3],
    right_side: f32,
    right_delta: [f32; 3],
    left_falloff: f32,
    right_falloff: f32,
) -> Rgba<u8> {
    let mut mixed = mix2(rolled, original, weight_original);
    for channel in 0..3 {
        let correction = 0.5
            * (left_delta[channel] * left_side * left_falloff
                + right_delta[channel] * right_side * right_falloff);
        mixed.0[channel] = clamp_channel(mixed.0[channel] as f32 + correction);
    }
    mixed
}

fn make_horizontal_seam_cut(source: &RgbaImage, blend_percent: f32) -> RgbaImage {
    let width = source.width();
    let height = source.height();
    if width < 4 || height == 0 {
        let mut output = source.clone();
        snap_horizontal_edges(&mut output);
        return output;
    }

    let shift_x = width / 2;
    let center = shift_x;
    let band = seam_cut_band(width, blend_percent);
    let left_start = center.saturating_sub(band);
    let left_end = center.min(width - 1);
    let right_start = center.min(width - 1);
    let right_end = (center + band).min(width - 1);
    let left_path = minimum_vertical_path(source, left_start, left_end, shift_x);
    let right_path = minimum_vertical_path(source, right_start, right_end, shift_x);
    let left_delta = path_delta(source, &left_path, shift_x);
    let right_delta = path_delta(source, &right_path, shift_x);
    let falloff = (band as f32 / 2.0).max(1.0);
    let feather = SEAM_FEATHER as f32;
    let mut output = RgbaImage::new(width, height);

    for y in 0..height {
        let left = left_path[y as usize] as i32;
        let right = right_path[y as usize] as i32;
        for x in 0..width {
            let xi = x as i32;
            let rolled = sample_shifted(source, x, y, shift_x, 0);
            let original = *source.get_pixel(x, y);
            let left_t = smoothstep((xi - left + SEAM_FEATHER) as f32 / (2.0 * feather));
            let right_t = smoothstep((xi - right + SEAM_FEATHER) as f32 / (2.0 * feather));
            let weight_original = (left_t * (1.0 - right_t)).clamp(0.0, 1.0);
            let left_distance = (xi - left).abs() as f32;
            let right_distance = (xi - right).abs() as f32;
            let left_falloff = if left_distance <= band as f32 {
                (-left_distance / falloff).exp()
            } else {
                0.0
            };
            let right_falloff = if right_distance <= band as f32 {
                (-right_distance / falloff).exp()
            } else {
                0.0
            };
            let left_side = (2.0 * left_t - 1.0).clamp(-1.0, 1.0);
            let right_side = (1.0 - 2.0 * right_t).clamp(-1.0, 1.0);
            let pixel = corrected_mix(
                rolled,
                original,
                weight_original,
                left_side,
                left_delta[y as usize],
                right_side,
                right_delta[y as usize],
                left_falloff,
                right_falloff,
            );
            output.put_pixel(x, y, pixel);
        }
    }

    snap_horizontal_edges(&mut output);
    output
}

fn make_seam_cut_seamless(source: &RgbaImage, mode: &str, blend_percent: f32) -> RgbaImage {
    match mode {
        "horizontal" => make_horizontal_seam_cut(source, blend_percent),
        "vertical" => transpose_image(&make_horizontal_seam_cut(
            &transpose_image(source),
            blend_percent,
        )),
        "tile" => {
            let horizontal = make_horizontal_seam_cut(source, blend_percent);
            let mut output = transpose_image(&make_horizontal_seam_cut(
                &transpose_image(&horizontal),
                blend_percent,
            ));
            snap_horizontal_edges(&mut output);
            snap_vertical_edges(&mut output);
            output
        }
        _ => source.clone(),
    }
}

fn flatten_low_frequency(source: &RgbaImage, strength: f32) -> RgbaImage {
    let strength = strength.clamp(0.0, 1.0);
    if strength <= 0.0 || source.width() == 0 || source.height() == 0 {
        return source.clone();
    }

    let max_dim = source.width().max(source.height());
    let scale = 32.0 / max_dim as f32;
    let low_width = ((source.width() as f32 * scale).round() as u32).max(1);
    let low_height = ((source.height() as f32 * scale).round() as u32).max(1);
    let low_small = image::imageops::resize(source, low_width, low_height, FilterType::Triangle);
    let low = image::imageops::resize(
        &low_small,
        source.width(),
        source.height(),
        FilterType::Triangle,
    );
    let mut mean = [0.0f32; 3];
    let count = (low.width() * low.height()) as f32;
    for pixel in low.pixels() {
        for channel in 0..3 {
            mean[channel] += pixel.0[channel] as f32 / count;
        }
    }

    let mut output = source.clone();
    for y in 0..source.height() {
        for x in 0..source.width() {
            let src = *source.get_pixel(x, y);
            let low_pixel = *low.get_pixel(x, y);
            let mut out = src;
            for channel in 0..3 {
                out.0[channel] = clamp_channel(
                    src.0[channel] as f32
                        - strength * (low_pixel.0[channel] as f32 - mean[channel]),
                );
            }
            output.put_pixel(x, y, out);
        }
    }
    output
}

fn make_content_aware_seamless(
    source: &RgbaImage,
    mode: &str,
    blend_percent: f32,
) -> Result<RgbaImage, String> {
    let temp_dir = temp_job_dir()?;
    let source_path = temp_dir.join("source.png");
    let mask_path = temp_dir.join("mask.png");
    let generated_path = temp_dir.join("generated.png");

    source
        .save(&source_path)
        .map_err(|error| format!("Unable to write synthesis source: {error}"))?;
    synthesis_mask(source.width(), source.height(), mode, blend_percent)
        .save(&mask_path)
        .map_err(|error| format!("Unable to write synthesis mask: {error}"))?;

    let example = texsynth::Example::builder(&source_path).set_sample_method(&mask_path);
    let session = texsynth::Session::builder()
        .inpaint_example(
            &mask_path,
            example,
            texsynth::Dims::new(source.width(), source.height()),
        )
        .tiling_mode(true)
        .seed(211)
        .nearest_neighbors(32)
        .random_sample_locations(32)
        .backtrack_percent(0.35)
        .backtrack_stages(3)
        .build()
        .map_err(|error| format!("Unable to build texture synthesis session: {error}"))?;

    session
        .run(None)
        .save(&generated_path)
        .map_err(|error| format!("Unable to save texture synthesis output: {error}"))?;

    let mut generated = image::open(&generated_path)
        .map_err(|error| format!("Unable to read texture synthesis output: {error}"))?
        .to_rgba8();
    let edge_polish = blend_percent.min(8.0);
    if matches!(mode, "horizontal" | "tile") {
        reconcile_horizontal_edges(&mut generated, edge_polish);
    }
    if matches!(mode, "vertical" | "tile") {
        reconcile_vertical_edges(&mut generated, edge_polish);
    }
    let _ = fs::remove_dir_all(&temp_dir);
    Ok(generated)
}

fn process_one(path: &Path, options: &SeamlessOptions) -> Result<ProcessedOutput, String> {
    if !path.is_file() {
        return Err("Input path is not a file.".to_string());
    }

    let mode = normalized_mode(&options.mode)?;
    let strategy = normalized_strategy(&options.strategy)?;
    let (_, image_format) = normalized_format(&options.output_format)?;
    let image = read_image_rgba(path)?;
    if image.width() < 2 || image.height() < 2 {
        return Err("Image must be at least 2x2 pixels.".to_string());
    }

    let snap_result = if options.snap_period {
        Some(snap_periodic_crop_with_info(&image, mode))
    } else {
        None
    };
    let (image, snap_info) = match snap_result {
        Some(result) => (result.image, Some(result.info)),
        None => (image, None),
    };
    let prepared = flatten_low_frequency(&image, options.flatten);
    let seamless = match strategy {
        "seam-cut" => make_seam_cut_seamless(&prepared, mode, options.blend_percent),
        "synthesis" => make_content_aware_seamless(&prepared, mode, options.blend_percent)
            .unwrap_or_else(|_| make_seam_cut_seamless(&prepared, mode, options.blend_percent)),
        "blend" => make_seamless(&prepared, mode, options.blend_percent),
        _ => unreachable!("strategy normalized before dispatch"),
    };
    let output_path = output_path_for(path, options)?;
    seamless
        .save_with_format(&output_path, image_format)
        .map_err(|error| format!("Unable to save output: {error}"))?;
    Ok(ProcessedOutput {
        output_path,
        message: snap_status_message(snap_info),
    })
}

fn process_paths(
    app: AppHandle,
    active_jobs: ActiveJobState,
    job_id: String,
    paths: Vec<String>,
    options: SeamlessOptions,
) -> Result<Vec<ProcessResult>, String> {
    let mut results = Vec::new();
    for path in paths {
        if active_jobs.is_canceled(&job_id) {
            break;
        }
        let _ = app.emit(
            "seamless-worker-event",
            serde_json::json!({ "type": "image_start", "path": path }),
        );
        let input_path = PathBuf::from(&path);
        match process_one(&input_path, &options) {
            Ok(processed) => {
                let output = processed.output_path.display().to_string();
                let message = processed.message;
                let _ = app.emit(
                    "seamless-worker-event",
                    serde_json::json!({ "type": "image_done", "path": path, "output": output, "message": message }),
                );
                results.push(ProcessResult {
                    input_path: path,
                    output_path: Some(output),
                    status: "done".to_string(),
                    message,
                });
            }
            Err(error) => {
                let _ = app.emit(
                    "seamless-worker-event",
                    serde_json::json!({ "type": "image_error", "path": path, "message": error }),
                );
                results.push(ProcessResult {
                    input_path: path,
                    output_path: None,
                    status: "error".to_string(),
                    message: error,
                });
            }
        }
    }

    let was_canceled = active_jobs.finish(&job_id)?;
    let event = if was_canceled {
        serde_json::json!({ "type": "canceled", "message": "Seamless job canceled." })
    } else {
        serde_json::json!({ "type": "done" })
    };
    let _ = app.emit("seamless-worker-event", event);
    Ok(results)
}

#[tauri::command]
fn resolve_inputs(paths: Vec<String>, recursive: bool) -> Vec<String> {
    let mut output = Vec::new();
    for path in paths {
        collect_images(Path::new(&path), recursive, &mut output);
    }
    output.sort();
    output.dedup();
    output
}

#[tauri::command]
fn preview_image_data_url(path: String) -> Result<String, String> {
    let image_path = PathBuf::from(path);
    if !image_path.is_file() {
        return Err("Preview image was not found.".to_string());
    }
    if is_webp_file(&image_path) {
        let image = read_image_rgba(&image_path)?;
        return rgba_png_data_url(&image);
    }
    let mime_type = image_mime_type(&image_path)?;
    let bytes =
        fs::read(&image_path).map_err(|error| format!("Unable to read preview image: {error}"))?;
    Ok(format!(
        "data:{mime_type};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    ))
}

#[tauri::command]
fn open_containing_folder(app: AppHandle, path: String) -> Result<(), String> {
    let requested_path = PathBuf::from(path);
    let folder = if requested_path.is_dir() {
        requested_path
    } else {
        requested_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "Unable to resolve containing folder.".to_string())?
    };
    if !folder.is_dir() {
        return Err(format!("Folder does not exist: {}", folder.display()));
    }
    app.opener()
        .open_path(folder.display().to_string(), None::<&str>)
        .map_err(|error| format!("Unable to open folder: {error}"))
}

#[tauri::command]
async fn start_seamless_job(
    app: AppHandle,
    active_jobs: State<'_, ActiveJobState>,
    paths: Vec<String>,
    options: SeamlessOptions,
) -> Result<Vec<ProcessResult>, String> {
    if paths.is_empty() {
        return Err("No input images were provided.".to_string());
    }
    let job_id = next_job_id();
    active_jobs.start(job_id.clone())?;
    let active_jobs = active_jobs.inner().clone();

    tauri::async_runtime::spawn_blocking(move || process_paths(app, active_jobs, job_id, paths, options))
        .await
        .map_err(|error| format!("Seamless worker failed: {error}"))?
}

#[tauri::command]
fn cancel_active_job(active_jobs: State<'_, ActiveJobState>) -> Result<(), String> {
    active_jobs.request_cancel()
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn validate_agent_api_port(port: Option<u16>) -> Result<u16, String> {
    let port = port
        .or_else(read_registered_agent_api_port)
        .unwrap_or(DEFAULT_AGENT_API_PORT);
    if port == 0 {
        return Err("Agent API port must be between 1 and 65535.".to_string());
    }
    Ok(port)
}

fn agent_api_url(port: u16) -> String {
    format!("http://{AGENT_API_BIND_ADDRESS}:{port}")
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn shared_neko_legends_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library").join("Application Support"))
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("NekoLegends"))
}

fn agent_api_registry_path() -> Option<PathBuf> {
    Some(shared_neko_legends_dir()?.join(AGENT_API_REGISTRY_FILE))
}

fn read_agent_api_registry() -> AgentApiRegistry {
    let updated_at = timestamp_string();
    let Some(path) = agent_api_registry_path() else {
        return AgentApiRegistry {
            updated_at,
            apps: Vec::new(),
        };
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(AgentApiRegistry {
            updated_at,
            apps: Vec::new(),
        })
}

fn read_registered_agent_api_port() -> Option<u16> {
    read_agent_api_registry()
        .apps
        .into_iter()
        .find(|entry| entry.app_id == AGENT_APP_ID)
        .map(|entry| entry.port)
        .filter(|port| *port > 0)
}

fn publish_agent_api_status(status: &AgentServerStatus) {
    let Some(path) = agent_api_registry_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut registry = read_agent_api_registry();
    let updated_at = timestamp_string();
    let entry = AgentApiRegistryEntry {
        app_id: AGENT_APP_ID.to_string(),
        app_name: AGENT_APP_NAME.to_string(),
        default_port: DEFAULT_AGENT_API_PORT,
        bind_address: AGENT_API_BIND_ADDRESS.to_string(),
        port: status.port,
        enabled: status.enabled,
        url: status.url.clone(),
        openapi_url: status.openapi_url.clone(),
        busy: status.busy,
        active_job_id: status.active_job_id.clone(),
        last_seen: Some(updated_at.clone()),
        note: Some("Local Agent API.".to_string()),
    };
    if let Some(existing) = registry
        .apps
        .iter_mut()
        .find(|entry| entry.app_id == AGENT_APP_ID)
    {
        *existing = entry;
    } else {
        registry.apps.push(entry);
    }
    registry.updated_at = updated_at;
    if let Ok(raw) = serde_json::to_string_pretty(&registry) {
        let _ = fs::write(path, raw);
    }
}

fn agent_status_from(
    agent_state: &AgentServerState,
    active_jobs: &ActiveJobState,
) -> Result<AgentServerStatus, String> {
    let (enabled, port) = {
        let control = agent_state
            .inner
            .lock()
            .map_err(|_| "Unable to lock agent server state.".to_string())?;
        (control.enabled, control.port())
    };
    let busy = active_jobs.is_busy()?;
    let active_job_id = active_jobs.active_job_id()?;
    let status = AgentServerStatus {
        enabled,
        port,
        url: agent_api_url(port),
        openapi_url: format!("{}/openapi.json", agent_api_url(port)),
        busy,
        active_job_id,
        message: if enabled {
            "Agent API is enabled.".to_string()
        } else {
            "Agent API is off.".to_string()
        },
    };
    publish_agent_api_status(&status);
    Ok(status)
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("Unable to set read timeout: {error}"))?;
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_len: Option<usize> = None;

    loop {
        let bytes_read = stream
            .read(&mut buffer)
            .map_err(|error| format!("Unable to read agent request: {error}"))?;
        if bytes_read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..bytes_read]);
        if let Some(header_end) = find_header_end(&data) {
            if expected_len.is_none() {
                let headers = String::from_utf8_lossy(&data[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|len| data.len() >= len) {
                break;
            }
        }
        if data.len() > 2 * 1024 * 1024 {
            return Err("Agent request is too large.".to_string());
        }
    }

    let header_end = find_header_end(&data).ok_or_else(|| "Invalid HTTP request.".to_string())?;
    let headers = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "Invalid HTTP request line.".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let raw_path = parts.next().unwrap_or("/").to_string();
    let path = raw_path.split('?').next().unwrap_or("/").to_string();
    let body_start = header_end + 4;
    let body = if body_start <= data.len() {
        data[body_start..].to_vec()
    } else {
        Vec::new()
    };
    Ok(HttpRequest { method, path, body })
}

fn write_json_response(
    stream: &mut TcpStream,
    status: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(&payload)
        .map_err(|error| format!("Unable to serialize agent response: {error}"))?;
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| format!("Unable to write agent response: {error}"))
}

fn write_empty_response(stream: &mut TcpStream, status: &str) -> Result<(), String> {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(headers.as_bytes())
        .map_err(|error| format!("Unable to write agent response: {error}"))
}

fn agent_openapi(port: u16) -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Seamless Image Edit Agent API",
            "version": env!("CARGO_PKG_VERSION")
        },
        "servers": [{ "url": agent_api_url(port) }],
        "paths": {
            "/health": { "get": { "summary": "Check API status" } },
            "/status": { "get": { "summary": "Check active job status" } },
            "/process": { "post": { "summary": "Create seamless image outputs" } },
            "/generate": { "post": { "summary": "Alias for /process" } },
            "/cancel": { "post": { "summary": "Cancel the active batch before the next image" } }
        }
    })
}

fn parse_agent_job_request(body: &[u8]) -> Result<AgentJobRequest, String> {
    if body.is_empty() {
        return Ok(AgentJobRequest {
            paths: None,
            options: None,
        });
    }
    serde_json::from_slice(body).map_err(|error| format!("Invalid JSON request: {error}"))
}

fn agent_options_from_request(options: Option<serde_json::Value>) -> Result<SeamlessOptions, String> {
    let mut merged = serde_json::to_value(default_seamless_options())
        .map_err(|error| format!("Unable to build default options: {error}"))?;
    let Some(options) = options else {
        return serde_json::from_value(merged)
            .map_err(|error| format!("Unable to read default options: {error}"));
    };
    if options.is_null() {
        return serde_json::from_value(merged)
            .map_err(|error| format!("Unable to read default options: {error}"));
    }
    let overrides = options
        .as_object()
        .ok_or_else(|| "Agent options must be a JSON object.".to_string())?;
    let base = merged
        .as_object_mut()
        .ok_or_else(|| "Unable to merge default options.".to_string())?;
    for (key, value) in overrides {
        base.insert(key.clone(), value.clone());
    }
    serde_json::from_value(merged).map_err(|error| format!("Invalid agent options: {error}"))
}

fn handle_agent_route(
    request: HttpRequest,
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    agent_state: &AgentServerState,
) -> Result<serde_json::Value, String> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => Ok(serde_json::json!({
            "ok": true,
            "service": "Seamless Image Edit",
            "version": env!("CARGO_PKG_VERSION"),
            "url": agent_status_from(agent_state, active_jobs)?.url
        })),
        ("GET", "/openapi.json") => Ok(agent_openapi(agent_status_from(agent_state, active_jobs)?.port)),
        ("GET", "/status") => {
            serde_json::to_value(agent_status_from(agent_state, active_jobs)?)
                .map_err(|error| error.to_string())
        }
        ("POST", "/process") | ("POST", "/generate") => {
            let request = parse_agent_job_request(&request.body)?;
            let options = agent_options_from_request(request.options)?;
            let paths = resolve_inputs(request.paths.unwrap_or_default(), options.recursive);
            if paths.is_empty() {
                return Err("No input images were provided.".to_string());
            }
            let job_id = next_job_id();
            active_jobs.start(job_id.clone())?;
            let results = process_paths(app.clone(), active_jobs.clone(), job_id.clone(), paths, options)?;
            Ok(serde_json::json!({ "ok": true, "jobId": job_id, "results": results }))
        }
        ("POST", "/cancel") => {
            active_jobs.request_cancel()?;
            Ok(serde_json::json!({ "ok": true }))
        }
        _ => Err(format!(
            "No agent endpoint for {} {}",
            request.method, request.path
        )),
    }
}

fn handle_agent_stream(
    mut stream: TcpStream,
    app: &AppHandle,
    active_jobs: &ActiveJobState,
    agent_state: &AgentServerState,
) {
    let result = read_http_request(&mut stream).and_then(|request| {
        if request.method == "OPTIONS" {
            return write_empty_response(&mut stream, "204 No Content");
        }
        match handle_agent_route(request, app, active_jobs, agent_state) {
            Ok(payload) => write_json_response(&mut stream, "200 OK", payload),
            Err(error) => write_json_response(
                &mut stream,
                "400 Bad Request",
                serde_json::json!({ "ok": false, "error": error }),
            ),
        }
    });
    if let Err(error) = result {
        let _ = write_json_response(
            &mut stream,
            "400 Bad Request",
            serde_json::json!({ "ok": false, "error": error }),
        );
    }
}

fn run_agent_server(
    listener: TcpListener,
    app: AppHandle,
    active_jobs: ActiveJobState,
    agent_state: AgentServerState,
    stop: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle_agent_stream(stream, &app, &active_jobs, &agent_state),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(80));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(150));
            }
        }
    }
}

#[tauri::command]
fn get_agent_server_status(
    agent_state: State<'_, AgentServerState>,
    active_jobs: State<'_, ActiveJobState>,
) -> Result<AgentServerStatus, String> {
    agent_status_from(agent_state.inner(), active_jobs.inner())
}

fn set_agent_server_enabled_inner(
    app: AppHandle,
    agent_state: &AgentServerState,
    active_jobs: &ActiveJobState,
    enabled: bool,
    port: Option<u16>,
) -> Result<AgentServerStatus, String> {
    let port = validate_agent_api_port(port)?;
    {
        let mut control = agent_state
            .inner
            .lock()
            .map_err(|_| "Unable to lock agent server state.".to_string())?;

        if control.enabled && (!enabled || control.port() != port) {
            if let Some(stop) = control.stop.take() {
                stop.store(true, Ordering::SeqCst);
            }
            control.enabled = false;
        }
        control.port = port;

        if enabled && !control.enabled {
            let listener = TcpListener::bind(("127.0.0.1", port))
                .map_err(|error| format!("Unable to start Agent API: {error}"))?;
            let stop = Arc::new(AtomicBool::new(false));
            thread::spawn({
                let app = app.clone();
                let active_jobs = active_jobs.clone();
                let agent_state = agent_state.clone();
                let stop = stop.clone();
                move || run_agent_server(listener, app, active_jobs, agent_state, stop)
            });
            control.enabled = true;
            control.stop = Some(stop);
        }
    }

    agent_status_from(agent_state, active_jobs)
}

#[tauri::command]
fn set_agent_server_enabled(
    app: AppHandle,
    agent_state: State<'_, AgentServerState>,
    active_jobs: State<'_, ActiveJobState>,
    enabled: bool,
    port: Option<u16>,
) -> Result<AgentServerStatus, String> {
    set_agent_server_enabled_inner(
        app,
        agent_state.inner(),
        active_jobs.inner(),
        enabled,
        port,
    )
}

pub fn run() {
    tauri::Builder::default()
        .manage(ActiveJobState::default())
        .manage(AgentServerState::default())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            cancel_active_job,
            get_agent_server_status,
            open_containing_folder,
            preview_image_data_url,
            resolve_inputs,
            set_agent_server_enabled,
            start_seamless_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.put_pixel(
                    x,
                    y,
                    Rgba([(x * 17) as u8, (y * 19) as u8, (x + y) as u8, 255]),
                );
            }
        }
        image
    }

    #[test]
    fn horizontal_mode_preserves_wrapped_outer_edges() {
        let source = gradient(16, 12);
        let output = make_seamless(&source, "horizontal", 18.0);
        for y in 0..source.height() {
            assert_eq!(*output.get_pixel(0, y), *output.get_pixel(15, y));
        }
    }

    #[test]
    fn tile_mode_preserves_wrapped_corners_and_repairs_center() {
        let source = gradient(16, 12);
        let output = make_seamless(&source, "tile", 18.0);
        assert_eq!(*output.get_pixel(0, 0), *output.get_pixel(15, 0));
        assert_eq!(*output.get_pixel(0, 11), *output.get_pixel(15, 11));
        assert_eq!(*output.get_pixel(0, 0), *output.get_pixel(0, 11));
        assert_eq!(*output.get_pixel(15, 0), *output.get_pixel(15, 11));
        assert_ne!(*output.get_pixel(8, 6), sample_shifted(&source, 8, 6, 8, 6));
    }

    fn harsh_nonseamless_fixture(size: u32) -> RgbaImage {
        let mut image = RgbaImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let checker = if ((x / 16) + (y / 16)) % 2 == 0 {
                    30
                } else {
                    95
                };
                let red = ((x as f32 / size as f32) * 190.0).round() as u8;
                let green = ((y as f32 / size as f32) * 180.0).round() as u8;
                image.put_pixel(x, y, Rgba([red.saturating_add(checker), green, 150, 255]));
            }
        }

        for i in 0..size {
            image.put_pixel(0, i, Rgba([255, 0, 0, 255]));
            image.put_pixel(size - 1, i, Rgba([0, 255, 255, 255]));
            image.put_pixel(i, 0, Rgba([255, 255, 0, 255]));
            image.put_pixel(i, size - 1, Rgba([0, 0, 255, 255]));
        }

        for i in 20..(size - 20) {
            image.put_pixel(i, i, Rgba([255, 255, 255, 255]));
            image.put_pixel(size - 1 - i, i, Rgba([0, 0, 0, 255]));
        }

        image
    }

    fn tile_2x2(source: &RgbaImage) -> RgbaImage {
        let mut tiled = RgbaImage::new(source.width() * 2, source.height() * 2);
        for tile_y in 0..2 {
            for tile_x in 0..2 {
                for y in 0..source.height() {
                    for x in 0..source.width() {
                        tiled.put_pixel(
                            tile_x * source.width() + x,
                            tile_y * source.height() + y,
                            *source.get_pixel(x, y),
                        );
                    }
                }
            }
        }
        tiled
    }

    fn paste(target: &mut RgbaImage, source: &RgbaImage, offset_x: u32, offset_y: u32) {
        for y in 0..source.height() {
            for x in 0..source.width() {
                target.put_pixel(offset_x + x, offset_y + y, *source.get_pixel(x, y));
            }
        }
    }

    fn seam_score(image: &RgbaImage) -> f32 {
        let width = image.width();
        let height = image.height();
        let horizontal = (0..height)
            .map(|y| channel_delta(*image.get_pixel(0, y), *image.get_pixel(width - 1, y)))
            .sum::<f32>()
            / height as f32;
        let vertical = (0..width)
            .map(|x| channel_delta(*image.get_pixel(x, 0), *image.get_pixel(x, height - 1)))
            .sum::<f32>()
            / width as f32;
        (horizontal + vertical) / 2.0
    }

    fn channel_delta(a: Rgba<u8>, b: Rgba<u8>) -> f32 {
        (0..3)
            .map(|channel| (a.0[channel] as f32 - b.0[channel] as f32).abs())
            .sum::<f32>()
            / 3.0
    }

    fn assert_horizontal_edges_exact(image: &RgbaImage) {
        for y in 0..image.height() {
            assert_eq!(
                *image.get_pixel(0, y),
                *image.get_pixel(image.width() - 1, y)
            );
        }
    }

    fn assert_vertical_edges_exact(image: &RgbaImage) {
        for x in 0..image.width() {
            assert_eq!(
                *image.get_pixel(x, 0),
                *image.get_pixel(x, image.height() - 1)
            );
        }
    }

    fn vertical_bars(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let color = if (x / 8) % 2 == 0 {
                    Rgba([25, 210, 90, 255])
                } else {
                    Rgba([220, 35, 180, 255])
                };
                image.put_pixel(x, y, color);
            }
        }
        image
    }

    fn illumination_gradient(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        let center_x = 0.0;
        let center_y = (height.saturating_sub(1)) as f32 * 0.5;
        let max_distance = ((width.saturating_sub(1) as f32 - center_x).powi(2) + center_y.powi(2))
            .sqrt()
            .max(1.0);
        for y in 0..height {
            for x in 0..width {
                let distance = ((x as f32 - center_x).powi(2) + (y as f32 - center_y).powi(2))
                    .sqrt()
                    / max_distance;
                let shade = (220.0 - 140.0 * distance).clamp(30.0, 240.0) as u8;
                image.put_pixel(x, y, Rgba([shade, shade.saturating_add(8), shade, 255]));
            }
        }
        image
    }

    fn noise_fixture(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let value = ((x * 37 + y * 53 + x * y * 3) % 255) as u8;
                image.put_pixel(
                    x,
                    y,
                    Rgba([value, value.wrapping_add(29), value.wrapping_add(83), 255]),
                );
            }
        }
        image
    }

    fn edge_luminance_gap(image: &RgbaImage) -> f32 {
        let band = 4.min(image.width() / 2).max(1);
        let mut left = 0.0;
        let mut right = 0.0;
        let count = (band * image.height()) as f32;
        for y in 0..image.height() {
            for offset in 0..band {
                left += channel_luma(*image.get_pixel(offset, y)) / count;
                right += channel_luma(*image.get_pixel(image.width() - 1 - offset, y)) / count;
            }
        }
        (left - right).abs()
    }

    fn channel_luma(pixel: Rgba<u8>) -> f32 {
        0.2126 * pixel.0[0] as f32 + 0.7152 * pixel.0[1] as f32 + 0.0722 * pixel.0[2] as f32
    }

    fn source_layer_fraction(output: &RgbaImage, source: &RgbaImage, blend_percent: f32) -> f32 {
        let width = source.width();
        let height = source.height();
        let shift_x = width / 2;
        let center = width / 2;
        let band = seam_cut_band(width, blend_percent);
        let start = center.saturating_sub(band);
        let end = (center + band).min(width - 1);
        let left_path = minimum_vertical_path(source, start, center.min(width - 1), shift_x);
        let right_path = minimum_vertical_path(source, center.min(width - 1), end, shift_x);
        let mut source_pixels = 0usize;
        let mut total = 0usize;

        for y in 0..height {
            for x in start..=end {
                let xi = x as i32;
                let near_left = (xi - left_path[y as usize] as i32).abs() < SEAM_FEATHER;
                let near_right = (xi - right_path[y as usize] as i32).abs() < SEAM_FEATHER;
                if near_left || near_right {
                    continue;
                }
                let pixel = *output.get_pixel(x, y);
                let rolled = sample_shifted(source, x, y, shift_x, 0);
                let original = *source.get_pixel(x, y);
                if pixel == rolled || pixel == original {
                    source_pixels += 1;
                }
                total += 1;
            }
        }

        source_pixels as f32 / total as f32
    }

    #[test]
    fn seam_cut_edges_are_exact_in_every_mode() {
        let source = harsh_nonseamless_fixture(96);
        let gradient = illumination_gradient(120, 88);
        for fixture in [&source, &gradient] {
            let horizontal = make_seam_cut_seamless(fixture, "horizontal", 18.0);
            assert_horizontal_edges_exact(&horizontal);

            let vertical = make_seam_cut_seamless(fixture, "vertical", 18.0);
            assert_vertical_edges_exact(&vertical);

            let tile = make_seam_cut_seamless(fixture, "tile", 18.0);
            assert_horizontal_edges_exact(&tile);
            assert_vertical_edges_exact(&tile);
            assert!(seam_score(&tile) <= 0.01);
        }
    }

    #[test]
    fn seam_cut_avoids_blend_ghosting_on_structured_bars() {
        let source = vertical_bars(112, 80);
        let seam_cut = make_seam_cut_seamless(&source, "horizontal", 18.0);
        let blend = make_seamless(&source, "horizontal", 18.0);
        let seam_cut_fraction = source_layer_fraction(&seam_cut, &source, 18.0);
        let blend_fraction = source_layer_fraction(&blend, &source, 18.0);

        assert!(
            seam_cut_fraction >= 0.95,
            "expected seam-cut pixels to mostly come from one source layer, got {seam_cut_fraction:.3}"
        );
        assert!(
            blend_fraction < 0.95,
            "expected blend strategy to fail the no-ghosting property, got {blend_fraction:.3}"
        );
    }

    #[test]
    fn tile_mode_keeps_both_axes_exact_after_sequential_cuts() {
        let source = harsh_nonseamless_fixture(128);
        let output = make_seam_cut_seamless(&source, "tile", 20.0);
        assert_horizontal_edges_exact(&output);
        assert_vertical_edges_exact(&output);
    }

    #[test]
    fn flatten_reduces_low_frequency_edge_gap() {
        let source = illumination_gradient(160, 96);
        let flattened = flatten_low_frequency(&source, 0.7);
        let before = edge_luminance_gap(&source);
        let after = edge_luminance_gap(&flattened);

        assert!(
            after <= before * 0.4,
            "expected flatten to shrink edge gap by at least 60%, before={before:.2}, after={after:.2}"
        );
    }

    #[test]
    fn seam_cut_handles_degenerate_sizes() {
        for (width, height) in [(2, 2), (3, 17), (17, 3), (16, 3000)] {
            let source = gradient(width, height);
            let horizontal = make_seam_cut_seamless(&source, "horizontal", 18.0);
            assert_horizontal_edges_exact(&horizontal);
            let vertical = make_seam_cut_seamless(&source, "vertical", 18.0);
            assert_vertical_edges_exact(&vertical);
            let tile = make_seam_cut_seamless(&source, "tile", 18.0);
            assert_horizontal_edges_exact(&tile);
            assert_vertical_edges_exact(&tile);
        }
    }

    fn brick_pattern(width: u32, height: u32, brick_w: u32, brick_h: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let row = y / brick_h;
                let course_offset = if row % 2 == 0 { 0 } else { brick_w / 2 };
                let local_x = (x + course_offset) % brick_w;
                let local_y = y % brick_h;
                let mortar = local_x < 2 || local_y < 2;
                let color = if mortar {
                    Rgba([180, 176, 168, 255])
                } else {
                    Rgba([150, 62, 48, 255])
                };
                image.put_pixel(x, y, color);
            }
        }
        image
    }

    fn varied_brick_pattern(width: u32, height: u32, brick_w: u32, brick_h: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let row = y / brick_h;
                let course_offset = if row % 2 == 0 { 0 } else { brick_w / 2 };
                let shifted_x = x + course_offset;
                let brick = shifted_x / brick_w;
                let local_x = shifted_x % brick_w;
                let local_y = y % brick_h;
                let mortar = local_x < 2 || local_y < 2;
                let color = if mortar {
                    Rgba([178, 174, 166, 255])
                } else if (brick + row * 3) % 2 == 0 {
                    Rgba([72, 28, 24, 255])
                } else {
                    let variation = ((brick * 29 + row * 17) % 36) as u8;
                    Rgba([
                        206u8.saturating_add(variation),
                        82u8.saturating_add(variation / 3),
                        60u8.saturating_add(variation / 4),
                        255,
                    ])
                };
                image.put_pixel(x, y, color);
            }
        }
        image
    }

    #[test]
    fn snap_period_crops_bricks_to_whole_pattern_repeats() {
        // 25 px bricks, 16 px courses: the pattern repeats every 25x32 px,
        // so a 132x100 image should snap down to 125x96.
        let source = brick_pattern(132, 100, 25, 16);
        let crop = detect_periodic_crop_width(&source).expect("period detected");
        assert_eq!(crop % 25, 0, "crop width {crop} is not a brick multiple");

        let cropped = snap_periodic_crop(&source, "tile");
        assert_eq!(cropped.width() % 25, 0);
        assert_eq!(cropped.height() % 32, 0, "height should snap to course pair");
        // The cropped image must already wrap perfectly before any seam work.
        for y in 0..cropped.height() {
            assert_eq!(
                *cropped.get_pixel(0, y),
                *source.get_pixel(cropped.width(), y)
            );
        }
        for x in 0..cropped.width() {
            assert_eq!(
                *cropped.get_pixel(x, 0),
                *source.get_pixel(x, cropped.height())
            );
        }
    }

    #[test]
    fn snap_period_detects_varied_brick_structure() {
        let source = varied_brick_pattern(132, 100, 25, 16);
        assert!(
            horizontal_shift_score(&source, 25) > 26.0,
            "fixture should be too varied for the raw pixel detector"
        );

        let crop = detect_periodic_crop_width(&source).expect("structural period detected");
        assert_eq!(crop, 125);

        let snapped = snap_periodic_crop_with_info(&source, "tile");
        assert_eq!(snapped.image.dimensions(), (125, 96));
        assert_eq!(snapped.info.period_x, Some(25));
        assert_eq!(snapped.info.period_y, Some(32));
    }

    #[test]
    fn snap_period_status_reports_crop_dimensions() {
        let dir = std::env::temp_dir().join(format!(
            "seamless-snap-status-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let input = dir.join("varied-bricks.png");
        varied_brick_pattern(132, 100, 25, 16)
            .save(&input)
            .expect("save input");
        let options = SeamlessOptions {
            mode: "tile".to_string(),
            output_format: "png".to_string(),
            same_folder: false,
            output_dir: dir.display().to_string(),
            suffix: "_snapped".to_string(),
            recursive: false,
            overwrite: true,
            blend_percent: 18.0,
            strategy: "seam-cut".to_string(),
            flatten: 0.0,
            snap_period: true,
        };

        let output = process_one(&input, &options).expect("process snapped image");
        assert!(
            output
                .message
                .contains("snapped 132x100 to 125x96"),
            "unexpected message: {}",
            output.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn snap_period_skips_images_already_sized_to_the_pattern() {
        // 128 is an exact multiple of the 32 px checker period: nothing to fix.
        let source = brick_pattern(128, 96, 32, 16);
        let unchanged = snap_periodic_crop(&source, "tile");
        assert_eq!(unchanged.dimensions(), source.dimensions());
    }

    #[test]
    fn snap_period_leaves_non_periodic_images_untouched() {
        let noise = noise_fixture(128, 96);
        assert_eq!(detect_periodic_crop_width(&noise), None);
        let unchanged = snap_periodic_crop(&noise, "tile");
        assert_eq!(unchanged.dimensions(), noise.dimensions());

        let vignette = illumination_gradient(160, 96);
        assert_eq!(detect_periodic_crop_width(&vignette), None);
    }

    #[test]
    fn snap_period_respects_mode_axes() {
        let source = brick_pattern(132, 100, 25, 16);
        let horizontal = snap_periodic_crop(&source, "horizontal");
        assert_eq!(horizontal.height(), source.height());
        assert!(horizontal.width() < source.width());

        let vertical = snap_periodic_crop(&source, "vertical");
        assert_eq!(vertical.width(), source.width());
        assert!(vertical.height() < source.height());
    }

    #[test]
    fn headless_cli_defaults_to_seam_cut_strategy() {
        let dir = std::env::temp_dir().join(format!(
            "seamless-cli-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let input = dir.join("bars.png");
        vertical_bars(112, 80).save(&input).expect("save input");

        run_headless_cli(vec![
            "seamless-image-edit".to_string(),
            "--headless".to_string(),
            "--same-folder".to_string(),
            "--format".to_string(),
            "png".to_string(),
            input.display().to_string(),
        ])
        .expect("headless run");

        let output = dir.join("bars_seamless.png");
        assert!(output.is_file(), "expected output at {}", output.display());
        let image = image::open(&output).expect("open output").to_rgba8();
        assert!(source_layer_fraction(&image, &vertical_bars(112, 80), 20.0) >= 0.95);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_blend_strategy_keeps_old_crossfade_route() {
        let source = vertical_bars(112, 80);
        let output = make_seamless(&source, "horizontal", 18.0);
        assert!(source_layer_fraction(&output, &source, 18.0) < 0.95);
    }

    fn write_visual_set(name: &str, source: &RgbaImage) {
        let output = make_seam_cut_seamless(source, "tile", 18.0);
        let output_dir = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("visual-seam-test");
        std::fs::create_dir_all(&output_dir).expect("create visual seam test dir");
        source
            .save(output_dir.join(format!("{name}-01-source.png")))
            .expect("save source fixture");
        output
            .save(output_dir.join(format!("{name}-02-seam-cut.png")))
            .expect("save seam-cut fixture");

        let source_tiled = tile_2x2(source);
        let output_tiled = tile_2x2(&output);
        source_tiled
            .save(output_dir.join(format!("{name}-03-source-tiled-2x2.png")))
            .expect("save source tiled fixture");
        output_tiled
            .save(output_dir.join(format!("{name}-04-output-tiled-2x2.png")))
            .expect("save output tiled fixture");

        let gutter = 12;
        let mut contact = RgbaImage::from_pixel(
            source_tiled.width() + output_tiled.width() + gutter,
            source_tiled.height().max(output_tiled.height()),
            Rgba([12, 16, 24, 255]),
        );
        paste(&mut contact, &source_tiled, 0, 0);
        paste(
            &mut contact,
            &output_tiled,
            source_tiled.width() + gutter,
            0,
        );
        contact
            .save(output_dir.join(format!("{name}-05-before-after-contact.png")))
            .expect("save visual contact sheet");
        println!("{name} seam-cut seam score={:.2}", seam_score(&output));
    }

    #[test]
    fn writes_seam_cut_visual_fixtures() {
        write_visual_set("brick-bars", &vertical_bars(112, 96));
        write_visual_set("vignette", &illumination_gradient(128, 96));
        write_visual_set("noise", &noise_fixture(128, 96));
    }

    #[test]
    fn writes_visual_tile_fixture_and_reduces_edge_seams() {
        let source = harsh_nonseamless_fixture(160);
        let output =
            make_content_aware_seamless(&source, "tile", 22.0).expect("content-aware seamless");
        let source_score = seam_score(&source);
        let output_score = seam_score(&output);
        println!("visual seam score source={source_score:.2}, output={output_score:.2}");
        assert!(
            output_score <= 0.01,
            "expected opposite tile edges to match, source={source_score:.2}, output={output_score:.2}"
        );

        let output_dir = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("visual-seam-test");
        std::fs::create_dir_all(&output_dir).expect("create visual seam test dir");
        source
            .save(output_dir.join("01-source-not-seamless.png"))
            .expect("save source fixture");
        output
            .save(output_dir.join("02-output-seamless.png"))
            .expect("save seamless fixture");
        let source_tiled = tile_2x2(&source);
        let output_tiled = tile_2x2(&output);
        source_tiled
            .save(output_dir.join("03-source-tiled-2x2.png"))
            .expect("save source tiled fixture");
        output_tiled
            .save(output_dir.join("04-output-tiled-2x2.png"))
            .expect("save output tiled fixture");

        let gutter = 12;
        let mut contact = RgbaImage::from_pixel(
            source_tiled.width() + output_tiled.width() + gutter,
            source_tiled.height().max(output_tiled.height()),
            Rgba([12, 16, 24, 255]),
        );
        paste(&mut contact, &source_tiled, 0, 0);
        paste(
            &mut contact,
            &output_tiled,
            source_tiled.width() + gutter,
            0,
        );
        contact
            .save(output_dir.join("05-before-after-tiled-contact.png"))
            .expect("save visual contact sheet");
    }
}
