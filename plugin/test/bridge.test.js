// 配置桥测试：node:test，无外部依赖。
// 用临时目录隔离 %LOCALAPPDATA%，不碰真实 token / pet-config。
import assert from 'node:assert/strict'
import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { after, before, test } from 'node:test'
import { createBridge } from '../bridge.js'

const ORIGIN = 'http://127.0.0.1:13970'
const LOCALHOST_ORIGIN = 'http://localhost:13970'

let previousLocalAppData
let tmpDir
let configPath
let bridge
let token

function authHeaders(extra = {}) {
  return { 'x-croc-pet-token': token, ...extra }
}

function request(pathname, { method = 'GET', body, headers = {} } = {}) {
  const payload = body === undefined ? null : typeof body === 'string' ? body : JSON.stringify(body)
  const finalHeaders = { ...headers }
  if (payload !== null && finalHeaders['content-type'] === undefined) {
    finalHeaders['content-type'] = 'application/json'
  }
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: '127.0.0.1', port: bridge.port, path: pathname, method, headers: finalHeaders },
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
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'croc-bridge-'))
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

test('/token 缺失 Sec-Fetch-Site 或缺失 Origin 一律 403', async () => {
  const noFetchSite = await request('/token', { headers: { origin: ORIGIN } })
  assert.equal(noFetchSite.status, 403)

  const crossSite = await request('/token', {
    headers: { origin: ORIGIN, 'sec-fetch-site': 'cross-site' },
  })
  assert.equal(crossSite.status, 403)

  const noOrigin = await request('/token', { headers: { 'sec-fetch-site': 'same-site' } })
  assert.equal(noOrigin.status, 403)

  const nullOrigin = await request('/token', {
    headers: { origin: 'null', 'sec-fetch-site': 'same-origin' },
  })
  assert.equal(nullOrigin.status, 403)
})

test('/token 对白名单 Origin + 同站标记返回 token，ACAO 精确回显且无 *', async () => {
  const res = await request('/token', {
    headers: { origin: ORIGIN, 'sec-fetch-site': 'same-site' },
  })
  assert.equal(res.status, 200)
  assert.equal(res.json.ok, true)
  assert.equal(res.json.token, token)
  assert.equal(res.headers['access-control-allow-origin'], ORIGIN)
  assert.match(String(res.headers.vary || ''), /origin/i)
  assert.notEqual(res.headers['access-control-allow-origin'], '*')
  assert.ok(!res.text.includes('*'))
})

test('带正确 token 的 GET /config 返回 200（127.0.0.1 与 localhost 均可用）', async () => {
  for (const origin of [ORIGIN, LOCALHOST_ORIGIN]) {
    const res = await request('/config', {
      headers: authHeaders({ origin, 'sec-fetch-site': 'same-site' }),
    })
    assert.equal(res.status, 200)
    assert.equal(res.json.config.target, '192.168.1.50')
    assert.equal(res.headers['access-control-allow-origin'], origin)
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
      headers: authHeaders({ origin: ORIGIN, 'sec-fetch-site': 'same-site' }),
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
      headers: authHeaders({ origin: ORIGIN, 'sec-fetch-site': 'same-site' }),
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
    headers: authHeaders({ origin: ORIGIN, 'sec-fetch-site': 'same-site' }),
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
    headers: authHeaders({ origin: ORIGIN, 'sec-fetch-site': 'same-site' }),
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
    const res = await new Promise((resolve, reject) => {
      const req = http.request(
        {
          host: '127.0.0.1',
          port: testBridge.port,
          path: '/test',
          method: 'POST',
          headers: {
            'content-type': 'application/json',
            'x-croc-pet-token': testBridge.token,
          },
        },
        r => {
          let text = ''
          r.on('data', chunk => (text += chunk))
          r.on('end', () => resolve({ status: r.statusCode, json: JSON.parse(text || '{}') }))
        },
      )
      req.on('error', reject)
      req.write(JSON.stringify({ target: '10.9.9.9' }))
      req.end()
    })
    assert.equal(res.status, 200)
    assert.equal(res.json.ok, true)
    assert.deepEqual(calls, [['check', '--target', '192.168.1.50']])
    assert.ok(!JSON.stringify(calls).includes('10.9.9.9'))
  } finally {
    await testBridge.close()
  }
})

test('POST 非 application/json 返回 415', async () => {
  const res = await request('/config', {
    method: 'POST',
    body: JSON.stringify({ target: '192.168.1.50' }),
    headers: authHeaders({
      origin: ORIGIN,
      'sec-fetch-site': 'same-site',
      'content-type': 'text/plain',
    }),
  })
  assert.equal(res.status, 415)
})

test('OPTIONS 仅对白名单回 204，带正确的 allow-headers/methods；非白名单 403', async () => {
  const ok = await request('/config', {
    method: 'OPTIONS',
    headers: {
      origin: ORIGIN,
      'access-control-request-method': 'POST',
      'access-control-request-headers': 'content-type, x-croc-pet-token',
    },
  })
  assert.equal(ok.status, 204)
  assert.equal(ok.headers['access-control-allow-origin'], ORIGIN)
  assert.match(String(ok.headers['access-control-allow-headers']), /x-croc-pet-token/)
  assert.match(String(ok.headers['access-control-allow-methods']), /GET/)
  assert.match(String(ok.headers['access-control-allow-methods']), /POST/)
  assert.match(String(ok.headers['access-control-allow-methods']), /OPTIONS/)

  const bad = await request('/config', {
    method: 'OPTIONS',
    headers: { origin: 'https://evil.example' },
  })
  assert.equal(bad.status, 403)
  assert.equal(bad.headers['access-control-allow-origin'], undefined)
})

test('DSH 桌面 App 的 dsh-app://app origin 可用：cross-site 也放行', async () => {
  const APP = 'dsh-app://app'
  const t = await request('/token', { headers: { origin: APP, 'sec-fetch-site': 'cross-site' } })
  assert.equal(t.status, 200)
  assert.equal(t.headers['access-control-allow-origin'], APP)
  assert.ok(t.json.token)
  const cfg = await request('/config', {
    headers: { origin: APP, 'sec-fetch-site': 'cross-site', 'x-croc-pet-token': t.json.token },
  })
  assert.equal(cfg.status, 200)
  assert.equal(cfg.json.config.target, '192.168.1.50')
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
