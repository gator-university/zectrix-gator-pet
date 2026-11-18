// 鳄鱼大宠物 · 本机配置桥（只监听 127.0.0.1）。
//
// 安全模型：
// - 每次安装生成一个随机 token，落在 %LOCALAPPDATA%\zectrix-dsh-pet\bridge-token；
//   /token 只对 DSH 桌面 App 的 `dsh-app://app` 渲染进程返回，且每次调用都轮换令牌。
// - 令牌 10 分钟有效；/config、/test 同时校验令牌内容与有效期。
// - /config、/test 等需要 X-Croc-Pet-Token；CORS 不再返回 `*`。
// - POST /config 的白名单字段做严格校验，先写临时文件再 rename，避免半份 JSON。
import { randomBytes, timingSafeEqual } from 'node:crypto'
import fs from 'node:fs'
import { createServer } from 'node:http'
import { isIPv4 } from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { hardenDataDir } from './winacl.js'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const VERSION = JSON.parse(fs.readFileSync(path.join(HERE, 'package.json'), 'utf8')).version

const MAX_BODY = 64 * 1024
const TOKEN_HEADER = 'x-croc-pet-token'
/** 令牌有效期：10 分钟。过期后 /config、/test 一律 401，需重新 GET /token。 */
const TOKEN_TTL_MS = 10 * 60 * 1000
/** 同一台设备的「测试连接」冷却：3 秒内重复请求回 429。 */
const TEST_COOLDOWN_MS = 3000

/**
 * 设置页只允许改这些字段；未知字段直接丢弃（旧版本落盘的 `pet_skin`、
 * `animation` 之类键会被静默忽略，不报错）。
 *
 * `title_source` 在列，但 UI 必须同时写清隐私含义（prompt = 提示词首行会落盘/上屏）。
 */
const ALLOWED_KEYS = new Set([
  'follow',
  'sound',
  'volume',
  'target',
  'format',
  'mode',
  'refresh_secs',
  'stale_minutes',
  'title_source',
])

const FORMATS = new Set(['mono', 'gray4', 'gray16'])
const MODES = new Set(['realtime', 'balance', 'calm', 'minimal'])
const TITLE_SOURCES = new Set(['session', 'prompt'])

/**
 * 只认 DSH 桌面应用自己的渲染进程（自定义 scheme，见 Electron
 * `--cors-schemes=dsh-app`）。它跨源到 127.0.0.1 时 Sec-Fetch-Site 是
 * cross-site，所以不能拿它当门槛；远程网页与普通 http 页面都无法伪造这个 origin。
 *
 * 注意：`http://127.0.0.1:<port>` / `http://localhost:<port>` 不再受信——
 * 本机网页同样可能被 XSS、恶意扩展或调试端口劫持，不能凭「来自回环」放行。
 */
const APP_ORIGIN = 'dsh-app://app'

function httpError(statusCode, message) {
  const err = new Error(message)
  err.statusCode = statusCode
  return err
}

function isTrustedOrigin(origin) {
  return origin === APP_ORIGIN
}

/**
 * target 必须是单个 IPv4 字面量，且只能落在私网/回环段。
 * 拒绝协议前缀、端口、CIDR、IPv6、空串、localhost 等域名。
 */
function isPrivateIPv4(value) {
  if (typeof value !== 'string' || !isIPv4(value)) return false
  const [a, b] = value.split('.').map(Number)
  if (a === 10 || a === 127) return true
  if (a === 172 && b >= 16 && b <= 31) return true
  if (a === 192 && b === 168) return true
  return false
}

function validatePatch(patch) {
  if (patch === null || typeof patch !== 'object' || Array.isArray(patch)) {
    return '请求体必须是 JSON 对象'
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'target') && !isPrivateIPv4(patch.target)) {
    return 'target 必须是单个私网/回环 IPv4 地址（例如 192.168.1.162）'
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'volume')) {
    const volume = patch.volume
    if (!Number.isInteger(volume) || volume < 0 || volume > 100) {
      return 'volume 必须是 0–100 的整数'
    }
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'format') && !FORMATS.has(patch.format)) {
    return 'format 必须是 mono / gray4 / gray16'
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'mode') && !MODES.has(patch.mode)) {
    return 'mode 必须是 realtime / balance / calm / minimal'
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'title_source') && !TITLE_SOURCES.has(patch.title_source)) {
    return 'title_source 必须是 session / prompt'
  }
  return null
}

