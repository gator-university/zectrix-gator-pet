// 鳄鱼大宠物 · DSH 宿主编：在 DSH 启动时拉起本机宠物 exe，并给插件设置页提供本地配置桥。
//
// 生命周期：bundle apply 时 spawn 一次 `dsh-pet.exe watch --watch-pid <本进程 pid>`；
// DSH 正常退出时 ctx.effect 的清理函数会 kill 子进程；DSH 被强杀时宿主进程消失，
// exe 每 2 秒自检父进程，10 秒内自己退出并释放设备（见 FR-LIFE-02/03）。
// 配置：HTTP 桥见 bridge.js（只监听 127.0.0.1 + token 鉴权）；这里只负责进程生命周期。
import { spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createBridge } from './bridge.js'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const PORT = 4517
const DATA_DIR = path.join(process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local'), 'zectrix-dsh-pet')
const CONFIG_FILE = path.join(DATA_DIR, 'pet-config.json')
const LOG_FILE = path.join(DATA_DIR, 'pet.log')

function ensureDir() {
  fs.mkdirSync(DATA_DIR, { recursive: true })
}

function log(line) {
  try {
    ensureDir()
    fs.appendFileSync(LOG_FILE, `[${new Date().toISOString()}] ${line}\n`, 'utf8')
  } catch {
    // 日志写不进去不影响主流程。
  }
}

function exePath() {
  const exe = process.platform === 'win32' ? 'dsh-pet.exe' : 'dsh-pet'
  const bundled = path.join(HERE, 'bin', exe)
  if (fs.existsSync(bundled)) return bundled
  // link: 安装（开发机）时插件目录在仓库内，回退到 cargo 的输出目录。
  const release = path.join(HERE, '..', 'target', 'release', exe)
  if (fs.existsSync(release)) return release
  return bundled
}

/** 跑一次 CLI 并收集输出（设置页「测试连接」用）。 */
function runCli(args, timeoutMs = 8000) {
  return new Promise(resolve => {
    const exe = exePath()
    if (!fs.existsSync(exe)) {
      resolve({ ok: false, output: `找不到宠物程序：${exe}` })
      return
    }
    const child = spawn(exe, args, { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
    let out = ''
    const timer = setTimeout(() => {
      try {
        child.kill()
      } catch {}
      resolve({ ok: false, output: `${out}\n（超时 ${timeoutMs / 1000} 秒）`.trim() })
    }, timeoutMs)
    child.stdout.on('data', d => (out += d.toString()))
    child.stderr.on('data', d => (out += d.toString()))
    child.on('error', e => {
      clearTimeout(timer)
      resolve({ ok: false, output: String(e) })
    })
    child.on('close', code => {
      clearTimeout(timer)
      const text = out.trim()
      // 设备被本插件自己的宠物进程占着时，只读检查也会报「忙碌」。
      // 对设置页来说这代表「设备在线」，不算失败。
      const busy = /占用|忙碌|busy/i.test(text)
      if (code !== 0 && busy) {
        resolve({ ok: true, output: `${text}\n（设备在线；当前画面由宠物维护，属正常）` })
        return
      }
      resolve({ ok: code === 0, output: text })
    })
  })
}

function spawnPet() {
  const exe = exePath()
  if (!fs.existsSync(exe)) {
    log(`找不到宠物程序，跳过启动：${exe}`)
    return null
  }
  ensureDir()
  const out = fs.openSync(LOG_FILE, 'a')
  const child = spawn(exe, ['watch', '--watch-pid', String(process.pid)], {
    cwd: path.dirname(exe),
    windowsHide: true,
    stdio: ['ignore', out, out],
  })
  child.on('error', e => log(`启动宠物失败：${e}`))
  child.on('exit', (code, signal) => log(`宠物进程退出：code=${code} signal=${signal}`))
  log(`已启动宠物：${exe}（父进程 ${process.pid}）`)
  return child
}

export const name = 'zectrix-gator-pet'

export function apply(ctx) {
  ctx.effect(() => {
    // 先拉起宠物再开配置桥：即使设置页暂时连不上，宠物也必须跟着 DSH 起来。
    let child = null
    try {
      child = spawnPet()
    } catch (e) {
      log(`启动宠物失败：${e}`)
    }
    let bridge = null
    let closing = false
    createBridge({ port: PORT, petConfigPath: CONFIG_FILE, runCli, log })
      .then(started => {
        bridge = started
        if (closing) {
          started.close()
        } else {
          log(`配置桥已监听 127.0.0.1:${started.port}`)
        }
      })
      .catch(e => log(`配置桥启动失败：${e}`))
    return () => {
      closing = true
      try {
        child?.kill()
      } catch {}
      try {
        bridge?.close()
      } catch {}
      log('DSH 退出：已停止宠物进程与配置桥。')
    }
  }, 'zectrix-gator-pet')
}
