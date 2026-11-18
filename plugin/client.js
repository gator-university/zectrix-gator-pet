// 鳄鱼大宠物 · DSH 插件设置页（浏览器半侧）。
// 注册在 Plugins 页的组合包配置 slot：`plugins.bundle.config`，key 为包名。
window.__ModuleLoader__.load({
  id: 'zectrix-croc-pet',
  factory: require => {
    const React = require('react')
    const h = React.createElement
    const BASE = 'http://127.0.0.1:4517'

    // 原生 <select> 的下拉选项由系统绘制，默认白底；在深色主题里会变成白底白字。
    // 这里按 DSH 的 body[data-ds-dark-theme] 显式给 option 上色，并在停用时移除。
    const STYLE_ID = 'zectrix-croc-pet-style'
    const STYLE_TEXT = [
      '.croc-pet-settings select option { background-color: #1d1e21; color: #e9e9ea; }',
      'body:not([data-ds-dark-theme]) .croc-pet-settings select option { background-color: #ffffff; color: #1d1e21; }',
    ].join('\n')
    if (typeof document !== 'undefined' && !document.getElementById(STYLE_ID)) {
      const styleEl = document.createElement('style')
      styleEl.id = STYLE_ID
      styleEl.textContent = STYLE_TEXT
      document.head.appendChild(styleEl)
    }

    const MODES = [
      { value: 'realtime', label: '实时', hint: '状态一变就刷（耗电、闪得勤）' },
      { value: 'balance', label: '平衡', hint: '状态切换立即刷，细节 10 秒合并' },
      { value: 'calm', label: '安静', hint: '细节 30 秒合并，尽量少闪' },
      { value: 'minimal', label: '极简', hint: '细节 120 秒合并，最省电' },
    ]
    const FORMATS = [
      { value: 'mono', label: 'mono（单色，约 1-2 秒/帧）' },
      { value: 'gray4', label: 'gray4（4 级灰，约 3.5 秒/帧）' },
      { value: 'gray16', label: 'gray16（16 级灰，约 5 秒/帧）' },
    ]
    // 标题来源：session 是默认值，prompt 是显式 opt-in（会把提示词首行写进
    // state.json 并显示在设备上）。文案必须把这个代价说清楚。
    const TITLE_SOURCES = [
      { value: 'session', label: '会话标题（默认）', hint: '只读 DSH 的会话标题，不读正文' },
      { value: 'prompt', label: '提示词首行', hint: '会把提示词首行写入 state.json 与设备' },
    ]
    // 默认值以 Rust 侧为准（src/main.rs 的 Settings::default）：
    // pet-config.json 没写这些键时，宠物就是这么跑的，面板不能另立一套默认。
    const DEFAULT_VOLUME = 90
    const DEFAULT_MODE = 'realtime'

    const row = { display: 'flex', alignItems: 'center', gap: 12, margin: '14px 0' }
    const label = { width: 132, flex: 'none', opacity: 0.72, fontSize: 13 }
    const input = {
      flex: 1,
      minWidth: 0,
      padding: '6px 8px',
      borderRadius: 6,
      border: '1px solid rgba(127,127,127,0.35)',
      background: 'transparent',
      color: 'inherit',
      fontSize: 13,
    }
    const button = {
      padding: '6px 14px',
      borderRadius: 6,
      border: '1px solid rgba(127,127,127,0.45)',
      background: 'transparent',
      color: 'inherit',
      cursor: 'pointer',
      fontSize: 13,
    }

    // 配置桥每次安装生成随机 token，只对同源/同站的浏览器页面返回。
    // 首次请求先 GET /token（浏览器会自动带 Origin 与 Sec-Fetch-Site），
    // 之后所有请求都带 X-Croc-Pet-Token。
    let tokenPromise = null
    function getToken() {
      if (!tokenPromise) {
        tokenPromise = fetch(BASE + '/token', { cache: 'no-store' })
          .then(async r => {
            const body = await r.json().catch(() => ({}))
            if (!r.ok || !body.token) throw new Error(body.error || '无法获取设置桥令牌')
            return body.token
          })
          .catch(e => {
            // 失败不缓存，设置页「重试」会重新取。
            tokenPromise = null
            throw new Error(`无法获取设置桥令牌：${String(e.message || e)}`)
          })
      }
      return tokenPromise
    }

    async function api(path, init) {
      const token = await getToken()
      const headers = { ...((init && init.headers) || {}), 'x-croc-pet-token': token }
      const r = await fetch(BASE + path, { ...init, headers })
      const body = await r.json().catch(() => ({}))
      if (!r.ok || body.ok === false) throw new Error(body.error || '请求失败')
      return body
    }

    function Field(props) {
      return h('div', { style: row }, h('div', { style: label }, props.label), props.children)
    }

    function PetSettings() {
      const [cfg, setCfg] = React.useState(null)
      const [err, setErr] = React.useState('')
      const [note, setNote] = React.useState('')
      const [busy, setBusy] = React.useState(false)
      const [testing, setTesting] = React.useState(false)
      const [testOut, setTestOut] = React.useState('')

      const load = React.useCallback(() => {
        setErr('')
        api('/config')
          .then(body => setCfg(body.config || {}))
          .catch(e => setErr(String(e.message || e)))
      }, [])
      React.useEffect(load, [load])

      const set = (key, value) => setCfg(c => ({ ...(c || {}), [key]: value }))

      const save = () => {
        setBusy(true)
        setNote('')
        const body = { ...(cfg || {}) }
        const wantTitle = body.title_source
        api('/config', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(body),
        })
          .then(async () => {
            // 回读一次：设置桥对不认识的键会静默丢弃，这里说清楚哪一项没落盘，
            // 而不是让用户以为「已保存」。
            const next = await api('/config')
            const got = next.config || {}
            setCfg(got)
            if (wantTitle && got.title_source !== wantTitle) {
              setNote(
                `已保存其余项；标题来源没能写入 pet-config（设置桥暂不支持该字段），` +
                  `请在终端执行：dsh-pet set --title-source ${wantTitle}`,
              )
            } else {
              setNote('已保存，宠物 2 秒内生效')
            }
          })
          .catch(e => setErr(String(e.message || e)))
          .finally(() => setBusy(false))
      }

      const test = () => {
        setTesting(true)
        setTestOut('')
        api('/test', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ target: (cfg && cfg.target) || '' }),
        })
          .then(r => setTestOut((r.ok ? '连接成功\n' : '连接失败\n') + (r.output || '')))
          .catch(e => setTestOut(String(e.message || e)))
          .finally(() => setTesting(false))
      }

      if (err && !cfg) {
        return h(
          'div',
          { style: { padding: 20, fontSize: 13, lineHeight: 1.7 } },
          h('strong', null, '鳄鱼大宠物'),
          h('p', { style: { opacity: 0.75 } }, '设置桥不可用：' + err),
          h(
            'p',
            { style: { opacity: 0.75 } },
            '确认 DeepSeek Harness 已启用本插件，并且宠物进程已随它启动。',
          ),
          h('button', { style: button, onClick: load }, '重试'),
        )
      }

      if (!cfg) return h('div', { style: { padding: 20, fontSize: 13 } }, '读取配置中…')

      return h(
        'div',
        { className: 'croc-pet-settings', style: { padding: '8px 4px 24px', fontSize: 13, lineHeight: 1.6 } },
        h('h3', { style: { margin: '4px 0 2px', fontSize: 15 } }, '鳄鱼大宠物'),
        h(
          'p',
          { style: { margin: '0 0 8px', opacity: 0.65 } },
          'DSH 启动时自动拉起宠物；改完保存后 2 秒内生效，不用重启 DSH。',
        ),

        h(
          Field,
          { label: '跟随' },
          h('input', {
            type: 'checkbox',
            checked: cfg.follow !== false,
            onChange: e => set('follow', e.target.checked),
          }),
          h('span', { style: { opacity: 0.6 } }, '关闭后立即释放设备，回普通界面'),
        ),

        h(
          Field,
          { label: '模式' },
          h(
            'select',
            { style: input, value: cfg.mode || DEFAULT_MODE, onChange: e => set('mode', e.target.value) },
            MODES.map(m => h('option', { key: m.value, value: m.value }, `${m.label} · ${m.hint}`)),
          ),
        ),

        h(
          Field,
          { label: '提示音' },
          h('input', {
            type: 'checkbox',
            checked: cfg.sound !== false,
            onChange: e => set('sound', e.target.checked),
          }),
          h('input', {
            type: 'range',
            min: 0,
            max: 100,
            value: Number(cfg.volume ?? DEFAULT_VOLUME),
            onChange: e => set('volume', Number(e.target.value)),
            style: { flex: 1, maxWidth: 180 },
          }),
          h('span', { style: { opacity: 0.6 } }, `${cfg.volume ?? DEFAULT_VOLUME}`),
        ),

        h(
          Field,
          { label: '设备 IP' },
          h('input', {
            style: input,
            value: cfg.target || '',
            placeholder: '例如 192.168.1.162',
            onChange: e => set('target', e.target.value.trim()),
          }),
          h('button', { style: button, disabled: testing, onClick: test }, testing ? '测试中…' : '测试连接'),
        ),

        testOut
          ? h(
              'pre',
              {
                style: {
                  margin: '0 0 14px 144px',
                  padding: 10,
                  borderRadius: 6,
                  background: 'rgba(127,127,127,0.12)',
                  whiteSpace: 'pre-wrap',
                  fontSize: 12,
                  maxHeight: 200,
                  overflow: 'auto',
                },
              },
              testOut,
            )
          : null,

        h(
          Field,
          { label: '刷新格式' },
          h(
            'select',
            { style: input, value: cfg.format || 'mono', onChange: e => set('format', e.target.value) },
            FORMATS.map(f => h('option', { key: f.value, value: f.value }, f.label)),
          ),
        ),

        h(
          Field,
          { label: '标题来源' },
          h(
            'select',
            {
              style: input,
              value: cfg.title_source || 'session',
              onChange: e => set('title_source', e.target.value),
            },
            TITLE_SOURCES.map(t =>
              h('option', { key: t.value, value: t.value }, `${t.label} · ${t.hint}`),
            ),
          ),
        ),
        h(
          'p',
          { style: { margin: '-8px 0 12px 144px', opacity: 0.6, fontSize: 12 } },
          '默认只读 DSH 的会话标题；选「提示词首行」是显式 opt-in，会把提示词首行写入 state.json 并显示在设备上。',
        ),

        h(
          'div',
          { style: { display: 'flex', gap: 10, alignItems: 'center', marginTop: 18 } },
          h('button', { style: { ...button, fontWeight: 600 }, disabled: busy, onClick: save }, busy ? '保存中…' : '保存'),
          h('button', { style: button, onClick: load }, '重新读取'),
          note ? h('span', { style: { opacity: 0.65 } }, note) : null,
          err ? h('span', { style: { opacity: 0.85 } }, err) : null,
        ),
      )
    }

    return {
      inject: ['slots'],
      apply(ctx) {
        ctx.effect(
          () => {
            const dispose = ctx.slots.inject('plugins.bundle.config', () =>
              ctx.slots.register(
                { name: 'plugins.bundle.config', key: 'zectrix-croc-pet' },
                PetSettings,
              ),
            )
            return () => {
              if (typeof dispose === 'function') dispose()
              if (typeof document !== 'undefined') {
                const el = document.getElementById(STYLE_ID)
                if (el) el.remove()
              }
            }
          },
          'zectrix-croc-pet: settings page',
        )
      },
    }
  },
})
