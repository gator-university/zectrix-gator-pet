// 鳄鱼大宠物 · DSH 插件设置页（浏览器半侧）。
// 注册在 Plugins 页的组合包配置 slot：`plugins.bundle.config`，key 为包名。
window.__ModuleLoader__.load({
  id: 'zectrix-croc-pet',
  factory: require => {
    const React = require('react')
    const h = React.createElement
    const BASE = 'http://127.0.0.1:4517'

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

    function api(path, init) {
      return fetch(BASE + path, init).then(async r => {
        const body = await r.json().catch(() => ({}))
        if (!r.ok || body.ok === false) throw new Error(body.error || '请求失败')
        return body
      })
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
        api('/config', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(cfg || {}),
        })
          .then(() => {
            setNote('已保存，宠物 2 秒内生效')
            load()
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
        { style: { padding: '8px 4px 24px', fontSize: 13, lineHeight: 1.6 } },
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
            { style: input, value: cfg.mode || 'calm', onChange: e => set('mode', e.target.value) },
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
            value: Number(cfg.volume ?? 40),
            onChange: e => set('volume', Number(e.target.value)),
            style: { flex: 1, maxWidth: 180 },
          }),
          h('span', { style: { opacity: 0.6 } }, `${cfg.volume ?? 40}`),
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
        ctx.slots.inject('plugins.bundle.config', () =>
          ctx.slots.register(
            { name: 'plugins.bundle.config', key: 'zectrix-croc-pet' },
            PetSettings,
          ),
        )
      },
    }
  },
})
