# ╔══════════════════════════════════════════════════════════════════════════╗
# ║  VENDORED FILE — DO NOT EDIT HERE (when copied into an app).               ║
# ║  Canonical source: github.com/neko-legends/NekoLegendsAI-Shared           ║
# ║                    python/neko_store.py                                    ║
# ║  Fix the canonical copy, then re-vendor with neko_suite_doctor.py --fix.   ║
# ╚══════════════════════════════════════════════════════════════════════════╝
"""neko_store — shared local AI store helper for Neko Legends / ForPublic workers.

Companion to ``neko_store.rs``. The Rust side resolves the store and exports
environment variables to the Python worker process. This helper simply *reads*
those variables and gives the worker convenient, already-resolved paths plus a
uv-aware venv bootstrapper.

It deliberately has **no third-party dependencies** so it can run under the
system Python before any venv exists.

Environment variables set by the Rust resolver (``NekoStore::apply_env``):

    NEKO_AI_STORE_ROOT     root of the resolved store
    NEKO_AI_STORE_SHARED   "1" if shared, "0" if app-local fallback
    HF_HOME                shared Hugging Face home (models download once)
    HF_HUB_CACHE           shared HF hub cache
    PIP_CACHE_DIR          shared pip wheel cache
    UV_CACHE_DIR           shared uv cache
    TORCH_HOME             shared torch hub cache

Typical worker usage::

    from neko_store import StoreContext

    store = StoreContext.from_env()
    python = store.ensure_venv()          # creates env, prefers uv, hardlink dedup
    store.pip_install(python, ["torch==2.7.1+cu128"],
                      index_url="https://download.pytorch.org/whl/cu128")
"""

from __future__ import annotations

import os
import platform
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence


def _env_path(key: str) -> Path | None:
    value = os.environ.get(key, "").strip()
    return Path(value) if value else None


def venv_python(env_dir: Path) -> Path:
    """Path to the python executable inside a venv for the current OS."""
    if platform.system() == "Windows":
        return env_dir / "Scripts" / "python.exe"
    return env_dir / "bin" / "python"


def uv_available() -> bool:
    """True when the `uv` package manager is on PATH.

    uv is preferred because it keeps one content-addressed cache and hardlinks
    packages into each venv on the same volume, so torch/cu wheels occupy disk
    once across every app in the suite.
    """
    return shutil.which("uv") is not None


@dataclass
class StoreContext:
    """Resolved store paths for a Python worker, read from the environment."""

    root: Path
    shared: bool
    env_dir: Path
    models_dir: Path
    hf_home: Path
    pip_cache: Path
    uv_cache: Path
    torch_home: Path

    @classmethod
    def from_env(cls, app_slug: str | None = None, env_dir: Path | None = None) -> "StoreContext":
        """Build a context from the env vars exported by the Rust resolver.

        ``env_dir`` may be passed explicitly (e.g. from the worker's job JSON,
        which is how the existing apps already plumb it). If omitted, it is
        derived from the store root and ``app_slug``.
        """
        root = _env_path("NEKO_AI_STORE_ROOT") or Path.cwd()
        shared = os.environ.get("NEKO_AI_STORE_SHARED", "0") == "1"
        models_dir = _env_path("HF_HOME").parent if _env_path("HF_HOME") else root / "models"
        hf_home = _env_path("HF_HOME") or (models_dir / "huggingface")
        cache = root / "cache"

        if env_dir is None:
            if shared and app_slug:
                env_dir = root / "envs" / app_slug
            else:
                env_dir = root / "python-env"

        return cls(
            root=root,
            shared=shared,
            env_dir=Path(env_dir),
            models_dir=models_dir,
            hf_home=hf_home,
            pip_cache=_env_path("PIP_CACHE_DIR") or (cache / "pip"),
            uv_cache=_env_path("UV_CACHE_DIR") or (cache / "uv"),
            torch_home=_env_path("TORCH_HOME") or (cache / "torch"),
        )

    # -- environment ------------------------------------------------------

    def child_env(self) -> dict[str, str]:
        """Environment dict to pass to subprocesses so caches stay shared."""
        env = dict(os.environ)
        env.update(
            {
                "HF_HOME": str(self.hf_home),
                "HF_HUB_CACHE": str(self.hf_home / "hub"),
                "HUGGINGFACE_HUB_CACHE": str(self.hf_home / "hub"),
                "PIP_CACHE_DIR": str(self.pip_cache),
                "UV_CACHE_DIR": str(self.uv_cache),
                "TORCH_HOME": str(self.torch_home),
            }
        )
        return env

    def ensure_dirs(self) -> None:
        for d in (self.models_dir, self.hf_home, self.pip_cache, self.uv_cache, self.torch_home):
            d.mkdir(parents=True, exist_ok=True)
        self.env_dir.parent.mkdir(parents=True, exist_ok=True)

    # -- venv -------------------------------------------------------------

    def ensure_venv(self, python_version: str = "3.11") -> Path:
        """Create the app venv if missing and return its python path.

        Prefers ``uv venv`` (fast, hardlinked, fetches an interpreter if needed)
        and falls back to the stdlib ``venv`` module so the worker still works on
        machines without uv.
        """
        self.ensure_dirs()
        python_path = venv_python(self.env_dir)
        if python_path.exists():
            return python_path

        if uv_available():
            cmd = ["uv", "venv", "--python", python_version, str(self.env_dir)]
            subprocess.check_call(cmd, env=self.child_env())
        else:
            import venv as _venv

            _venv.EnvBuilder(with_pip=True).create(self.env_dir)

        if not python_path.exists():
            raise RuntimeError(f"venv creation did not produce {python_path}")
        return python_path

    def pip_install(
        self,
        python_path: Path,
        packages: Sequence[str],
        *,
        index_url: str | None = None,
        extra_args: Iterable[str] | None = None,
        upgrade: bool = False,
    ) -> None:
        """Install packages into the venv using uv (if present) or pip.

        All downloads route through the shared cache via ``child_env``.
        """
        env = self.child_env()
        if uv_available():
            cmd = ["uv", "pip", "install", "--python", str(python_path)]
        else:
            cmd = [str(python_path), "-m", "pip", "install"]
        if upgrade:
            cmd.append("--upgrade")
        if index_url:
            cmd += ["--index-url", index_url]
        if extra_args:
            cmd += list(extra_args)
        cmd += list(packages)
        subprocess.check_call(cmd, env=env)


if __name__ == "__main__":
    # Tiny self-report for manual checks.
    ctx = StoreContext.from_env(app_slug="selftest")
    print(f"root        = {ctx.root}")
    print(f"shared      = {ctx.shared}")
    print(f"env_dir     = {ctx.env_dir}")
    print(f"hf_home     = {ctx.hf_home}")
    print(f"pip_cache   = {ctx.pip_cache}")
    print(f"uv_cache    = {ctx.uv_cache}")
    print(f"torch_home  = {ctx.torch_home}")
    print(f"uv_available= {uv_available()}")
    sys.exit(0)
