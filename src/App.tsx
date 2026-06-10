import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { open } from '@tauri-apps/plugin-dialog'
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
import { CSSProperties, useCallback, useEffect, useMemo, useState } from 'react'

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
  type: 'image_start' | 'image_done' | 'image_error' | 'done'
  path?: string
  output?: string
  message?: string
}

type PreviewState = {
  input?: string
  output?: string
  error?: string
}

const SETTINGS_KEY = 'seamlessImageEdit.settings.v1'

const defaultOptions: SeamlessOptions = {
  mode: 'horizontal',
  outputFormat: 'webp',
  sameFolder: true,
  outputDir: '',
  suffix: '_seamless',
  recursive: true,
  overwrite: false,
  blendPercent: 18,
  strategy: 'seam-cut',
  flatten: 0,
}

const modeOptions: Array<{ id: SeamMode; label: string; icon: typeof ArrowLeftRight }> = [
  { id: 'horizontal', label: 'Horizontal', icon: ArrowLeftRight },
  { id: 'vertical', label: 'Vertical', icon: ArrowUpDown },
  { id: 'tile', label: 'Tile', icon: Grid3X3 },
]

const strategyOptions: Array<{ id: SeamStrategy; label: string }> = [
  { id: 'seam-cut', label: 'Seam cut' },
  { id: 'synthesis', label: 'Synthesis' },
  { id: 'blend', label: 'Blend' },
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

function loadOptions(): SeamlessOptions {
  try {
    const raw = window.localStorage.getItem(SETTINGS_KEY)
    if (!raw) return defaultOptions
    const parsed = JSON.parse(raw) as Partial<SeamlessOptions>
    return {
      mode: coerceMode(parsed.mode),
      outputFormat: coerceFormat(parsed.outputFormat),
      sameFolder: coerceBoolean(parsed.sameFolder, defaultOptions.sameFolder),
      outputDir: typeof parsed.outputDir === 'string' ? parsed.outputDir : defaultOptions.outputDir,
      suffix: typeof parsed.suffix === 'string' && parsed.suffix.trim() ? parsed.suffix : defaultOptions.suffix,
      recursive: coerceBoolean(parsed.recursive, defaultOptions.recursive),
      overwrite: coerceBoolean(parsed.overwrite, defaultOptions.overwrite),
      blendPercent: coerceNumber(parsed.blendPercent, defaultOptions.blendPercent, 4, 45),
      strategy: coerceStrategy(parsed.strategy),
      flatten: coerceNumber(parsed.flatten, defaultOptions.flatten, 0, 1),
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

function App() {
  const [options, setOptions] = useState<SeamlessOptions>(loadOptions)
  const [queue, setQueue] = useState<QueueItem[]>([])
  const [selectedPath, setSelectedPath] = useState<string | null>(null)
  const [preview, setPreview] = useState<PreviewState>({})
  const [dragging, setDragging] = useState(false)
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState('Ready')
  const [log, setLog] = useState<string[]>([])

  const selectedItem = useMemo(
    () => queue.find((item) => item.path === selectedPath) ?? queue[0] ?? null,
    [queue, selectedPath],
  )
  const completeCount = useMemo(() => queue.filter((item) => item.status === 'done').length, [queue])
  const errorCount = useMemo(() => queue.filter((item) => item.status === 'error').length, [queue])
  const canStart = queue.length > 0 && !busy && (options.sameFolder || Boolean(options.outputDir.trim()))

  const pushLog = useCallback((message: string) => {
    setLog((current) => [message, ...current].slice(0, 80))
  }, [])

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

  useEffect(() => {
    window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(options))
  }, [options])

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
          message: 'Saved',
          outputPath: payload.output,
        })
        setSelectedPath(payload.path)
        pushLog(`Saved ${payload.output ? fileName(payload.output) : fileName(payload.path)}.`)
      }
      if (payload.type === 'image_error' && payload.path) {
        patchQueueItem(payload.path, { status: 'error', message: payload.message ?? 'Failed' })
        pushLog(`${fileName(payload.path)} failed: ${payload.message ?? 'Unknown error'}`)
      }
      if (payload.type === 'done') {
        setBusy(false)
      }
    })

    const cleanupDrag = getCurrentWindow().onDragDropEvent((event) => {
      if (event.payload.type === 'over') {
        setDragging(true)
      }
      if (event.payload.type === 'leave') {
        setDragging(false)
      }
      if (event.payload.type === 'drop') {
        setDragging(false)
        void addPaths(event.payload.paths)
      }
    })

    return () => {
      void cleanupEvents.then((unlisten) => unlisten())
      void cleanupDrag.then((unlisten) => unlisten())
    }
  }, [addPaths, patchQueueItem, pushLog])

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

  return (
    <div className="app-shell theme-neko-tron">
      <header className="topbar">
        <div className="brand-lockup">
          <div className="brand-icon">SI</div>
          <div>
            <h1>Seamless Image Edit</h1>
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
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={options.sameFolder}
                onChange={(event) => setOptions((current) => ({ ...current, sameFolder: event.currentTarget.checked }))}
              />
              <span>Save beside source</span>
            </label>
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
                  title={`${strategy.label} strategy`}
                >
                  {strategy.label}
                </button>
              ))}
            </div>
            <label className="range-field">
              <span>Seam band</span>
              <strong>{options.blendPercent.toFixed(0)}%</strong>
              <input
                type="range"
                min="4"
                max="45"
                step="1"
                value={options.blendPercent}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, blendPercent: Number(event.currentTarget.value) || current.blendPercent }))
                }
              />
            </label>
            <label className="range-field">
              <span>Flatten</span>
              <strong>{options.flatten.toFixed(1)}</strong>
              <input
                type="range"
                min="0"
                max="1"
                step="0.1"
                value={options.flatten}
                onChange={(event) =>
                  setOptions((current) => ({ ...current, flatten: Number(event.currentTarget.value) || 0 }))
                }
              />
            </label>
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={options.recursive}
                onChange={(event) => setOptions((current) => ({ ...current, recursive: event.currentTarget.checked }))}
              />
              <span>Scan folders recursively</span>
            </label>
            <label className="toggle-row">
              <input
                type="checkbox"
                checked={options.overwrite}
                onChange={(event) => setOptions((current) => ({ ...current, overwrite: event.currentTarget.checked }))}
              />
              <span>Overwrite outputs</span>
            </label>
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

            <div className="preview-panel tile-check">
              <div className="preview-head">
                <strong>Tile Check</strong>
                <small>{preview.output ? '2x2 repeat' : 'Pending'}</small>
              </div>
              {preview.output ? (
                <div
                  className="tile-preview"
                  style={{ '--tile-image': `url(${preview.output})` } as CSSProperties}
                />
              ) : (
                <div className="preview-empty">Pending</div>
              )}
            </div>
          </section>

          {preview.error ? <div className="notice-line error">{preview.error}</div> : null}

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
