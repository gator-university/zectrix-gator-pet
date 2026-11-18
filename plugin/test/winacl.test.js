// winacl 单元测试：全部注入假的 execFile / platform，
// 不真的执行 icacls，也不会改用户目录 ACL。
import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { test } from 'node:test'
import { aclArgs, hardenDataDir, hardenPath, resolveUser } from '../winacl.js'

const USER = 'DESKTOP-1\\alice'

test('aclArgs：目录带 (OI)(CI)F，文件用 :F，且都移除继承', () => {
  assert.deepEqual(aclArgs('C:\\data', USER), [
    'C:\\data',
    '/inheritance:r',
    '/grant:r',
    `${USER}:(OI)(CI)F`,
    '/grant:r',
    'SYSTEM:(OI)(CI)F',
  ])
  assert.deepEqual(aclArgs('C:\\data\\bridge-token', USER, { directory: false }), [
    'C:\\data\\bridge-token',
    '/inheritance:r',
    '/grant:r',
    `${USER}:F`,
    '/grant:r',
    'SYSTEM:F',
  ])
})

test('resolveUser 优先用 whoami，失败退回 os.userInfo().username', async () => {
  const calls = []
  const fromWhoami = await resolveUser({
    platform: 'win32',
    execFile: async (file, args) => {
      calls.push([file, args])
      return { stdout: `${USER}\r\n` }
    },
  })
  assert.equal(fromWhoami, USER)
  assert.deepEqual(calls, [['whoami', []]])

  const fallback = await resolveUser({
    platform: 'win32',
    execFile: async () => {
      throw new Error('whoami 不可用')
    },
  })
  assert.equal(fallback, os.userInfo().username)

  assert.equal(await resolveUser({ platform: 'linux', execFile: async () => assert.fail('不应执行') }), null)
})

test('hardenPath 成功时执行 icacls 并返回 true', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'winacl-dir-'))
  const calls = []
  try {
    const ok = await hardenPath(dir, {
      user: USER,
      platform: 'win32',
      execFile: async (file, args) => {
        calls.push([file, args])
        return { stdout: '已处理 1 个文件。' }
      },
    })
    assert.equal(ok, true)
    assert.equal(calls.length, 1)
    assert.equal(calls[0][0], 'icacls')
    assert.deepEqual(calls[0][1], aclArgs(dir, USER, { directory: true }))
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

test('hardenPath 失败只记警告、不抛异常', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'winacl-dir-'))
  const logs = []
  try {
    const ok = await hardenPath(dir, {
      user: USER,
      platform: 'win32',
      log: line => logs.push(line),
      execFile: async () => {
        throw new Error('拒绝访问')
      },
    })
    assert.equal(ok, false)
    assert.equal(logs.length, 1)
    assert.match(logs[0], /ACL 收紧失败/)
    assert.match(logs[0], /拒绝访问/)
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

test('非 Windows 直接跳过，不执行任何命令', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'winacl-dir-'))
  const calls = []
  try {
    const ok = await hardenDataDir(dir, path.join(dir, 'bridge-token'), {
      platform: 'linux',
      execFile: async (...args) => {
        calls.push(args)
        return { stdout: '' }
      },
    })
    assert.equal(ok, false)
    assert.deepEqual(calls, [])
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

test('hardenDataDir 幂等收紧目录与已存在的 token 文件；缺文件时只处理目录', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'winacl-data-'))
  const tokenFile = path.join(dir, 'bridge-token')
  const calls = []
  const run = () =>
    hardenDataDir(dir, tokenFile, {
      user: USER,
      platform: 'win32',
      execFile: async (file, args) => {
        calls.push([file, args])
        return { stdout: 'ok' }
      },
    })
  try {
    // token 文件不存在：只收紧目录。
    assert.equal(await run(), true)
    assert.equal(calls.length, 1)
    assert.deepEqual(calls[0][1], aclArgs(dir, USER, { directory: true }))

    fs.writeFileSync(tokenFile, 'token\n')
    calls.length = 0
    assert.equal(await run(), true)
    assert.equal(calls.length, 2)
    assert.deepEqual(calls[0][1], aclArgs(dir, USER, { directory: true }))
    assert.deepEqual(calls[1][1], aclArgs(tokenFile, USER, { directory: false }))

    // 再跑一次结果一致（幂等）。
    calls.length = 0
    assert.equal(await run(), true)
    assert.equal(calls.length, 2)
    assert.deepEqual(calls[1][1], aclArgs(tokenFile, USER, { directory: false }))
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

test('hardenDataDir 失败返回 false 并警告，不阻塞调用方', async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'winacl-data-'))
  const logs = []
  try {
    const ok = await hardenDataDir(dir, path.join(dir, 'bridge-token'), {
      user: USER,
      platform: 'win32',
      log: line => logs.push(line),
      execFile: async () => {
        throw new Error('icacls 不存在')
      },
    })
    assert.equal(ok, false)
    assert.ok(logs.some(line => /ACL 收紧失败/.test(line)))
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})
