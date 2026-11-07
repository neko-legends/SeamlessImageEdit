from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

from neko_store import StoreContext, uv_available


PYTORCH_CUDA_INDEX_URL = "https://download.pytorch.org/whl/cu128"
BOOTSTRAP_PACKAGES = ["pip", "setuptools", "wheel"]
ML_PACKAGES = [
    "accelerate",
    "diffusers",
    "huggingface_hub",
    "numpy",
    "pillow",
    "safetensors",
    "transformers",
]
PYTORCH_CPU_PACKAGES = ["torch", "torchvision"]
PYTORCH_CUDA_PACKAGES = ["torch", "torchvision"]
DEPS_MARKER = ".seamless-image-edit-inference-v2-ready"


def emit(event_type: str, message: str | None = None, **payload: Any) -> None:
    event: dict[str, Any] = {"type": event_type}
    if message:
        event["message"] = message
    event.update(payload)
    print(json.dumps(event, ensure_ascii=False), flush=True)


def sanitize_installer_line(line: str) -> str | None:
    if line.startswith("Collecting "):
        return f"Resolving dependency: {line.removeprefix('Collecting ').split()[0]}"
    if line.startswith("Downloading "):
        return "Downloading dependency packages"
    if line.startswith("Installing collected packages"):
        return "Installing dependency packages"
    if line.startswith("Successfully installed"):
        return "Dependency install step finished"
    if line.startswith("ERROR:") or " error:" in line.lower() or "failed" in line.lower():
        return line
    return None


def run_checked(command: list[str], message: str, env: dict[str, str]) -> None:
    emit("setup", f"{message}. This can take several minutes.")
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
    )
    assert process.stdout is not None
    tail: list[str] = []
    last_status: str | None = None
    for line in process.stdout:
        line = line.strip()
        if not line:
            continue
        tail.append(line)
        tail = tail[-18:]
        status = sanitize_installer_line(line)
        if status and status != last_status:
            emit("setup", status)
            last_status = status
    code = process.wait()
    if code != 0:
        details = "\n".join(tail) if tail else "No installer output was captured."
        raise RuntimeError(f"{message} failed.\n\nLast installer output:\n{details}")


def nvidia_smi_available() -> bool:
    if shutil.which("nvidia-smi") is None:
        return False
    try:
        subprocess.check_output(
            ["nvidia-smi", "--query-gpu=name", "--format=csv,noheader"],
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=10,
        )
    except Exception:
        return False
    return True


def ensure_env(env_dir: Path) -> Path:
    store = StoreContext.from_env(app_slug="seamlessimageedit", env_dir=env_dir)
    python = store.ensure_venv("3.11")
    marker = env_dir / DEPS_MARKER
    if marker.exists():
        return python

    env = store.child_env()
    # `uv venv` (used by neko_store when uv is installed) creates environments
    # without pip, so install through uv in that case.
    if uv_available():
        install = ["uv", "pip", "install", "--python", str(python)]
    else:
        install = [str(python), "-m", "pip", "install"]
        run_checked(install + ["--upgrade", *BOOTSTRAP_PACKAGES], "Updating Python packaging tools", env)
    run_checked(install + ["--upgrade", *ML_PACKAGES], "Installing inference runtime packages", env)
    if nvidia_smi_available():
        run_checked(
            install + ["--upgrade", *PYTORCH_CUDA_PACKAGES, "--index-url", PYTORCH_CUDA_INDEX_URL],
            "Installing CUDA PyTorch packages",
            env,
        )
    else:
        run_checked(install + ["--upgrade", *PYTORCH_CPU_PACKAGES], "Installing PyTorch packages", env)
    marker.write_text("ok\n", encoding="utf-8")
    return python


def relaunch_in_env(job_path: Path, job: dict[str, Any]) -> int:
    python = ensure_env(Path(job["env_dir"]))
    env = os.environ.copy()
    env["SEAMLESS_IMAGE_EDIT_VENV_ACTIVE"] = "1"
    args = [str(python), str(Path(__file__).resolve()), "--job", str(job_path)]
    process = subprocess.Popen(
        args,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
    )
    assert process.stdout is not None
    for line in process.stdout:
        print(line, end="", flush=True)
    return process.wait()


def clamp(value: float, low: float, high: float) -> float:
    return max(low, min(high, value))


