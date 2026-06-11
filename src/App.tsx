import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { open } from '@tauri-apps/plugin-dialog'
import packageInfo from '../package.json'
import {
  ArrowLeftRight,
  ArrowUpDown,
  CheckCircle2,
  FolderOpen,
  Grid3X3,
  Image as ImageIcon,
  Loader2,
  Play,
  RefreshCw,
  Sparkles,
  Trash2,
  TriangleAlert,
  Wand2,
  XCircle,
} from 'lucide-react'
import { type KeyboardEvent, type PointerEvent as ReactPointerEvent, useCallback, useEffect, useMemo, useRef, useState } from 'react'

type SeamMode = 'horizontal' | 'vertical' | 'tile'
type SeamStrategy = 'seam-cut' | 'synthesis' | 'blend'
type OutputFormat = 'webp' | 'png'
type QueueStatus = 'pending' | 'running' | 'done' | 'error'

type SeamlessOptions = {
  mode: SeamMode
  outputFormat: OutputFormat
  sameFolder: boolean
  outputDir: string
  suffix: string
  recursive: boolean
  overwrite: boolean
  blendPercent: number
  strategy: SeamStrategy
  flatten: number
  snapPeriod: boolean
}

type QueueItem = {
  path: string
  status: QueueStatus
  message?: string
  outputPath?: string
}

type ProcessResult = {
  inputPath: string
  outputPath?: string | null
  status: QueueStatus
  message: string
}

type WorkerEvent = {
  type: 'image_start' | 'image_done' | 'image_error' | 'done' | 'canceled'
  path?: string
  output?: string
  message?: string
}

type AgentServerStatus = {
  enabled: boolean
  port: number
  url: string
  openapiUrl: string
  busy: boolean
  activeJobId: string | null
  message: string
}

type PreviewState = {
  input?: string
  output?: string
  error?: string
}

type TileCheckState = {
  dataUrl?: string
  path?: string
  error?: string
}

type DropPosition = {
  x: number
  y: number
}

type RangeControlProps = {
  label: string
  title?: string
  value: number
  min: number
  max: number
  step: number
  valueText: string
  onChange: (value: number) => void
}

type ToggleControlProps = {
  checked: boolean
  label: string
  title?: string
  onChange: (checked: boolean) => void
}

const LEGACY_SETTINGS_KEY = 'seamlessImageEdit.settings.v1'
const SETTINGS_KEY = 'seamlessImageEdit.settings.v2'
const AGENT_STORAGE_KEY = 'seamlessImageEdit.agentControlEnabled.v1'
const AGENT_PORT_STORAGE_KEY = 'seamlessImageEdit.agentApiPort.v1'
const DEFAULT_AGENT_API_PORT = 17335
const APP_VERSION = `v${packageInfo.version}`

const defaultOptions: SeamlessOptions = {
  mode: 'tile',
  outputFormat: 'webp',
  sameFolder: true,
  outputDir: '',
  suffix: '_seamless',
  recursive: true,
  overwrite: false,
  blendPercent: 18,
  strategy: 'seam-cut',
  flatten: 0,
  snapPeriod: false,
}

const modeOptions: Array<{ id: SeamMode; label: string; icon: typeof ArrowLeftRight }> = [
  { id: 'tile', label: 'Tile', icon: Grid3X3 },
  { id: 'horizontal', label: 'Horizontal', icon: ArrowLeftRight },
  { id: 'vertical', label: 'Vertical', icon: ArrowUpDown },
]

const strategyOptions: Array<{ id: SeamStrategy; label: string; title: string }> = [
  {
    id: 'seam-cut',
    label: 'Seam cut',
    title: 'Best first choice for structured patterns: bricks, tiles, planks, grids, stripes, and fabric.',
  },
  {
    id: 'synthesis',
    label: 'Synthesis',
    title: 'Best for organic textures: grass, moss, dirt, gravel, bark, foliage, stone, clouds, and noise.',
  },
  {
    id: 'blend',
    label: 'Blend',
    title: 'Fast soft crossfade for blurry, low-detail, or almost-seamless images. Weak for bricks, stripes, text, or visible geometry.',
  },
]

const imageFilters = [
  {
    name: 'Images',
    extensions: ['png', 'jpg', 'jpeg', 'jpe', 'jfif', 'webp', 'bmp', 'tif', 'tiff'],
  },
]

function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

function normalizeAgentPort(value: string) {
  const port = Number(value)
  return Number.isInteger(port) && port >= 1 && port <= 65535 ? port : null
}

function classNames(...items: Array<string | false | null | undefined>): string {
  return items.filter(Boolean).join(' ')
}