function tokenFilePath() {
  const base = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local')
  return path.join(base, 'zectrix-dsh-pet', 'bridge-token')
}

/** 写临时文件再 rename；失败时清掉临时文件，绝不留下半份内容。 */
function atomicWrite(file, text, mode) {
  const dir = path.dirname(file)
  fs.mkdirSync(dir, { recursive: true })
  const tmp = path.join(dir, `.${path.basename(file)}.${process.pid}.${randomBytes(4).toString('hex')}.tmp`)
  fs.writeFileSync(tmp, text, mode === undefined ? 'utf8' : { encoding: 'utf8', mode })
  try {
    fs.renameSync(tmp, file)
  } catch (e) {
    try {
      fs.unlinkSync(tmp)
    } catch {}
    throw e
  }
}

/** 首次启动生成 48 位 hex token 并落盘；后续复用。文件损坏时重新生成。 */
function loadOrCreateToken() {
  const file = tokenFilePath()
  try {
    const existing = fs.readFileSync(file, 'utf8').trim()
    if (/^[0-9a-f]{32,}$/i.test(existing)) return existing
  } catch {
    // 文件不存在或读不到：走生成分支。
  }
  const token = randomBytes(24).toString('hex')
  atomicWrite(file, `${token}\n`, 0o600)
  return token
}

function requestOrigin(req) {
  return req.headers.origin
}

function sendJson(req, res, statusCode, body) {
  if (res.writableEnded || res.destroyed) return
  const headers = {
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    vary: 'Origin',
  }
  const origin = requestOrigin(req)
  // 只回白名单 Origin；绝不回 `*`。
  if (isTrustedOrigin(origin)) headers['access-control-allow-origin'] = origin
  try {
    res.writeHead(statusCode, headers)
    res.end(JSON.stringify(body))
  } catch {
    try {
      res.destroy()
    } catch {}
  }
}

function hasJsonContentType(req) {
  const contentType = String(req.headers['content-type'] || '')
  return /^application\/json(?:\s*;.*)?$/i.test(contentType)
}

/** 读 JSON body：非 application/json → 415；超过 64KB → 立即 destroy + 413。 */
function readBody(req) {
  return new Promise((resolve, reject) => {
    if (!hasJsonContentType(req)) {
      reject(httpError(415, 'content-type 必须是 application/json'))
      return
    }
    let raw = ''
    let size = 0
    let settled = false
    const fail = (err, destroy) => {
      if (settled) return
      settled = true
      if (destroy) {
        try {
          req.destroy()
        } catch {}
      }
      reject(err)
    }
    req.on('data', chunk => {
      size += chunk.length
      if (size > MAX_BODY) {
        fail(httpError(413, '请求体过大'), true)
        return
      }
      raw += chunk.toString('utf8')
    })
    req.on('end', () => {
      if (settled) return
      settled = true
      try {
        const parsed = raw ? JSON.parse(raw) : {}
        if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
          reject(httpError(400, '请求体必须是 JSON 对象'))
          return
        }
        resolve(parsed)
      } catch {
        reject(httpError(400, '请求体不是合法 JSON'))
      }
    })
    req.on('error', err => fail(err, false))
  })
}

function tokenMatches(headerValue, token) {
  if (typeof headerValue !== 'string' || headerValue.length === 0) return false
  const given = Buffer.from(headerValue)
  const expected = Buffer.from(token)
  if (given.length !== expected.length) return false
  return timingSafeEqual(given, expected)
}

