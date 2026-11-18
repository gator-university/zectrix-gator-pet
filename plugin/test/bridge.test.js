// 配置桥测试：node:test，无外部依赖。
// 用临时目录隔离 %LOCALAPPDATA%，不碰真实 token / pet-config。
import assert from 'node:assert/strict'
import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { after, before, test } from 'node:test'
import { createBridge } from '../bridge.js'

// 只读负例：普通 http 页面（含回环）不再受信。
const HTTP_ORIGIN = 'http://127.0.0.1:13970'
const LOCALHOST_ORIGIN = 'http://localhost:13970'
// 唯一受信来源：DSH 桌面 App 的自定义 scheme。
const APP_ORIGIN = 'dsh-app://app'

let previousLocalAppData
let tmpDir
let configPath
let bridge
let token

function authHeaders(extra = {}) {
  return { 'x-gator-pet-token': token, ...extra }
}

function request(pathname, { method = 'GET', body, headers = {}, port = bridge.port } = {}) {
  const payload = body === undefined ? null : typeof body === 'string' ? body : JSON.stringify(body)
  const finalHeaders = { ...headers }
  if (payload !== null && finalHeaders['content-type'] === undefined) {
    finalHeaders['content-type'] = 'application/json'
  }
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: '127.0.0.1', port, path: pathname, method, headers: finalHeaders },
      res => {
        let text = ''
        res.setEncoding('utf8')
        res.on('data', chunk => (text += chunk))
        res.on('end', () => {
          let json = null
          try {
            json = text ? JSON.parse(text) : null
          } catch {}
          resolve({ status: res.statusCode, headers: res.headers, text, json })
        })
      },
    )
    req.on('error', reject)
    if (payload !== null) req.write(payload)
    req.end()
  })
}

function resetConfig(config = { target: '192.168.1.50' }) {
  fs.writeFileSync(configPath, `${JSON.stringify(config, null, 2)}\n`, 'utf8')
}

function readConfigFile() {
  return JSON.parse(fs.readFileSync(configPath, 'utf8'))
}

before(async () => {
  previousLocalAppData = process.env.LOCALAPPDATA
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'gator-bridge-'))
  process.env.LOCALAPPDATA = tmpDir
  configPath = path.join(tmpDir, 'pet-config.json')
  resetConfig()
  bridge = await createBridge({
    port: 0,
    petConfigPath: configPath,
    runCli: async args => ({ ok: true, output: `ran ${args.join(' ')}` }),
  })
  token = bridge.token
})