function coerceMode(value: unknown): SeamMode {
  return value === 'vertical' || value === 'tile' || value === 'horizontal' ? value : defaultOptions.mode
}

function coerceFormat(value: unknown): OutputFormat {
  return value === 'png' || value === 'webp' ? value : defaultOptions.outputFormat
}

function coerceStrategy(value: unknown): SeamStrategy {
  return value === 'synthesis' || value === 'blend' || value === 'seam-cut' ? value : defaultOptions.strategy
}

function coerceBoolean(value: unknown, fallback: boolean): boolean {
  return typeof value === 'boolean' ? value : fallback
}

function coerceNumber(value: unknown, fallback: number, min: number, max: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? Math.max(min, Math.min(max, value)) : fallback
}

function formatRangeValue(value: number, fallback: number, min: number, max: number, digits: number): string {
  return coerceNumber(value, fallback, min, max).toFixed(digits)
}

function snapRangeValue(value: number, min: number, max: number, step: number): number {
  const clamped = Math.max(min, Math.min(max, value))
  const stepped = min + Math.round((clamped - min) / step) * step
  const precision = Math.max(0, `${step}`.split('.')[1]?.length ?? 0)
  return Number(Math.max(min, Math.min(max, stepped)).toFixed(precision))
}

function loadOptions(): SeamlessOptions {
  try {
    const raw = window.localStorage.getItem(SETTINGS_KEY)
    const legacyRaw = raw ? null : window.localStorage.getItem(LEGACY_SETTINGS_KEY)
    if (!raw && !legacyRaw) return defaultOptions
    const parsed = JSON.parse(raw ?? legacyRaw ?? '{}') as Partial<SeamlessOptions>
    return {
      mode: raw ? coerceMode(parsed.mode) : defaultOptions.mode,
      outputFormat: coerceFormat(parsed.outputFormat),
      sameFolder: coerceBoolean(parsed.sameFolder, defaultOptions.sameFolder),
      outputDir: typeof parsed.outputDir === 'string' ? parsed.outputDir : defaultOptions.outputDir,
      suffix: typeof parsed.suffix === 'string' && parsed.suffix.trim() ? parsed.suffix : defaultOptions.suffix,
      recursive: coerceBoolean(parsed.recursive, defaultOptions.recursive),
      overwrite: coerceBoolean(parsed.overwrite, defaultOptions.overwrite),
      blendPercent: coerceNumber(parsed.blendPercent, defaultOptions.blendPercent, 4, 45),
      strategy: coerceStrategy(parsed.strategy),
      flatten: coerceNumber(parsed.flatten, defaultOptions.flatten, 0, 1),
      snapPeriod: coerceBoolean(parsed.snapPeriod, defaultOptions.snapPeriod),
    }
  } catch {
    return defaultOptions
  }
}

function fileName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path
}

function compactPath(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean)
  if (parts.length <= 3) return path
  const separator = path.includes('\\') ? '\\' : '/'
  return `...${separator}${parts.slice(-3).join(separator)}`
}

function statusIcon(status: QueueStatus) {
  if (status === 'running') return <Loader2 className="spin" size={16} />
  if (status === 'done') return <CheckCircle2 size={16} />
  if (status === 'error') return <XCircle size={16} />
  return <ImageIcon size={16} />
}

function outputLabel(options: SeamlessOptions): string {
  if (options.sameFolder) return 'Source folder'
  return options.outputDir ? compactPath(options.outputDir) : 'Choose output folder'
}

function isPositionInsideElement(position: DropPosition, element: HTMLElement | null): boolean {
  if (!element) return false
  const scale = window.devicePixelRatio || 1
  const x = position.x / scale
  const y = position.y / scale
  const rect = element.getBoundingClientRect()
  return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom
}

function TilePreview({ src }: { src: string }) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    let canceled = false
    const image = new window.Image()

    image.onload = () => {
      if (canceled) return
      const canvas = canvasRef.current
      const context = canvas?.getContext('2d')
      if (!canvas || !context) return

      const size = 512
      const scale = Math.max(1, Math.min(window.devicePixelRatio || 1, 2))
      canvas.width = Math.round(size * scale)
      canvas.height = Math.round(size * scale)
      context.setTransform(scale, 0, 0, scale, 0, 0)
      context.clearRect(0, 0, size, size)
      context.imageSmoothingEnabled = true

      const tileSize = size / 2
      for (let y = 0; y < 2; y += 1) {
        for (let x = 0; x < 2; x += 1) {
          context.drawImage(image, x * tileSize, y * tileSize, tileSize, tileSize)
        }
      }
    }

    image.src = src
    return () => {
      canceled = true
    }
  }, [src])

  return <canvas className="tile-preview" ref={canvasRef} />
}