/**
 * 启动配置桥。
 * @param {object} options
 * @param {number} [options.port] 0 表示随机端口（测试用）
 * @param {string} options.petConfigPath pet-config.json 路径
 * @param {(args: string[], timeoutMs?: number) => Promise<{ok: boolean, output: string}>} options.runCli
 * @param {(line: string) => void} [options.log]
 * @param {number} [options.tokenTtlMs] 令牌有效期（测试用；默认 10 分钟）
 * @returns {Promise<{server: import('node:http').Server, port: number, token: string, close: () => Promise<void>}>}
 */
export async function createBridge({ port = 4517, petConfigPath, runCli, log, tokenTtlMs = TOKEN_TTL_MS } = {}) {
  if (!petConfigPath) throw new Error('createBridge 需要 petConfigPath')
  const cli = typeof runCli === 'function' ? runCli : async () => ({ ok: false, output: 'runCli 未注入' })
  const logLine = typeof log === 'function' ? log : () => {}
  // 先建目录并收紧 ACL，再生成/复用令牌：令牌落盘时目录已经只对
  // 当前用户 + SYSTEM 开放。icacls 失败只警告，不阻塞配置桥。
  const dataDir = path.dirname(tokenFilePath())
  try {
    fs.mkdirSync(dataDir, { recursive: true })
  } catch {
    // 目录建不出来时 loadOrCreateToken 会自己再试一次并报错。
  }
  await hardenDataDir(dataDir, tokenFilePath(), { log: logLine })
  let token = loadOrCreateToken()
  // 有效期从「最近一次签发」算起：/token 每次调用都换新令牌，旧令牌立即作废。
  let tokenIssuedAt = Date.now()
  // 同一台设备的测试连接冷却：key = target（未配置时用 auto），value = 上次毫秒时间戳。
  const testCooldown = new Map()

  const tokenAlive = () => Date.now() - tokenIssuedAt <= tokenTtlMs

  /** 轮换令牌并落盘；返回新令牌。落盘失败只警告，本次进程内仍按新令牌鉴权。 */
  function issueToken() {
    const next = randomBytes(24).toString('hex')
    token = next
    tokenIssuedAt = Date.now()
    try {
      atomicWrite(tokenFilePath(), `${next}\n`, 0o600)
    } catch (e) {
      logLine(`令牌落盘失败（本次运行仍生效）：${e.message || e}`)
    }
    return next
  }

  function readConfig() {
    try {
      const parsed = JSON.parse(fs.readFileSync(petConfigPath, 'utf8'))
      return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : {}
    } catch {
      return {}
    }
  }

  function writeConfig(patch) {
    const next = { ...readConfig() }
    const changed = []
    for (const [key, value] of Object.entries(patch || {})) {
      if (!ALLOWED_KEYS.has(key)) continue
      if (value === undefined || value === null) continue
      next[key] = value
      changed.push(key)
    }
    const text = JSON.stringify(next, null, 2)
    // 写前自校验，确保落盘的一定是合法 JSON。
    JSON.parse(text)
    atomicWrite(petConfigPath, `${text}\n`)
    // 只记改动的键名：值里可能有设备地址等信息，不进日志。
    logLine(`配置已更新：${changed.length ? changed.join(', ') : '（无）'}`)
    return next
  }

  async function route(req, res) {
    const url = new URL(req.url || '/', 'http://127.0.0.1')

    if (req.method === 'OPTIONS') {
      const origin = requestOrigin(req)
      if (!isTrustedOrigin(origin)) {
        sendJson(req, res, 403, { ok: false, error: 'forbidden' })
        return
      }
      res.writeHead(204, {
        'access-control-allow-origin': origin,
        'access-control-allow-headers': 'content-type, x-croc-pet-token',
        'access-control-allow-methods': 'GET, POST, OPTIONS',
        vary: 'Origin',
        'cache-control': 'no-store',
      })
      res.end()
      return
    }

    // 浏览器带了 Origin 但不是 dsh-app://app → 一律 403；没有 Origin 的非浏览器
    // 客户端仍可凭 token 访问（本机脚本 / curl）。
    const origin = requestOrigin(req)
    if (origin !== undefined && !isTrustedOrigin(origin)) {
      sendJson(req, res, 403, { ok: false, error: 'forbidden' })
      return
    }
    // 跨站请求即使 token 泄漏也拒绝；DSH 桌面 App 的 dsh-app://app 例外
    // （自定义 scheme 跨源到本机 HTTP 时浏览器固定标 cross-site）。
    if (req.headers['sec-fetch-site'] === 'cross-site' && origin !== APP_ORIGIN) {
      sendJson(req, res, 403, { ok: false, error: 'forbidden' })
      return
    }

    if (req.method === 'GET' && url.pathname === '/health') {
      // 只回版本，不泄漏安装路径。
      sendJson(req, res, 200, { ok: true, version: VERSION })
      return
    }

    if (req.method === 'GET' && url.pathname === '/token') {
      // 只认 DSH 桌面 App 的渲染进程；Sec-Fetch-Site 不作额外门槛
      // （自定义 scheme 跨源到本机 HTTP 时它固定是 cross-site）。
      // 每次调用都轮换令牌，旧令牌立即失效。
      if (origin !== APP_ORIGIN) {
        sendJson(req, res, 403, { ok: false, error: 'forbidden' })
        return
      }
      sendJson(req, res, 200, { ok: true, token: issueToken() })
      return
    }

    // 其余路由一律要求 token，且必须在有效期内。
    if (!tokenAlive() || !tokenMatches(req.headers[TOKEN_HEADER], token)) {
      sendJson(req, res, 401, { ok: false, error: 'unauthorized' })
      return
    }

    if (req.method === 'GET' && url.pathname === '/config') {
      sendJson(req, res, 200, { ok: true, config: readConfig() })
      return
    }

    if (req.method === 'POST' && url.pathname === '/config') {
      const patch = await readBody(req)
      const invalid = validatePatch(patch)
      if (invalid) {
        // 校验失败不落盘。
        sendJson(req, res, 400, { ok: false, error: invalid })
        return
      }
      sendJson(req, res, 200, { ok: true, config: writeConfig(patch) })
      return
    }

    if (req.method === 'POST' && url.pathname === '/test') {
      // 忽略 body 里的 target，只用已保存的值，避免被跨站页面当 UDP 探测器。
      await readBody(req)
      const saved = readConfig()
      const device = typeof saved.target === 'string' && saved.target ? saved.target : 'auto'
      const nowMs = Date.now()
      const last = testCooldown.get(device)
      if (last !== undefined && nowMs - last < TEST_COOLDOWN_MS) {
        const wait = Math.ceil((TEST_COOLDOWN_MS - (nowMs - last)) / 1000)
        sendJson(req, res, 429, { ok: false, error: `测试过于频繁，请 ${wait} 秒后再试` })
        return
      }
      // 先记冷却再执行：并发请求不能绕过 3 秒窗口。
      testCooldown.set(device, nowMs)
      const args = ['check']
      if (typeof saved.target === 'string' && saved.target) args.push('--target', saved.target)
      sendJson(req, res, 200, await cli(args))
      return
    }

    sendJson(req, res, 404, { ok: false, error: 'not found' })
  }

  const server = createServer((req, res) => {
    route(req, res).catch(err => {
      const statusCode = Number.isInteger(err?.statusCode) ? err.statusCode : 500
      const message = statusCode === 500 ? 'internal error' : String(err?.message || err)
      sendJson(req, res, statusCode, { ok: false, error: message })
    })
  })

  // 本机桥只服务设置页：请求 10 秒超时、请求头 8 秒、最多 8 条并发连接，
  // 避免慢连接/半开连接（Slowloris）把宿主进程的句柄拖住。
  server.requestTimeout = 10_000
  server.headersTimeout = 8_000
  server.maxConnections = 8

  await new Promise((resolve, reject) => {
    const onError = err => {
      server.off('listening', onListening)
      reject(err)
    }
    const onListening = () => {
      server.off('error', onError)
      resolve()
    }
    server.once('error', onError)
    server.once('listening', onListening)
    server.listen(port, '127.0.0.1')
  })

  const actualPort = server.address().port
  const close = () =>
    new Promise(resolve => {
      if (!server.listening) {
        resolve()
        return
      }
      server.close(() => resolve())
    })

  return { server, port: actualPort, token, close }
}