after(async () => {
  await bridge?.close()
  if (previousLocalAppData === undefined) delete process.env.LOCALAPPDATA
  else process.env.LOCALAPPDATA = previousLocalAppData
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

test('无 token 的 /config、/test 返回 401，且不泄漏 target 或安装路径', async () => {
  for (const [method, pathname] of [
    ['GET', '/config'],
    ['POST', '/config'],
    ['POST', '/test'],
  ]) {
    const res = await request(pathname, { method, body: method === 'POST' ? {} : undefined })
    assert.equal(res.status, 401, `${method} ${pathname}`)
    assert.ok(!res.text.includes('192.168.1.50'), '响应不能包含 target')
    assert.ok(!res.text.includes(tmpDir), '响应不能包含本机路径')
    assert.equal(res.headers['access-control-allow-origin'], undefined, '401 不应回 ACAO')
  }
})

test('伪造 Origin 的 /token、/config 返回 403，且不回 ACAO', async () => {
  const tokenRes = await request('/token', {
    headers: { origin: 'https://evil.example', 'sec-fetch-site': 'same-origin' },
  })
  assert.equal(tokenRes.status, 403)
  assert.equal(tokenRes.headers['access-control-allow-origin'], undefined)

  const configRes = await request('/config', {
    headers: authHeaders({ origin: 'https://evil.example', 'sec-fetch-site': 'same-origin' }),
  })
  assert.equal(configRes.status, 403)
  assert.equal(configRes.headers['access-control-allow-origin'], undefined)
})

test('http 回环 origin（127.0.0.1 / localhost）一律拿不到 token', async () => {
  for (const origin of [HTTP_ORIGIN, LOCALHOST_ORIGIN]) {
    for (const fetchSite of ['same-origin', 'same-site', 'cross-site', undefined]) {
      const headers = { origin }
      if (fetchSite) headers['sec-fetch-site'] = fetchSite
      const res = await request('/token', { headers })
      assert.equal(res.status, 403, `${origin} / ${fetchSite}`)
      assert.equal(res.headers['access-control-allow-origin'], undefined)
    }
  }
  // 缺 Origin / null Origin 同样拿不到。
  const noOrigin = await request('/token', { headers: { 'sec-fetch-site': 'same-site' } })
  assert.equal(noOrigin.status, 403)
  const nullOrigin = await request('/token', {
    headers: { origin: 'null', 'sec-fetch-site': 'same-origin' },
  })
  assert.equal(nullOrigin.status, 403)
})

test('/token 只认 dsh-app://app：无需 Sec-Fetch-Site，且每次调用轮换令牌', async () => {
  const before = token
  const first = await request('/token', { headers: { origin: APP_ORIGIN } })
  assert.equal(first.status, 200)
  assert.equal(first.json.ok, true)
  assert.equal(first.headers['access-control-allow-origin'], APP_ORIGIN)
  assert.match(String(first.headers.vary || ''), /origin/i)
  assert.notEqual(first.headers['access-control-allow-origin'], '*')
  assert.match(first.json.token, /^[0-9a-f]{32,}$/i)
  assert.notEqual(first.json.token, before, '每次 /token 都应换新令牌')

  const second = await request('/token', {
    headers: { origin: APP_ORIGIN, 'sec-fetch-site': 'cross-site' },
  })
  assert.equal(second.status, 200)
  assert.notEqual(second.json.token, first.json.token, '旧令牌应在下次 /token 时作废')

  // 旧令牌立即失效，新令牌可用。
  const old = await request('/config', { headers: { 'x-gator-pet-token': first.json.token } })
  assert.equal(old.status, 401)
  const fresh = await request('/config', { headers: { 'x-gator-pet-token': second.json.token } })
  assert.equal(fresh.status, 200)
  token = second.json.token
})

test('带正确 token 的 GET /config 只对 dsh-app://app 生效；http 回环带 token 也 403', async () => {
  const ok = await request('/config', { headers: authHeaders({ origin: APP_ORIGIN }) })
  assert.equal(ok.status, 200)
  assert.equal(ok.json.config.target, '192.168.1.50')
  assert.equal(ok.headers['access-control-allow-origin'], APP_ORIGIN)

  for (const origin of [HTTP_ORIGIN, LOCALHOST_ORIGIN]) {
    const res = await request('/config', {
      headers: authHeaders({ origin, 'sec-fetch-site': 'same-site' }),
    })
    assert.equal(res.status, 403, origin)
    assert.equal(res.headers['access-control-allow-origin'], undefined)
  }
})

test('/health 无需 token，只回 {ok,version}，不含路径', async () => {
  const res = await request('/health')
  assert.equal(res.status, 200)
  assert.deepEqual(Object.keys(res.json).sort(), ['ok', 'version'])
  assert.equal(res.json.ok, true)
  assert.match(res.json.version, /^\d+\.\d+\.\d+/)
  assert.ok(!res.text.includes(tmpDir))
  assert.ok(!res.text.includes('dsh-pet.exe'))
})

test('POST /config 非法 target 返回 400 且不落盘', async () => {
  const invalidTargets = [
    'http://192.168.1.50',
    '192.168.1.50:8080',
    '192.168.1.0/24',
    '::1',
    '',
    'localhost',
    '192.168.1.256',
    '8.8.8.8',
  ]
  resetConfig()
  const beforeText = fs.readFileSync(configPath, 'utf8')
  for (const target of invalidTargets) {
    const res = await request('/config', {
      method: 'POST',
      body: { target },
      headers: authHeaders({ origin: APP_ORIGIN }),
    })
    assert.equal(res.status, 400, `target=${JSON.stringify(target)}`)
  }
  assert.equal(fs.readFileSync(configPath, 'utf8'), beforeText, '校验失败不应写文件')
})

test('POST /config 非法 volume/format/mode 返回 400 且不落盘', async () => {
  resetConfig()
  const beforeText = fs.readFileSync(configPath, 'utf8')
  const invalidPatches = [
    { volume: 101 },
    { volume: 3.5 },
    { volume: '50' },
    { format: 'rgb' },
    { mode: 'turbo' },
    { title_source: 'all' },
    { title_source: true },
  ]
  for (const patch of invalidPatches) {
    const res = await request('/config', {
      method: 'POST',
      body: patch,
      headers: authHeaders({ origin: APP_ORIGIN }),
    })
    assert.equal(res.status, 400, JSON.stringify(patch))
  }
  assert.equal(fs.readFileSync(configPath, 'utf8'), beforeText)
})

test('POST /config 合法值原子落盘并能读回', async () => {
  resetConfig()
  const patch = { target: '10.0.0.9', volume: 55, format: 'gray16', mode: 'calm', title_source: 'prompt' }
  const res = await request('/config', {
    method: 'POST',
    body: patch,
    headers: authHeaders({ origin: APP_ORIGIN }),
  })
  assert.equal(res.status, 200)
  assert.equal(res.json.config.target, '10.0.0.9')
  assert.deepEqual(readConfigFile(), patch)
  // tmp 文件不能残留。
  assert.deepEqual(
    fs.readdirSync(tmpDir).filter(name => name.endsWith('.tmp')),
    [],
  )
  resetConfig()
})

test('POST /config 静默丢弃已下线的 animation / pet_skin 字段', async () => {
  resetConfig()
  const res = await request('/config', {
    method: 'POST',
    body: { volume: 40, animation: false, pet_skin: 'astro' },
    headers: authHeaders({ origin: APP_ORIGIN }),
  })
  assert.equal(res.status, 200)
  assert.equal(res.json.config.volume, 40)
  assert.equal('animation' in res.json.config, false, 'animation 已下线，不应落盘')
  assert.equal('pet_skin' in res.json.config, false, 'pet_skin 已下线，不应落盘')
  const onDisk = fs.readFileSync(configPath, 'utf8')
  assert.ok(!onDisk.includes('animation'))
  assert.ok(!onDisk.includes('pet_skin'))
  resetConfig()
})

test('配置写日志只记 key 名，不记值', async () => {
  resetConfig()
  const logs = []
  const logBridge = await createBridge({
    port: 0,
    petConfigPath: configPath,
    runCli: async () => ({ ok: true, output: '' }),
    log: line => logs.push(line),
  })
  try {
    const res = await request('/config', {
      method: 'POST',
      body: { target: '192.168.1.77', volume: 40 },
      headers: { origin: APP_ORIGIN, 'x-gator-pet-token': logBridge.token },
      port: logBridge.port,
    })
    assert.equal(res.status, 200)
    const line = logs.find(l => l.includes('配置已更新'))
    assert.ok(line, '应记录一条配置更新日志')
    assert.ok(line.includes('target'), '日志应包含键名 target')
    assert.ok(line.includes('volume'), '日志应包含键名 volume')
    assert.ok(!line.includes('192.168.1.77'), '日志不能出现设备地址')
    assert.ok(!line.includes('40'), '日志不能出现配置值')
  } finally {
    await logBridge.close()
    resetConfig()
  }
})

test('POST /test 忽略 body 里的 target，只用已保存的 target', async () => {
  resetConfig({ target: '192.168.1.50' })
  const calls = []
  const testBridge = await createBridge({
    port: 0,
    petConfigPath: configPath,
    runCli: async args => {
      calls.push(args)
      return { ok: true, output: 'device online' }
    },
  })
  try {
    const res = await request('/test', {
      method: 'POST',
      body: { target: '10.9.9.9' },
      headers: { origin: APP_ORIGIN, 'x-gator-pet-token': testBridge.token },
      port: testBridge.port,
    })
    assert.equal(res.status, 200)
    assert.equal(res.json.ok, true)
    assert.deepEqual(calls, [['check', '--target', '192.168.1.50']])
    assert.ok(!JSON.stringify(calls).includes('10.9.9.9'))
  } finally {
    await testBridge.close()
  }
})

test('POST /test 同设备 3 秒冷却：第二次返回 429', async () => {
  resetConfig({ target: '192.168.1.50' })
  const first = await request('/test', {
    method: 'POST',
    body: {},
    headers: authHeaders({ origin: APP_ORIGIN }),
  })
  assert.equal(first.status, 200)
  const second = await request('/test', {
    method: 'POST',
    body: {},
    headers: authHeaders({ origin: APP_ORIGIN }),
  })
  assert.equal(second.status, 429)
  assert.equal(second.json.ok, false)
  assert.match(second.json.error, /秒/)
})

test('token 超过有效期后 401，重新 GET /token 可恢复', async () => {
  const saved = process.env.LOCALAPPDATA
  const ttlDir = fs.mkdtempSync(path.join(os.tmpdir(), 'gator-bridge-ttl-'))
  process.env.LOCALAPPDATA = ttlDir
  let short = null
  try {
    short = await createBridge({
      port: 0,
      petConfigPath: path.join(ttlDir, 'pet-config.json'),
      tokenTtlMs: 40,
      runCli: async () => ({ ok: true, output: '' }),
    })
    const got = await request('/token', { headers: { origin: APP_ORIGIN }, port: short.port })
    assert.equal(got.status, 200)
    const issued = got.json.token
    await new Promise(resolve => setTimeout(resolve, 80))

    const expired = await request('/config', {
      headers: { 'x-gator-pet-token': issued },
      port: short.port,
    })
    assert.equal(expired.status, 401, '过期令牌必须 401')

    const renewed = await request('/token', {
      headers: { origin: APP_ORIGIN },
      port: short.port,
    })
    assert.equal(renewed.status, 200)
    assert.notEqual(renewed.json.token, issued)
    const ok = await request('/config', {
      headers: { 'x-gator-pet-token': renewed.json.token },
      port: short.port,
    })
    assert.equal(ok.status, 200)
  } finally {
    if (short) await short.close()
    process.env.LOCALAPPDATA = saved
    fs.rmSync(ttlDir, { recursive: true, force: true })
  }
})

test('POST 非 application/json 返回 415', async () => {
  const res = await request('/config', {
    method: 'POST',
    body: JSON.stringify({ target: '192.168.1.50' }),
    headers: authHeaders({
      origin: APP_ORIGIN,
      'content-type': 'text/plain',
    }),
  })
  assert.equal(res.status, 415)
})

test('OPTIONS 仅对 dsh-app://app 回 204；http 回环与非白名单 403', async () => {
  const ok = await request('/config', {
    method: 'OPTIONS',
    headers: {
      origin: APP_ORIGIN,
      'access-control-request-method': 'POST',
      'access-control-request-headers': 'content-type, x-gator-pet-token',
    },
  })
  assert.equal(ok.status, 204)
  assert.equal(ok.headers['access-control-allow-origin'], APP_ORIGIN)
  assert.match(String(ok.headers['access-control-allow-headers']), /x-gator-pet-token/)
  assert.match(String(ok.headers['access-control-allow-methods']), /GET/)
  assert.match(String(ok.headers['access-control-allow-methods']), /POST/)
  assert.match(String(ok.headers['access-control-allow-methods']), /OPTIONS/)

  for (const origin of [HTTP_ORIGIN, 'https://evil.example']) {
    const bad = await request('/config', { method: 'OPTIONS', headers: { origin } })
    assert.equal(bad.status, 403, origin)
    assert.equal(bad.headers['access-control-allow-origin'], undefined)
  }
})

test('远程网页 origin 仍然拿不到 token', async () => {
  const r = await request('/token', {
    headers: { origin: 'https://evil.example', 'sec-fetch-site': 'cross-site' },
  })
  assert.equal(r.status, 403)
  assert.equal(r.headers['access-control-allow-origin'], undefined)
})

test('token 落盘在 LOCALAPPDATA 下且为 32+ hex', async () => {
  const tokenFile = path.join(tmpDir, 'zectrix-dsh-pet', 'bridge-token')
  const stored = fs.readFileSync(tokenFile, 'utf8').trim()
  assert.equal(stored, token)
  assert.match(stored, /^[0-9a-f]{32,}$/i)
})