function RangeControl({ label, title, value, min, max, step, valueText, onChange }: RangeControlProps) {
  const safeValue = snapRangeValue(value, min, max, step)
  const percent = ((safeValue - min) / (max - min)) * 100

  function updateFromClientX(clientX: number, element: HTMLElement) {
    const rect = element.getBoundingClientRect()
    const ratio = rect.width > 0 ? (clientX - rect.left) / rect.width : 0
    onChange(snapRangeValue(min + ratio * (max - min), min, max, step))
  }

  function handlePointerDown(event: ReactPointerEvent<HTMLDivElement>) {
    const slider = event.currentTarget
    const pointerId = event.pointerId
    event.preventDefault()
    slider.focus()
    updateFromClientX(event.clientX, slider)

    const cleanup = () => {
      window.removeEventListener('pointermove', handlePointerMove)
      window.removeEventListener('pointerup', handlePointerEnd)
      window.removeEventListener('pointercancel', handlePointerEnd)
    }

    const handlePointerMove = (moveEvent: globalThis.PointerEvent) => {
      if (moveEvent.pointerId !== pointerId) return
      moveEvent.preventDefault()
      updateFromClientX(moveEvent.clientX, slider)
    }

    const handlePointerEnd = (endEvent: globalThis.PointerEvent) => {
      if (endEvent.pointerId !== pointerId) return
      cleanup()
    }

    window.addEventListener('pointermove', handlePointerMove)
    window.addEventListener('pointerup', handlePointerEnd)
    window.addEventListener('pointercancel', handlePointerEnd)
  }

  function handleKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const largeStep = step * 5
    const nextValue =
      event.key === 'ArrowRight' || event.key === 'ArrowUp'
        ? safeValue + step
        : event.key === 'ArrowLeft' || event.key === 'ArrowDown'
          ? safeValue - step
          : event.key === 'PageUp'
            ? safeValue + largeStep
            : event.key === 'PageDown'
              ? safeValue - largeStep
              : event.key === 'Home'
                ? min
                : event.key === 'End'
                  ? max
                  : null

    if (nextValue === null) return
    event.preventDefault()
    onChange(snapRangeValue(nextValue, min, max, step))
  }

  return (
    <div className="range-field range-control" title={title}>
      <div className="range-control-head">
        <span>{label}</span>
        <strong>{valueText}</strong>
      </div>
      <div
        aria-label={label}
        aria-valuemax={max}
        aria-valuemin={min}
        aria-valuenow={safeValue}
        aria-valuetext={valueText}
        className="range-slider"
        onKeyDown={handleKeyDown}
        onPointerDown={handlePointerDown}
        role="slider"
        tabIndex={0}
      >
        <div className="range-track">
          <div className="range-fill" style={{ width: `${percent}%` }} />
        </div>
        <div className="range-thumb" style={{ left: `${percent}%` }} />
      </div>
    </div>
  )
}

function ToggleControl({ checked, label, title, onChange }: ToggleControlProps) {
  return (
    <button
      aria-checked={checked}
      className={classNames('toggle-row', 'toggle-control', checked && 'active')}
      onClick={() => onChange(!checked)}
      role="switch"
      title={title}
      type="button"
    >
      <span className="toggle-switch" aria-hidden="true">
        <span />
      </span>
      <span className="toggle-label">{label}</span>
    </button>
  )
}