def rounded_to_multiple(value: int, multiple: int) -> int:
    return max(multiple, (value // multiple) * multiple)


def resized_for_sdxl(image: Any, max_size: int = 1024) -> tuple[Any, tuple[int, int]]:
    from PIL import Image

    original_size = image.size
    width, height = image.size
    scale = min(1.0, max_size / max(width, height))
    next_size = (
        rounded_to_multiple(max(64, int(width * scale)), 8),
        rounded_to_multiple(max(64, int(height * scale)), 8),
    )
    if next_size != image.size:
        image = image.resize(next_size, Image.Resampling.LANCZOS)
    return image, original_size


def seam_mask(size: tuple[int, int], mode: str, band_percent: float) -> Any:
    from PIL import Image, ImageDraw, ImageFilter

    width, height = size
    band = clamp(band_percent, 4.0, 45.0) / 100.0
    mask = Image.new("L", size, 0)
    draw = ImageDraw.Draw(mask)
    feather = max(8, round(min(width, height) * 0.015))
    if mode in ("horizontal", "tile"):
        half = max(8, round(width * band * 0.5))
        cx = width // 2
        draw.rectangle((cx - half, 0, cx + half, height), fill=255)
    if mode in ("vertical", "tile"):
        half = max(8, round(height * band * 0.5))
        cy = height // 2
        draw.rectangle((0, cy - half, width, cy + half), fill=255)
    return mask.filter(ImageFilter.GaussianBlur(feather))


def offset_image(image: Any, mode: str, direction: int = 1) -> Any:
    from PIL import ImageChops

    width, height = image.size
    x = width // 2 if mode in ("horizontal", "tile") else 0
    y = height // 2 if mode in ("vertical", "tile") else 0
    return ImageChops.offset(image, direction * x, direction * y)


def choose_device(torch: Any, requested: str) -> str:
    if requested.startswith("cuda"):
        if not torch.cuda.is_available():
            raise RuntimeError("CUDA was requested, but torch.cuda is not available.")
        return requested
    if requested == "cpu":
        return "cpu"
    return "cuda" if torch.cuda.is_available() else "cpu"


def enable_circular_padding(pipe: Any) -> int:
    import torch.nn as nn

    patched = 0
    for root_name in ("unet", "vae"):
        root = getattr(pipe, root_name, None)
        if root is None:
            continue
        for module in root.modules():
            if isinstance(module, nn.Conv2d) and module.padding != (0, 0):
                module.padding_mode = "circular"
                patched += 1
    return patched


def original_alpha(image: Any) -> Any | None:
    if image.mode in ("RGBA", "LA"):
        alpha = image.getchannel("A")
        if alpha.getextrema() != (255, 255):
            return alpha
    return None


def run_inference(job: dict[str, Any]) -> None:
    import torch
    from diffusers import StableDiffusionXLInpaintPipeline
    from PIL import Image

    options = job["options"]
    mode = options.get("mode", "tile")
    model_id = options.get("inferenceModel") or "diffusers/stable-diffusion-xl-1.0-inpainting-0.1"
    prompt = options.get("inferencePrompt") or ""
    negative = options.get("inferenceNegativePrompt") or ""
    strength = clamp(float(options.get("inferenceDenoise", 0.35)), 0.05, 0.95)
    guidance = clamp(float(options.get("inferenceGuidance", 5.0)), 1.0, 15.0)
    steps = int(clamp(float(options.get("inferenceSteps", 18)), 4.0, 80.0))
    seed = int(options.get("inferenceSeed", 0) or 0)

    emit("setup", f"Loading {model_id}")
    torch_dtype = torch.float16 if torch.cuda.is_available() else torch.float32
    pipe = StableDiffusionXLInpaintPipeline.from_pretrained(
        model_id,
        torch_dtype=torch_dtype,
        variant="fp16" if torch.cuda.is_available() else None,
        use_safetensors=True,
    )
    device = choose_device(torch, options.get("inferenceDevice", "auto"))
    pipe = pipe.to(device)
    pipe.set_progress_bar_config(disable=True)
    try:
        pipe.enable_attention_slicing()
    except Exception:
        pass
    try:
        pipe.enable_vae_tiling()
    except Exception:
        pass
    patched = enable_circular_padding(pipe)
    emit("setup", f"Tile-aware inference ready ({patched} circular layers)")

    with Image.open(job["input_path"]) as opened:
        alpha = original_alpha(opened)
        image = opened.convert("RGB")
    working, original_size = resized_for_sdxl(image)
    shifted = offset_image(working, mode, 1)
    mask = seam_mask(working.size, mode, float(options.get("blendPercent", 18.0)))

    generator = None
    if seed > 0:
        generator = torch.Generator(device=device).manual_seed(seed)

    emit("setup", "Repairing texture edges with inference")
    result = pipe(
        prompt=prompt,
        negative_prompt=negative,
        image=shifted,
        mask_image=mask,
        # Without an explicit size SDXL renders at 1024x1024, which no longer
        # matches smaller inputs when composited back below.
        width=shifted.width,
        height=shifted.height,
        strength=strength,
        guidance_scale=guidance,
        num_inference_steps=steps,
        generator=generator,
    ).images[0]
    repaired_shifted = Image.composite(result.convert("RGB"), shifted, mask)
    result = offset_image(repaired_shifted, mode, -1)
    if result.size != original_size:
        result = result.resize(original_size, Image.Resampling.LANCZOS)
    if alpha is not None:
        result.putalpha(alpha.resize(result.size, Image.Resampling.LANCZOS))
    result.save(job["output_path"], "PNG")
    emit("done", "Inference repair finished.")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--job", required=True)
    args = parser.parse_args()
    job_path = Path(args.job)
    job = json.loads(job_path.read_text(encoding="utf-8"))

    if os.environ.get("SEAMLESS_IMAGE_EDIT_VENV_ACTIVE") != "1":
        return relaunch_in_env(job_path, job)

    try:
        run_inference(job)
        return 0
    except Exception as error:
        emit("error", str(error))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
