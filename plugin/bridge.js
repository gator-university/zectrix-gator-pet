// 鳄鱼大宠物 · 本机配置桥（只监听 127.0.0.1）。
//
// 安全模型：
// - 每次安装生成一个随机 token，落在 %LOCALAPPDATA%\zectrix-dsh-pet\bridge-token；
//   /token 只对「白名单 Origin + 同源/同站 Sec-Fetch-Site」的浏览器请求返回，避免任意网页读取。
// - /config、/test 等需要 X-Croc-Pet-Token；CORS 不再返回 `*`。
// - POST /config 的白名单字段做严格校验，先写临时文件再 rename，避免半份 JSON。
import { randomBytes, timingSafeEqual } from 'node:crypto'
import fs from 'node:fs'
import { createServer } from 'node:http'
import { isIPv4 } from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const VERSION = JSON.parse(fs.readFileSync(path.join(HERE, 'package.json'), 'utf8')).version

const MAX_BODY = 64 * 1024
const TOKEN_HEADER = 'x-croc-pet-token'
const TRUSTED_FETCH_SITES = new Set(['same-origin', 'same-site'])

/**
 * 设置页只允许改这些字段；未知字段直接丢弃。
 *
 * 注意：`pet_skin` 刻意不在此列——它由 `dsh-pet skin set` 写入（含造型校验）。
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
  'animation',
  'stale_minutes',
  'title_source',
])

const FORMATS = new Set(['mono', 'gray4', 'gray16'])
const MODES = new Set(['realtime', 'balance', 'calm', 'minimal'])
const TITLE_SOURCES = new Set(['session', 'prompt'])

/** 只认 http://127.0.0.1:<port> 与 http://localhost:<port>，端口任意。 */
const TRUSTED_ORIGIN = /^http:\/\/(?:127\.0\.0\.1|localhost):(\d{1,5})$/

function httpError(statusCode, message) {
  const err = new Error(message)
  err.statusCode = statusCode
  return err
}

function isTrustedOrigin(origin) {
  if (typeof origin !== 'string') return false
  const match = TRUSTED_ORIGIN.exec(origin)
  if (!match) return false
  const port = Number(match[1])
  return port >= 0 && port <= 65535
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
 * @returns {Promise<{server: import('node:http').Server, port: number, token: string, close: () => Promise<void>}>}
 */
export async function createBridge({ port = 4517, petConfigPath, runCli, log } = {}) {
  if (!petConfigPath) throw new Error('createBridge 需要 petConfigPath')
  const cli = typeof runCli === 'function' ? runCli : async () => ({ ok: false, output: 'runCli 未注入' })
  const logLine = typeof log === 'function' ? log : () => {}
  const token = loadOrCreateToken()

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
    for (const [key, value] of Object.entries(patch || {})) {
      if (!ALLOWED_KEYS.has(key)) continue
      if (value === undefined || value === null) continue
      next[key] = value
    }
    const text = JSON.stringify(next, null, 2)
    // 写前自校验，确保落盘的一定是合法 JSON。
    JSON.parse(text)
    atomicWrite(petConfigPath, `${text}\n`)
    logLine(`配置已更新：${JSON.stringify(next)}`)
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

    // 浏览器带了 Origin 但不是白名单 → 一律 403；没有 Origin 的非浏览器客户端
    // 仍可凭 token 访问（本机脚本 / curl）。
    const origin = requestOrigin(req)
    if (origin !== undefined && !isTrustedOrigin(origin)) {
      sendJson(req, res, 403, { ok: false, error: 'forbidden' })
      return
    }
    // 跨站请求即使 token 泄漏也拒绝。
    if (req.headers['sec-fetch-site'] === 'cross-site') {
      sendJson(req, res, 403, { ok: false, error: 'forbidden' })
      return
    }

    if (req.method === 'GET' && url.pathname === '/health') {
      // 只回版本，不泄漏安装路径。
      sendJson(req, res, 200, { ok: true, version: VERSION })
      return
    }

    if (req.method === 'GET' && url.pathname === '/token') {
      // 非浏览器客户端拿不到 token：必须同时有白名单 Origin 和同源/同站标记。
      if (!isTrustedOrigin(origin) || !TRUSTED_FETCH_SITES.has(req.headers['sec-fetch-site'])) {
        sendJson(req, res, 403, { ok: false, error: 'forbidden' })
        return
      }
      sendJson(req, res, 200, { ok: true, token })
      return
    }

    // 其余路由一律要求 token。
    if (!tokenMatches(req.headers[TOKEN_HEADER], token)) {
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