function App() {
  const tileCheckRef = useRef<HTMLDivElement | null>(null)
  const [options, setOptions] = useState<SeamlessOptions>(loadOptions)
  const [queue, setQueue] = useState<QueueItem[]>([])
  const [selectedPath, setSelectedPath] = useState<string | null>(null)
  const [preview, setPreview] = useState<PreviewState>({})
  const [tileCheck, setTileCheck] = useState<TileCheckState>({})
  const [dragging, setDragging] = useState(false)
  const [tileCheckDragging, setTileCheckDragging] = useState(false)
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState('Ready')
  const [log, setLog] = useState<string[]>([])
  const [agentControlEnabled, setAgentControlEnabled] = useState(
    () => window.localStorage.getItem(AGENT_STORAGE_KEY) === '1',
  )
  const [agentPort, setAgentPort] = useState(
    () => window.localStorage.getItem(AGENT_PORT_STORAGE_KEY) ?? String(DEFAULT_AGENT_API_PORT),
  )
  const [agentStatus, setAgentStatus] = useState<AgentServerStatus | null>(null)

  const selectedItem = useMemo(
    () => queue.find((item) => item.path === selectedPath) ?? queue[0] ?? null,
    [queue, selectedPath],
  )
  const completeCount = useMemo(() => queue.filter((item) => item.status === 'done').length, [queue])
  const errorCount = useMemo(() => queue.filter((item) => item.status === 'error').length, [queue])
  const canStart = queue.length > 0 && !busy && (options.sameFolder || Boolean(options.outputDir.trim()))
  const tileCheckImage = tileCheck.dataUrl ?? preview.output
  const tileCheckLabel = tileCheck.path
    ? fileName(tileCheck.path)
    : preview.output
      ? 'Output 2x2 repeat'
      : 'Drop image to check'
  const previewError = preview.error ?? tileCheck.error

  const pushLog = useCallback((message: string) => {
    setLog((current) => [message, ...current].slice(0, 80))
  }, [])

  const refreshAgentStatus = useCallback(async () => {
    if (!isTauriRuntime()) return
    const status = await invoke<AgentServerStatus>('get_agent_server_status')
    setAgentStatus(status)
    if (status.enabled || !window.localStorage.getItem(AGENT_PORT_STORAGE_KEY)) {
      setAgentPort(String(status.port || DEFAULT_AGENT_API_PORT))
    }
  }, [])

  const toggleAgentControl = useCallback(
    async (enabled: boolean) => {
      const port = normalizeAgentPort(agentPort)
      if (enabled && port === null) {
        setNotice('Choose an Agent API port between 1 and 65535')
        return
      }
      setAgentControlEnabled(enabled)
      window.localStorage.setItem(AGENT_STORAGE_KEY, enabled ? '1' : '0')
      if (port !== null) {
        window.localStorage.setItem(AGENT_PORT_STORAGE_KEY, String(port))
      }
      if (!isTauriRuntime()) return
      try {
        const status = await invoke<AgentServerStatus>('set_agent_server_enabled', {
          enabled,
          port: port ?? agentStatus?.port ?? DEFAULT_AGENT_API_PORT,
        })
        setAgentStatus(status)
        setAgentPort(String(status.port))
        setNotice(status.message)
        pushLog(status.message)
      } catch (error) {
        setAgentControlEnabled(false)
        window.localStorage.setItem(AGENT_STORAGE_KEY, '0')
        setNotice(String(error))
        pushLog(String(error))
      }
    },
    [agentPort, agentStatus?.port, pushLog],
  )

  const applyAgentPort = useCallback(async () => {
    const port = normalizeAgentPort(agentPort)
    if (port === null) {
      setNotice('Choose an Agent API port between 1 and 65535')
      return
    }
    window.localStorage.setItem(AGENT_PORT_STORAGE_KEY, String(port))
    if (!isTauriRuntime() || !agentControlEnabled) return
    try {
      const status = await invoke<AgentServerStatus>('set_agent_server_enabled', { enabled: true, port })
      setAgentStatus(status)
      setAgentPort(String(status.port))
      setNotice(`Agent API moved to ${status.url}`)
      pushLog(`Agent API moved to ${status.url}`)
    } catch (error) {
      setNotice(String(error))
      pushLog(String(error))
    }
  }, [agentControlEnabled, agentPort, pushLog])

  const patchQueueItem = useCallback((path: string, patch: Partial<QueueItem>) => {
    setQueue((current) => current.map((item) => (item.path === path ? { ...item, ...patch } : item)))
  }, [])

  const addPaths = useCallback(
    async (paths: string[]) => {
      if (paths.length === 0) return
      if (!isTauriRuntime()) {
        setNotice('Desktop runtime required for filesystem access')
        return
      }

      let resolved: string[] = []
      try {
        resolved = await invoke<string[]>('resolve_inputs', {
          paths,
          recursive: options.recursive,
        })
      } catch (error) {
        setNotice(String(error))
        pushLog(`Input scan failed: ${String(error)}`)
        return
      }

      if (resolved.length === 0) {
        setNotice('No supported images found')
        return
      }

      setQueue((current) => {
        const known = new Set(current.map((item) => item.path))
        const additions = resolved
          .filter((path) => !known.has(path))
          .map((path) => ({ path, status: 'pending' as QueueStatus }))
        return [...current, ...additions]
      })
      setSelectedPath((current) => current ?? resolved[0] ?? null)
      setNotice(`Queued ${resolved.length} image${resolved.length === 1 ? '' : 's'}`)
      pushLog(`Queued ${resolved.length} image${resolved.length === 1 ? '' : 's'}.`)
    },
    [options.recursive, pushLog],
  )

  const loadTileCheckImage = useCallback(
    async (paths: string[]) => {
      if (paths.length === 0) return
      if (!isTauriRuntime()) {
        setNotice('Desktop runtime required for filesystem access')
        return
      }

      try {
        const resolved = await invoke<string[]>('resolve_inputs', {
          paths,
          recursive: false,
        })
        const imagePath = resolved[0]
        if (!imagePath) {
          const message = 'Tile check needs a supported image'
          setTileCheck((current) => ({ ...current, error: message }))
          setNotice(message)
          pushLog(message)
          return
        }

        const dataUrl = await invoke<string>('preview_image_data_url', { path: imagePath })
        setTileCheck({ dataUrl, path: imagePath })
        setNotice(`Tile check loaded ${fileName(imagePath)}`)
        pushLog(`Tile check loaded ${fileName(imagePath)}.`)
      } catch (error) {
        const message = String(error)
        setTileCheck((current) => ({ ...current, error: message }))
        setNotice(message)
        pushLog(`Tile check failed: ${message}`)
      }
    },
    [pushLog],
  )

  useEffect(() => {
    const timeout = window.setTimeout(() => {
      window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(options))
      window.localStorage.removeItem(LEGACY_SETTINGS_KEY)
    }, 160)
    return () => window.clearTimeout(timeout)
  }, [options])

  useEffect(() => {
    if (!isTauriRuntime()) return
    void refreshAgentStatus().catch((error) => {
      pushLog(`Agent API check failed: ${String(error)}`)
    })
  }, [pushLog, refreshAgentStatus])

  useEffect(() => {
    if (!isTauriRuntime() || !agentControlEnabled) return
    const port = normalizeAgentPort(agentPort) ?? DEFAULT_AGENT_API_PORT
    void invoke<AgentServerStatus>('set_agent_server_enabled', { enabled: true, port })
      .then((status) => {
        setAgentStatus(status)
        setAgentPort(String(status.port))
      })
      .catch((error) => {
        setAgentControlEnabled(false)
        window.localStorage.setItem(AGENT_STORAGE_KEY, '0')
        setNotice(`Agent API failed to start: ${String(error)}`)
        pushLog(`Agent API failed to start: ${String(error)}`)
      })
  }, [agentControlEnabled, agentPort, pushLog])

  useEffect(() => {
    if (!isTauriRuntime()) return

    const cleanupEvents = listen<WorkerEvent>('seamless-worker-event', (event) => {
      const payload = event.payload
      if (payload.type === 'image_start' && payload.path) {
        patchQueueItem(payload.path, { status: 'running', message: 'Processing' })
      }
      if (payload.type === 'image_done' && payload.path) {
        patchQueueItem(payload.path, {
          status: 'done',
          message: payload.message ?? 'Saved',
          outputPath: payload.output,
        })
        setSelectedPath(payload.path)
        pushLog(`Saved ${payload.output ? fileName(payload.output) : fileName(payload.path)}.`)
      }
      if (payload.type === 'image_error' && payload.path) {
        patchQueueItem(payload.path, { status: 'error', message: payload.message ?? 'Failed' })
        pushLog(`${fileName(payload.path)} failed: ${payload.message ?? 'Unknown error'}`)
      }
      if (payload.type === 'canceled') {
        setQueue((current) =>
          current.map((item) => (item.status === 'pending' || item.status === 'running' ? { ...item, status: 'error', message: 'Canceled' } : item)),
        )
        setNotice(payload.message ?? 'Seamless job canceled')
        setBusy(false)
      }
      if (payload.type === 'done') {
        setBusy(false)
      }
    })

    const cleanupDrag = getCurrentWindow().onDragDropEvent((event) => {
      if (event.payload.type === 'enter' || event.payload.type === 'over') {
        const isTileCheckDrop = isPositionInsideElement(event.payload.position, tileCheckRef.current)
        setDragging(!isTileCheckDrop)
        setTileCheckDragging(isTileCheckDrop)
      }
      if (event.payload.type === 'leave') {
        setDragging(false)
        setTileCheckDragging(false)
      }
      if (event.payload.type === 'drop') {
        const isTileCheckDrop = isPositionInsideElement(event.payload.position, tileCheckRef.current)
        setDragging(false)
        setTileCheckDragging(false)
        if (isTileCheckDrop) {
          void loadTileCheckImage(event.payload.paths)
        } else {
          void addPaths(event.payload.paths)
        }
      }
    })

    return () => {
      void cleanupEvents.then((unlisten) => unlisten())
      void cleanupDrag.then((unlisten) => unlisten())
    }
  }, [addPaths, loadTileCheckImage, patchQueueItem, pushLog])

  useEffect(() => {
    if (!selectedItem || !isTauriRuntime()) {
      setPreview({})
      return
    }

    let canceled = false
    setPreview({})

    async function loadPreview() {
      try {
        const input = await invoke<string>('preview_image_data_url', { path: selectedItem.path })
        const output = selectedItem.outputPath
          ? await invoke<string>('preview_image_data_url', { path: selectedItem.outputPath })
          : undefined
        if (!canceled) setPreview({ input, output })
      } catch (error) {
        if (!canceled) setPreview({ error: String(error) })
      }
    }

    void loadPreview()
    return () => {
      canceled = true
    }
  }, [selectedItem])

  async function chooseImages() {
    if (!isTauriRuntime()) {
      setNotice('Desktop runtime required for file picking')
      return
    }
    const selected = await open({
      multiple: true,
      directory: false,
      filters: imageFilters,
    })
    if (Array.isArray(selected)) {
      await addPaths(selected)
    } else if (typeof selected === 'string') {
      await addPaths([selected])
    }
  }

  async function chooseFolder() {
    if (!isTauriRuntime()) {
      setNotice('Desktop runtime required for folder picking')
      return
    }
    const selected = await open({
      multiple: false,
      directory: true,
    })
    if (typeof selected === 'string') {
      await addPaths([selected])
    }
  }

  async function chooseOutputFolder() {
    if (!isTauriRuntime()) {
      setNotice('Desktop runtime required for folder picking')
      return
    }
    const selected = await open({
      multiple: false,
      directory: true,
      defaultPath: options.outputDir || undefined,
    })
    if (typeof selected === 'string') {
      setOptions((current) => ({ ...current, outputDir: selected, sameFolder: false }))
    }
  }

  async function openOutputFolder(path?: string) {
    const target = path ?? selectedItem?.outputPath ?? selectedItem?.path
    if (!target || !isTauriRuntime()) return
    try {
      await invoke('open_containing_folder', { path: target })
    } catch (error) {
      setNotice(String(error))
    }
  }

  async function start() {
    if (!canStart) {
      setNotice(options.sameFolder ? 'Queue an image first' : 'Choose an output folder')
      return
    }
    setBusy(true)
    setNotice('Making seamless outputs...')
    setQueue((current) =>
      current.map((item) => ({
        ...item,
        status: 'pending',
        message: undefined,
        outputPath: undefined,
      })),
    )

    try {
      const results = await invoke<ProcessResult[]>('start_seamless_job', {
        paths: queue.map((item) => item.path),
        options,
      })
      const byPath = new Map(results.map((result) => [result.inputPath, result]))
      setQueue((current) =>
        current.map((item) => {
          const result = byPath.get(item.path)
          if (!result) return item
          return {
            ...item,
            status: result.status,
            message: result.message,
            outputPath: result.outputPath ?? undefined,
          }
        }),
      )
      const saved = results.filter((result) => result.status === 'done').length
      const failed = results.length - saved
      setNotice(`Saved ${saved}/${results.length}${failed > 0 ? `, ${failed} failed` : ''}`)
    } catch (error) {
      setNotice(String(error))
      pushLog(`Run failed: ${String(error)}`)
    } finally {
      setBusy(false)
    }
  }

  function resetQueue() {
    setQueue([])
    setSelectedPath(null)
    setPreview({})
    setNotice('Queue cleared')
  }

  function removeItem(path: string) {
    setQueue((current) => current.filter((item) => item.path !== path))
    setSelectedPath((current) => (current === path ? null : current))
  }

  function clearTileCheckImage() {
    setTileCheck({})
    setNotice(preview.output ? 'Tile check using selected output' : 'Tile check cleared')
  }

  return (
    <div className="app-shell theme-neko-tron">
      <header className="topbar">
        <div className="brand-lockup">
          <div className="brand-icon">SI</div>
          <div>
            <div className="brand-title-row">
              <h1>Seamless Image Edit</h1>
              <span>{APP_VERSION}</span>
            </div>
            <p>{notice}</p>
          </div>
        </div>
        <div className="topbar-actions">
          <button className="secondary-action" type="button" onClick={chooseImages} title="Add images">
            <ImageIcon size={17} />
            Images
          </button>
          <button className="secondary-action" type="button" onClick={chooseFolder} title="Add folder">
            <FolderOpen size={17} />
            Folder
          </button>
          <button className="icon-button" type="button" onClick={resetQueue} title="Clear queue" disabled={busy || queue.length === 0}>
            <RefreshCw size={17} />
          </button>
          <button className="primary-action" type="button" onClick={start} disabled={!canStart}>
            {busy ? <Loader2 className="spin" size={18} /> : <Wand2 size={18} />}
            Make Seamless
          </button>
        </div>
      </header>

      <main className="workspace">
        <aside className="tool-panel">
          <section className="panel-section">
            <div className="section-title">
              <Sparkles size={16} />
              Mode
            </div>
            <div className="mode-grid">
              {modeOptions.map((mode) => {
                const Icon = mode.icon
                return (
                  <button
                    className={classNames('mode-button', options.mode === mode.id && 'active')}
                    key={mode.id}
                    type="button"
                    onClick={() => setOptions((current) => ({ ...current, mode: mode.id }))}
                    title={`${mode.label} seamless mode`}
                  >
                    <Icon size={17} />
                    <span>{mode.label}</span>
                  </button>
                )
              })}
            </div>
          </section>

          <section className="panel-section">
            <div className="section-title">
              <ImageIcon size={16} />
              Output
            </div>
            <div className="format-toggle">
              {(['webp', 'png'] as OutputFormat[]).map((format) => (
                <button
                  className={classNames('format-button', options.outputFormat === format && 'active')}
                  type="button"
                  key={format}
                  onClick={() => setOptions((current) => ({ ...current, outputFormat: format }))}
                >
                  {format.toUpperCase()}
                </button>
              ))}
            </div>
            <ToggleControl
              checked={options.sameFolder}
              label="Save beside source"
              onChange={(checked) => setOptions((current) => ({ ...current, sameFolder: checked }))}
            />
            <div className="folder-row">
              <input value={outputLabel(options)} readOnly disabled={options.sameFolder} />
              <button className="icon-button compact" type="button" onClick={chooseOutputFolder} title="Choose output folder">
                <FolderOpen size={15} />
              </button>
            </div>
            <label className="field">
              <span>Suffix</span>
              <input
                value={options.suffix}
                onChange={(event) => setOptions((current) => ({ ...current, suffix: event.currentTarget.value }))}
              />
            </label>
          </section>

          <section className="panel-section">
            <div className="section-title">
              <Wand2 size={16} />
              Agent API
            </div>
            <ToggleControl
              checked={agentControlEnabled}
              label="Enable local API"
              onChange={(checked) => void toggleAgentControl(checked)}
            />
            <label className="field">
              <span>API port</span>
              <input
                type="number"
                min="1"
                max="65535"
                value={agentPort}
                onChange={(event) => setAgentPort(event.currentTarget.value)}
                onBlur={() => void applyAgentPort()}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') {
                    event.currentTarget.blur()
                  }
                }}
              />
            </label>
            {agentStatus?.enabled ? (
              <div className="agent-status">
                <span>{agentStatus.url}</span>
                <small>OpenAPI: {agentStatus.openapiUrl}</small>
              </div>
            ) : null}
          </section>

          <section className="panel-section">
            <div className="section-title">
              <Grid3X3 size={16} />
              Tuning
            </div>
            <div className="strategy-toggle">
              {strategyOptions.map((strategy) => (
                <button
                  className={classNames('strategy-button', options.strategy === strategy.id && 'active')}
                  type="button"
                  key={strategy.id}
                  onClick={() => setOptions((current) => ({ ...current, strategy: strategy.id }))}
                  title={strategy.title}
                >
                  {strategy.label}
                </button>
              ))}
            </div>
            <RangeControl
              label="Seam band"
              max={45}
              min={4}
              onChange={(value) =>
                setOptions((current) => (current.blendPercent === value ? current : { ...current, blendPercent: value }))
              }
              step={1}
              title="How wide the repaired edge area is. Lower keeps more original detail; higher hides bigger edge mismatches but can soften the texture."
              value={options.blendPercent}
              valueText={`${formatRangeValue(options.blendPercent, defaultOptions.blendPercent, 4, 45, 0)}%`}
            />
            <RangeControl
              label="Flatten"
              max={1}
              min={0}
              onChange={(value) => setOptions((current) => (current.flatten === value ? current : { ...current, flatten: value }))}
              step={0.1}
              title="Reduces broad lighting gradients before seam repair. Use a little for photos with vignettes or directional light; leave low for already-even textures."
              value={options.flatten}
              valueText={formatRangeValue(options.flatten, defaultOptions.flatten, 0, 1, 1)}
            />
            <ToggleControl
              checked={options.snapPeriod}
              label="Snap to pattern repeat"
              onChange={(checked) => setOptions((current) => ({ ...current, snapPeriod: checked }))}
              title="Detect repeating patterns (bricks, tiles) and crop to a whole number of repeats so no partial bricks appear when tiling"
            />
            <ToggleControl
              checked={options.recursive}
              label="Scan folders recursively"
              onChange={(checked) => setOptions((current) => ({ ...current, recursive: checked }))}
            />
            <ToggleControl
              checked={options.overwrite}
              label="Overwrite outputs"
              onChange={(checked) => setOptions((current) => ({ ...current, overwrite: checked }))}
            />
          </section>

        </aside>

        <section className="main-surface">
          <div className="stats-row">
            <div>
              <span>{queue.length}</span>
              queued
            </div>
            <div>
              <span>{completeCount}</span>
              saved
            </div>
            <div>
              <span>{errorCount}</span>
              failed
            </div>
            <div>
              <span>{options.mode}</span>
              mode
            </div>
            <div>
              <span>{options.strategy}</span>
              strategy
            </div>
          </div>

          <section className={classNames('drop-zone', dragging && 'dragging')}>
            <div className="drop-glyph">
              {busy ? <Loader2 className="spin" size={34} /> : <Play size={34} />}
            </div>
            <div>
              <h2>Drop images or folders</h2>
              <p>{options.outputFormat.toUpperCase()} outputs write to {outputLabel(options)}.</p>
            </div>
          </section>

          <section className="preview-grid">
            <div className="preview-panel">
              <div className="preview-head">
                <strong>Input</strong>
                <small>{selectedItem ? fileName(selectedItem.path) : 'No image selected'}</small>
              </div>
              {preview.input ? <img src={preview.input} alt="" /> : <div className="preview-empty">Pending</div>}
            </div>

            <div className="preview-panel">
              <div className="preview-head">
                <strong>Output</strong>
                <small>{selectedItem?.outputPath ? fileName(selectedItem.outputPath) : 'No output yet'}</small>
              </div>
              {preview.output ? <img src={preview.output} alt="" /> : <div className="preview-empty">Pending</div>}
            </div>

            <div className={classNames('preview-panel tile-check', tileCheckDragging && 'dragging')} ref={tileCheckRef}>
              <div className="preview-head">
                <div className="preview-title">
                  <strong>2x2 Edge Check</strong>
                  <small>{tileCheckLabel}</small>
                </div>
                {tileCheck.dataUrl ? (
                  <button className="icon-button compact tile-check-clear" type="button" onClick={clearTileCheckImage} title="Use selected output">
                    <XCircle size={14} />
                  </button>
                ) : null}
              </div>
              {tileCheckImage ? (
                <TilePreview src={tileCheckImage} />
              ) : (
                <div className="preview-empty">Drop image</div>
              )}
            </div>
          </section>

          {previewError ? <div className="notice-line error">{previewError}</div> : null}

          <div className="bottom-stack">
            <section className="queue-panel">
              <div className="queue-header">
                <span>Image</span>
                <span>Status</span>
                <span>Output</span>
                <span />
              </div>
              {queue.length === 0 ? (
                <div className="empty-row">
                  <TriangleAlert size={18} />
                  No images queued.
                </div>
              ) : (
                queue.map((item) => (
                  <button
                    type="button"
                    className={classNames('queue-row', item.status, selectedItem?.path === item.path && 'selected')}
                    key={item.path}
                    onClick={() => setSelectedPath(item.path)}
                  >
                    <span className="queue-path">
                      <strong>{fileName(item.path)}</strong>
                      <small>{compactPath(item.path)}</small>
                    </span>
                    <span className="queue-status">
                      {statusIcon(item.status)}
                      {item.message ?? item.status}
                    </span>
                    <span className="queue-output">
                      {item.outputPath ? (
                        <span onClick={(event) => event.stopPropagation()}>
                          <button
                            className="link-action"
                            type="button"
                            onClick={() => void openOutputFolder(item.outputPath)}
                          >
                            {fileName(item.outputPath)}
                          </button>
                        </span>
                      ) : (
                        'Pending'
                      )}
                    </span>
                    <span className="queue-tools">
                      <button
                        className="icon-button compact danger"
                        type="button"
                        title="Remove"
                        onClick={(event) => {
                          event.stopPropagation()
                          removeItem(item.path)
                        }}
                        disabled={busy}
                      >
                        <Trash2 size={14} />
                      </button>
                    </span>
                  </button>
                ))
              )}
            </section>

            <section className="log-panel">
              <div className="section-title">
                <TriangleAlert size={16} />
                Log
              </div>
              <div className="log-lines">
                {log.length === 0 ? <p>Ready.</p> : log.map((line, index) => <p key={`${line}-${index}`}>{line}</p>)}
              </div>
            </section>
          </div>
        </section>
      </main>
    </div>
  )
}

export default App
