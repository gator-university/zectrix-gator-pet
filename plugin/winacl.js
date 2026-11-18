// Windows 数据目录 ACL 收紧：去掉继承，只留「当前用户 + SYSTEM」完全控制。
//
// 默认情况下 %LOCALAPPDATA%\zectrix-dsh-pet 会继承用户目录的 ACL，
// 本机其他账户（以及 Administrators 组）可能读到 bridge-token、pet.log、
// state.json。这里在配置桥启动时幂等执行一次 icacls：
//
//   icacls <dir> /inheritance:r /grant:r "<user>:(OI)(CI)F" /grant:r "SYSTEM:(OI)(CI)F"
//
// 失败只警告，绝不阻塞宠物/配置桥启动；非 Windows 直接跳过。
// 单元测试通过注入 execFile / platform，不依赖真实 icacls，也不会改用户目录 ACL。
import { execFile as nodeExecFile } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import { promisify } from 'node:util'

const execFileAsync = promisify(nodeExecFile)

const SYSTEM = 'SYSTEM'
const ACL_TIMEOUT_MS = 10_000

/**
 * 构造 icacls 参数。目录带 (OI)(CI) 让新文件/子目录继承；
 * 文件本身继承标志无意义，直接用 :F。
 */
export function aclArgs(target, user, { directory = true } = {}) {
  const grant = directory ? ':(OI)(CI)F' : ':F'
  return [target, '/inheritance:r', '/grant:r', `${user}${grant}`, '/grant:r', `${SYSTEM}${grant}`]
}

/**
 * 当前账户名：优先 `whoami`（域账户是 `DOMAIN\name`），
 * 取不到再退回 `os.userInfo().username`。
 */
export async function resolveUser({ execFile = execFileAsync, platform = process.platform } = {}) {
  if (platform !== 'win32') return null
  try {
    const { stdout } = await execFile('whoami', [], { windowsHide: true, timeout: 5000 })
    const name = String(stdout || '').trim()
    if (name) return name
  } catch {
    // whoami 不存在/被拦截：走 os.userInfo 兜底。
  }
  try {
    return os.userInfo().username || null
  } catch {
    return null
  }
}

/**
 * 收紧单个路径。成功返回 true；失败记一条警告并返回 false（不抛异常）。
 */
export async function hardenPath(
  target,
  { user, execFile = execFileAsync, platform = process.platform, log = () => {} } = {},
) {
  if (platform !== 'win32') return false
  if (!user) return false
  let directory = true
  try {
    directory = fs.statSync(target).isDirectory()
  } catch {
    // 路径不存在（例如首次启动时的 token 文件）：按文件处理，交给 icacls 报错。
  }
  try {
    await execFile('icacls', aclArgs(target, user, { directory }), {
      windowsHide: true,
      timeout: ACL_TIMEOUT_MS,
    })
    return true
  } catch (e) {
    log(`ACL 收紧失败（继续运行）：${target}：${e.message || e}`)
    return false
  }
}

/**
 * 收紧数据目录（含已存在的 token 文件）。幂等：重复执行结果相同。
 * 返回目录是否收紧成功；token 文件不存在不算失败（首次启动会随后创建）。
 */
export async function hardenDataDir(dir, tokenFile, opts = {}) {
  const { platform = process.platform, log = () => {} } = opts
  if (platform !== 'win32') return false
  const user = opts.user || (await resolveUser({ ...opts, platform }))
  if (!user) {
    log('ACL 收紧跳过：取不到当前用户名')
    return false
  }
  const dirOk = await hardenPath(dir, { ...opts, platform, user, log })
  let fileOk = true
  try {
    if (tokenFile && fs.existsSync(tokenFile)) {
      fileOk = await hardenPath(tokenFile, { ...opts, platform, user, log })
    }
  } catch {
    // 检查 token 文件是否存在失败：目录 ACL 已收紧，新文件会继承，忽略。
  }
  return dirOk && fileOk
}
